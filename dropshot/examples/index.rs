// Copyright 2021 Oxide Computer Company
//! Example use of Dropshot for matching wildcard paths to serve static content.

use dropshot::ApiDescription;
use dropshot::ConfigDropshot;
use dropshot::HttpError;
use dropshot::HttpServerStarter;
use dropshot::RequestContext;
use dropshot::{endpoint, Path};
use http::{Response, StatusCode};
use hyper::Body;
use schemars::JsonSchema;
use serde::Deserialize;
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
    api.register(index).unwrap();

    // Set up the server.
    let server = HttpServerStarter::new(&config_dropshot, api, None, ())
        .map_err(|error| format!("failed to create server: {}", error))?
        .start();

    info!(address = server.local_addr().to_string(), "started http server");

    // Wait for the server to stop.  Note that there's not any code to shut down
    // this server, so we should never get past this point.
    server.await
}

#[derive(Deserialize, JsonSchema)]
struct AllPath {
    path: Vec<String>,
}

/// Return static content for all paths.
#[endpoint {
    method = GET,

    /*
     * Match literally every path including the empty path.
     */
    path = "/{path:.*}",

    /*
     * This isn't an API so we don't want this to appear in the OpenAPI
     * description if we were to generate it.
     */
    unpublished = true,
}]
async fn index(
    _rqctx: RequestContext<()>,
    path: Path<AllPath>,
) -> Result<Response<Body>, HttpError> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "text/html")
        .body(
            format!(
                "<HTML><HEAD>nothing at {:?}</HEAD></HTML>",
                path.into_inner().path
            )
            .into(),
        )?)
}
