//! The model: its JSON definition and the validated, immutable snapshot compiled from it.

mod definition;
mod error;
mod snapshot;

pub use definition::{
    AttributeType, ChildDef, CubeDef, DimensionDef, DimensionType, ElementDef, ElementType,
    ModelDef,
};
pub use error::ModelError;
pub(crate) use snapshot::key;
pub use snapshot::{Attribute, Cube, Dimension, DimensionId, Element, ElementId, Model};
