// Copyright 2023 Oxide Computer Company
//! Automated testing facilities.  These are intended for use both by this crate
//! and dependents of this crate.

use camino::Utf8PathBuf;
use chrono::DateTime;
use chrono::Utc;
use http::method::Method;
use http_body_util::BodyExt as _;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::Uri;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use std::convert::TryFrom;
use std::fmt::Debug;
use std::fs;
use std::iter::Iterator;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use crate::api_description::ApiDescription;
use crate::body::Body;
use crate::config::ConfigDropshot;
use crate::error::HttpErrorResponseBody;
use crate::http_util::CONTENT_TYPE_URL_ENCODED;
use crate::pagination::ResultsPage;
use crate::server::{HttpServer, ServerBuilder, ServerContext};
use tracing::info;

enum AllowedValue<'a> {
    Any,
    OneOf(&'a [&'a str]),
}

struct AllowedHeader<'a> {
    name: &'a str,
    value: AllowedValue<'a>,
}

impl<'a> AllowedHeader<'a> {
    const fn new(name: &'a str) -> Self {
        Self { name, value: AllowedValue::Any }
    }
}

pub const TEST_HEADER_1: &str = "x-dropshot-test-header-1";
pub const TEST_HEADER_2: &str = "x-dropshot-test-header-2";

// List of allowed HTTP headers in responses.
// Used to make sure we don't leak headers unexpectedly.
const ALLOWED_HEADERS: [AllowedHeader<'static>; 8] = [
    AllowedHeader::new("content-length"),
    AllowedHeader::new("content-type"),
    AllowedHeader::new("date"),
    AllowedHeader::new("location"),
    AllowedHeader::new("x-request-id"),
    AllowedHeader {
        name: "transfer-encoding",
        value: AllowedValue::OneOf(&["chunked"]),
    },
    AllowedHeader::new(TEST_HEADER_1),
    AllowedHeader::new(TEST_HEADER_2),
];

/// ClientTestContext encapsulates several facilities associated with using an
/// HTTP client for testing.
#[derive(Clone)]
pub struct ClientTestContext {
    /// actual bind address of the HTTP server under test
    pub bind_address: SocketAddr,
    /// HTTP client, used for making requests against the test server
    pub client: Client<HttpConnector, crate::Body>,
}

// Macro to generate methods on `ClientTestContext` and
// `TypedErrorClientTestContext` that have identical implementations but
// differing error types. The type for which this macro is invoked must have a
// `async fn make_request_with_request` method, as the generated methods call
// that.
macro_rules! impl_client_test_context {
    { type Error = $Error:ty; } => {
        /// Execute an HTTP request against the test server and perform basic
        /// validation of the result, including:
        ///
        /// - the expected status code
        /// - the expected Date header (within reason)
        /// - for error responses: the expected body content
        /// - header names are in allowed list
        /// - any other semantics that can be verified in general
        ///
        /// The body will be JSON encoded.
        pub async fn make_request<RequestBodyType: Serialize + Debug>(
            &self,
            method: Method,
            path: &str,
            request_body: Option<RequestBodyType>,
            expected_status: StatusCode,
        ) -> Result<Response<Body>, $Error> {
            let body = match request_body {
                None => Body::empty(),
                Some(input) => serde_json::to_string(&input).unwrap().into(),
            };

            self.make_request_with_body(method, path, body, expected_status).await
        }

        /// Execute an HTTP request against the test server and perform basic
        /// validation of the result like [`ClientTestContext::make_request`],
        /// but with a content type of "application/x-www-form-urlencoded".
        pub async fn make_request_url_encoded<
            RequestBodyType: Serialize + Debug,
        >(
            &self,
            method: Method,
            path: &str,
            request_body: Option<RequestBodyType>,
            expected_status: StatusCode,
        ) -> Result<Response<Body>, $Error> {
            let body: Body = match request_body {
                None => Body::empty(),
                Some(input) => serde_urlencoded::to_string(&input).unwrap().into(),
            };

            self.make_request_with_body_url_encoded(
                method,
                path,
                body,
                expected_status,
            )
            .await
        }

        pub async fn make_request_no_body(
            &self,
            method: Method,
            path: &str,
            expected_status: StatusCode,
        ) -> Result<Response<Body>, $Error> {
            self.make_request_with_body(
                method,
                path,
                Body::empty(),
                expected_status,
            )
            .await
        }

        /// Fetches a resource for which we expect to get an error response.
        pub async fn make_request_error(
            &self,
            method: Method,
            path: &str,
            expected_status: StatusCode,
        ) -> $Error {
            self.make_request_with_body(method, path, "".into(), expected_status)
                .await
                .unwrap_err()
        }

        /// Fetches a resource for which we expect to get an error response.
        /// TODO-cleanup the make_request_error* interfaces are slightly
        /// different than the non-error ones (and probably a bit more
        /// ergonomic).
        pub async fn make_request_error_body<T: Serialize + Debug>(
            &self,
            method: Method,
            path: &str,
            body: T,
            expected_status: StatusCode,
        ) -> $Error {
            self.make_request(method, path, Some(body), expected_status)
                .await
                .unwrap_err()
        }

        pub async fn make_request_with_body(
            &self,
            method: Method,
            path: &str,
            body: Body,
            expected_status: StatusCode,
        ) -> Result<Response<Body>, $Error> {
            let uri = self.url(path);
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .body(body)
                .expect("attempted to construct invalid request");
            self.make_request_with_request(request, expected_status).await
        }

        pub async fn make_request_with_body_url_encoded(
            &self,
            method: Method,
            path: &str,
            body: Body,
            expected_status: StatusCode,
        ) -> Result<Response<Body>, $Error> {
            let uri = self.url(path);
            let request = Request::builder()
                .method(method)
                .header(http::header::CONTENT_TYPE, CONTENT_TYPE_URL_ENCODED)
                .uri(uri)
                .body(body)
                .expect("attempted to construct invalid request");
            self.make_request_with_request(request, expected_status).await
        }
    }
}

