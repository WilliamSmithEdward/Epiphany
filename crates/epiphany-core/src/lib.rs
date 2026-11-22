//! The Epiphany OLAP engine: model, cell store, consolidation, rules, MDX and transactions.

pub mod access;
pub mod cells;
pub mod database;
pub mod mdx;
pub mod model;
mod reader;
mod scalar;

pub use scalar::Scalar;
