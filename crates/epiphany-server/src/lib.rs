//! The Epiphany HTTP server: a thin JSON layer over `epiphany-core`.

mod api;
mod storage;

pub use api::{app, lab, openapi};
pub use storage::{load_model, open};
