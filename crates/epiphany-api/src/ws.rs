//! WebSocket change notifications. One authenticated stream per client; each
//! committed write or batch broadcasts exactly one event (a batch is one event).
//! Consolidations are not pushed: clients refetch the views they display. Each
//! `CellsChanged`/`ObjectsChanged` event carries the originating cube's commit
//! version, which is monotonic per cube; there is no single global version, so
//! the opening `Hello` frame sends a placeholder `version: 0` rather than a
//! gap-detection baseline. Clients refetch on any event, so they do not rely on
//! the version (or the coordinate payload) for gap detection.
//!
//! The stream stays authorized for its whole life, not just at upgrade
//! (ADR-0017): before delivering each event the pump re-validates the session
//! token against the live store, so a logout, a password change
//! (`revoke_user_except`), an admin reset/delete (`revoke_user`), or a TTL/idle
//! expiry closes the socket instead of letting a stale (or stolen) session keep
//! streaming. It also re-resolves the subscriber's admin flag and cube access per
//! event from the live security store, so a demotion or a revoked grant takes
//! effect immediately and a subscriber is never sent change activity for a cube
//! they cannot read (mirroring the `get_cube` name-suppression guarantee,
//! ADR-0015/0033).

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use epiphany_engine::{CommitEvent, CommitObserver};
use epiphany_security::AccessLevel;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::auth::AuthPrincipal;
use crate::authz::cube_level;
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

/// The commit-ordered change feed (A1): an engine [`CommitObserver`] that turns
/// each committed cube advance into exactly one `CellsChanged` broadcast, emitted
/// from INSIDE the commit path (`Engine::notify_commit`, under the cube's writer
/// lock, right after the new version is published). This is the single source of
/// change events, replacing the old post-commit `state.events.send(...)` calls in
/// the write handlers.
///
/// Emitting in-commit fixes the reordering the API layer suffered before: a
/// handler that sent its event AFTER `apply_batch` returned could be overtaken by a
/// later commit's handler, so a subscriber could see version N+1 before N (or an
/// event before the commit that caused it was visible to a reader). The engine
/// fires the hook in per-cube version order under the writer lock, so the feed is
/// now monotonic per cube and never precedes commit visibility.
///
/// The event carries the originating cube + version and (for a private what-if
/// write) the sandbox name; the OWNER is resolved per subscriber at delivery time
/// from the live snapshot (see [`delivery_for`]), so a sandbox write stays private
/// to its owner and a base commit reaches every cube reader. Coordinates are not
/// carried (clients refetch on any event); the payload is intentionally minimal.
///
/// One caveat is documented for A1: the observer cannot distinguish a cell write
/// from a saved-object edit (the engine reports only `(cube, version, sandbox)`),
/// so every commit surfaces as `cells_changed` rather than the old
/// `cells_changed`/`objects_changed` split. Clients refetch identically on either
/// (web `CubeApp` treats them the same), and routing object edits through the same
/// per-subscriber cube-read filter as cell writes actually TIGHTENS security (an
/// object edit to a cube the subscriber cannot read no longer notifies them). The
/// registry-only dimension ops that never reach the engine commit path (promote,
/// register, delete a library dimension) still emit their own `ObjectsChanged`
/// from the handler, since the observer never fires for them.
pub(crate) struct ChangeFeed {
    events: broadcast::Sender<ChangeEvent>,
}

impl ChangeFeed {
    pub(crate) fn new(events: broadcast::Sender<ChangeEvent>) -> Arc<Self> {
        Arc::new(Self { events })
    }
}

impl CommitObserver for ChangeFeed {
    fn on_commit(&self, event: &CommitEvent) {
        // One `CellsChanged` per commit, in commit order. `owner` is left `None`
        // here and resolved per subscriber at delivery (from the sandbox name), so
        // the same broadcast event serves every subscriber while a private what-if
        // write is filtered to its owner. A send error (no live subscribers) is
        // ignored — the broadcast channel drops when empty, by design.
        let _ = self.events.send(ChangeEvent::CellsChanged {
            cube: event.cube.clone(),
            version: event.version,
            coords: Vec::new(),
            sandbox: event.sandbox.clone(),
            owner: None,
        });
    }
}

