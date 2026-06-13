//! Model-as-code: canonical TOML (de)serialization (ADR-0003).
//!
//! A cube and its dimensions round-trip losslessly through a human-readable,
//! Git-friendly TOML document. Serialization is canonical: elements in
//! definition order, edges, attributes, and cells sorted, so re-serializing a
//! parsed model reproduces byte-identical text (verified by a round-trip test).
//!
//! The format is model-shaped: top-level `[[dimension]]` blocks plus a `[cube]`
//! that references them by name, so it stays forward-compatible with a future
//! multi-cube model that shares dimensions.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{AttributeKind, AttributeValue, Cube, Dimension, ElementKind, Fixed, ModelError};

const FORMAT_TAG: &str = "epiphany-model/v0";

/// An error loading a model from text.
#[derive(Debug)]
pub enum LoadError {
    /// The TOML could not be parsed.
    Toml(toml::de::Error),
    /// The document's `format` tag was missing or unrecognized.
    UnknownFormat(String),
    /// A cube referenced a dimension that is not defined.
    UnknownDimension(String),
    /// An edge or cell referenced an element not present in its dimension.
    UnknownElement { dimension: String, element: String },
    /// A cell coordinate had the wrong number of components for its cube.
    CoordRank {
        cube: String,
        expected: usize,
        got: usize,
    },
    /// Building the model failed a structural rule.
    Model(ModelError),
    /// The model file could not be read.
    Io(std::io::Error),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Toml(e) => write!(f, "invalid model TOML: {e}"),
            LoadError::UnknownFormat(tag) => {
                write!(
                    f,
                    "unknown model format tag '{tag}' (expected '{FORMAT_TAG}')"
                )
            }
            LoadError::UnknownDimension(name) => {
                write!(f, "cube references undefined dimension '{name}'")
            }
            LoadError::UnknownElement { dimension, element } => {
                write!(f, "unknown element '{element}' in dimension '{dimension}'")
            }
            LoadError::CoordRank {
                cube,
                expected,
                got,
            } => write!(
                f,
                "cell in cube '{cube}' has {got} coordinates but the cube has {expected} dimensions"
            ),
            LoadError::Model(e) => write!(f, "{e}"),
            LoadError::Io(e) => write!(f, "could not read model file: {e}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<ModelError> for LoadError {
    fn from(e: ModelError) -> Self {
        LoadError::Model(e)
    }
}

/// An error saving a model to text.
#[derive(Debug)]
pub enum SaveError {
    /// TOML serialization failed.
    Toml(toml::ser::Error),
    /// The model file could not be written.
    Io(std::io::Error),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::Toml(e) => write!(f, "failed to serialize model: {e}"),
            SaveError::Io(e) => write!(f, "failed to write model file: {e}"),
        }
    }
}

impl std::error::Error for SaveError {}

// ---- serialized document shape ----

#[derive(Serialize, Deserialize)]
struct ModelDoc {
    format: String,
    cube: CubeDoc,
    #[serde(default, rename = "dimension")]
    dimensions: Vec<DimDoc>,
    #[serde(default, rename = "cell")]
    cells: Vec<CellDoc>,
    #[serde(default, rename = "string_cell", skip_serializing_if = "Vec::is_empty")]
    string_cells: Vec<StringCellDoc>,
}

