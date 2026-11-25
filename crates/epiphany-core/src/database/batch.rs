//! Cells by name as a request brings them, each name kept once and looked up once.

use std::fmt;

use rustc_hash::FxHashMap;
use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

use super::{CellRef, CellWrite};
use crate::Scalar;
use crate::cells::CellError;
use crate::model::{ElementId, Model};

/// Writes to cells by name, read as a list of [`CellWrite`]s is.
///
/// A large write names the same cube and the same few hundred elements over and over:
/// each name is kept once, and each cell as the numbers of its names and its value. Half
/// a million cells read into [`CellWrite`]s took 266 bytes a cell, two and a half million
/// small texts; here a cell takes some 50, and a name is looked up once, not once for
/// every cell it appears in.
#[derive(Debug, Clone, Default)]
pub struct CellWrites {
    listed: Listed,
    values: Vec<Scalar>,
}

/// Cells by name, read as a list of [`CellRef`]s is, each name kept once as in
/// [`CellWrites`].
#[derive(Debug, Clone, Default)]
pub struct CellRefs {
    listed: Listed,
}

/// The names of a list of cells.
#[derive(Debug, Clone, Default)]
pub(crate) struct Listed {
    names: Vec<String>,
    /// Where each name is in `names`, while the list is read.
    numbered: FxHashMap<String, u32>,
    /// The number of each cell's cube name, and where its element names start in
    /// `elements`.
    cells: Vec<(u32, u32)>,
    /// The numbers of each cell's element names, one cell after another.
    elements: Vec<u32>,
}

impl Listed {
    pub(crate) fn len(&self) -> usize {
        self.cells.len()
    }

    /// The numbers of a cell's element names.
    fn element_names(&self, cell: usize) -> &[u32] {
        let from = self.cells[cell].1 as usize;
        let to = self
            .cells
            .get(cell + 1)
            .map_or(self.elements.len(), |&(_, next)| next as usize);
        &self.elements[from..to]
    }

    fn name(&self, number: u32) -> &str {
        &self.names[number as usize]
    }

    /// A cell's cube, by name.
    pub(crate) fn cube(&self, cell: usize) -> &str {
        self.name(self.cells[cell].0)
    }

    /// A cell's elements, by name.
    pub(crate) fn at(&self, cell: usize) -> impl Iterator<Item = &str> {
        self.element_names(cell).iter().map(|&name| self.name(name))
    }

    fn number(&mut self, name: &str) -> u32 {
        if let Some(&number) = self.numbered.get(name) {
            return number;
        }
        let number = self.names.len() as u32;
        self.names.push(name.to_owned());
        self.numbered.insert(name.to_owned(), number);
        number
    }

    fn push<'a>(&mut self, cube: &str, at: impl IntoIterator<Item = &'a str>) {
        let cube = self.number(cube);
        let from = self.elements.len() as u32;
        for name in at {
            let name = self.number(name);
            self.elements.push(name);
        }
        self.cells.push((cube, from));
    }

    /// Done with reading: the names are no longer looked up by text.
    fn read(mut self) -> Self {
        self.numbered = FxHashMap::default();
        self
    }
}

impl CellWrites {
    pub fn len(&self) -> usize {
        self.listed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.listed.len() == 0
    }

    /// A cell's cube, by name.
    pub fn cube(&self, cell: usize) -> &str {
        self.listed.cube(cell)
    }

    pub(crate) fn listed(&self) -> &Listed {
        &self.listed
    }

    pub(crate) fn value(&self, cell: usize) -> &Scalar {
        &self.values[cell]
    }
}

impl CellRefs {
    pub fn len(&self) -> usize {
        self.listed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.listed.len() == 0
    }

    /// A cell's cube, by name.
    pub fn cube(&self, cell: usize) -> &str {
        self.listed.cube(cell)
    }

    pub(crate) fn listed(&self) -> &Listed {
        &self.listed
    }
}

impl From<&[CellWrite]> for CellWrites {
    fn from(writes: &[CellWrite]) -> Self {
        let mut listed = Listed::default();
        for write in writes {
            listed.push(&write.cube, write.at.iter().map(String::as_str));
        }
        let values = writes.iter().map(|write| write.value.clone()).collect();
        Self {
            listed: listed.read(),
            values,
        }
    }
}