impl ClientTestContext {
    /// Set up a `ClientTestContext` for running tests against an API server.
    pub fn new(server_addr: SocketAddr) -> ClientTestContext {
        ClientTestContext { bind_address: server_addr,             client: Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(HttpConnector::new()), }
    }

    /// Given the path for an API endpoint (e.g., "/projects"), return a Uri that
    /// we can use to invoke this endpoint from the client.  This essentially
    /// appends the path to a base URL constructed from the server's IP address
    /// and port.
    pub fn url(&self, path: &str) -> Uri {
        Uri::builder()
            .scheme("http")
            .authority(format!("{}", self.bind_address).as_str())
            .path_and_query(path)
            .build()
            .expect("attempted to construct invalid URI")
    }

    /// Temporarily configures the client to expect `E`-typed error responses,
    /// rather than [`dropshot::HttpError`][crate::HttpError] error responses.
    ///
    /// `ClientTestContext` expects that all error responses are
    /// `dropshot::HttpError`. For testing APIs that return other error types, this
    /// method borrows the `ClientTestContext` and returns a
    /// [`TypedErrorClientTestContext`]`<E>`, which expects `E`-typed error
    /// responses from  the API server, instead.
    ///
    /// Because the `TypedClientErrorTestContext` is a borrowed wrapper around
    /// the underlying `ClientTestContext`, the same client may be used to
    /// expect multiple error types from different endpoints of the same API.
    pub fn with_error_type<E>(&self) -> TypedErrorClientTestContext<'_, E>
    where
        E: DeserializeOwned + std::fmt::Debug,
    {
        TypedErrorClientTestContext { client: self, _error: PhantomData }
    }

    impl_client_test_context! {
        type Error = HttpErrorResponseBody;
    }

    pub async fn make_request_with_request(
        &self,
        request: Request<Body>,
        expected_status: StatusCode,
    ) -> Result<Response<Body>, HttpErrorResponseBody> {
        self.make_request_inner::<HttpErrorResponseBody>(
            request,
            expected_status,
        )
        .await
        .map_err(|(request_id_header, error_body)| {
            assert_eq!(error_body.request_id, request_id_header);
            error_body
        })
    }

