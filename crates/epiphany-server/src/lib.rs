//! The Epiphany HTTP server: a thin JSON layer over `epiphany-core`.

mod api;
mod load;

pub use api::{app, openapi};
pub use load::load_model;