#[derive(Serialize, Deserialize)]
struct CubeDoc {
    name: String,
    dimensions: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct DimDoc {
    name: String,
    elements: Vec<ElDoc>,
    #[serde(default)]
    edges: Vec<EdgeDoc>,
    #[serde(default)]
    attributes: Vec<AttrDefDoc>,
    #[serde(default)]
    attribute_values: Vec<AttrValDoc>,
}

#[derive(Serialize, Deserialize)]
struct ElDoc {
    name: String,
    kind: KindDoc,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum KindDoc {
    Leaf,
    String,
    Consolidated,
}

#[derive(Serialize, Deserialize)]
struct EdgeDoc {
    parent: String,
    child: String,
    weight: i64,
}

#[derive(Serialize, Deserialize)]
struct AttrDefDoc {
    name: String,
    kind: AttrKindDoc,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum AttrKindDoc {
    Text,
    Numeric,
    Alias,
}

#[derive(Serialize, Deserialize)]
struct AttrValDoc {
    element: String,
    attribute: String,
    value: String,
}

#[derive(Serialize, Deserialize)]
struct CellDoc {
    coord: Vec<String>,
    value: String,
}

#[derive(Serialize, Deserialize)]
struct StringCellDoc {
    coord: Vec<String>,
    value: String,
}

impl From<ElementKind> for KindDoc {
    fn from(kind: ElementKind) -> Self {
        match kind {
            ElementKind::Leaf => KindDoc::Leaf,
            ElementKind::String => KindDoc::String,
            ElementKind::Consolidated => KindDoc::Consolidated,
        }
    }
}

impl From<AttributeKind> for AttrKindDoc {
    fn from(kind: AttributeKind) -> Self {
        match kind {
            AttributeKind::Text => AttrKindDoc::Text,
            AttributeKind::Numeric => AttrKindDoc::Numeric,
            AttributeKind::Alias => AttrKindDoc::Alias,
        }
    }
}

impl From<AttrKindDoc> for AttributeKind {
    fn from(kind: AttrKindDoc) -> Self {
        match kind {
            AttrKindDoc::Text => AttributeKind::Text,
            AttrKindDoc::Numeric => AttributeKind::Numeric,
            AttrKindDoc::Alias => AttributeKind::Alias,
        }
    }
}

fn dim_doc(dim: &Dimension) -> DimDoc {
    let elements = dim
        .iter_elements()
        .map(|el| ElDoc {
            name: el.name.clone(),
            kind: el.kind.into(),
        })
        .collect();

    let edges = dim
        .edges()
        .into_iter()
        .map(|(parent, child, weight)| EdgeDoc {
            parent: dim.element(parent).expect("valid index").name.clone(),
            child: dim.element(child).expect("valid index").name.clone(),
            weight,
        })
        .collect();

    let attributes = dim
        .attribute_defs()
        .iter()
        .map(|a| AttrDefDoc {
            name: a.name.clone(),
            kind: a.kind.into(),
        })
        .collect();

    let attribute_values = dim
        .attribute_values()
        .into_iter()
        .map(|(element, attr_index, value)| AttrValDoc {
            element: dim.element(element).expect("valid index").name.clone(),
            attribute: dim.attribute_defs()[attr_index as usize].name.clone(),
            value: match value {
                AttributeValue::Text(text) => text,
                AttributeValue::Numeric(number) => number.to_string(),
            },
        })
        .collect();

    DimDoc {
        name: dim.name().to_string(),
        elements,
        edges,
        attributes,
        attribute_values,
    }
}

fn build_dimension(dim_doc: &DimDoc) -> Result<Dimension, LoadError> {
    let mut dim = Dimension::new(&dim_doc.name);
    for el in &dim_doc.elements {
        match el.kind {
            KindDoc::Leaf => dim.add_leaf(&el.name),
            KindDoc::String => dim.add_string(&el.name),
            KindDoc::Consolidated => dim.add_consolidated(&el.name),
        };
    }
    for edge in &dim_doc.edges {
        let parent = dim
            .index_of(&edge.parent)
            .ok_or_else(|| LoadError::UnknownElement {
                dimension: dim_doc.name.clone(),
                element: edge.parent.clone(),
            })?;
        let child = dim
            .index_of(&edge.child)
            .ok_or_else(|| LoadError::UnknownElement {
                dimension: dim_doc.name.clone(),
                element: edge.child.clone(),
            })?;
        dim.add_child(parent, child, edge.weight)?;
    }
    for attr in &dim_doc.attributes {
        dim.add_attribute(&attr.name, attr.kind.into());
    }
    for av in &dim_doc.attribute_values {
        let element = dim
            .index_of(&av.element)
            .ok_or_else(|| LoadError::UnknownElement {
                dimension: dim_doc.name.clone(),
                element: av.element.clone(),
            })?;
        let kind = dim
            .attribute_index(&av.attribute)
            .and_then(|i| dim.attribute_defs().get(i as usize).map(|d| d.kind))
            .ok_or_else(|| ModelError::AttributeNotFound {
                dimension: dim_doc.name.clone(),
                attribute: av.attribute.clone(),
            })?;
        let value = match kind {
            AttributeKind::Numeric => AttributeValue::Numeric(Fixed::from_str(&av.value)?),
            AttributeKind::Text | AttributeKind::Alias => AttributeValue::Text(av.value.clone()),
        };
        dim.set_attribute(element, &av.attribute, value)?;
    }
    Ok(dim)
}

/// Resolve a coordinate's element names to indices for `cube`, validating rank.
fn resolve_coord(cube: &Cube, cube_name: &str, names: &[String]) -> Result<Vec<u32>, LoadError> {
    if names.len() != cube.rank() {
        return Err(LoadError::CoordRank {
            cube: cube_name.to_string(),
            expected: cube.rank(),
            got: names.len(),
        });
    }
    let mut coord = Vec::with_capacity(names.len());
    for (d, name) in names.iter().enumerate() {
        let idx = cube
            .dimension(d)
            .index_of(name)
            .ok_or_else(|| LoadError::UnknownElement {
                dimension: cube.dimension(d).name().to_string(),
                element: name.clone(),
            })?;
        coord.push(idx);
    }
    Ok(coord)
}

impl Cube {
    /// Serialize this cube and its dimensions to canonical model-as-code TOML.
    pub fn to_model_text(&self) -> Result<String, SaveError> {
        let dimensions: Vec<DimDoc> = self.dimensions().iter().map(dim_doc).collect();

        // Cells, sorted by coordinate (element-index tuple) for canonical output.
        let mut sorted: Vec<(Vec<u32>, Fixed)> = self.cell_entries().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let coord_names = |coord: &[u32]| -> Vec<String> {
            coord
                .iter()
                .enumerate()
                .map(|(d, &idx)| {
                    self.dimension(d)
                        .element(idx)
                        .expect("valid index")
                        .name
                        .clone()
                })
                .collect()
        };

        let cells: Vec<CellDoc> = sorted
            .into_iter()
            .map(|(coord, value)| CellDoc {
                coord: coord_names(&coord),
                value: value.to_string(),
            })
            .collect();

        // String cells, sorted by coordinate for canonical output.
        let mut sorted_strings: Vec<(Vec<u32>, String)> = self
            .string_cell_entries()
            .map(|(coord, value)| (coord, value.to_string()))
            .collect();
        sorted_strings.sort_by(|a, b| a.0.cmp(&b.0));
        let string_cells: Vec<StringCellDoc> = sorted_strings
            .into_iter()
            .map(|(coord, value)| StringCellDoc {
                coord: coord_names(&coord),
                value,
            })
            .collect();

        let doc = ModelDoc {
            format: FORMAT_TAG.to_string(),
            cube: CubeDoc {
                name: self.name().to_string(),
                dimensions: self
                    .dimensions()
                    .iter()
                    .map(|d| d.name().to_string())
                    .collect(),
            },
            dimensions,
            cells,
            string_cells,
        };
        toml::to_string(&doc).map_err(SaveError::Toml)
    }