    /// Internal implementation detail of `make_request_with_request` and
    /// `make_request_with_error` that's generic over the error type, and
    /// returns both the parsed error and the request ID header in the error
    /// case.
    async fn make_request_inner<E>(
        &self,
        request: Request<Body>,
        expected_status: StatusCode,
    ) -> Result<Response<Body>, (String, E)>
    where
        E: DeserializeOwned + std::fmt::Debug,
    {
        let time_before = chrono::offset::Utc::now().timestamp();
        info!(
            method = %request.method(),
            uri = %request.uri(),
            body = ?&request.body(),
            "client request"
        );

        let mut response = self
            .client
            .request(request)
            .await
            .expect("failed to make request to server");

        // Check that we got the expected response code.
        let status = response.status();
        info!(status = ?status, "client received response");
        assert_eq!(expected_status, status);

        // Check that we didn't have any unexpected headers.  This could be more
        // efficient by putting the allowed headers into a BTree or Hash, but
        // right now the structure is tiny and it's convenient to have it
        // statically-defined above.
        let headers = response.headers();
        for (header_name, header_value) in headers {
            let mut okay = false;
            for allowed_header in ALLOWED_HEADERS.iter() {
                if header_name == allowed_header.name {
                    match allowed_header.value {
                        AllowedValue::Any => {
                            okay = true;
                        }
                        AllowedValue::OneOf(allowed_values) => {
                            let header = header_value
                                .to_str()
                                .expect("Cannot turn header value to string");
                            okay = allowed_values.contains(&header);
                        }
                    }
                    break;
                }
            }

            if !okay {
                panic!("header name not in allowed list: \"{}\"", header_name);
            }
        }

        // Sanity check the Date header in the response.  Note that this
        // assertion will fail spuriously in the unlikely event that the system
        // clock is adjusted backwards in between when we sent the request and
        // when we received the response, but we consider that case unlikely
        // enough to be worth doing this check anyway.  (We'll try to check for
        // the clock reset condition, too, but we cannot catch all cases that
        // would cause the Date header check to be incorrect.)
        //
        // Note that the Date header typically only has precision down to one
        // second, so we don't want to try to do a more precise comparison.
        let time_after = chrono::offset::Utc::now().timestamp();
        let date_header = headers
            .get(http::header::DATE)
            .expect("missing Date header")
            .to_str()
            .expect("non-ASCII characters in Date header");
        let time_request = chrono::DateTime::parse_from_rfc2822(date_header)
            .expect("unable to parse server's Date header");
        assert!(
            time_before <= time_after,
            "time obviously went backwards during the test"
        );
        assert!(time_request.timestamp() >= time_before - 1);
        assert!(time_request.timestamp() <= time_after + 1);

        // Validate that we have a request id header.
        // TODO-coverage check that it's unique among requests we've issued
        let request_id_header = headers
            .get(crate::HEADER_REQUEST_ID)
            .expect("missing request id header")
            .to_str()
            .expect("non-ASCII characters in request id")
            .to_string();

        // For "204 No Content" responses, validate that we got no content in
        // the body.
        if status == StatusCode::NO_CONTENT {
            let body_bytes = read_bytes(&mut response).await;
            assert_eq!(0, body_bytes.len());
        }

        let mut response = response.map(Body::wrap);
        // If this was a successful response, there's nothing else to check
        // here.  Return the response so the caller can validate the content if
        // they want.
        if !status.is_client_error() && !status.is_server_error() {
            return Ok(response);
        }

        // We got an error.  Parse the response body to make sure it's valid and
        // then return that.
        let error_body: E = read_json(&mut response).await;
        info!(error_body = ?error_body, "client error");
        Err((request_id_header, error_body))
    }
}

/// A `ClientTestContext` wrapper which expects that the API server under test
/// will return `E`-typed error responses.
///
/// `ClientTestContext` expects that all error responses are
/// `dropshot::HttpError`. For testing APIs that return other error types, the
/// [`ClientTestContext::with_error_type`]`<E>` method allows constructing a
/// `TypedErrorClientTestContext`, which expects `E`-typed error responses from
/// the API server, instead.
///
/// In order to make API requests with a `TypedErrorTestContext`, `E` must
/// implement [`DeserializeOwned`] and [`std::fmt::Debug`].
pub struct TypedErrorClientTestContext<'client, E> {
    pub client: &'client ClientTestContext,
    _error: PhantomData<fn(E)>,
}

// A manual implementation of `Clone` is required to avoid requiring that `E:
// Clone`, as this type does not actually contain an `E`. Unfortunately,
// `#[derive(Clone)]` is not aware of `PhantomData`, and will always require
// that all of a generic type's type parameters are `Clone`, even if the type
// does not actually contain them.
impl<E> Clone for TypedErrorClientTestContext<'_, E> {
    fn clone(&self) -> Self {
        Self { client: self.client, _error: PhantomData }
    }
}

impl<E> TypedErrorClientTestContext<'_, E>
where
    E: DeserializeOwned + std::fmt::Debug,
{
    /// Given the path for an API endpoint (e.g., "/projects"), return a Uri that
    /// we can use to invoke this endpoint from the client.  This essentially
    /// appends the path to a base URL constructed from the server's IP address
    /// and port.
    pub fn url(&self, path: &str) -> Uri {
        self.client.url(path)
    }

    // Generate all the methods with identical implementations to
    // `ClientTestContext`.
    impl_client_test_context! {
        type Error = E;
    }

    pub async fn make_request_with_request(
        &self,
        request: Request<Body>,
        expected_status: StatusCode,
    ) -> Result<Response<Body>, E> {
        self.client
            .make_request_inner::<E>(request, expected_status)
            .await
            .map_err(|(_, error_body)| error_body)
    }
}