/// How a change event should be delivered to a subscriber, re-resolved against
/// the live security store per event (never frozen at upgrade). `is_admin` and the
/// subscriber's cube access can change under a live connection, so both are read
/// fresh here.
enum Delivery {
    /// Deliver the event to this subscriber (with coordinates stripped for a
    /// `CellsChanged` -- clients refetch on any event, so the coordinate payload
    /// carries no information the subscriber is entitled to and every write's cube
    /// name + member names would otherwise leak past cube and element security).
    Send,
    /// Drop this event for this subscriber (not visible to them).
    Drop,
}

/// Decide delivery for one event and subscriber against the live store:
/// - a sandbox write (`CellsChanged` with a `sandbox`) is private to its owner and
///   admins; the owner is resolved from the live snapshot here (the commit-ordered
///   feed leaves `owner` unset), so it always reflects the sandbox's current owner
///   and a since-discarded sandbox denies every non-admin (fail-closed);
/// - a base `CellsChanged` is delivered only if the subscriber can currently read
///   the cube (`Cube:Read`), so a user with no access never learns of writes to a
///   cube (nor sees its name/coordinates); and
/// - `Hello`/`ObjectsChanged` stay public (a list refetch is gated by the read
///   endpoints themselves).
fn delivery_for(state: &AppState, event: &ChangeEvent, username: &str, is_admin: bool) -> Delivery {
    match event {
        // A private what-if write: resolve the sandbox's owner from the live cube
        // model and deliver only to that owner (or an admin). An event may carry an
        // explicit `owner` (a handler-emitted sandbox event, if any remain); honor
        // it directly. Otherwise look up the sandbox by name.
        ChangeEvent::CellsChanged {
            owner: Some(owner), ..
        } => bool_delivery(sandbox_owner_visible(owner, username, is_admin)),
        ChangeEvent::CellsChanged {
            cube,
            sandbox: Some(sandbox),
            ..
        } => {
            let owner = sandbox_owner(state, cube, sandbox);
            // A vanished sandbox (owner unknown) is visible only to admins
            // (fail-closed): the write was private, so absent an owner to match, do
            // not leak it to non-admin cube readers.
            bool_delivery(match owner {
                Some(owner) => sandbox_owner_visible(&owner, username, is_admin),
                None => is_admin,
            })
        }
        ChangeEvent::CellsChanged { cube, .. } => {
            bool_delivery(is_admin || cube_level(state, username, cube) >= AccessLevel::Read)
        }
        _ => Delivery::Send,
    }
}

/// The current owner of `sandbox` in `cube`, from the live (lock-free) snapshot, or
/// `None` if the cube or sandbox no longer exists. Used to keep a private what-if
/// write's change event scoped to its owner even though the commit-ordered feed
/// carries only the sandbox NAME (owner is an API/security concept, not engine
/// state on the commit event).
fn sandbox_owner(state: &AppState, cube: &str, sandbox: &str) -> Option<String> {
    let snap = state.engine.snapshot(cube)?;
    snap.model().sandbox(sandbox).map(|sb| sb.owner.clone())
}

/// A private sandbox write is visible only to its owner and to admins (unchanged
/// from the original `visible_to` sandbox rule). Pure, so it is unit-testable
/// without an `AppState`; the base-cube read gate needs the live store and is
/// covered by an integration test.
fn sandbox_owner_visible(owner: &str, username: &str, is_admin: bool) -> bool {
    is_admin || owner == username
}