    /// Parse a cube and its dimensions from model-as-code TOML.
    pub fn from_model_text(text: &str) -> Result<Cube, LoadError> {
        let doc: ModelDoc = toml::from_str(text).map_err(LoadError::Toml)?;
        if doc.format != FORMAT_TAG {
            return Err(LoadError::UnknownFormat(doc.format));
        }

        // Build each dimension, keyed by name.
        let mut dims_by_name: HashMap<String, Dimension> = HashMap::new();
        for dim_doc in &doc.dimensions {
            dims_by_name.insert(dim_doc.name.clone(), build_dimension(dim_doc)?);
        }

        // Assemble the cube's dimensions in referenced order.
        let mut cube_dims = Vec::with_capacity(doc.cube.dimensions.len());
        for name in &doc.cube.dimensions {
            let dim = dims_by_name
                .get(name)
                .ok_or_else(|| LoadError::UnknownDimension(name.clone()))?;
            cube_dims.push(dim.clone());
        }
        let mut cube = Cube::new(&doc.cube.name, cube_dims)?;

        // Populate numeric cells, then string cells.
        for cell in &doc.cells {
            let coord = resolve_coord(&cube, &doc.cube.name, &cell.coord)?;
            cube.set_leaf(&coord, Fixed::from_str(&cell.value)?)?;
        }
        for cell in &doc.string_cells {
            let coord = resolve_coord(&cube, &doc.cube.name, &cell.coord)?;
            cube.set_string(&coord, &cell.value)?;
        }

        Ok(cube)
    }

    /// Save this cube to a model-as-code file (canonical TOML).
    pub fn save_to_path(&self, path: impl AsRef<std::path::Path>) -> Result<(), SaveError> {
        let text = self.to_model_text()?;
        std::fs::write(path, text).map_err(SaveError::Io)
    }

