//! The model snapshot: a validated, immutable form of a definition, indexed for lookup.

use std::collections::{BTreeMap, HashMap};

use super::definition::{
    AttributeType, CubeDef, DimensionDef, ElementDef, ElementType, ModelDef, SubsetDef, ViewDef,
};
use super::error::ModelError;
use crate::{Scalar, mdx};

/// Position of a dimension in [`Model::dimensions`].
pub type DimensionId = u32;
/// Position of an element in [`Dimension::elements`].
pub type ElementId = u32;

#[derive(Debug)]
pub struct Model {
    dimensions: Vec<Dimension>,
    cubes: Vec<Cube>,
    dimension_ids: HashMap<String, u32>,
    cube_ids: HashMap<String, u32>,
}

impl Model {
    /// Validates a definition and compiles it, or reports the first problem found.
    pub fn compile(def: &ModelDef) -> Result<Self, ModelError> {
        const WITHIN: &str = "the model";
        let dimension_names = def.dimensions.iter().map(|d| d.name.as_str());
        let dimension_ids = index("dimension", WITHIN, dimension_names)?;
        let cube_ids = index("cube", WITHIN, def.cubes.iter().map(|c| c.name.as_str()))?;
        let dimensions = def.dimensions.iter().map(Dimension::compile);
        let cubes = def.cubes.iter().map(|c| Cube::compile(c, &dimension_ids));
        let mut model = Self {
            dimensions: dimensions.collect::<Result<_, _>>()?,
            cubes: cubes.collect::<Result<_, _>>()?,
            dimension_ids,
            cube_ids,
        };

        // Subsets and views are written in MDX, which needs the rest of the model in place.
        // A subset is worked out once, here; it may build on the subsets before it by name.
        for (id, dimension) in def.dimensions.iter().enumerate() {
            let within = format!("dimension '{}'", dimension.name);
            index(
                "subset",
                &within,
                dimension.subsets.keys().map(String::as_str),
            )?;
            for (name, subset) in &dimension.subsets {
                let within = format!("subset '{name}' of {within}");
                let elements =
                    match subset {
                        SubsetDef::Elements(names) => {
                            let ids = &model.dimensions[id].element_ids;
                            let element = |name: &String| resolve(ids, "element", &within, name);
                            names.iter().map(element).collect::<Result<_, _>>()?
                        }
                        SubsetDef::Mdx(text) => mdx::subset(&model, id as DimensionId, text)
                            .map_err(|error| ModelError::Mdx {
                                within,
                                error: error.to_string(),
                            })?,
                    };
                model.dimensions[id].subsets.push(Subset {
                    name: name.clone(),
                    elements,
                });
            }
        }
        for (cube, definition) in model.cubes.iter().zip(&def.cubes) {
            let within = format!("cube '{}'", cube.name);
            index("view", &within, definition.views.keys().map(String::as_str))?;
            for (name, view) in &definition.views {
                mdx::check_view(&model, cube, &view.mdx).map_err(|error| ModelError::Mdx {
                    within: format!("view '{name}' of {within}"),
                    error: error.to_string(),
                })?;
            }
        }
        Ok(model)
    }

    pub fn dimensions(&self) -> &[Dimension] {
        &self.dimensions
    }

    pub fn cubes(&self) -> &[Cube] {
        &self.cubes
    }

    pub fn dimension(&self, name: &str) -> Option<&Dimension> {
        let id = *self.dimension_ids.get(&key(name))?;
        Some(&self.dimensions[id as usize])
    }

    /// The position of a cube in [`Model::cubes`].
    pub fn cube_id(&self, name: &str) -> Option<usize> {
        self.cube_ids.get(&key(name)).map(|&id| id as usize)
    }

    pub fn cube(&self, name: &str) -> Option<&Cube> {
        self.cube_id(name).map(|id| &self.cubes[id])
    }
}

#[derive(Debug)]
pub struct Cube {
    pub name: String,
    pub dimensions: Vec<DimensionId>,
    /// Saved queries, in name order.
    pub views: Vec<View>,
}

#[derive(Debug)]
pub struct View {
    pub name: String,
    pub mdx: String,
}

/// The elements a subset selects, in order.
#[derive(Debug)]
pub struct Subset {
    pub name: String,
    pub elements: Vec<ElementId>,
}