/// TestContext is used to manage a matched server and client for the common
/// test-case pattern of setting up a logger, server, and client and tearing them
/// all down at the end.
pub struct TestContext<Context: ServerContext> {
    pub client_testctx: ClientTestContext,
    pub server: HttpServer<Context>,
}

impl<Context: ServerContext> TestContext<Context> {
    /// Instantiate a TestContext by creating a new Dropshot server with `api`,
    /// `private`, `config_dropshot`, and `log`, and then creating a
    /// `ClientTestContext` with whatever address the server wound up bound to.
    ///
    /// This interfaces requires that `config_dropshot.bind_address.port()` be
    /// `0` to allow the server to bind to any available port.  This is necessary
    /// in order for it to be used concurrently by many tests.
    pub fn new(
        api: ApiDescription<Context>,
        private: Context,
        config_dropshot: &ConfigDropshot,
    ) -> TestContext<Context> {
        assert_eq!(
            0,
            config_dropshot.bind_address.port(),
            "test suite only supports binding on port 0 (any available port)"
        );

        // Set up the server itself.
        let server = ServerBuilder::new(api, private, None )
            .config(config_dropshot.clone())
            .start()
            .unwrap();

        let server_addr = server.local_addr();
        let client_testctx = ClientTestContext::new(server_addr);

        TestContext { client_testctx, server }
    }

    /// Requests a graceful shutdown of the server, waits for that to complete,
    /// and cleans up the associated log context (if any).
    // TODO-cleanup: is there an async analog to Drop?
    pub async fn teardown(self) {
        self.server.close().await.expect("server stopped with an error");
    }
}

/// Given a Hyper Response whose body is expected to represent newline-separated
/// JSON, each line of which is expected to be parseable via Serde as type T,
/// asynchronously read the body of the response and parse it accordingly,
/// returning a vector of T.
pub async fn read_ndjson<T: DeserializeOwned>(
    response: &mut Response<Body>,
) -> Vec<T> {
    let headers = response.headers();
    assert_eq!(
        crate::CONTENT_TYPE_NDJSON,
        headers.get(http::header::CONTENT_TYPE).expect("missing content-type")
    );
    let body_string = read_string(response).await;

    // TODO-cleanup: Consider using serde_json::StreamDeserializer or maybe
    // implementing an NDJSON-based Serde type?
    // TODO-correctness: If we don't do that, this should split on (\r?\n)+ to
    // be NDJSON-compatible.
    body_string
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .expect("failed to parse server body as expected type")
        })
        .collect::<Vec<T>>()
}

/// Given a Hyper response whose body is expected to be a JSON object that should
/// be parseable via Serde as type T, asynchronously read the body of the
/// response and parse it, returning an instance of T.
pub async fn read_json<T: DeserializeOwned>(
    response: &mut Response<Body>,
) -> T {
    let headers = response.headers();
    assert_eq!(
        crate::CONTENT_TYPE_JSON,
        headers.get(http::header::CONTENT_TYPE).expect("missing content-type")
    );
    let body_bytes = read_bytes(response).await;
    serde_json::from_slice(body_bytes.as_ref())
        .expect("failed to parse server body as expected type")
}

/// Given a Hyper Response whose body is expected to be a UTF-8-encoded string,
/// asynchronously read the body.
pub async fn read_string(response: &mut Response<Body>) -> String {
    let body_bytes = read_bytes(response).await;
    String::from_utf8(body_bytes.as_ref().into())
        .expect("response contained non-UTF-8 bytes")
}

async fn read_bytes<B>(response: &mut Response<B>) -> hyper::body::Bytes
where
    B: hyper::body::Body + Unpin,
    B::Error: std::fmt::Debug,
{
    response.body_mut().collect().await.expect("error reading body").to_bytes()
}