impl From<&[CellRef]> for CellRefs {
    fn from(cells: &[CellRef]) -> Self {
        let mut listed = Listed::default();
        for cell in cells {
            listed.push(&cell.cube, cell.at.iter().map(String::as_str));
        }
        Self {
            listed: listed.read(),
        }
    }
}

/// Looks the names of a list of cells up in a model: each name once as a cube, and once in
/// each dimension it is met in.
pub(crate) struct Resolver<'a> {
    model: &'a Model,
    listed: &'a Listed,
    cubes: Vec<Option<Option<usize>>>,
    elements: FxHashMap<(u32, u32), Option<ElementId>>,
    at: Vec<ElementId>,
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(model: &'a Model, listed: &'a Listed) -> Self {
        Self {
            model,
            listed,
            cubes: vec![None; listed.names.len()],
            elements: FxHashMap::default(),
            at: Vec::new(),
        }
    }

    /// A cell's cube and coordinate, as [`crate::cells::coordinate`] finds them.
    pub(crate) fn locate(&mut self, cell: usize) -> Result<(usize, &[ElementId]), CellError> {
        let (model, listed) = (self.model, self.listed);
        let name = listed.cells[cell].0;
        let cube =
            *self.cubes[name as usize].get_or_insert_with(|| model.cube_id(listed.name(name)));
        let cube = cube.ok_or_else(|| CellError::UnknownCube {
            cube: listed.name(name).to_owned(),
        })?;
        let dimensions = &model.cubes()[cube].dimensions;
        let names = listed.element_names(cell);
        if names.len() != dimensions.len() {
            return Err(CellError::Arity {
                expected: dimensions.len(),
                found: names.len(),
            });
        }
        self.at.clear();
        for (&id, &name) in dimensions.iter().zip(names) {
            let dimension = &model.dimensions()[id as usize];
            let found = self
                .elements
                .entry((id, name))
                .or_insert_with(|| dimension.element_id(listed.name(name)));
            self.at.push(found.ok_or_else(|| CellError::UnknownElement {
                dimension: dimension.name.clone(),
                element: listed.name(name).to_owned(),
            })?);
        }
        Ok((cube, &self.at))
    }
}

impl<'de> Deserialize<'de> for CellWrites {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut writes = CellWrites::default();
        deserializer.deserialize_seq(Reading {
            listed: &mut writes.listed,
            values: Some(&mut writes.values),
            elements: Vec::new(),
        })?;
        writes.listed = std::mem::take(&mut writes.listed).read();
        Ok(writes)
    }
}

impl<'de> Deserialize<'de> for CellRefs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut refs = CellRefs::default();
        deserializer.deserialize_seq(Reading {
            listed: &mut refs.listed,
            values: None,
            elements: Vec::new(),
        })?;
        refs.listed = std::mem::take(&mut refs.listed).read();
        Ok(refs)
    }
}

/// The list, read a cell at a time: with a value to each cell where there are `values`,
/// and none allowed where there are not. `elements` is kept from one cell to the next.
struct Reading<'a> {
    listed: &'a mut Listed,
    values: Option<&'a mut Vec<Scalar>>,
    elements: Vec<u32>,
}

impl<'de> Visitor<'de> for Reading<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a list of cells")
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut cells: A) -> Result<(), A::Error> {
        while cells.next_element_seed(Cell(&mut self))?.is_some() {}
        Ok(())
    }
}

/// One cell: an object of its cube, the names of its elements, and its value if it is
/// written; or a list of the same in that order. Read as the derived [`CellWrite`] and
/// [`CellRef`] are, refused as they are.
struct Cell<'a, 'b>(&'a mut Reading<'b>);

/// The fields of a cell write, and of a cell to read.
pub(crate) const WRITE: &[&str] = &["cube", "at", "value"];
const REF: &[&str] = &["cube", "at"];

