// Implementation note: hyper's API is not adapted to async/await at all, and there's
// unfortunately a lot of boilerplate here that could be removed once/if it gets reworked.
//
// Additionally, despite the fact that hyper is capable of performing requests to multiple different
// servers through the same `hyper::Client`, we don't use that feature on purpose. The reason is
// that we need to be guaranteed that hyper doesn't re-use an existing connection if we ever reset
// the JSON-RPC request id to a value that might have already been used.

use hyper::body::{Body, HttpBody};
use hyper::client::{Client, HttpConnector};
use hyper::http::uri::Scheme;
use hyper::http::{HeaderMap, HeaderValue};
use jsonrpsee_core::client::CertificateStore;
use jsonrpsee_core::error::GenericTransportError;
use jsonrpsee_core::http_helpers;
use jsonrpsee_core::tracing::{rx_log_from_bytes, tx_log_from_str};
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;
use tower::{Layer, Service, ServiceExt};
use url::Url;

const CONTENT_TYPE_JSON: &str = "application/json";

/// Wrapper over HTTP transport and connector.
#[derive(Debug)]
pub enum HttpBackend<B = Body> {
	/// Hyper client with https connector.
	#[cfg(feature = "__tls")]
	Https(Client<hyper_rustls::HttpsConnector<HttpConnector>, B>),
	/// Hyper client with http connector.
	Http(Client<HttpConnector, B>),
}

impl Clone for HttpBackend {
	fn clone(&self) -> Self {
		match self {
			Self::Http(inner) => Self::Http(inner.clone()),
			#[cfg(feature = "__tls")]
			Self::Https(inner) => Self::Https(inner.clone()),
		}
	}
}

impl<B> tower::Service<hyper::Request<B>> for HttpBackend<B>
where
	B: HttpBody + Send + 'static,
	B::Data: Send,
	B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
	type Response = hyper::Response<Body>;
	type Error = Error;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		match self {
			Self::Http(inner) => inner.poll_ready(ctx),
			#[cfg(feature = "__tls")]
			Self::Https(inner) => inner.poll_ready(ctx),
		}
		.map_err(Into::into)
	}

	fn call(&mut self, req: hyper::Request<B>) -> Self::Future {
		let resp = match self {
			Self::Http(inner) => inner.call(req),
			#[cfg(feature = "__tls")]
			Self::Https(inner) => inner.call(req),
		};

		Box::pin(async move { resp.await.map_err(Into::into) })
	}
}

/// HTTP Transport Client.
#[derive(Debug, Clone)]
pub struct HttpTransportClient<S> {
	/// Target to connect to.
	target: String,
	/// HTTP client
	client: S,
	/// Configurable max request body size
	max_request_size: u32,
	/// Configurable max response body size
	max_response_size: u32,
	/// Max length for logging for requests and responses
	///
	/// Logs bigger than this limit will be truncated.
	max_log_length: u32,
	/// Custom headers to pass with every request.
	headers: HeaderMap,
	/// Replace 'https' with 'http' in links and redirects.
	http_only: bool,
}

