//! The model snapshot: a validated, immutable form of a definition, indexed for lookup.

// Names are looked up for every cell read or written by name, so with the quick hash.
use rustc_hash::FxHashMap as HashMap;

use super::definition::{
    AttributeType, CubeDef, DimensionDef, DimensionType, ElementDef, ElementType, MembersDef,
    ModelDef, PicklistDef, SubsetDef, ViewDef,
};
use super::error::ModelError;
use crate::Scalar;
use crate::mdx::{self, Expr, Feeder, Rule};

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
                let (elements, reading) =
                    match subset {
                        SubsetDef::Elements(names) => {
                            let ids = &model.dimensions[id].element_ids;
                            let element = |name: &String| resolve(ids, "element", &within, name);
                            (names.iter().map(element).collect::<Result<_, _>>()?, None)
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
                    reading,
                });
            }
        }
        // A picklist may offer another dimension's elements, or a subset's, so every subset
        // is in place first; it is worked out to its values once, here.
        for (id, dimension) in def.dimensions.iter().enumerate() {
            for (element, definition) in dimension.elements.iter().enumerate() {
                let Some(picklist) = &definition.picklist else {
                    continue;
                };
                if definition.kind != ElementType::Text {
                    return Err(ModelError::Picklist {
                        dimension: dimension.name.clone(),
                        element: definition.name.clone(),
                    });
                }
                let values = model.picklist(picklist, &dimension.name, &definition.name)?;
                let of = match picklist {
                    PicklistDef::Members(members) => model.dimension_id(&members.dimension),
                    PicklistDef::Values(_) => None,
                };
                let picklist = Picklist { values, of };
                model.dimensions[id]
                    .picklists
                    .insert(element as ElementId, picklist);
            }
        }
        // Rules and feeders may refer to other cubes, so every cube is in place first.
        for (id, definition) in def.cubes.iter().enumerate() {
            let within = |kind: &str, number: usize| {
                format!("{kind} {} of cube '{}'", number + 1, definition.name)
            };
            let mut rules = Vec::new();
            for (number, text) in definition.rules.iter().enumerate() {
                rules.push(mdx::compile_rule(&model, id, text).map_err(|error| {
                    ModelError::Mdx {
                        within: within("rule", number),
                        error: error.to_string(),
                    }
                })?);
            }
            let mut feeders = Vec::new();
            for (number, text) in definition.feeders.iter().enumerate() {
                feeders.push(mdx::compile_feeder(&model, id, text).map_err(|error| {
                    ModelError::Mdx {
                        within: within("feeder", number),
                        error: error.to_string(),
                    }
                })?);
            }
            for fed in feeders.iter().flat_map(|feeder| &feeder.feeds) {
                model.cubes[*fed].sparse = true;
            }
            // A check is a rule whose expression says whether a value is acceptable.
            let mut validations = Vec::new();
            for (number, validation) in definition.validations.iter().enumerate() {
                let text = format!("{} = {}", validation.cells, validation.valid);
                let check =
                    mdx::compile_rule(&model, id, &text).map_err(|error| ModelError::Mdx {
                        within: within("validation", number),
                        error: error.to_string(),
                    })?;
                let message = match validation.message.trim() {
                    "" => format!("the value breaks {}", within("validation", number)),
                    message => message.to_owned(),
                };
                validations.push(Validation {
                    check,
                    message,
                    refuse: validation.refuse,
                });
            }
            model.cubes[id].rules = rules;
            model.cubes[id].feeders = feeders;
            model.cubes[id].validations = validations;
        }
        for (id, definition) in def.cubes.iter().enumerate() {
            let within = format!("cube '{}'", definition.name);
            index("view", &within, definition.views.keys().map(String::as_str))?;
            for (name, view) in &definition.views {
                mdx::check_view(&model, id, &view.mdx).map_err(|error| ModelError::Mdx {
                    within: format!("view '{name}' of {within}"),
                    error: error.to_string(),
                })?;
            }
        }
        // Every tile of a dashboard shows a view that is there.
        index(
            "dashboard",
            WITHIN,
            def.dashboards.iter().map(|d| d.name.as_str()),
        )?;
        for dashboard in &def.dashboards {
            let within = format!("dashboard '{}'", dashboard.name);
            for filter in &dashboard.filters {
                resolve(&model.dimension_ids, "dimension", &within, filter)?;
            }
            for tile in &dashboard.tiles {
                let cube = resolve(&model.cube_ids, "cube", &within, &tile.cube)?;
                let views = def.cubes[cube as usize].views.keys().map(String::as_str);
                let within = format!("{within}, cube '{}'", tile.cube);
                resolve(&index("view", &within, views)?, "view", &within, &tile.view)?;
                if !(1..=2).contains(&tile.width) {
                    return Err(ModelError::TileWidth {
                        dashboard: dashboard.name.clone(),
                        width: tile.width,
                    });
                }
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
        self.dimension_id(name)
            .map(|id| &self.dimensions[id as usize])
    }

    /// The position of a dimension in [`Model::dimensions`].
    pub fn dimension_id(&self, name: &str) -> Option<DimensionId> {
        self.dimension_ids.get(&key(name)).copied()
    }

    /// The values a picklist offers: those it gives, or the names of a dimension's elements,
    /// or of a subset's, in order.
    fn picklist(
        &self,
        picklist: &PicklistDef,
        dimension: &str,
        element: &str,
    ) -> Result<Vec<String>, ModelError> {
        let MembersDef {
            dimension: from,
            subset,
        } = match picklist {
            PicklistDef::Values(values) => return Ok(values.clone()),
            PicklistDef::Members(members) => members,
        };
        let unknown = |what, name: &str| ModelError::Unknown {
            what,
            within: format!("the picklist of element '{element}' of dimension '{dimension}'"),
            name: name.to_owned(),
        };
        let source = self
            .dimension(from)
            .ok_or_else(|| unknown("dimension", from))?;
        let name = |&id: &ElementId| source.elements[id as usize].name.clone();
        Ok(match subset {
            Some(subset) => {
                let chosen = source
                    .subset(subset)
                    .ok_or_else(|| unknown("subset", subset))?;
                // Worked out once, here, a picklist cannot follow the cells.
                if chosen.reads_cells() {
                    return Err(ModelError::PicklistReadsCells {
                        dimension: dimension.to_owned(),
                        element: element.to_owned(),
                        subset: chosen.name.clone(),
                    });
                }
                chosen.elements.iter().map(name).collect()
            }
            None => source
                .elements
                .iter()
                .map(|each| each.name.clone())
                .collect(),
        })
    }

    /// The position of a cube in [`Model::cubes`].
    pub fn cube_id(&self, name: &str) -> Option<usize> {
        by_key(name, |key| self.cube_ids.get(key).map(|&id| id as usize))
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
    /// The rules that calculate cells, in the order they are tried.
    pub rules: Vec<Rule>,
    pub feeders: Vec<Feeder>,
    /// The checks of its values, in the order they are made.
    pub validations: Vec<Validation>,
    /// Whether some feeder feeds this cube, in which case its consolidations add up only
    /// stored and fed cells.
    pub sparse: bool,
}

/// A check of the values in an area of a cube: a rule whose expression holds wherever a
/// value is acceptable, what is said where it does not, and whether such a value is refused.
#[derive(Debug)]
pub struct Validation {
    pub check: Rule,
    pub message: String,
    pub refuse: bool,
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
    /// The elements it selects; for one that reads cells, as worked out with every cell
    /// empty, which [`Database::subset_elements`](crate::database::Database::subset_elements)
    /// works out as the cells stand.
    pub elements: Vec<ElementId>,
    /// The set, for a subset whose MDX reads cells, to be worked out whenever it is used.
    reading: Option<Expr>,
}

impl Subset {
    /// Whether it reads cells, and so selects what the cells say when it is used.
    pub fn reads_cells(&self) -> bool {
        self.reading.is_some()
    }

    pub(crate) fn reading(&self) -> Option<&Expr> {
        self.reading.as_ref()
    }
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
            rules: Vec::new(),
            feeders: Vec::new(),
            validations: Vec::new(),
            sparse: false,
        })
    }

    /// The first rule that may decide a leaf cell, if any: such a cell cannot be written.
    pub(crate) fn rule_for_leaf(&self, at: &[ElementId], text: bool) -> Option<&Rule> {
        self.rules.iter().find(|rule| rule.covers(at, text, false))
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
    pub kind: DimensionType,
    /// The names given to the levels, from the leaves up.
    pub levels: Vec<String>,
    /// Elements in declaration order.
    pub elements: Vec<Element>,
    pub attributes: Vec<Attribute>,
    /// Reusable selections of elements, in name order.
    pub subsets: Vec<Subset>,
    /// Element ids by the key of every name and alias.
    element_ids: HashMap<String, ElementId>,
    attribute_ids: HashMap<String, u32>,
    /// The picklists of the few string elements that have one, by element.
    picklists: HashMap<ElementId, Picklist>,
}