fn bool_delivery(send: bool) -> Delivery {
    if send {
        Delivery::Send
    } else {
        Delivery::Drop
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
    let token = auth.token;
    upgrade.on_upgrade(move |socket| pump(socket, state, receiver, username, token))
}

async fn pump(
    mut socket: WebSocket,
    state: AppState,
    mut receiver: broadcast::Receiver<ChangeEvent>,
    username: String,
    token: String,
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
                    // Re-authorize on every event so the stream cannot outlive the
                    // session (logout, password change, admin reset/delete, TTL/idle
                    // expiry) or the subscriber's rights. `is_valid` does not slide
                    // the idle window, so another user's write never keeps this
                    // subscriber's session alive.
                    let now = state.clock.now_millis();
                    let is_admin = {
                        let sessions = state.sessions.lock().expect("session store mutex");
                        if !sessions.is_valid(&token, now) {
                            break; // session gone -> close the stream
                        }
                        drop(sessions);
                        // Re-resolve the admin flag live: a demoted admin must lose
                        // the private-sandbox firehose immediately.
                        state
                            .security
                            .lock()
                            .expect("security mutex")
                            .principal(&username)
                            .is_some_and(|p| p.is_admin)
                    };
                    match delivery_for(&state, &event, &username, is_admin) {
                        Delivery::Drop => continue,
                        Delivery::Send => {
                            if send_event(&mut socket, &strip_coords(event)).await.is_err() {
                                break;
                            }
                        }
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

/// Drop the coordinate payload from a `CellsChanged` before it goes on the wire:
/// clients refetch on any event and never read `coords`, so sending the full
/// dimension->member name map for every written cell only leaks names (past cube
/// and element security) and, for a large batch, costs a deep clone plus a
/// multi-megabyte JSON serialization per subscriber. Other events are unchanged.
fn strip_coords(event: ChangeEvent) -> ChangeEvent {
    match event {
        ChangeEvent::CellsChanged {
            cube,
            version,
            sandbox,
            owner,
            ..
        } => ChangeEvent::CellsChanged {
            cube,
            version,
            coords: Vec::new(),
            sandbox,
            owner,
        },
        other => other,
    }
}

async fn send_event(socket: &mut WebSocket, event: &ChangeEvent) -> Result<(), axum::Error> {
    let json = serde_json::to_string(event).unwrap_or_default();
    socket.send(Message::Text(json.into())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_events_are_private_to_owner_and_admins() {
        // The owner sees their own sandbox write; another user does not; an admin
        // sees any sandbox event. (The base-cube read gate is store-backed and is
        // covered by the `ws_stream_*` integration tests.)
        assert!(sandbox_owner_visible("ann", "ann", false), "owner sees it");
        assert!(
            !sandbox_owner_visible("ann", "bob", false),
            "another user does not"
        );
        assert!(
            sandbox_owner_visible("ann", "bob", true),
            "an admin sees any sandbox event"
        );
    }

    #[test]
    fn strip_coords_clears_only_cells_changed_payload() {
        let coord: CoordMap = [("Region".to_string(), "North".to_string())]
            .into_iter()
            .collect();
        let ev = ChangeEvent::CellsChanged {
            cube: "Sales".into(),
            version: 2,
            coords: vec![coord],
            sandbox: None,
            owner: None,
        };
        match strip_coords(ev) {
            ChangeEvent::CellsChanged {
                coords,
                cube,
                version,
                ..
            } => {
                assert!(
                    coords.is_empty(),
                    "coordinates are stripped before the wire"
                );
                assert_eq!(cube, "Sales");
                assert_eq!(version, 2);
            }
            other => panic!("expected CellsChanged, got {other:?}"),
        }
        // Non-cell events pass through untouched.
        match strip_coords(ChangeEvent::ObjectsChanged {
            cube: "Sales".into(),
            version: 3,
        }) {
            ChangeEvent::ObjectsChanged { cube, version } => {
                assert_eq!((cube.as_str(), version), ("Sales", 3));
            }
            other => panic!("expected ObjectsChanged, got {other:?}"),
        }
    }
}