impl<B, S> HttpTransportClient<S>
where
	S: Service<hyper::Request<Body>, Response = hyper::Response<B>, Error = Error> + Clone,
	B: HttpBody + Send + 'static,
	B::Data: Send,
	B::Error: Into<Box<dyn StdError + Send + Sync>>,
{
	/// Initializes a new HTTP client.
	pub(crate) fn new<L: Layer<HttpBackend<Body>, Service = S>>(
		max_request_size: u32,
		target: impl AsRef<str>,
		max_response_size: u32,
		cert_store: CertificateStore,
		max_log_length: u32,
		headers: HeaderMap,
		service_builder: tower::ServiceBuilder<L>,
		http_only: bool,
	) -> Result<Self, Error> {
		let mut url = Url::parse(target.as_ref()).map_err(|e| Error::Url(format!("Invalid URL: {e}")))?;
		if url.host_str().is_none() {
			return Err(Error::Url("Invalid host".into()));
		}
		url.set_fragment(None);

		let client = match url.scheme() {
			"http" => HttpBackend::Http(Client::new()),
			#[cfg(feature = "__tls")]
			"https" => {
				let connector = match cert_store {
					#[cfg(feature = "native-tls")]
					CertificateStore::Native => hyper_rustls::HttpsConnectorBuilder::new()
						.with_native_roots()
						.https_or_http()
						.enable_http1()
						.build(),
					#[cfg(feature = "webpki-tls")]
					CertificateStore::WebPki => hyper_rustls::HttpsConnectorBuilder::new()
						.with_webpki_roots()
						.https_or_http()
						.enable_http1()
						.build(),
					_ => return Err(Error::InvalidCertficateStore),
				};
				HttpBackend::Https(Client::builder().build::<_, hyper::Body>(connector))
			}
			_ => {
				#[cfg(feature = "__tls")]
				let err = "URL scheme not supported, expects 'http' or 'https'";
				#[cfg(not(feature = "__tls"))]
				let err = "URL scheme not supported, expects 'http'";
				return Err(Error::Url(err.into()));
			}
		};

		// Cache request headers: 2 default headers, followed by user custom headers.
		// Maintain order for headers in case of duplicate keys:
		// https://datatracker.ietf.org/doc/html/rfc7230#section-3.2.2
		let mut cached_headers = HeaderMap::with_capacity(2 + headers.len());
		cached_headers.insert(hyper::header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE_JSON));
		cached_headers.insert(hyper::header::ACCEPT, HeaderValue::from_static(CONTENT_TYPE_JSON));
		for (key, value) in headers.into_iter() {
			if let Some(key) = key {
				cached_headers.insert(key, value);
			}
		}

		let client = service_builder.service(client);

		Ok(Self {
			target: url.as_str().to_owned(),
			client,
			max_request_size,
			max_response_size,
			max_log_length,
			headers: cached_headers,
			http_only,
		})
	}

	async fn inner_send(&self, body: String) -> Result<hyper::Response<B>, Error> {
		tx_log_from_str(&body, self.max_log_length);

		if body.len() > self.max_request_size as usize {
			return Err(Error::RequestTooLarge);
		}

		let mut target = self.target.clone();
		let mut n = 32; // Maximum redirects

		for _ in (0..n) {
			if self.http_only
				&& let Some(new_target) = target.strip_prefix("https://").map(|s| format!("http://{}", s))
			{
				target = new_target;
			}
			let mut req = hyper::Request::post(target);
			if let Some(headers) = req.headers_mut() {
				*headers = self.headers.clone();
			}
			let req = req.body(From::from(body.clone())).expect("URI and request headers are valid; qed");
			let response = self.client.clone().ready().await?.call(req).await?;

			if response.status().is_redirection()
				&& let Some(location) = response.headers().get(hyper::header::LOCATION)
			{
				match location.to_str() {
					Ok(location) => {
						target = location.to_owned();
					}
					Err(e) => {
						return Err(Error::Url(format!("Invalid redirect URL: {e}")));
					}
				}
			} else if response.status().is_success() {
				return Ok(response);
			} else {
				return Err(Error::RequestFailure { status_code: response.status().into() });
			}
		}

		Err(Error::TooManyRedirects)
	}

	/// Send serialized message and wait until all bytes from the HTTP message body have been read.
	pub(crate) async fn send_and_read_body(&self, body: String) -> Result<Vec<u8>, Error> {
		let response = self.inner_send(body).await?;
		let (parts, body) = response.into_parts();
		let (body, _) = http_helpers::read_body(&parts.headers, body, self.max_response_size).await?;

		rx_log_from_bytes(&body, self.max_log_length);

		Ok(body)
	}

	/// Send serialized message without reading the HTTP message body.
	pub(crate) async fn send(&self, body: String) -> Result<(), Error> {
		let _ = self.inner_send(body).await?;

		Ok(())
	}
}

/// Error that can happen during a request.
#[derive(Debug, Error)]
pub enum Error {
	/// Invalid URL.
	#[error("Invalid Url: {0}")]
	Url(String),

	/// Error during the HTTP request, including networking errors and HTTP protocol errors.
	#[error("HTTP error: {0}")]
	Http(Box<dyn std::error::Error + Send + Sync>),

	/// Server returned a non-success status code.
	#[error("Server returned an error status code: {:?}", status_code)]
	RequestFailure {
		/// Status code returned by the server.
		status_code: u16,
	},

	/// Request body too large.
	#[error("The request body was too large")]
	RequestTooLarge,

	/// Malformed request.
	#[error("Malformed request")]
	Malformed,

	/// Invalid certificate store.
	#[error("Invalid certificate store")]
	InvalidCertficateStore,

	/// Too many redirects.
	#[error("Too many redirects")]
	TooManyRedirects,
}

impl From<GenericTransportError> for Error {
	fn from(err: GenericTransportError) -> Self {
		match err {
			GenericTransportError::TooLarge => Self::RequestTooLarge,
			GenericTransportError::Malformed => Self::Malformed,
			GenericTransportError::Inner(e) => Self::Http(e.into()),
		}
	}
}

