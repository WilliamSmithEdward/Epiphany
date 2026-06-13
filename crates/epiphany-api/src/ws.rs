//! WebSocket change notifications. One authenticated stream per client; each
//! committed write or batch broadcasts exactly one event (a batch is one event).
//! Consolidations are not pushed: clients refetch the views they display. The
//! version is the engine's monotonic commit version, so the stream is ordered
//! and gap-detectable.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::auth::AuthPrincipal;
use crate::dto::CoordMap;
use crate::AppState;

/// A change-notification broadcast to all connected clients.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChangeEvent {
    /// Sent once when a client connects.
    Hello { version: u64 },
    /// The leaf coordinates changed by one committed write or batch.
    CellsChanged {
        cube: String,
        version: u64,
        coords: Vec<CoordMap>,
    },
}

/// `GET /api/v1/ws` -> a JSON change-event stream (authentication required).
pub(crate) async fn ws(
    _auth: AuthPrincipal,
    State(state): State<AppState>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let receiver = state.events.subscribe();
    upgrade.on_upgrade(move |socket| pump(socket, receiver))
}

async fn pump(mut socket: WebSocket, mut receiver: broadcast::Receiver<ChangeEvent>) {
    if send_event(&mut socket, &ChangeEvent::Hello { version: 0 })
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            event = receiver.recv() => match event {
                Ok(event) => {
                    if send_event(&mut socket, &event).await.is_err() {
                        break;
                    }
                }
                // A slow client may miss events; keep the connection (clients refetch).
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(_)) => {} // ignore client frames
                _ => break,       // client closed or errored
            },
        }
    }
}

async fn send_event(socket: &mut WebSocket, event: &ChangeEvent) -> Result<(), axum::Error> {
    let json = serde_json::to_string(event).unwrap_or_default();
    socket.send(Message::Text(json.into())).await
}
