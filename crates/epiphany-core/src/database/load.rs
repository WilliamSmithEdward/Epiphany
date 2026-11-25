//! Recorded cells stored as they are read, each name looked up as it comes: loading
//! millions of cells makes nothing of a cell but its place and its value.

use std::fmt;

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

use super::Database;
use super::batch::{Key, WRITE};
use crate::Scalar;
use crate::cells::{check, coordinate};
use crate::model::{Dimension, ElementId, Model};

impl Database {
    /// Stores a list of previously recorded cells, written as [`super::CellWrite`]s are, as
    /// it is read, without recording them again. Cells that no longer fit the model are
    /// skipped; returns how many.
    pub fn load_from<'de, D: Deserializer<'de>>(&mut self, cells: D) -> Result<usize, D::Error> {
        let mut loading = Loading {
            database: self,
            at: Vec::new(),
            skipped: 0,
        };
        let read = cells.deserialize_seq(&mut loading);
        let skipped = loading.skipped;
        // What was stored before a failure is stored all the same.
        self.changed();
        read.map(|()| skipped)
    }
}

struct Loading<'a> {
    database: &'a mut Database,
    /// Where the cell being read is, kept from one cell to the next.
    at: Vec<ElementId>,
    skipped: usize,
}

impl<'de> Visitor<'de> for &mut Loading<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a list of cells")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut cells: A) -> Result<(), A::Error> {
        while cells.next_element_seed(Cell(&mut *self))?.is_some() {}
        Ok(())
    }
}

/// One cell: an object of its cube, the names of its elements and its value, or a list of
/// the same in that order. Read as the derived [`super::CellWrite`] is, refused as it is.
struct Cell<'a, 'b>(&'a mut Loading<'b>);

/// A cell's names: looked up already, fitting or not, or kept until its cube is known.
enum At {
    Found(bool),
    Kept(Vec<String>),
}

impl Cell<'_, '_> {
    /// Stores a cell that has been read, if it fits the model.
    fn store(self, cube: Option<usize>, at: At, value: Scalar) {
        let Loading {
            database,
            at: found,
            skipped,
        } = self.0;
        let model = &database.model;
        let fits = cube.filter(|&cube| {
            let definition = &model.cubes()[cube];
            let located = match at {
                At::Found(fits) => fits,
                At::Kept(names) => coordinate(model, definition, &names)
                    .map(|ids| *found = ids)
                    .is_ok(),
            };
            located && check(model, definition, found, &value).is_ok()
        });
        match fits {
            Some(cube) => database.cells[cube].set(found, value),
            None => *skipped += 1,
        }
    }

    /// A cell's element names, looked up if its cube is known.
    fn names<'de, A: MapAccess<'de>>(
        &mut self,
        cube: Option<Option<usize>>,
        map: &mut A,
    ) -> Result<At, A::Error> {
        let Loading { database, at, .. } = &mut *self.0;
        Ok(match cube {
            Some(Some(cube)) => At::Found(map.next_value_seed(Names {
                model: &database.model,
                cube,
                found: at,
            })?),
            _ => At::Kept(map.next_value()?),
        })
    }
}

impl<'de> DeserializeSeed<'de> for Cell<'_, '_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_struct("CellWrite", WRITE, self)
    }
}

impl<'de> Visitor<'de> for Cell<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("struct CellWrite")
    }

    fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> Result<(), A::Error> {
        let (mut cube, mut at, mut value) = (None, None, None);
        while let Some(field) = map.next_key_seed(Key(WRITE))? {
            match field {
                0 if cube.is_some() => return Err(de::Error::duplicate_field("cube")),
                1 if at.is_some() => return Err(de::Error::duplicate_field("at")),
                2 if value.is_some() => return Err(de::Error::duplicate_field("value")),
                0 => cube = Some(map.next_value_seed(CubeName(&self.0.database.model))?),
                1 => at = Some(self.names(cube, &mut map)?),
                _ => value = Some(map.next_value::<Scalar>()?),
            }
        }
        let cube = cube.ok_or_else(|| de::Error::missing_field("cube"))?;
        let at = at.ok_or_else(|| de::Error::missing_field("at"))?;
        let value = value.ok_or_else(|| de::Error::missing_field("value"))?;
        self.store(cube, at, value);
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut parts: A) -> Result<(), A::Error> {
        let short = |found| de::Error::invalid_length(found, &"struct CellWrite with 3 elements");
        let cube = parts.next_element_seed(CubeName(&self.0.database.model))?;
        let cube = cube.ok_or_else(|| short(0))?;
        let names: Vec<String> = parts.next_element()?.ok_or_else(|| short(1))?;
        let value = parts.next_element::<Scalar>()?.ok_or_else(|| short(2))?;
        self.store(cube, At::Kept(names), value);
        Ok(())
    }
}
/// A cube's name, looked up: nothing if the model has no such cube.
struct CubeName<'a>(&'a Model);

impl<'de> DeserializeSeed<'de> for CubeName<'_> {
    type Value = Option<usize>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl Visitor<'_> for CubeName<'_> {
    type Value = Option<usize>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("the name of a cube")
    }

    fn visit_str<E: de::Error>(self, name: &str) -> Result<Self::Value, E> {
        Ok(self.0.cube_id(name))
    }
}

/// The names of a cell's elements, looked up into `found`. Says whether there is one for
/// each of the cube's dimensions and each is an element of it.
struct Names<'a> {
    model: &'a Model,
    cube: usize,
    found: &'a mut Vec<ElementId>,
}

impl<'de> DeserializeSeed<'de> for Names<'_> {
    type Value = bool;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<bool, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Names<'_> {
    type Value = bool;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a list of element names")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut names: A) -> Result<bool, A::Error> {
        let dimensions = &self.model.cubes()[self.cube].dimensions;
        self.found.clear();
        let (mut count, mut fits) = (0, true);
        loop {
            let dimension = dimensions
                .get(count)
                .map(|&d| &self.model.dimensions()[d as usize]);
            let Some(id) = names.next_element_seed(ElementName(dimension))? else {
                break;
            };
            count += 1;
            match id {
                Some(id) => self.found.push(id),
                None => fits = false,
            }
        }
        Ok(fits && count == dimensions.len())
    }
}

/// An element's name, looked up in its dimension: nothing if it is not one of its elements,
/// or if the cell has more names than its cube has dimensions.
struct ElementName<'a>(Option<&'a Dimension>);

impl<'de> DeserializeSeed<'de> for ElementName<'_> {
    type Value = Option<ElementId>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl Visitor<'_> for ElementName<'_> {
    type Value = Option<ElementId>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("the name of an element")
    }

    fn visit_str<E: de::Error>(self, name: &str) -> Result<Self::Value, E> {
        Ok(self.0.and_then(|dimension| dimension.element_id(name)))
    }
}
