// Copyright 2022 Oxide Computer Company
//! Example use of Dropshot with a websocket endpoint.

use dropshot::channel;
use dropshot::ApiDescription;
use dropshot::Query;
use dropshot::RequestContext;
use dropshot::ServerBuilder;
use dropshot::WebsocketConnection;
use futures::SinkExt;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), String> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .compact()
        .init();

    // Build a description of the API.
    let mut api = ApiDescription::new();
    api.register(example_api_websocket_counter).unwrap();

    let server = ServerBuilder::new(api, (), None)
        .start()
        .map_err(|error| format!("failed to create server: {}", error))?;

    info!(address = server.local_addr().to_string(), "started http server");

    // Wait for the server to stop.  Note that there's not any code to shut down
    // this server, so we should never get past this point.
    server.await
}

// HTTP API interface

#[derive(Deserialize, JsonSchema)]
struct QueryParams {
    start: Option<u8>,
}

/// An eternally-increasing sequence of bytes, wrapping on overflow, starting
/// from the value given for the query parameter "start."
#[channel {
    protocol = WEBSOCKETS,
    path = "/counter",
}]
async fn example_api_websocket_counter(
    _rqctx: RequestContext<()>,
    qp: Query<QueryParams>,
    upgraded: WebsocketConnection,
) -> dropshot::WebsocketChannelResult {
    let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
        upgraded.into_inner(),
        Role::Server,
        None,
    )
    .await;
    let mut count = qp.into_inner().start.unwrap_or(0);
    while ws.send(Message::Binary(vec![count])).await.is_ok() {
        count = count.wrapping_add(1);
    }
    Ok(())
}