/// Given a Hyper Response, extract and parse the Content-Length header.
pub fn read_content_length(response: &Response<Body>) -> usize {
    response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// Fetches a single resource from the API.
pub async fn object_get<T: DeserializeOwned>(
    client: &ClientTestContext,
    object_url: &str,
) -> T {
    let mut response = client
        .make_request_with_body(
            Method::GET,
            &object_url,
            "".into(),
            StatusCode::OK,
        )
        .await
        .unwrap();
    read_json::<T>(&mut response).await
}

/// Fetches a list of resources from the API.
pub async fn objects_list<T: DeserializeOwned>(
    client: &ClientTestContext,
    list_url: &str,
) -> Vec<T> {
    let mut response = client
        .make_request_with_body(
            Method::GET,
            &list_url,
            "".into(),
            StatusCode::OK,
        )
        .await
        .unwrap();
    read_ndjson::<T>(&mut response).await
}

/// Fetches a page of resources from the API.
pub async fn objects_list_page<ItemType>(
    client: &ClientTestContext,
    list_url: &str,
) -> ResultsPage<ItemType>
where
    ItemType: DeserializeOwned,
{
    let mut response = client
        .make_request_with_body(
            Method::GET,
            &list_url,
            "".into(),
            StatusCode::OK,
        )
        .await
        .unwrap();

    read_json::<ResultsPage<ItemType>>(&mut response).await
}

/// Issues an HTTP POST to the specified collection URL to create an object.
pub async fn objects_post<S: Serialize + Debug, T: DeserializeOwned>(
    client: &ClientTestContext,
    collection_url: &str,
    input: S,
) -> T {
    let mut response = client
        .make_request(
            Method::POST,
            &collection_url,
            Some(input),
            StatusCode::CREATED,
        )
        .await
        .unwrap();
    read_json::<T>(&mut response).await
}

/// Issues an HTTP PUT to the specified collection URL to update an object.
pub async fn object_put<S: Serialize + Debug, T: DeserializeOwned>(
    client: &ClientTestContext,
    object_url: &str,
    input: S,
    status: StatusCode,
) {
    client
        .make_request(Method::PUT, &object_url, Some(input), status)
        .await
        .unwrap();
}

/// Issues an HTTP DELETE to the specified object URL to delete an object.
pub async fn object_delete(client: &ClientTestContext, object_url: &str) {
    client
        .make_request_no_body(
            Method::DELETE,
            &object_url,
            StatusCode::NO_CONTENT,
        )
        .await
        .unwrap();
}

/// Iterate a paginated collection.
pub async fn iter_collection<T: Clone + DeserializeOwned>(
    client: &ClientTestContext,
    collection_url: &str,
    initial_params: &str,
    limit: usize,
) -> (Vec<T>, usize) {
    let mut page = objects_list_page::<T>(
        &client,
        &format!("{}?limit={}&{}", collection_url, limit, initial_params),
    )
    .await;
    assert!(page.items.len() <= limit);

    let mut rv = page.items.clone();
    let mut npages = 1;

    while let Some(token) = page.next_page {
        page = objects_list_page::<T>(
            &client,
            &format!("{}?limit={}&page_token={}", collection_url, limit, token),
        )
        .await;
        assert!(page.items.len() <= limit);
        rv.extend_from_slice(&page.items);
        npages += 1
    }

    (rv, npages)
}

static TEST_SUITE_LOGGER_ID: AtomicU32 = AtomicU32::new(0);

/// Returns a unique prefix for log files generated by other processes.
///
/// The return value is a combination of:
///
/// - a directory path within which the logs should go
/// - a unique string prefix that identifies this test and process ID, along
///   with a counter that is atomically bumped each time this function is
///   called.
pub fn log_prefix_for_test(test_name: &str) -> (Utf8PathBuf, String) {
    let arg0 = {
        let arg0path = Utf8PathBuf::from(std::env::args().next().unwrap());
        arg0path.file_name().unwrap().to_owned()
    };

    // We currently write all log files to the temporary directory.
    let dir = Utf8PathBuf::try_from(std::env::temp_dir())
        .expect("temp dir is valid UTF-8");
    let id = TEST_SUITE_LOGGER_ID.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    (dir, format!("{arg0}-{test_name}.{pid}.{id}"))
}

/// Returns a unique path name in a temporary directory that includes the given
/// `test_name`.
pub fn log_file_for_test(test_name: &str) -> Utf8PathBuf {
    let (mut dir, prefix) = log_prefix_for_test(test_name);
    dir.push(format!("{prefix}.log"));
    dir
}

/// Load an object of type `T` (usually a hunk of configuration) from the string
/// `contents`.  `label` is used as an identifying string in a log message.  It
/// should be unique for each test.
pub fn read_config<T: DeserializeOwned + Debug>(
    label: &str,
    contents: &str,
) -> Result<T, toml::de::Error> {
    let result = toml::from_str(contents);
    eprintln!("config \"{}\": {:?}", label, result);
    result
}

// Bunyan testing facilities

/// Represents a Bunyan log record.  This form does not support any non-standard
/// fields.  "level" is not yet supported because we don't (yet) need it.
#[derive(Deserialize)]
pub struct BunyanLogRecord {
    pub time: DateTime<Utc>,
    pub name: String,
    pub hostname: String,
    pub pid: u32,
    pub msg: String,
    pub v: usize,
}

/// Read a file containing a Bunyan-format log, returning an array of records.
pub fn read_bunyan_log(logpath: &Path) -> Vec<BunyanLogRecord> {
    let log_contents = fs::read_to_string(logpath).unwrap();
    log_contents
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<BunyanLogRecord>(line).unwrap())
        .collect::<Vec<BunyanLogRecord>>()
}