impl Cube {
    fn compile(def: &CubeDef, dimension_ids: &HashMap<String, u32>) -> Result<Self, ModelError> {
        let within = format!("cube '{}'", def.name);
        if def.dimensions.is_empty() {
            return Err(ModelError::NoDimensions {
                cube: def.name.clone(),
            });
        }
        index(
            "dimension",
            &within,
            def.dimensions.iter().map(String::as_str),
        )?;
        let dimensions = def
            .dimensions
            .iter()
            .map(|name| resolve(dimension_ids, "dimension", &within, name));
        let view = |(name, view): (&String, &ViewDef)| View {
            name: name.clone(),
            mdx: view.mdx.clone(),
        };
        Ok(Self {
            name: def.name.clone(),
            dimensions: dimensions.collect::<Result<_, _>>()?,
            views: def.views.iter().map(view).collect(),
        })
    }

    /// Finds a view by name, ignoring case and spaces.
    pub fn view(&self, name: &str) -> Option<&View> {
        self.views.iter().find(|view| key(&view.name) == key(name))
    }
}

#[derive(Debug)]
pub struct Attribute {
    pub name: String,
    pub kind: AttributeType,
}

#[derive(Debug)]
pub struct Dimension {
    pub name: String,
    /// Elements in declaration order.
    pub elements: Vec<Element>,
    pub attributes: Vec<Attribute>,
    /// Reusable selections of elements, in name order.
    pub subsets: Vec<Subset>,
    /// Element ids by the key of every name and alias.
    element_ids: HashMap<String, ElementId>,
    attribute_ids: HashMap<String, u32>,
}

impl Dimension {
    fn compile(def: &DimensionDef) -> Result<Self, ModelError> {
        let within = format!("dimension '{}'", def.name);
        let element_names = def.elements.iter().map(|e| e.name.as_str());
        let mut element_ids = index("element", &within, element_names)?;
        let attribute_ids = index(
            "attribute",
            &within,
            def.attributes.keys().map(String::as_str),
        )?;
        let attributes: Vec<Attribute> = def
            .attributes
            .iter()
            .map(|(name, &kind)| Attribute {
                name: name.clone(),
                kind,
            })
            .collect();
        let mut elements = def
            .elements
            .iter()
            .map(|e| Element::compile(e, &def.name, &element_ids, &attribute_ids, &attributes))
            .collect::<Result<Vec<_>, _>>()?;

        let links: Vec<(ElementId, ElementId)> = elements
            .iter()
            .enumerate()
            .flat_map(|(parent, e)| {
                e.children
                    .iter()
                    .map(move |&(child, _)| (parent as u32, child))
            })
            .collect();
        for (parent, child) in links {
            elements[child as usize].parents.push(parent);
        }

        for (id, element) in elements.iter().enumerate() {
            for (attribute, value) in attributes.iter().zip(&element.attributes) {
                let (AttributeType::Alias, Some(Scalar::Text(alias))) = (attribute.kind, value)
                else {
                    continue;
                };
                let key = key(alias);
                if key.is_empty() {
                    return Err(ModelError::EmptyName {
                        what: "alias",
                        within,
                    });
                }
                if *element_ids.entry(key).or_insert(id as u32) != id as u32 {
                    return Err(ModelError::AliasCollision {
                        dimension: def.name.clone(),
                        alias: alias.clone(),
                    });
                }
            }
        }

        let mut marks = vec![Mark::New; elements.len()];
        for id in 0..elements.len() {
            roll_up(&mut elements, &mut marks, id, &def.name)?;
        }
        Ok(Self {
            name: def.name.clone(),
            elements,
            attributes,
            subsets: Vec::new(),
            element_ids,
            attribute_ids,
        })
    }

    /// Finds a subset by name, ignoring case and spaces.
    pub fn subset(&self, name: &str) -> Option<&Subset> {
        self.subsets
            .iter()
            .find(|subset| key(&subset.name) == key(name))
    }

    /// Finds an element by name or alias, ignoring case and spaces.
    pub fn element_id(&self, name: &str) -> Option<ElementId> {
        self.element_ids.get(&key(name)).copied()
    }

    /// Finds an element by name or alias, ignoring case and spaces.
    pub fn element(&self, name: &str) -> Option<&Element> {
        self.element_id(name).map(|id| &self.elements[id as usize])
    }

    pub fn attribute_value(&self, element: ElementId, attribute: &str) -> Option<&Scalar> {
        let id = *self.attribute_ids.get(&key(attribute))?;
        self.elements[element as usize].attributes[id as usize].as_ref()
    }
}

#[derive(Debug)]
pub struct Element {
    pub name: String,
    pub kind: ElementType,
    /// Direct children and their weights, in declaration order.
    pub children: Vec<(ElementId, f64)>,
    /// Direct parents, in declaration order.
    pub parents: Vec<ElementId>,
    /// Zero for a leaf, otherwise one more than the highest child level.
    pub level: u32,
    /// The numeric leaves this element aggregates and the net weight of each, ordered by id.
    /// A leaf lists itself with weight one.
    pub leaves: Vec<(ElementId, f64)>,
    /// One value per attribute of the dimension, in [`Dimension::attributes`] order.
    pub attributes: Vec<Option<Scalar>>,
}

