use axum::extract::ws::{Message, WebSocket};
use axum::{extract::WebSocketUpgrade, response::IntoResponse, routing::get, Router};
use std::time::Duration;

use orderbook::engine::MarketFeedSimulator;

#[tokio::main]
async fn main() {
    let app = Router::new().route("/ws", get(ws_handler));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    println!(
        "HFT orderbook feed live on ws://{}/ws",
        listener.local_addr().unwrap()
    );
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    let mut sim = MarketFeedSimulator::new();

    // Send a full-book snapshot to establish the initial state on the client side.
    let boot = sim.bootstrap_update();
    let payload = serde_json::to_string(&boot).unwrap();
    if socket.send(Message::Text(payload)).await.is_err() {
        return;
    }

    // Continuously push incremental level-2 diffs until the client disconnects.
    loop {
        let upd = sim.next_update();
        let json = serde_json::to_string(&upd).unwrap();
        if socket.send(Message::Text(json)).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(sim.dt_ms())).await;
    }
}