/// Analogous to a BunyanLogRecord, but where all fields are optional.
pub struct BunyanLogRecordSpec {
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub pid: Option<u32>,
    pub v: Option<usize>,
}

/// Verify that the key fields of the log records emitted by `iter` match the
/// corresponding values in `expected`.  Fields that are `None` in `expected`
/// will not be checked.
pub fn verify_bunyan_records<'a, 'b, I>(
    iter: I,
    expected: &'a BunyanLogRecordSpec,
) where
    I: Iterator<Item = &'b BunyanLogRecord>,
{
    for record in iter {
        if let Some(ref expected_name) = expected.name {
            assert_eq!(expected_name, &record.name);
        }
        if let Some(ref expected_hostname) = expected.hostname {
            assert_eq!(expected_hostname, &record.hostname);
        }
        if let Some(expected_pid) = expected.pid {
            assert_eq!(expected_pid, record.pid);
        }
        if let Some(expected_v) = expected.v {
            assert_eq!(expected_v, record.v);
        }
    }
}

/// Verify that the Bunyan records emitted by `iter` are chronologically
/// sequential and after `maybe_time_before` and before `maybe_time_after`, if
/// those latter two parameters are specified.
pub fn verify_bunyan_records_sequential<'a, 'b, I>(
    iter: I,
    maybe_time_before: Option<&'a DateTime<Utc>>,
    maybe_time_after: Option<&'a DateTime<Utc>>,
) where
    I: Iterator<Item = &'a BunyanLogRecord>,
{
    let mut maybe_should_be_before = maybe_time_before;

    for record in iter {
        if let Some(should_be_before) = maybe_should_be_before {
            assert!(should_be_before.timestamp() <= record.time.timestamp());
        }
        maybe_should_be_before = Some(&record.time);
    }

    if let Some(should_be_before) = maybe_should_be_before {
        if let Some(time_after) = maybe_time_after {
            assert!(should_be_before.timestamp() <= time_after.timestamp());
        }
    }
}

#[cfg(test)]
mod test {
    const T1_STR: &str = "2020-03-24T00:00:00Z";
    const T2_STR: &str = "2020-03-25T00:00:00Z";

    use super::verify_bunyan_records;
    use super::verify_bunyan_records_sequential;
    use super::BunyanLogRecord;
    use super::BunyanLogRecordSpec;
    use chrono::DateTime;
    use chrono::Utc;

    fn make_dummy_record() -> BunyanLogRecord {
        let t1: DateTime<Utc> =
            DateTime::parse_from_rfc3339(T1_STR).unwrap().into();
        BunyanLogRecord {
            time: t1,
            name: "n1".to_string(),
            hostname: "h1".to_string(),
            pid: 1,
            msg: "msg1".to_string(),
            v: 0,
        }
    }

