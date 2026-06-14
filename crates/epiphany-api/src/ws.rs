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
    /// The leaf coordinates changed by one committed write or batch. A write to a
    /// sandbox carries that sandbox's name and owner, so the stream only delivers
    /// it to the owner (and admins); a base write leaves both `None` (public).
    CellsChanged {
        cube: String,
        version: u64,
        coords: Vec<CoordMap>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sandbox: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
    },
    /// A cube's saved objects (subsets/views) changed; clients refetch lists.
    ObjectsChanged { cube: String, version: u64 },
}

/// Whether a change event should be delivered to a given subscriber. A sandbox
/// write (a `CellsChanged` with an owner) is private to its owner and admins;
/// everything else is public.
fn visible_to(event: &ChangeEvent, username: &str, is_admin: bool) -> bool {
    match event {
        ChangeEvent::CellsChanged {
            owner: Some(owner), ..
        } => is_admin || owner == username,
        _ => true,
    }
}

/// `GET /api/v1/ws` -> a JSON change-event stream (authentication required).
pub(crate) async fn ws(
    auth: AuthPrincipal,
    State(state): State<AppState>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let receiver = state.events.subscribe();
    let username = auth.principal.username;
    let is_admin = auth.principal.is_admin;
    upgrade.on_upgrade(move |socket| pump(socket, receiver, username, is_admin))
}

async fn pump(
    mut socket: WebSocket,
    mut receiver: broadcast::Receiver<ChangeEvent>,
    username: String,
    is_admin: bool,
) {
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
                    // Drop another user's private sandbox events for this client.
                    if !visible_to(&event, &username, is_admin) {
                        continue;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox_event(owner: &str) -> ChangeEvent {
        ChangeEvent::CellsChanged {
            cube: "Sales".into(),
            version: 1,
            coords: Vec::new(),
            sandbox: Some("wi".into()),
            owner: Some(owner.into()),
        }
    }

    #[test]
    fn sandbox_events_are_private_to_owner_and_admins() {
        let ev = sandbox_event("ann");
        assert!(visible_to(&ev, "ann", false), "the owner sees it");
        assert!(!visible_to(&ev, "bob", false), "another user does not");
        assert!(
            visible_to(&ev, "bob", true),
            "an admin sees any sandbox event"
        );
    }

    #[test]
    fn base_and_meta_events_are_public() {
        let base = ChangeEvent::CellsChanged {
            cube: "Sales".into(),
            version: 2,
            coords: Vec::new(),
            sandbox: None,
            owner: None,
        };
        assert!(visible_to(&base, "bob", false));
        assert!(visible_to(&ChangeEvent::Hello { version: 0 }, "bob", false));
        assert!(visible_to(
            &ChangeEvent::ObjectsChanged {
                cube: "Sales".into(),
                version: 3
            },
            "bob",
            false
        ));
    }
}
