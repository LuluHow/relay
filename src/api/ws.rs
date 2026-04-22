use axum::{
    extract::{ws::Message, State, WebSocketUpgrade},
    response::IntoResponse,
};
use axum::extract::ws::WebSocket;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::api::state::{AppState, Event, HandoffEntry, SessionSnapshot};

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsMessage {
    Snapshot {
        sessions: Vec<SessionSnapshot>,
        handoffs: Vec<HandoffEntry>,
    },
    SessionsUpdated {
        sessions: Vec<SessionSnapshot>,
    },
    HandoffCreated {
        id: String,
    },
    Error {
        message: String,
    },
    Resync {
        sessions: Vec<SessionSnapshot>,
        handoffs: Vec<HandoffEntry>,
    },
}

pub async fn handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    // Send initial snapshot immediately on connect
    let sessions = state.sessions().await;
    let handoffs = state.handoffs().await;
    if let Ok(json) = serde_json::to_string(&WsMessage::Snapshot { sessions, handoffs }) {
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }

    let mut rx = state.subscribe();

    loop {
        match rx.recv().await {
            Ok(event) => {
                let msg = match event {
                    Event::SessionsUpdated => {
                        let sessions = state.sessions().await;
                        WsMessage::SessionsUpdated { sessions }
                    }
                    Event::HandoffCreated { id } => WsMessage::HandoffCreated { id },
                    Event::Error { message } => WsMessage::Error { message },
                };
                if let Ok(json) = serde_json::to_string(&msg) {
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        return;
                    }
                }
            }
            Err(RecvError::Lagged(_)) => {
                let sessions = state.sessions().await;
                let handoffs = state.handoffs().await;
                if let Ok(json) =
                    serde_json::to_string(&WsMessage::Resync { sessions, handoffs })
                {
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        return;
                    }
                }
            }
            Err(RecvError::Closed) => return,
        }
    }
}
