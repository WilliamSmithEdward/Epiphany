//! The model: its JSON definition and the validated, immutable snapshot compiled from it.

mod definition;
mod error;
mod snapshot;

pub use definition::{
    AttributeType, ChartKind, ChildDef, Comparison, CubeDef, DASHBOARD_COLUMNS, DashboardDef,
    DashboardLayout, DimensionDef, DimensionType, ElementDef, ElementType, Geometry, HighlightRule,
    Highlights, MembersDef, ModelDef, NumberFormat, PicklistDef, Scale, Shared, SubsetDef, TileDef,
    TilePlace, TileShow, Tone, ValidationDef, ViewChart, ViewDef,
};
pub use error::ModelError;
pub use snapshot::{
    Attribute, Cube, Dimension, DimensionId, Element, ElementId, Model, Picklist, Validation, key,
};
