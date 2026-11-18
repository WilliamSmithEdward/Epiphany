//! The Epiphany OLAP engine: model, cell store, consolidation, rules, MDX and transactions.

pub mod cells;
pub mod database;
pub mod model;
mod value;

pub use value::Value;
