// Copyright 2023 Oxide Computer Company
//! Example use of Dropshot for multipart form-data.

use dropshot::endpoint;
use dropshot::ApiDescription;
use dropshot::ConfigDropshot;
use dropshot::HttpError;
use dropshot::HttpServerStarter;
use dropshot::MultipartBody;
use dropshot::RequestContext;
use http::{Response, StatusCode};
use hyper::Body;

#[tokio::main]
async fn main() -> Result<(), String> {
    // We must specify a configuration with a bind address.  We'll use 127.0.0.1
    // since it's available and won't expose this server outside the host.  We
    // request port 0, which allows the operating system to pick any available
    // port.
    let config_dropshot: ConfigDropshot = Default::default();

    // Uncomment for logs
    // tracing_subscriber::fmt()
    //     .with_max_level(tracing::Level::INFO)
    //     .with_target(false)
    //     .compact()
    //     .init();

    // Build a description of the API.
    let mut api = ApiDescription::new();
    api.register(index).unwrap();

    // Set up the server.
    let server = HttpServerStarter::new(&config_dropshot, api, ())
        .map_err(|error| format!("failed to create server: {}", error))?
        .start();

    // Wait for the server to stop.  Note that there's not any code to shut down
    // this server, so we should never get past this point.
    server.await
}

/// Return static content for all paths.
#[endpoint {
    method = POST,

    /*
     * Match literally every path including the empty path.
     */
    path = "/",

    /*
     * This isn't an API so we don't want this to appear in the OpenAPI
     * description if we were to generate it.
     */
    unpublished = true,
}]
async fn index(
    _rqctx: RequestContext<()>,
    mut body: MultipartBody,
) -> Result<Response<Body>, HttpError> {
    // Iterate over the fields, use `next_field()` to get the next field.
    while let Some(mut field) = body.content.next_field().await.unwrap() {
        // Get field name.
        let name = field.name();
        // Get the field's filename if provided in "Content-Disposition" header.
        let file_name = field.file_name();

        println!("Name: {:?}, File Name: {:?}", name, file_name);

        // Process the field data chunks e.g. store them in a file.
        while let Some(chunk) = field.chunk().await.unwrap() {
            // Do something with field chunk.
            println!("Chunk: {:?}", chunk);
        }
    }

    Ok(Response::builder().status(StatusCode::OK).body("Ok".into())?)
}