impl Element {
    fn compile(
        def: &ElementDef,
        dimension: &str,
        element_ids: &HashMap<String, ElementId>,
        attribute_ids: &HashMap<String, u32>,
        attributes: &[Attribute],
    ) -> Result<Self, ModelError> {
        let within = format!("element '{}' of dimension '{dimension}'", def.name);
        let consolidated = def.kind == ElementType::Consolidated;
        if consolidated == def.children.is_empty() {
            let (dimension, element) = (dimension.to_owned(), def.name.clone());
            return Err(if consolidated {
                ModelError::NoChildren { dimension, element }
            } else {
                ModelError::ChildrenOnLeaf { dimension, element }
            });
        }

        index(
            "child",
            &within,
            def.children.iter().map(|c| c.name.as_str()),
        )?;
        let children = def
            .children
            .iter()
            .map(|c| Ok((resolve(element_ids, "child", &within, &c.name)?, c.weight)))
            .collect::<Result<_, ModelError>>()?;

        index(
            "attribute",
            &within,
            def.attributes.keys().map(String::as_str),
        )?;
        let mut values = vec![None; attributes.len()];
        for (name, value) in &def.attributes {
            let id = resolve(attribute_ids, "attribute", &within, name)? as usize;
            let fits = match attributes[id].kind {
                AttributeType::Numeric => matches!(value, Scalar::Number(_)),
                AttributeType::Text | AttributeType::Alias => {
                    matches!(value, Scalar::Text(_))
                }
            };
            if !fits {
                return Err(ModelError::AttributeType {
                    dimension: dimension.to_owned(),
                    element: def.name.clone(),
                    attribute: name.clone(),
                });
            }
            values[id] = Some(value.clone());
        }

        Ok(Self {
            name: def.name.clone(),
            kind: def.kind,
            children,
            parents: Vec::new(),
            level: 0,
            leaves: Vec::new(),
            attributes: values,
        })
    }
}

#[derive(Clone, Copy)]
enum Mark {
    New,
    Open,
    Done,
}

/// Sets the level and leaves of an element after those of its children, rejecting cycles.
fn roll_up(
    elements: &mut [Element],
    marks: &mut [Mark],
    id: usize,
    dimension: &str,
) -> Result<(), ModelError> {
    match marks[id] {
        Mark::Done => return Ok(()),
        Mark::Open => {
            return Err(ModelError::Cycle {
                dimension: dimension.to_owned(),
                element: elements[id].name.clone(),
            });
        }
        Mark::New => marks[id] = Mark::Open,
    }

    let mut level = 0;
    let mut leaves = BTreeMap::new();
    if elements[id].children.is_empty() {
        leaves.insert(id as ElementId, 1.0);
    }
    for (child, weight) in elements[id].children.clone() {
        roll_up(elements, marks, child as usize, dimension)?;
        let child = &elements[child as usize];
        level = level.max(child.level + 1);
        if child.kind != ElementType::Text {
            for &(leaf, leaf_weight) in &child.leaves {
                *leaves.entry(leaf).or_insert(0.0) += weight * leaf_weight;
            }
        }
    }

    let element = &mut elements[id];
    element.level = level;
    element.leaves = leaves.into_iter().collect();
    marks[id] = Mark::Done;
    Ok(())
}

/// The form in which names are compared: case and spaces are ignored.
pub(crate) fn key(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Maps the key of each name to its position, rejecting empty and duplicate names.
fn index<'a>(
    what: &'static str,
    within: &str,
    names: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<String, u32>, ModelError> {
    let mut ids = HashMap::new();
    for (id, name) in names.into_iter().enumerate() {
        let key = key(name);
        if key.is_empty() {
            return Err(ModelError::EmptyName {
                what,
                within: within.to_owned(),
            });
        }
        if ids.insert(key, id as u32).is_some() {
            return Err(ModelError::Duplicate {
                what,
                within: within.to_owned(),
                name: name.to_owned(),
            });
        }
    }
    Ok(ids)
}

/// Looks a name up in an index built by [`index`].
fn resolve(
    ids: &HashMap<String, u32>,
    what: &'static str,
    within: &str,
    name: &str,
) -> Result<u32, ModelError> {
    ids.get(&key(name))
        .copied()
        .ok_or_else(|| ModelError::Unknown {
            what,
            within: within.to_owned(),
            name: name.to_owned(),
        })
}
