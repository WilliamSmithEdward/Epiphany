//! The model: its JSON definition and the validated, immutable snapshot compiled from it.

mod definition;
mod error;
mod snapshot;

pub use definition::{
    AttributeType, AttributeValue, ChildDef, CubeDef, DimensionDef, DimensionType, ElementDef,
    ElementType, ModelDef,
};
pub use error::ModelError;
pub use snapshot::{Attribute, Cube, Dimension, DimensionId, Element, ElementId, Model};
