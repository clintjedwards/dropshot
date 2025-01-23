// Copyright 2020 Oxide Computer Company
//! Example use of middleware.

use dropshot::endpoint;
use dropshot::ApiDescription;
use dropshot::Body;
use dropshot::ConfigDropshot;
use dropshot::DropshotState;
use dropshot::HandlerError;
use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::HttpResponseUpdatedNoContent;
use dropshot::Middleware;
use dropshot::RequestContext;
use dropshot::ServerBuilder;
use dropshot::ServerContext;
use dropshot::TypedBody;
use futures::Future;
use http::Request;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), String> {
    // We must specify a configuration with a bind address.  We'll use 127.0.0.1
    // since it's available and won't expose this server outside the host.  We
    // request port 0, which allows the operating system to pick any available
    // port.
    let config_dropshot: ConfigDropshot = Default::default();

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .compact()
        .init();

    // Build a description of the API.
    let mut api = ApiDescription::new();
    api.register(example_api_get_counter).unwrap();
    api.register(example_api_put_counter).unwrap();

    // The functions that implement our API endpoints will share this context.
    let api_context = ExampleContext::new();

    // Set up the server.
    let server = ServerBuilder::new(
        api,
        api_context,
        Some(Arc::new(RequestTimeMiddleware)),
    )
    .config(config_dropshot)
    .start()
    .map_err(|error| format!("failed to create server: {}", error))?;

    info!(address = server.local_addr().to_string(), "started http server");

    // Wait for the server to stop.  Note that there's not any code to shut down
    // this server, so we should never get past this point.
    server.await
}

/// Application-specific example context (state shared by handler functions)
struct ExampleContext {
    /// counter that can be manipulated by requests to the HTTP API
    counter: AtomicU64,
}

impl ExampleContext {
    /// Return a new ExampleContext.
    pub fn new() -> ExampleContext {
        ExampleContext { counter: AtomicU64::new(0) }
    }
}

// HTTP API interface

/// `CounterValue` represents the value of the API's counter, either as the
/// response to a GET request to fetch the counter or as the body of a PUT
/// request to update the counter.
#[derive(Deserialize, Serialize, JsonSchema)]
struct CounterValue {
    counter: u64,
}

/// Fetch the current value of the counter.
#[endpoint {
    method = GET,
    path = "/counter",
}]
async fn example_api_get_counter(
    rqctx: RequestContext<ExampleContext>,
) -> Result<HttpResponseOk<CounterValue>, HttpError> {
    let api_context = rqctx.context();

    Ok(HttpResponseOk(CounterValue {
        counter: api_context.counter.load(Ordering::SeqCst),
    }))
}

/// Update the current value of the counter.  Note that the special value of 10
/// is not allowed (just to demonstrate how to generate an error).
#[endpoint {
    method = PUT,
    path = "/counter",
}]
async fn example_api_put_counter(
    rqctx: RequestContext<ExampleContext>,
    update: TypedBody<CounterValue>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let api_context = rqctx.context();
    let updated_value = update.into_inner();

    if updated_value.counter == 10 {
        Err(HttpError::for_bad_request(
            Some(String::from("BadInput")),
            format!("do not like the number {}", updated_value.counter),
        ))
    } else {
        api_context.counter.store(updated_value.counter, Ordering::SeqCst);
        Ok(HttpResponseUpdatedNoContent())
    }
}

#[derive(Debug)]
struct RequestTimeMiddleware;

#[async_trait::async_trait]
impl<C: ServerContext> Middleware<C> for RequestTimeMiddleware {
    async fn handle(
        &self,
        server: Arc<DropshotState<C>>,
        request: Request<hyper::body::Incoming>,
        request_id: String,
        remote_addr: SocketAddr,
        next: fn(
            Arc<DropshotState<C>>,
            Request<hyper::body::Incoming>,
            String,
            SocketAddr,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<hyper::Response<Body>, HandlerError>>
                    + Send,
            >,
        >,
    ) -> Result<http::Response<Body>, HandlerError> {
        let start_time = std::time::Instant::now();

        let response =
            next(server.clone(), request, request_id, remote_addr).await;

        info!(
            duration = format!("{}μs", start_time.elapsed().as_micros()),
            "request completed"
        );

        response
    }
}
