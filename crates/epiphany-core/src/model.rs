//! The model: its JSON definition and the validated, immutable snapshot compiled from it.

mod definition;
mod error;
mod snapshot;

pub use definition::{
    AttributeType, ChildDef, Comparison, CubeDef, DashboardDef, DimensionDef, DimensionType,
    ElementDef, ElementType, Geometry, HighlightRule, Highlights, MembersDef, ModelDef,
    NumberFormat, PicklistDef, Scale, SubsetDef, TileDef, TileShow, Tone, ValidationDef, ViewDef,
};
pub use error::ModelError;
pub use snapshot::{
    Attribute, Cube, Dimension, DimensionId, Element, ElementId, Model, Picklist, Validation, key,
};
