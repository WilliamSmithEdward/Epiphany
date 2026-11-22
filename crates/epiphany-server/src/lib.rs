//! The Epiphany HTTP server: a thin JSON layer over `epiphany-core`.

mod api;
mod auth;
mod storage;
mod views;

pub use api::{app, app_with, openapi, with_lab};
pub use auth::{Auth, Grants, Refusal, Role, set_user};
pub use storage::{load_model, open};