impl From<hyper::Error> for Error {
	fn from(err: hyper::Error) -> Self {
		Self::Http(Box::new(err))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use jsonrpsee_core::client::CertificateStore;

	#[test]
	fn invalid_http_url_rejected() {
		let err = HttpTransportClient::new(
			80,
			"ws://localhost:9933",
			80,
			CertificateStore::Native,
			80,
			HeaderMap::new(),
			tower::ServiceBuilder::new(),
			false,
		)
		.unwrap_err();
		assert!(matches!(err, Error::Url(_)));
	}

	#[cfg(feature = "__tls")]
	#[test]
	fn https_works() {
		let client = HttpTransportClient::new(
			80,
			"https://localhost",
			80,
			CertificateStore::Native,
			80,
			HeaderMap::new(),
			tower::ServiceBuilder::new(),
			false,
		)
		.unwrap();
		assert_eq!(&client.target, "https://localhost/");
	}

	#[cfg(not(feature = "__tls"))]
	#[test]
	fn https_fails_without_tls_feature() {
		let err = HttpTransportClient::new(
			80,
			"https://localhost:9933",
			80,
			CertificateStore::Native,
			80,
			HeaderMap::new(),
			tower::ServiceBuilder::new(),
			false,
		)
		.unwrap_err();
		assert!(matches!(err, Error::Url(_)));
	}

	#[test]
	fn faulty_port() {
		let err = HttpTransportClient::new(
			80,
			"http://localhost:-43",
			80,
			CertificateStore::Native,
			80,
			HeaderMap::new(),
			tower::ServiceBuilder::new(),
			false,
		)
		.unwrap_err();
		assert!(matches!(err, Error::Url(_)));
		let err = HttpTransportClient::new(
			80,
			"http://localhost:-99999",
			80,
			CertificateStore::Native,
			80,
			HeaderMap::new(),
			tower::ServiceBuilder::new(),
			false,
		)
		.unwrap_err();
		assert!(matches!(err, Error::Url(_)));
	}

	#[test]
	fn url_with_path_works() {
		let client = HttpTransportClient::new(
			1337,
			"http://localhost/my-special-path",
			1337,
			CertificateStore::Native,
			80,
			HeaderMap::new(),
			tower::ServiceBuilder::new(),
			false,
		)
		.unwrap();
		assert_eq!(&client.target, "http://localhost/my-special-path");
	}

	#[test]
	fn url_with_query_works() {
		let client = HttpTransportClient::new(
			u32::MAX,
			"http://127.0.0.1/my?name1=value1&name2=value2",
			u32::MAX,
			CertificateStore::Native,
			80,
			HeaderMap::new(),
			tower::ServiceBuilder::new(),
			false,
		)
		.unwrap();
		assert_eq!(&client.target, "http://127.0.0.1/my?name1=value1&name2=value2");
	}

	#[test]
	fn url_with_fragment_is_ignored() {
		let client = HttpTransportClient::new(
			999,
			"http://127.0.0.1/my.htm#ignore",
			999,
			CertificateStore::Native,
			80,
			HeaderMap::new(),
			tower::ServiceBuilder::new(),
			false,
		)
		.unwrap();
		assert_eq!(&client.target, "http://127.0.0.1/my.htm");
	}

	#[test]
	fn url_default_port_is_omitted() {
		let client = HttpTransportClient::new(
			999,
			"http://127.0.0.1:80",
			999,
			CertificateStore::Native,
			80,
			HeaderMap::new(),
			tower::ServiceBuilder::new(),
			false,
		)
		.unwrap();
		assert_eq!(&client.target, "http://127.0.0.1/");
	}

	#[cfg(feature = "__tls")]
	#[test]
	fn https_custom_port_works() {
		let client = HttpTransportClient::new(
			80,
			"https://localhost:9999",
			80,
			CertificateStore::Native,
			80,
			HeaderMap::new(),
			tower::ServiceBuilder::new(),
			false,
		)
		.unwrap();
		assert_eq!(&client.target, "https://localhost:9999/");
	}

	#[test]
	fn http_custom_port_works() {
		let client = HttpTransportClient::new(
			80,
			"http://localhost:9999",
			80,
			CertificateStore::Native,
			80,
			HeaderMap::new(),
			tower::ServiceBuilder::new(),
			false,
		)
		.unwrap();
		assert_eq!(&client.target, "http://localhost:9999/");
	}

	#[tokio::test]
	async fn request_limit_works() {
		let eighty_bytes_limit = 80;
		let fifty_bytes_limit = 50;

		let client = HttpTransportClient::new(
			eighty_bytes_limit,
			"http://localhost:9933",
			fifty_bytes_limit,
			CertificateStore::Native,
			99,
			HeaderMap::new(),
			tower::ServiceBuilder::new(),
			false,
		)
		.unwrap();
		assert_eq!(client.max_request_size, eighty_bytes_limit);
		assert_eq!(client.max_response_size, fifty_bytes_limit);

		let body = "a".repeat(81);
		assert_eq!(body.len(), 81);
		let response = client.send(body).await.unwrap_err();
		assert!(matches!(response, Error::RequestTooLarge));
	}
}
