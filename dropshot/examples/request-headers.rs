// Copyright 2023 Oxide Computer Company

//! Example use of Dropshot with request headers
//!
//! The headers accessed here will not be recorded as inputs in the OpenAPI
//! spec.  This is not currently supported out-of-the-box with Dropshot, but it
//! could be done by implementing you're own `SharedExtractor` that pulls the
//! headers out, similar to what's done here.
//!
//! This example is based on the "basic.rs" one.  See that one for more detailed
//! comments on the common code.

use dropshot::endpoint;
use dropshot::ApiDescription;
use dropshot::ConfigDropshot;
use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::HttpServerStarter;
use dropshot::RequestContext;

#[tokio::main]
async fn main() -> Result<(), String> {
    // Uncomment for logs
    // tracing_subscriber::fmt()
    //     .with_max_level(tracing::Level::INFO)
    //     .with_target(false)
    //     .compact()
    //     .init();

    let config_dropshot: ConfigDropshot = Default::default();
    let mut api = ApiDescription::new();
    api.register(example_api_get_header_generic).unwrap();

    let server = HttpServerStarter::new(&config_dropshot, api, ())
        .map_err(|error| format!("failed to create server: {}", error))?
        .start();
    server.await
}

/// Shows how to access a header that's not part of the OpenAPI spec
#[endpoint {
    method = GET,
    path = "/header-example-generic",
}]
async fn example_api_get_header_generic(
    rqctx: RequestContext<()>,
) -> Result<HttpResponseOk<String>, HttpError> {
    // Note that clients can provide multiple values for a header.  See
    // http::HeaderMap for ways to get all of them.
    let header = rqctx.request.headers().get("demo-header");
    Ok(HttpResponseOk(format!("value for header: {:?}", header)))
}