    /// Load a cube from a model-as-code file.
    pub fn load_from_path(path: impl AsRef<std::path::Path>) -> Result<Cube, LoadError> {
        let text = std::fs::read_to_string(path).map_err(LoadError::Io)?;
        Cube::from_model_text(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dimension;

    fn sample_cube() -> Cube {
        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        let south = region.add_leaf("South");
        let east = region.add_leaf("East");
        let total = region.add_consolidated("Total");
        let coastal = region.add_consolidated("Coastal");
        for leaf in [north, south, east] {
            region.add_child(total, leaf, 1).unwrap();
        }
        region.add_child(coastal, north, 1).unwrap();
        region.add_child(coastal, east, 1).unwrap();
        region.add_attribute("Code", AttributeKind::Text);
        region.add_attribute("FullName", AttributeKind::Alias);
        region
            .set_attribute(north, "Code", AttributeValue::Text("N".into()))
            .unwrap();
        region
            .set_attribute(north, "FullName", AttributeValue::Text("Northern".into()))
            .unwrap();

        let mut version = Dimension::new("Version");
        let actual = version.add_leaf("Actual");
        let budget = version.add_leaf("Budget");
        let variance = version.add_consolidated("Variance");
        version.add_child(variance, actual, 1).unwrap();
        version.add_child(variance, budget, -1).unwrap();

        let mut cube = Cube::new("Sales", vec![region, version]).unwrap();
        cube.set_leaf(&[north, actual], Fixed::from(100)).unwrap();
        cube.set_leaf(&[north, budget], Fixed::from(80)).unwrap();
        cube.set_leaf(&[south, actual], Fixed::from_str("12.5").unwrap())
            .unwrap();
        cube
    }

    #[test]
    fn round_trips_through_text_canonically() {
        let cube = sample_cube();
        let text1 = cube.to_model_text().unwrap();
        let cube2 = Cube::from_model_text(&text1).unwrap();
        let text2 = cube2.to_model_text().unwrap();
        assert_eq!(text1, text2, "model must round-trip to byte-identical text");
    }

    #[test]
    fn round_trip_preserves_values_and_consolidation() {
        let cube = sample_cube();
        let text = cube.to_model_text().unwrap();
        let cube2 = Cube::from_model_text(&text).unwrap();

        assert_eq!(cube2.rank(), cube.rank());
        assert_eq!(cube2.cell_count(), cube.cell_count());

        let total = cube2.dimension(0).index_of("Total").unwrap();
        let actual = cube2.dimension(1).index_of("Actual").unwrap();
        let variance = cube2.dimension(1).index_of("Variance").unwrap();
        // Total / Actual = 100 + 12.5
        assert_eq!(
            cube2.get(&[total, actual]).unwrap(),
            Fixed::from_str("112.5").unwrap()
        );
        // Total / Variance = (100 - 80) + (12.5 - 0)
        assert_eq!(
            cube2.get(&[total, variance]).unwrap(),
            Fixed::from_str("32.5").unwrap()
        );
    }

    #[test]
    fn round_trip_preserves_attributes_and_aliases() {
        let cube = sample_cube();
        let text = cube.to_model_text().unwrap();
        let cube2 = Cube::from_model_text(&text).unwrap();
        let region = cube2.dimension(0);
        let north = region.index_of("North").unwrap();
        assert_eq!(
            region.attribute(north, "Code"),
            Some(&AttributeValue::Text("N".into()))
        );
        assert_eq!(region.resolve("Northern"), Some(north));
    }

    #[test]
    fn rejects_unknown_format() {
        let text = "format = \"nope\"\n\n[cube]\nname = \"X\"\ndimensions = []\n";
        assert!(matches!(
            Cube::from_model_text(text).unwrap_err(),
            LoadError::UnknownFormat(_)
        ));
    }

    #[test]
    fn saves_and_loads_through_a_file() {
        let cube = sample_cube();
        let path =
            std::env::temp_dir().join(format!("epiphany-model-test-{}.toml", std::process::id()));
        cube.save_to_path(&path).unwrap();
        let loaded = Cube::load_from_path(&path).unwrap();
        std::fs::remove_file(&path).ok();
        // Identical canonical text after a full disk round-trip ("restart and recover").
        assert_eq!(
            loaded.to_model_text().unwrap(),
            cube.to_model_text().unwrap()
        );
    }

    #[test]
    fn round_trips_string_cells() {
        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        let mut measure = Dimension::new("Measure");
        let sales = measure.add_leaf("Sales");
        let comment = measure.add_string("Comment");
        let mut cube = Cube::new("Sales", vec![region, measure]).unwrap();
        cube.set_leaf(&[north, sales], Fixed::from(42)).unwrap();
        cube.set_string(&[north, comment], "high").unwrap();

        let text = cube.to_model_text().unwrap();
        let cube2 = Cube::from_model_text(&text).unwrap();
        // Canonical fixed point, including the string cell and string element.
        assert_eq!(text, cube2.to_model_text().unwrap());

        let region2 = cube2.dimension(0).index_of("North").unwrap();
        let measure2 = cube2.dimension(1);
        let comment2 = measure2.index_of("Comment").unwrap();
        let sales2 = measure2.index_of("Sales").unwrap();
        assert_eq!(
            measure2.element(comment2).unwrap().kind,
            ElementKind::String
        );
        assert_eq!(
            cube2.get_string(&[region2, comment2]).unwrap(),
            Some("high")
        );
        assert_eq!(cube2.get_leaf(&[region2, sales2]).unwrap(), Fixed::from(42));
    }
}