    // Tests various cases where verify_bunyan_records() should not panic.
    #[test]
    fn test_bunyan_easy_cases() {
        let t1: DateTime<Utc> =
            DateTime::parse_from_rfc3339(T1_STR).unwrap().into();
        let r1 = make_dummy_record();
        let r2 = BunyanLogRecord {
            time: t1,
            name: "n1".to_string(),
            hostname: "h2".to_string(),
            pid: 1,
            msg: "msg2".to_string(),
            v: 1,
        };

        // Test case: nothing to check.
        let records: Vec<&BunyanLogRecord> = vec![&r1];
        let iter = records.iter().map(|x| *x);
        verify_bunyan_records(
            iter,
            &BunyanLogRecordSpec {
                name: None,
                hostname: None,
                pid: None,
                v: None,
            },
        );

        // Test case: check name, no problem.
        let records: Vec<&BunyanLogRecord> = vec![&r1];
        let iter = records.iter().map(|x| *x);
        verify_bunyan_records(
            iter,
            &BunyanLogRecordSpec {
                name: Some("n1".to_string()),
                hostname: None,
                pid: None,
                v: None,
            },
        );

        // Test case: check hostname, no problem.
        let records: Vec<&BunyanLogRecord> = vec![&r1];
        let iter = records.iter().map(|x| *x);
        verify_bunyan_records(
            iter,
            &BunyanLogRecordSpec {
                name: None,
                hostname: Some("h1".to_string()),
                pid: None,
                v: None,
            },
        );

        // Test case: check pid, no problem.
        let records: Vec<&BunyanLogRecord> = vec![&r1];
        let iter = records.iter().map(|x| *x);
        verify_bunyan_records(
            iter,
            &BunyanLogRecordSpec {
                name: None,
                hostname: None,
                pid: Some(1),
                v: None,
            },
        );

        // Test case: check hostname, no problem.
        let records: Vec<&BunyanLogRecord> = vec![&r1];
        let iter = records.iter().map(|x| *x);
        verify_bunyan_records(
            iter,
            &BunyanLogRecordSpec {
                name: None,
                hostname: None,
                pid: None,
                v: Some(0),
            },
        );

        // Test case: check all, no problem.
        let records: Vec<&BunyanLogRecord> = vec![&r1];
        let iter = records.iter().map(|x| *x);
        verify_bunyan_records(
            iter,
            &BunyanLogRecordSpec {
                name: Some("n1".to_string()),
                hostname: Some("h1".to_string()),
                pid: Some(1),
                v: Some(0),
            },
        );

        // Test case: check multiple records, no problem.
        let records: Vec<&BunyanLogRecord> = vec![&r1, &r2];
        let iter = records.iter().map(|x| *x);
        verify_bunyan_records(
            iter,
            &BunyanLogRecordSpec {
                name: Some("n1".to_string()),
                hostname: None,
                pid: Some(1),
                v: None,
            },
        );
    }

    // Test cases exercising violations of each of the fields.

    #[test]
    #[should_panic(expected = "assertion `left == right` failed")]
    fn test_bunyan_bad_name() {
        let r1 = make_dummy_record();
        let records: Vec<&BunyanLogRecord> = vec![&r1];
        let iter = records.iter().map(|x| *x);
        verify_bunyan_records(
            iter,
            &BunyanLogRecordSpec {
                name: Some("n2".to_string()),
                hostname: None,
                pid: None,
                v: None,
            },
        );
    }

    #[test]
    #[should_panic(expected = "assertion `left == right` failed")]
    fn test_bunyan_bad_hostname() {
        let r1 = make_dummy_record();
        let records: Vec<&BunyanLogRecord> = vec![&r1];
        let iter = records.iter().map(|x| *x);
        verify_bunyan_records(
            iter,
            &BunyanLogRecordSpec {
                name: None,
                hostname: Some("h2".to_string()),
                pid: None,
                v: None,
            },
        );
    }

    #[test]
    #[should_panic(expected = "assertion `left == right` failed")]
    fn test_bunyan_bad_pid() {
        let r1 = make_dummy_record();
        let records: Vec<&BunyanLogRecord> = vec![&r1];
        let iter = records.iter().map(|x| *x);
        verify_bunyan_records(
            iter,
            &BunyanLogRecordSpec {
                name: None,
                hostname: None,
                pid: Some(2),
                v: None,
            },
        );
    }

    #[test]
    #[should_panic(expected = "assertion `left == right` failed")]
    fn test_bunyan_bad_v() {
        let r1 = make_dummy_record();
        let records: Vec<&BunyanLogRecord> = vec![&r1];
        let iter = records.iter().map(|x| *x);
        verify_bunyan_records(
            iter,
            &BunyanLogRecordSpec {
                name: None,
                hostname: None,
                pid: None,
                v: Some(1),
            },
        );
    }

