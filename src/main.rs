use std::env;

use async_std::net::TcpListener;

#[macro_use]
extern crate log;

pub mod config;

mod database;
mod websocket;

#[async_std::main]
async fn main() {
    // Configure requirements for Bonfire.
    revolt_quark::setup_logging();
    database::connect().await;

    // Setup a TCP listener to accept WebSocket connections on.
    // By default, we bind to port 9000 on all interfaces.
    let bind = env::var("HOST").unwrap_or_else(|_| "0.0.0.0:9000".into());
    let try_socket = TcpListener::bind(bind).await;
    let listener = try_socket.expect("Failed to bind");

    // Start accepting new connections and spawn a client for each connection.
    while let Ok((stream, addr)) = listener.accept().await {
        websocket::spawn_client(database::get_db(), stream, addr);
    }
}
