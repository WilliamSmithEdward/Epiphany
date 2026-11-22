//! Cell writes by name as a request brings them, each name kept once.

use std::fmt;

use rustc_hash::FxHashMap;
use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

use super::CellWrite;
use crate::Scalar;

/// Writes to cells by name, read as a list of [`CellWrite`]s is.
///
/// A large write names the same cube and the same few hundred elements over and over:
/// each name is kept once, and each cell as the numbers of its names and its value. Half
/// a million cells read into [`CellWrite`]s took 266 bytes a cell, two and a half million
/// small texts; here a cell takes some 50, and a name is looked up once, not once for
/// every cell it appears in.
#[derive(Debug, Clone, Default)]
pub struct Batch {
    names: Vec<String>,
    /// Where each name is in `names`.
    numbered: FxHashMap<String, u32>,
    cells: Vec<Cell>,
    /// The numbers of each cell's element names, one cell after another.
    elements: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
struct Cell {
    cube: u32,
    /// Where the cell's element names start in `elements`.
    from: u32,
    value: Scalar,
}

impl Batch {
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// The number of a cell's cube name.
    pub(crate) fn cube_name(&self, cell: usize) -> u32 {
        self.cells[cell].cube
    }

    /// The numbers of a cell's element names.
    pub(crate) fn element_names(&self, cell: usize) -> &[u32] {
        let from = self.cells[cell].from as usize;
        let to = self
            .cells
            .get(cell + 1)
            .map_or(self.elements.len(), |next| next.from as usize);
        &self.elements[from..to]
    }

    /// A name by its number.
    pub(crate) fn name(&self, number: u32) -> &str {
        &self.names[number as usize]
    }

    /// How many names there are.
    pub(crate) fn names(&self) -> usize {
        self.names.len()
    }

    /// A cell's cube, by name.
    pub fn cube(&self, cell: usize) -> &str {
        self.name(self.cube_name(cell))
    }

    pub(crate) fn value(&self, cell: usize) -> &Scalar {
        &self.cells[cell].value
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

    fn push(&mut self, cube: u32, elements: &[u32], value: Scalar) {
        let from = self.elements.len() as u32;
        self.elements.extend_from_slice(elements);
        self.cells.push(Cell { cube, from, value });
    }
}

impl From<&[CellWrite]> for Batch {
    fn from(writes: &[CellWrite]) -> Self {
        let mut batch = Batch::default();
        let mut elements = Vec::new();
        for write in writes {
            let cube = batch.number(&write.cube);
            elements.clear();
            elements.extend(write.at.iter().map(|name| batch.number(name)));
            batch.push(cube, &elements, write.value.clone());
        }
        batch.numbered = FxHashMap::default();
        batch
    }
}

impl<'de> Deserialize<'de> for Batch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut batch = Batch::default();
        deserializer.deserialize_seq(Reading {
            batch: &mut batch,
            elements: Vec::new(),
        })?;
        batch.numbered = FxHashMap::default();
        Ok(batch)
    }
}

/// The list, read a cell at a time into the batch. `elements` is kept from one cell to the
/// next.
struct Reading<'a> {
    batch: &'a mut Batch,
    elements: Vec<u32>,
}

impl<'de> Visitor<'de> for Reading<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a list of cell writes")
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut cells: A) -> Result<(), A::Error> {
        while cells.next_element_seed(Write(&mut self))?.is_some() {}
        Ok(())
    }
}

/// One cell write: an object of its cube, the names of its elements and its value.
struct Write<'a, 'b>(&'a mut Reading<'b>);

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "lowercase")]
enum Field {
    Cube,
    At,
    Value,
}

impl<'de> DeserializeSeed<'de> for Write<'_, '_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Write<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a cell write, with its cube, elements and value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let Reading { batch, elements } = self.0;
        let (mut cube, mut at, mut value) = (None, false, None);
        while let Some(field) = map.next_key()? {
            match field {
                Field::Cube if cube.is_some() => return Err(de::Error::duplicate_field("cube")),
                Field::At if at => return Err(de::Error::duplicate_field("at")),
                Field::Value if value.is_some() => {
                    return Err(de::Error::duplicate_field("value"));
                }
                Field::Cube => cube = Some(map.next_value_seed(Name(batch))?),
                Field::At => {
                    elements.clear();
                    map.next_value_seed(Names { batch, elements })?;
                    at = true;
                }
                Field::Value => value = Some(map.next_value::<Scalar>()?),
            }
        }
        let cube = cube.ok_or_else(|| de::Error::missing_field("cube"))?;
        if !at {
            return Err(de::Error::missing_field("at"));
        }
        let value = value.ok_or_else(|| de::Error::missing_field("value"))?;
        batch.push(cube, elements, value);
        Ok(())
    }
}

/// A name, numbered as it is read.
struct Name<'a>(&'a mut Batch);

impl<'de> DeserializeSeed<'de> for Name<'_> {
    type Value = u32;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<u32, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl Visitor<'_> for Name<'_> {
    type Value = u32;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a name")
    }

    fn visit_str<E: de::Error>(self, name: &str) -> Result<u32, E> {
        Ok(self.0.number(name))
    }
}

/// A list of element names, numbered into `elements` as they are read.
struct Names<'a> {
    batch: &'a mut Batch,
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
        formatter.write_str("a list of element names")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut names: A) -> Result<(), A::Error> {
        while let Some(name) = names.next_element_seed(Name(&mut *self.batch))? {
            self.elements.push(name);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(batch: &Batch) -> Vec<(String, Vec<String>, Scalar)> {
        (0..batch.len())
            .map(|cell| {
                let at = batch.element_names(cell).iter();
                let at = at.map(|&name| batch.name(name).to_owned()).collect();
                (batch.cube(cell).to_owned(), at, batch.value(cell).clone())
            })
            .collect()
    }

    #[test]
    fn a_batch_reads_as_its_cell_writes_do_and_keeps_each_name_once() {
        let text = r#"[
            { "cube": "C", "at": ["a", "b"], "value": 1 },
            { "value": "note", "at": ["b", "a", "c"], "cube": "C" },
            { "at": [], "cube": "D", "value": 0 }
        ]"#;
        let batch: Batch = serde_json::from_str(text).unwrap();
        let writes: Vec<CellWrite> = serde_json::from_str(text).unwrap();
        assert_eq!(cells(&batch), cells(&Batch::from(writes.as_slice())));
        let expected = |cube: &str, at: &[&str], value: Scalar| {
            (
                cube.to_owned(),
                at.iter().map(|&n| n.to_owned()).collect(),
                value,
            )
        };
        assert_eq!(
            cells(&batch),
            [
                expected("C", &["a", "b"], Scalar::Number(1.0)),
                expected("C", &["b", "a", "c"], Scalar::Text("note".to_owned())),
                expected("D", &[], Scalar::Number(0.0)),
            ]
        );
        assert_eq!(batch.names(), 5);
    }

    #[test]
    fn a_cell_write_with_a_field_missing_repeated_or_unknown_is_refused() {
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
                "expected a name",
            ),
        ] {
            let error = serde_json::from_str::<Batch>(text).unwrap_err().to_string();
            assert!(error.contains(refused), "{text}: {error}");
        }
    }
}
