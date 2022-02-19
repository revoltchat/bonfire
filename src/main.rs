use std::env;

use async_std::net::TcpListener;

#[macro_use]
extern crate log;

mod database;
mod websocket;

#[async_std::main]
async fn main() {
    revolt_quark::setup_logging();
    database::connect().await;

    let bind = env::var("HOST").unwrap_or_else(|_| "0.0.0.0:7000".into());
    let try_socket = TcpListener::bind(bind).await;
    let listener = try_socket.expect("Failed to bind");

    while let Ok((stream, addr)) = listener.accept().await {
        websocket::spawn_client(database::get_db(), stream, addr);
    }
}
