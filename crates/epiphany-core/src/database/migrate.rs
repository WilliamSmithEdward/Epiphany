//! Carries stored cells from one model to the next.

use crate::cells::{Cells, check};
use crate::model::{Cube, ElementId, Model};

/// Moves the cells of each cube into the new model, matching cubes, dimensions and elements
/// by name. Returns the new cells and the number of cells that no longer have a place: their
/// cube is gone or has different dimensions, one of their elements is gone, or the cell is
/// no longer a leaf of the right type.
pub(super) fn migrate(old: &Model, new: &Model, cells: Vec<Cells>) -> (Vec<Cells>, usize) {
    let mut migrated = vec![Cells::default(); new.cubes().len()];
    let mut dropped = 0;
    for (source, cells) in old.cubes().iter().zip(cells) {
        let target = new.cube_id(&source.name).and_then(|id| {
            let cube = &new.cubes()[id];
            Some((id, cube, element_maps(old, source, new, cube)?))
        });
        for (at, value) in cells.into_entries() {
            let moved = target.as_ref().and_then(|(id, cube, maps)| {
                let at: Vec<ElementId> = at
                    .iter()
                    .zip(maps)
                    .map(|(&element, map)| map[element as usize])
                    .collect::<Option<_>>()?;
                check(new, cube, &at, &value).ok()?;
                Some((*id, at))
            });
            match moved {
                Some((id, at)) => migrated[id].set(&at, value),
                None => dropped += 1,
            }
        }
    }
    (migrated, dropped)
}

/// For each dimension of a cube, the new id of every old element, where it still exists.
/// None when the cube no longer has the same dimensions in the same order.
fn element_maps(
    old: &Model,
    source: &Cube,
    new: &Model,
    target: &Cube,
) -> Option<Vec<Vec<Option<ElementId>>>> {
    if source.dimensions.len() != target.dimensions.len() {
        return None;
    }
    let map = |(&from, &to): (&u32, &u32)| {
        let (from, to) = (
            &old.dimensions()[from as usize],
            &new.dimensions()[to as usize],
        );
        let same = new
            .dimension(&from.name)
            .is_some_and(|found| std::ptr::eq(found, to));
        same.then(|| {
            from.elements
                .iter()
                .map(|e| to.element_id(&e.name))
                .collect()
        })
    };
    source
        .dimensions
        .iter()
        .zip(&target.dimensions)
        .map(map)
        .collect()
}
