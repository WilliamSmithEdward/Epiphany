//! Resolve name-addressed coordinates to element indices, classifying the cell
//! (string vs numeric, editable vs consolidated) for the read and write paths.

use epiphany_core::{Cube, ElementKind};

use crate::dto::CoordMap;
use crate::ApiError;

/// A resolved coordinate plus classification.
pub(crate) struct Resolved {
    pub indices: Vec<u32>,
    /// The coordinate addresses a string element (a string cell).
    pub has_string: bool,
    /// Every element is a leaf (so the cell is writable).
    pub all_leaf: bool,
}

/// Map a `{dimension: element}` coordinate to element indices in dimension order.
/// Rejects coordinates with the wrong dimensions or unknown elements (422).
pub(crate) fn resolve(cube: &Cube, coord: &CoordMap) -> Result<Resolved, ApiError> {
    if coord.len() != cube.rank() {
        return Err(ApiError::unprocessable(
            "BAD_COORD",
            format!(
                "coordinate has {} entries but cube '{}' has {} dimensions",
                coord.len(),
                cube.name(),
                cube.rank()
            ),
        ));
    }
    let mut indices = Vec::with_capacity(cube.rank());
    let mut has_string = false;
    let mut all_leaf = true;
    for d in 0..cube.rank() {
        let dim = cube.dimension(d);
        let name = coord.get(dim.name()).ok_or_else(|| {
            ApiError::unprocessable(
                "BAD_COORD",
                format!("coordinate missing dimension '{}'", dim.name()),
            )
        })?;
        let idx = dim.index_of(name).ok_or_else(|| {
            ApiError::unprocessable(
                "UNKNOWN_ELEMENT",
                format!("unknown element '{name}' in dimension '{}'", dim.name()),
            )
        })?;
        match dim.element(idx).expect("resolved index is valid").kind {
            ElementKind::String => has_string = true,
            ElementKind::Consolidated => all_leaf = false,
            ElementKind::Leaf => {}
        }
        indices.push(idx);
    }
    Ok(Resolved {
        indices,
        has_string,
        all_leaf,
    })
}

/// The JSON `kind` token for an element kind.
pub(crate) fn kind_str(kind: ElementKind) -> &'static str {
    match kind {
        ElementKind::Leaf => "numeric",
        ElementKind::String => "string",
        ElementKind::Consolidated => "consolidated",
    }
}