/// What a string element's text cells may be given: worked out with the model, from a list or
/// a dimension's or subset's elements.
#[derive(Debug)]
pub struct Picklist {
    pub values: Vec<String>,
    /// The dimension whose elements it offers, where it offers a dimension's or a subset's:
    /// for those it offers to be told by what each user may see of them.
    pub of: Option<DimensionId>,
}

impl Dimension {
    /// A string element's picklist, where it has one.
    pub fn picklist(&self, element: ElementId) -> Option<&Picklist> {
        self.picklists.get(&element)
    }

    fn compile(def: &DimensionDef) -> Result<Self, ModelError> {
        let within = format!("dimension '{}'", def.name);
        if def.elements.is_empty() {
            return Err(ModelError::NoElements {
                dimension: def.name.clone(),
            });
        }
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
        // For each element, the last element to take it as a child: a second time is a duplicate.
        let mut taken = vec![ElementId::MAX; def.elements.len()];
        let mut elements = def
            .elements
            .iter()
            .enumerate()
            .map(|(id, e)| {
                let names = Names {
                    elements: &element_ids,
                    attributes: &attribute_ids,
                };
                Element::compile(
                    e,
                    id as ElementId,
                    &def.name,
                    names,
                    &mut taken,
                    &attributes,
                )
            })
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
            roll_up(&mut elements, &mut marks, id, &def.name, 0)?;
        }
        // Two levels cannot share a name. A level and an element can, since the top of a
        // calendar is as likely as not called Year: there the name means the element.
        index("level", &within, def.levels.iter().map(String::as_str))?;
        Ok(Self {
            name: def.name.clone(),
            kind: def.kind,
            levels: def.levels.clone(),
            elements,
            attributes,
            subsets: Vec::new(),
            element_ids,
            attribute_ids,
            picklists: HashMap::default(),
        })
    }

    /// Finds a level by the name the dimension gives it, ignoring case and spaces.
    pub fn level(&self, name: &str) -> Option<u32> {
        let found = self.levels.iter().position(|level| key(level) == key(name));
        found.map(|level| level as u32)
    }

    /// Finds a subset by name, ignoring case and spaces.
    pub fn subset(&self, name: &str) -> Option<&Subset> {
        self.subsets
            .iter()
            .find(|subset| key(&subset.name) == key(name))
    }

    /// Finds an element by name or alias, ignoring case and spaces.
    pub fn element_id(&self, name: &str) -> Option<ElementId> {
        by_key(name, |key| self.element_ids.get(key).copied())
    }

    /// Finds an element by name or alias, ignoring case and spaces.
    pub fn element(&self, name: &str) -> Option<&Element> {
        self.element_id(name).map(|id| &self.elements[id as usize])
    }

    pub fn attribute_value(&self, element: ElementId, attribute: &str) -> Option<&Scalar> {
        let id = by_key(attribute, |key| self.attribute_ids.get(key).copied())?;
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
    /// One value per attribute of the dimension, in [`Dimension::attributes`] order. A
    /// picklist is the dimension's, by [`Dimension::picklist`]: few elements have one.
    pub attributes: Vec<Option<Scalar>>,
}