impl Cell<'_, '_> {
    fn shape(&self) -> (&'static str, &'static [&'static str]) {
        match self.0.values {
            Some(_) => ("CellWrite", WRITE),
            None => ("CellRef", REF),
        }
    }

    /// Adds a cell whose cube and element names have been numbered into the list.
    fn push(self, cube: u32, value: Option<Scalar>) {
        let Reading {
            listed,
            values,
            elements,
        } = self.0;
        if let (Some(values), Some(value)) = (values, value) {
            values.push(value);
        }
        let from = listed.elements.len() as u32;
        listed.elements.extend_from_slice(elements);
        listed.cells.push((cube, from));
    }
}

impl<'de> DeserializeSeed<'de> for Cell<'_, '_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        let (name, fields) = self.shape();
        deserializer.deserialize_struct(name, fields, self)
    }
}

impl<'de> Visitor<'de> for Cell<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "struct {}", self.shape().0)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let fields = self.shape().1;
        let (mut cube, mut at, mut value) = (None, false, None);
        while let Some(field) = map.next_key_seed(Key(fields))? {
            let Reading {
                listed, elements, ..
            } = &mut *self.0;
            match field {
                0 if cube.is_some() => return Err(de::Error::duplicate_field("cube")),
                1 if at => return Err(de::Error::duplicate_field("at")),
                2 if value.is_some() => return Err(de::Error::duplicate_field("value")),
                0 => cube = Some(map.next_value_seed(Name(listed))?),
                1 => {
                    elements.clear();
                    map.next_value_seed(Names { listed, elements })?;
                    at = true;
                }
                _ => value = Some(map.next_value::<Scalar>()?),
            }
        }
        let cube = cube.ok_or_else(|| de::Error::missing_field("cube"))?;
        if !at {
            return Err(de::Error::missing_field("at"));
        }
        if fields.len() == WRITE.len() && value.is_none() {
            return Err(de::Error::missing_field("value"));
        }
        self.push(cube, value);
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut parts: A) -> Result<(), A::Error> {
        let (name, fields) = self.shape();
        let short = |found| de::Error::invalid_length(found, &Fields(name, fields.len()));
        let Reading {
            listed, elements, ..
        } = &mut *self.0;
        let cube = parts
            .next_element_seed(Name(listed))?
            .ok_or_else(|| short(0))?;
        elements.clear();
        parts
            .next_element_seed(Names { listed, elements })?
            .ok_or_else(|| short(1))?;
        let value = match fields.len() == WRITE.len() {
            true => Some(parts.next_element::<Scalar>()?.ok_or_else(|| short(2))?),
            false => None,
        };
        self.push(cube, value);
        Ok(())
    }
}

/// What a cell given as a list must hold: as derived code says it.
struct Fields(&'static str, usize);

impl de::Expected for Fields {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "struct {} with {} elements", self.0, self.1)
    }
}

/// A field's name, given as its place among `fields`. One of no such name is refused as
/// derived code refuses it, naming the fields there are.
pub(crate) struct Key(pub(crate) &'static [&'static str]);

impl<'de> DeserializeSeed<'de> for Key {
    type Value = usize;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<usize, D::Error> {
        deserializer.deserialize_identifier(self)
    }
}

impl Visitor<'_> for Key {
    type Value = usize;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("field identifier")
    }

    fn visit_str<E: de::Error>(self, name: &str) -> Result<usize, E> {
        let place = self.0.iter().position(|&field| field == name);
        place.ok_or_else(|| E::unknown_field(name, self.0))
    }
}
/// A name, numbered as it is read.
struct Name<'a>(&'a mut Listed);

impl<'de> DeserializeSeed<'de> for Name<'_> {
    type Value = u32;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<u32, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl Visitor<'_> for Name<'_> {
    type Value = u32;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a string")
    }

    fn visit_str<E: de::Error>(self, name: &str) -> Result<u32, E> {
        Ok(self.0.number(name))
    }
}

/// A list of element names, numbered into `elements` as they are read.
struct Names<'a> {
    listed: &'a mut Listed,
    elements: &'a mut Vec<u32>,
}

