// Copyright 2023 Oxide Computer Company

//! An example that demonstrates server shutdown (and waiting for shutdown).

use dropshot::endpoint;
use dropshot::ApiDescription;
use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::RequestContext;
use dropshot::ServerBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), String> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .compact()
        .init();

    let mut api = ApiDescription::new();
    api.register(example_api_get_counter).unwrap();

    let api_context = Arc::new(ExampleContext::new());

    let server = ServerBuilder::new(api, api_context, None)
        .start()
        .map_err(|error| format!("failed to create server: {}", error))?;
    let shutdown = server.wait_for_shutdown();

    tokio::task::spawn(async move {
        loop {
            // We can access the server's data while simultaneously awaiting
            // its termination.
            let value =
                server.app_private().counter.fetch_add(1, Ordering::SeqCst);
            const TIMEOUT: u64 = 5;
            if value >= TIMEOUT {
                println!("Terminating now");
                break;
            } else {
                println!("Terminating in {} seconds", TIMEOUT - value);
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }
        // Once the timeout has been reached, we stop the server ourselves.
        server.close().await.unwrap();
    });

    // From a separate task, wait for the server to stop.
    shutdown.await
}

/// Application-specific example context (state shared by handler functions)
struct ExampleContext {
    counter: AtomicU64,
}

impl ExampleContext {
    pub fn new() -> ExampleContext {
        ExampleContext { counter: AtomicU64::new(0) }
    }
}

// HTTP API interface

#[derive(Deserialize, Serialize, JsonSchema)]
struct CounterValue {
    counter: u64,
}

#[endpoint {
    method = GET,
    path = "/counter",
}]
async fn example_api_get_counter(
    rqctx: RequestContext<Arc<ExampleContext>>,
) -> Result<HttpResponseOk<CounterValue>, HttpError> {
    let api_context = rqctx.context();

    Ok(HttpResponseOk(CounterValue {
        counter: api_context.counter.load(Ordering::SeqCst),
    }))
}