/// The indexes of a dimension's element and attribute names, as its elements are compiled.
#[derive(Clone, Copy)]
struct Names<'a> {
    elements: &'a HashMap<String, ElementId>,
    attributes: &'a HashMap<String, u32>,
}

impl Element {
    /// Compiles element `id`. `taken` holds, for each element, the last to take it as a child.
    fn compile(
        def: &ElementDef,
        id: ElementId,
        dimension: &str,
        names: Names,
        taken: &mut [ElementId],
        attributes: &[Attribute],
    ) -> Result<Self, ModelError> {
        // Written out for a refusal only: a model has as many elements as it may have.
        let within = || format!("element '{}' of dimension '{dimension}'", def.name);
        let consolidated = def.kind == ElementType::Consolidated;
        if consolidated == def.children.is_empty() {
            let (dimension, element) = (dimension.to_owned(), def.name.clone());
            return Err(if consolidated {
                ModelError::NoChildren { dimension, element }
            } else {
                ModelError::ChildrenOnLeaf { dimension, element }
            });
        }

        // Each child is looked up by the key of its name, made where the last was; one named
        // twice is found by its id, taken already by this element.
        let mut children = Vec::with_capacity(def.children.len());
        for child in &def.children {
            let found = by_key(&child.name, |key| {
                (!key.is_empty()).then(|| names.elements.get(key).copied())
            });
            let name = || child.name.clone();
            let child_id = match found {
                None => {
                    let within = within();
                    return Err(ModelError::EmptyName {
                        what: "child",
                        within,
                    });
                }
                Some(None) => {
                    let (within, name) = (within(), name());
                    return Err(ModelError::Unknown {
                        what: "child",
                        within,
                        name,
                    });
                }
                Some(Some(child_id)) => child_id,
            };
            if std::mem::replace(&mut taken[child_id as usize], id) == id {
                let (within, name) = (within(), name());
                return Err(ModelError::Duplicate {
                    what: "child",
                    within,
                    name,
                });
            }
            children.push((child_id, child.weight));
        }

        let mut values = vec![None; attributes.len()];
        if !def.attributes.is_empty() {
            let within = within();
            index(
                "attribute",
                &within,
                def.attributes.keys().map(String::as_str),
            )?;
            for (name, value) in &def.attributes {
                let id = resolve(names.attributes, "attribute", &within, name)? as usize;
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

/// How many levels a hierarchy may have. Working a hierarchy out goes as deep as it does, here
/// and wherever it is walked afterwards, and one of thousands of levels, which nothing a
/// person would draw comes near, would run a thread out of stack.
const MOST_LEVELS: usize = 100;

/// Sets the level and leaves of an element after those of its children, rejecting cycles
/// and hierarchies too deep. `above` is how many elements stand above this one on the way
/// here.
fn roll_up(
    elements: &mut [Element],
    marks: &mut [Mark],
    id: usize,
    dimension: &str,
    above: usize,
) -> Result<(), ModelError> {
    if above > MOST_LEVELS {
        return Err(ModelError::TooDeep {
            dimension: dimension.to_owned(),
            element: elements[id].name.clone(),
            most: MOST_LEVELS,
        });
    }
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
    let mut leaves = Vec::new();
    if elements[id].children.is_empty() {
        leaves.push((id as ElementId, 1.0));
    }
    for index in 0..elements[id].children.len() {
        let (child, weight) = elements[id].children[index];
        roll_up(elements, marks, child as usize, dimension, above + 1)?;
        let child = &elements[child as usize];
        level = level.max(child.level + 1);
        if child.kind != ElementType::Text {
            let weighed = child
                .leaves
                .iter()
                .map(|&(leaf, each)| (leaf, weight * each));
            leaves.extend(weighed);
        }
    }
    // In order of id, a leaf reached more than one way once, the weights of the ways added in
    // the order of the children: the sort keeps that order among them, and children whose
    // leaves follow one another, as they mostly do, are in order already.
    leaves.sort_by_key(|&(leaf, _)| leaf);
    leaves.dedup_by(|later, kept| {
        let same = later.0 == kept.0;
        if same {
            kept.1 += later.1;
        }
        same
    });

    // Reached from the top the depth is counted on the way down, and from anywhere else it
    // is the level, counted on the way back up.
    if level as usize > MOST_LEVELS {
        return Err(ModelError::TooDeep {
            dimension: dimension.to_owned(),
            element: elements[id].name.clone(),
            most: MOST_LEVELS,
        });
    }
    let element = &mut elements[id];
    element.level = level;
    element.leaves = leaves;
    marks[id] = Mark::Done;
    Ok(())
}

/// The form in which names are compared: case and spaces are ignored.
pub fn key(name: &str) -> String {
    let mut key = String::with_capacity(name.len());
    write_key(name, &mut key);
    key
}

/// Looks something up by the key of a name. The key is written where the last one was, so
/// that the lookups of every cell read or written by name make no new text each.
fn by_key<T>(name: &str, find: impl FnOnce(&str) -> T) -> T {
    thread_local! {
        static KEY: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    }
    KEY.with(|key| {
        let mut key = key.borrow_mut();
        key.clear();
        write_key(name, &mut key);
        find(&key)
    })
}

fn write_key(name: &str, key: &mut String) {
    // Most names are plain letters and digits, which are lowered a byte at a time.
    match name.is_ascii() {
        true => {
            let kept = name
                .bytes()
                .filter(|&byte| !char::from(byte).is_whitespace());
            key.extend(kept.map(|byte| char::from(byte.to_ascii_lowercase())));
        }
        false => {
            let kept = name.chars().filter(|c| !c.is_whitespace());
            key.extend(kept.flat_map(char::to_lowercase));
        }
    }
}

/// Maps the key of each name to its position, rejecting empty and duplicate names.
fn index<'a>(
    what: &'static str,
    within: &str,
    names: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<String, u32>, ModelError> {
    let mut ids = HashMap::default();
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