impl<'de> DeserializeSeed<'de> for Names<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Names<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sequence")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut names: A) -> Result<(), A::Error> {
        while let Some(name) = names.next_element_seed(Name(&mut *self.listed))? {
            self.elements.push(name);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(listed: &Listed) -> Vec<(String, Vec<String>)> {
        (0..listed.len())
            .map(|cell| {
                let at = listed.at(cell).map(str::to_owned).collect();
                (listed.cube(cell).to_owned(), at)
            })
            .collect()
    }

    #[test]
    fn cells_read_as_their_cell_writes_and_refs_do_and_keep_each_name_once() {
        let text = r#"[
            { "cube": "C", "at": ["a", "b"], "value": 1 },
            { "value": "note", "at": ["b", "a", "c"], "cube": "C" },
            { "at": [], "cube": "D", "value": 0 }
        ]"#;
        let batch: CellWrites = serde_json::from_str(text).unwrap();
        let writes: Vec<CellWrite> = serde_json::from_str(text).unwrap();
        let named =
            |cube: &str, at: &[&str]| (cube.to_owned(), at.iter().map(|&n| n.to_owned()).collect());
        let expected = [
            named("C", &["a", "b"]),
            named("C", &["b", "a", "c"]),
            named("D", &[]),
        ];
        assert_eq!(cells(&batch.listed), expected);
        assert_eq!(cells(&CellWrites::from(writes.as_slice()).listed), expected);
        let values: Vec<Scalar> = writes.into_iter().map(|write| write.value).collect();
        assert_eq!(batch.values, values);
        assert_eq!(batch.listed.names.len(), 5);

        let text = r#"[{ "cube": "C", "at": ["a", "b"] }, { "at": ["b"], "cube": "D" }]"#;
        let refs: CellRefs = serde_json::from_str(text).unwrap();
        // A cell may also be given as a list of its fields, in order.
        let listed: CellRefs =
            serde_json::from_str(r#"[["C", ["a", "b"]], ["D", ["b"]]]"#).unwrap();
        assert_eq!(cells(&listed.listed), cells(&refs.listed));
        let listed: CellWrites = serde_json::from_str(r#"[["C", ["a"], 2]]"#).unwrap();
        assert_eq!(
            (cells(&listed.listed), listed.values),
            (vec![named("C", &["a"])], vec![Scalar::Number(2.0)])
        );
        let expected = [named("C", &["a", "b"]), named("D", &["b"])];
        assert_eq!(cells(&refs.listed), expected);
    }

    #[test]
    fn a_cell_with_a_field_missing_repeated_or_unknown_is_refused() {
        for (text, refused) in [
            (r#"[{ "cube": "C", "at": ["a"] }]"#, "missing field `value`"),
            (r#"[{ "at": ["a"], "value": 1 }]"#, "missing field `cube`"),
            (r#"[{ "cube": "C", "value": 1 }]"#, "missing field `at`"),
            (
                r#"[{ "cube": "C", "cube": "D", "at": [], "value": 1 }]"#,
                "duplicate field `cube`",
            ),
            (
                r#"[{ "cube": "C", "at": [], "value": 1, "where": 2 }]"#,
                "unknown field `where`",
            ),
            (
                r#"[{ "cube": "C", "at": [1], "value": 1 }]"#,
                "expected a string",
            ),
        ] {
            let error = serde_json::from_str::<CellWrites>(text)
                .unwrap_err()
                .to_string();
            assert!(error.contains(refused), "{text}: {error}");
        }
        let refused = |text| {
            serde_json::from_str::<CellRefs>(text)
                .unwrap_err()
                .to_string()
        };
        let error = refused(r#"[{ "cube": "C", "at": [], "where": 1 }]"#);
        assert!(
            error.contains("unknown field `where`, expected `cube` or `at`"),
            "{error}"
        );
        let error = refused(r#"[["C"]]"#);
        assert!(error.contains("struct CellRef with 2 elements"), "{error}");
        let error = serde_json::from_str::<CellRefs>(r#"[{ "cube": "C", "at": [], "value": 1 }]"#);
        assert!(
            error
                .unwrap_err()
                .to_string()
                .contains("unknown field `value`")
        );
    }
}