    // These cases exercise 0, 1, and 2 records with every valid combination
    // of lower and upper bounds.
    #[test]
    fn test_bunyan_seq_easy_cases() {
        let t1: DateTime<Utc> =
            DateTime::parse_from_rfc3339(T1_STR).unwrap().into();
        let t2: DateTime<Utc> =
            DateTime::parse_from_rfc3339(T2_STR).unwrap().into();
        let v0: Vec<BunyanLogRecord> = vec![];
        let v1: Vec<BunyanLogRecord> = vec![BunyanLogRecord {
            time: t1,
            name: "dummy_name".to_string(),
            hostname: "dummy_hostname".to_string(),
            pid: 123,
            msg: "dummy_msg".to_string(),
            v: 0,
        }];
        let v2: Vec<BunyanLogRecord> = vec![
            BunyanLogRecord {
                time: t1,
                name: "dummy_name".to_string(),
                hostname: "dummy_hostname".to_string(),
                pid: 123,
                msg: "dummy_msg".to_string(),
                v: 0,
            },
            BunyanLogRecord {
                time: t2,
                name: "dummy_name".to_string(),
                hostname: "dummy_hostname".to_string(),
                pid: 123,
                msg: "dummy_msg".to_string(),
                v: 0,
            },
        ];

        verify_bunyan_records_sequential(v0.iter(), None, None);
        verify_bunyan_records_sequential(v0.iter(), Some(&t1), None);
        verify_bunyan_records_sequential(v0.iter(), None, Some(&t1));
        verify_bunyan_records_sequential(v0.iter(), Some(&t1), Some(&t2));
        verify_bunyan_records_sequential(v1.iter(), None, None);
        verify_bunyan_records_sequential(v1.iter(), Some(&t1), None);
        verify_bunyan_records_sequential(v1.iter(), None, Some(&t2));
        verify_bunyan_records_sequential(v1.iter(), Some(&t1), Some(&t2));
        verify_bunyan_records_sequential(v2.iter(), None, None);
        verify_bunyan_records_sequential(v2.iter(), Some(&t1), None);
        verify_bunyan_records_sequential(v2.iter(), None, Some(&t2));
        verify_bunyan_records_sequential(v2.iter(), Some(&t1), Some(&t2));
    }

    // Test case: no records, but the bounds themselves violate the constraint.
    #[test]
    #[should_panic(expected = "assertion failed: should_be_before")]
    fn test_bunyan_seq_bounds_bad() {
        let t1: DateTime<Utc> =
            DateTime::parse_from_rfc3339(T1_STR).unwrap().into();
        let t2: DateTime<Utc> =
            DateTime::parse_from_rfc3339(T2_STR).unwrap().into();
        let v0: Vec<BunyanLogRecord> = vec![];
        verify_bunyan_records_sequential(v0.iter(), Some(&t2), Some(&t1));
    }

    // Test case: sole record appears before early bound.
    #[test]
    #[should_panic(expected = "assertion failed: should_be_before")]
    fn test_bunyan_seq_lower_violated() {
        let t1: DateTime<Utc> =
            DateTime::parse_from_rfc3339(T1_STR).unwrap().into();
        let t2: DateTime<Utc> =
            DateTime::parse_from_rfc3339(T2_STR).unwrap().into();
        let v1: Vec<BunyanLogRecord> = vec![BunyanLogRecord {
            time: t1,
            name: "dummy_name".to_string(),
            hostname: "dummy_hostname".to_string(),
            pid: 123,
            msg: "dummy_msg".to_string(),
            v: 0,
        }];
        verify_bunyan_records_sequential(v1.iter(), Some(&t2), None);
    }

    // Test case: sole record appears after late bound.
    #[test]
    #[should_panic(expected = "assertion failed: should_be_before")]
    fn test_bunyan_seq_upper_violated() {
        let t1: DateTime<Utc> =
            DateTime::parse_from_rfc3339(T1_STR).unwrap().into();
        let t2: DateTime<Utc> =
            DateTime::parse_from_rfc3339(T2_STR).unwrap().into();
        let v1: Vec<BunyanLogRecord> = vec![BunyanLogRecord {
            time: t2,
            name: "dummy_name".to_string(),
            hostname: "dummy_hostname".to_string(),
            pid: 123,
            msg: "dummy_msg".to_string(),
            v: 0,
        }];
        verify_bunyan_records_sequential(v1.iter(), None, Some(&t1));
    }

    // Test case: two records out of order.
    #[test]
    #[should_panic(expected = "assertion failed: should_be_before")]
    fn test_bunyan_seq_bad_order() {
        let t1: DateTime<Utc> =
            DateTime::parse_from_rfc3339(T1_STR).unwrap().into();
        let t2: DateTime<Utc> =
            DateTime::parse_from_rfc3339(T2_STR).unwrap().into();
        let v2: Vec<BunyanLogRecord> = vec![
            BunyanLogRecord {
                time: t2,
                name: "dummy_name".to_string(),
                hostname: "dummy_hostname".to_string(),
                pid: 123,
                msg: "dummy_msg".to_string(),
                v: 0,
            },
            BunyanLogRecord {
                time: t1,
                name: "dummy_name".to_string(),
                hostname: "dummy_hostname".to_string(),
                pid: 123,
                msg: "dummy_msg".to_string(),
                v: 0,
            },
        ];
        verify_bunyan_records_sequential(v2.iter(), None, None);
    }
}
