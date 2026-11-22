//! The cells of one cube: stored leaf values, and consolidated values computed on read.

use std::collections::hash_map::Entry;
use std::fmt;

use rustc_hash::FxHashMap;

use crate::Scalar;
use crate::model::{Cube, Element, ElementId, ElementType, Model};

/// One element per dimension of a cube, in the cube's dimension order.
///
/// A cube of up to eight dimensions keeps the elements here, in place: the stored cells are
/// then one beside the next in memory, each whole, and a pass over them or a lookup among
/// them follows no pointer. A cube of more keeps them apart, as any list is kept.
#[derive(Clone)]
pub(crate) enum Coordinate {
    Few(u8, [ElementId; FEW]),
    Many(Box<[ElementId]>),
}

/// The most dimensions whose elements a coordinate keeps in place.
const FEW: usize = 8;

impl From<&[ElementId]> for Coordinate {
    fn from(at: &[ElementId]) -> Self {
        match at.len() <= FEW {
            true => {
                let mut few = [0; FEW];
                few[..at.len()].copy_from_slice(at);
                Self::Few(at.len() as u8, few)
            }
            false => Self::Many(at.into()),
        }
    }
}

impl std::ops::Deref for Coordinate {
    type Target = [ElementId];

    fn deref(&self) -> &[ElementId] {
        match self {
            Self::Few(dimensions, few) => &few[..usize::from(*dimensions)],
            Self::Many(many) => many,
        }
    }
}

// A coordinate is its elements and nothing else: it is compared, hashed and shown as they
// are, so that a cell can be looked up by the elements alone.
impl std::borrow::Borrow<[ElementId]> for Coordinate {
    fn borrow(&self) -> &[ElementId] {
        self
    }
}

impl PartialEq for Coordinate {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for Coordinate {}

impl std::hash::Hash for Coordinate {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl std::fmt::Debug for Coordinate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}
/// Where a stored cell is, as it is given out: unpacked from the number it is kept as, or
/// the elements it is kept with. It is compared, hashed and ordered as its elements are.
///
/// One plain value with a single choice in reaching its elements: given out for every cell
/// of a pass, a choice between two kinds of coordinate cost a pass over the cells half as
/// long again.
#[derive(Clone, Copy)]
pub(crate) struct At<'a> {
    kept: Option<&'a [ElementId]>,
    dimensions: u8,
    unpacked: [ElementId; FEW],
}

impl<'a> At<'a> {
    pub(crate) fn kept(at: &'a [ElementId]) -> Self {
        Self {
            kept: Some(at),
            dimensions: 0,
            unpacked: [0; FEW],
        }
    }
}

impl std::ops::Deref for At<'_> {
    type Target = [ElementId];

    fn deref(&self) -> &[ElementId] {
        match self.kept {
            Some(at) => at,
            None => &self.unpacked[..usize::from(self.dimensions)],
        }
    }
}

impl PartialEq for At<'_> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for At<'_> {}

impl PartialOrd for At<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for At<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (**self).cmp(&**other)
    }
}

impl std::hash::Hash for At<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl std::fmt::Debug for At<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

/// Values by coordinate.
///
/// Where every element of every coordinate fits, a coordinate is kept packed into one
/// number, each dimension given as many bits as the largest element stored in it needs: a
/// stored number then takes 16 bytes rather than 48. An element too large for its bits
/// packs every coordinate again, with more; one of more than eight dimensions, or needing
/// more than 64 bits in all, keeps every coordinate as its elements from then on.
#[derive(Debug, Clone)]
enum Table<V> {
    Packed {
        layout: Layout,
        cells: FxHashMap<u64, V>,
    },
    Plain(FxHashMap<Coordinate, V>),
}

impl<V> Default for Table<V> {
    fn default() -> Self {
        Self::Packed {
            layout: Layout::default(),
            cells: FxHashMap::default(),
        }
    }
}

/// Where each dimension's element is kept in a packed coordinate: its bits, from the
/// lowest. No dimensions before the first value is stored.
#[derive(Debug, Clone, Copy, Default)]
struct Layout {
    dimensions: usize,
    shifts: [u32; FEW],
    masks: [u64; FEW],
}

impl Layout {
    fn new(widths: &[u32]) -> Self {
        let mut layout = Self {
            dimensions: widths.len(),
            ..Self::default()
        };
        let mut shift = 0;
        for (dimension, &width) in widths.iter().enumerate() {
            layout.shifts[dimension] = shift;
            layout.masks[dimension] = (1u64 << width) - 1;
            shift += width;
        }
        layout
    }

    fn widths(&self) -> Vec<u32> {
        let masks = &self.masks[..self.dimensions];
        masks.iter().map(|mask| mask.count_ones()).collect()
    }

    /// A coordinate as one number, if each element fits its dimension's bits.
    fn pack(&self, at: &[ElementId]) -> Option<u64> {
        if self.dimensions != at.len() {
            return None;
        }
        let mut packed = 0;
        for ((&element, &shift), &mask) in at.iter().zip(&self.shifts).zip(&self.masks) {
            let element = u64::from(element);
            if element & !mask != 0 {
                return None;
            }
            packed |= element.wrapping_shl(shift);
        }
        Some(packed)
    }

    /// The coordinate a number was packed from. Every element is taken out at once, each
    /// apart from the others; those of dimensions the cube does not have come out as none.
    fn unpack(&self, packed: u64) -> At<'static> {
        let mut unpacked = [0; FEW];
        for ((element, &shift), &mask) in unpacked.iter_mut().zip(&self.shifts).zip(&self.masks) {
            *element = (packed.wrapping_shr(shift) & mask) as ElementId;
        }
        At {
            kept: None,
            dimensions: self.dimensions as u8,
            unpacked,
        }
    }
}

/// The bits an element needs.
fn bits(element: ElementId) -> u32 {
    ElementId::BITS - element.leading_zeros()
}

impl<V> Table<V> {
    fn get(&self, at: &[ElementId]) -> Option<&V> {
        match self {
            Self::Packed { layout, cells } => cells.get(&layout.pack(at)?),
            Self::Plain(cells) => cells.get(at),
        }
    }

    fn remove(&mut self, at: &[ElementId]) {
        match self {
            Self::Packed { layout, cells } => {
                if let Some(packed) = layout.pack(at) {
                    cells.remove(&packed);
                }
            }
            Self::Plain(cells) => drop(cells.remove(at)),
        }
    }

    fn insert(&mut self, at: &[ElementId], value: V) {
        if let Self::Packed { layout, cells } = self {
            if let Some(packed) = layout.pack(at) {
                cells.insert(packed, value);
                return;
            }
            self.widen(at);
        }
        match self {
            Self::Packed { layout, cells } => {
                let packed = layout.pack(at).expect("widened to fit");
                cells.insert(packed, value);
            }
            Self::Plain(cells) => drop(cells.insert(at.into(), value)),
        }
    }

    /// Gives each dimension the bits an element of `at` needs, if more than it has, and
    /// packs every coordinate again; or keeps them all as their elements, if they cannot
    /// all be packed.
    fn widen(&mut self, at: &[ElementId]) {
        let Self::Packed { layout, cells } = self else {
            return;
        };
        let mut wider: Vec<u32> = match layout.dimensions {
            0 => vec![0; at.len()],
            _ => layout.widths(),
        };
        let fits = wider.len() == at.len() && at.len() <= FEW;
        if fits {
            for (width, &element) in wider.iter_mut().zip(at) {
                *width = (*width).max(bits(element));
            }
        }
        let cells = std::mem::take(cells);
        let old = *layout;
        // The room made for cells about to be stored is kept.
        let room = cells.capacity();
        let unpacked = cells
            .into_iter()
            .map(|(packed, value)| (old.unpack(packed), value));
        *self = match fits && wider.iter().sum::<u32>() <= u64::BITS {
            true => {
                let layout = Layout::new(&wider);
                let mut repacked = FxHashMap::with_capacity_and_hasher(room, Default::default());
                repacked
                    .extend(unpacked.map(|(at, value)| (layout.pack(&at).expect("fits"), value)));
                Self::Packed {
                    cells: repacked,
                    layout,
                }
            }
            false => {
                let mut kept = FxHashMap::with_capacity_and_hasher(room, Default::default());
                kept.extend(unpacked.map(|(at, value)| (Coordinate::from(&*at), value)));
                Self::Plain(kept)
            }
        };
    }

    fn len(&self) -> usize {
        match self {
            Self::Packed { cells, .. } => cells.len(),
            Self::Plain(cells) => cells.len(),
        }
    }

    fn reserve(&mut self, more: usize) {
        match self {
            Self::Packed { cells, .. } => cells.reserve(more),
            Self::Plain(cells) => cells.reserve(more),
        }
    }

    fn iter(&self) -> Iter<'_, V> {
        match self {
            Self::Packed { layout, cells } => Iter::Packed(layout, cells.iter()),
            Self::Plain(cells) => Iter::Plain(cells.iter()),
        }
    }
}

/// The values of a table and where each is.
enum Iter<'a, V> {
    Packed(&'a Layout, std::collections::hash_map::Iter<'a, u64, V>),
    Plain(std::collections::hash_map::Iter<'a, Coordinate, V>),
}

impl<'a, V> Iterator for Iter<'a, V> {
    type Item = (At<'a>, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Packed(layout, cells) => {
                let (&packed, value) = cells.next()?;
                Some((layout.unpack(packed), value))
            }
            Self::Plain(cells) => cells.next().map(|(at, value)| (At::kept(at), value)),
        }
    }

    // A pass over all the cells goes through here, the place of each made where it is used
    // rather than handed back from one call to the next.
    fn fold<B, F: FnMut(B, Self::Item) -> B>(self, init: B, mut f: F) -> B {
        match self {
            Self::Packed(layout, cells) => cells.fold(init, |acc, (&packed, value)| {
                f(acc, (layout.unpack(packed), value))
            }),
            Self::Plain(cells) => {
                cells.fold(init, |acc, (at, value)| f(acc, (At::kept(at), value)))
            }
        }
    }
}

/// Values not yet stored, by the coordinate they will be written to.
pub(crate) type Writes = FxHashMap<Coordinate, Scalar>;
/// The leaves of one element and the weight of each, ordered by id.
pub(crate) type Leaves<'a> = &'a [(ElementId, f64)];

/// The stored cells of one cube. Zero and empty cells are not stored.
///
/// A cell is a text cell when its element in the last dimension is a string element, and a
/// numeric cell otherwise. Coordinates must come from [`coordinate`] on the same model.
#[derive(Debug, Clone, Default)]
pub struct Cells {
    numbers: Table<f64>,
    texts: Table<String>,
}

impl Cells {
    pub fn read(&self, model: &Model, cube: &Cube, at: &[ElementId]) -> Scalar {
        let elements = elements(model, cube, at);
        if is_text(elements.clone()) {
            return Scalar::Text(self.texts.get(at).cloned().unwrap_or_default());
        }
        if !elements.clone().any(is_consolidated) {
            return Scalar::Number(self.numbers.get(at).copied().unwrap_or(0.0));
        }
        let leaves: Vec<Leaves> = elements.map(|e| e.leaves.as_slice()).collect();
        let combinations = leaves
            .iter()
            .try_fold(1usize, |product, l| product.checked_mul(l.len()));
        Scalar::Number(match combinations {
            Some(0) => 0.0,
            Some(n) if n <= self.numbers.len() => self.sum_combinations(&leaves),
            _ => self.sum_stored(&leaves),
        })
    }

    /// Reads a cell as it would be once the pending writes were stored.
    pub(crate) fn read_with(
        &self,
        model: &Model,
        cube: &Cube,
        at: &[ElementId],
        pending: &Writes,
    ) -> Scalar {
        if let Some(value) = pending.get(at) {
            return value.clone();
        }
        let stored = match self.read(model, cube, at) {
            Scalar::Number(stored) => stored,
            text => return text,
        };
        let leaves: Vec<Leaves> = elements(model, cube, at)
            .map(|e| e.leaves.as_slice())
            .collect();
        let change = |(leaf, value): (&Coordinate, &Scalar)| {
            let Scalar::Number(number) = value else {
                return None;
            };
            let before = self.numbers.get(leaf).copied().unwrap_or(0.0);
            Some(weight(&leaves, leaf)? * (number - before))
        };
        Scalar::Number(stored + pending.iter().filter_map(change).sum::<f64>())
    }

    /// Where every stored cell is.
    pub(crate) fn stored_at(&self) -> impl Iterator<Item = At<'_>> {
        let numbers = self.numbers.iter().map(|(at, _)| at);
        numbers.chain(self.texts.iter().map(|(at, _)| at))
    }

    /// What is stored at a leaf cell: its text, or its number, zero if it holds none.
    pub(crate) fn value(&self, at: &[ElementId]) -> Scalar {
        match self.texts.get(at) {
            Some(text) => Scalar::Text(text.clone()),
            None => Scalar::Number(self.numbers.get(at).copied().unwrap_or(0.0)),
        }
    }

    /// Every stored cell and its value.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (At<'_>, Scalar)> {
        let numbers = self
            .numbers
            .iter()
            .map(|(at, &number)| (at, Scalar::Number(number)));
        let texts = self
            .texts
            .iter()
            .map(|(at, text)| (at, Scalar::Text(text.clone())));
        numbers.chain(texts)
    }

    /// Writes a leaf cell. Writing zero or an empty string clears it.
    pub fn write(
        &mut self,
        model: &Model,
        cube: &Cube,
        at: &[ElementId],
        value: Scalar,
    ) -> Result<(), CellError> {
        check(model, cube, at, &value)?;
        self.set(at, value);
        Ok(())
    }

    /// Stores a value that has passed [`check`].
    pub(crate) fn set(&mut self, at: &[ElementId], value: Scalar) {
        match value {
            Scalar::Number(0.0) => self.numbers.remove(at),
            Scalar::Number(number) => self.numbers.insert(at, number),
            Scalar::Text(text) if text.is_empty() => self.texts.remove(at),
            Scalar::Text(text) => self.texts.insert(at, text),
        }
    }

    /// Sums a consolidation by looking up every combination of its leaves.
    fn sum_combinations(&self, leaves: &[Leaves]) -> f64 {
        let mut positions = vec![0; leaves.len()];
        let mut at: Vec<ElementId> = leaves.iter().map(|l| l[0].0).collect();
        let mut total = 0.0;
        loop {
            if let Some(value) = self.numbers.get(&at) {
                let weights = positions.iter().zip(leaves).map(|(&p, l)| l[p].1);
                total += weights.product::<f64>() * value;
            }
            if !advance(leaves, &mut positions, &mut at) {
                return total;
            }
        }
    }

    /// The stored leaf numbers as they would be once the pending writes were stored.
    pub(crate) fn numbers_with<'a>(
        &'a self,
        pending: Option<&'a Writes>,
    ) -> impl Iterator<Item = (At<'a>, f64)> {
        let replaced =
            move |at: &[ElementId]| pending.is_some_and(|pending| pending.contains_key(at));
        let stored = self.numbers.iter().filter(move |(at, _)| !replaced(at));
        let written = pending
            .into_iter()
            .flatten()
            .filter_map(|(at, value)| match value {
                Scalar::Number(number) if *number != 0.0 => Some((At::kept(at), *number)),
                _ => None,
            });
        stored.map(|(at, &number)| (at, number)).chain(written)
    }

    /// One leaf number as it would be once the pending writes were stored.
    pub(crate) fn number_with(&self, at: &[ElementId], pending: Option<&Writes>) -> f64 {
        match pending.and_then(|pending| pending.get(at)) {
            Some(Scalar::Number(number)) => *number,
            Some(Scalar::Text(_)) => 0.0,
            None => self.numbers.get(at).copied().unwrap_or(0.0),
        }
    }

    /// Sums a consolidation by weighing every stored cell.
    fn sum_stored(&self, leaves: &[Leaves]) -> f64 {
        self.numbers
            .iter()
            .filter_map(|(at, value)| Some(weight(leaves, &at)? * value))
            .sum()
    }

    /// How many numbers are stored.
    pub(crate) fn stored(&self) -> usize {
        self.numbers.len()
    }

    /// How many cells are stored, numbers and texts.
    pub(crate) fn len(&self) -> usize {
        self.numbers.len() + self.texts.len()
    }

    /// Makes room for this many more numbers at once, rather than as they come. Storing
    /// millions, the table grown as they came took half the time.
    pub(crate) fn reserve(&mut self, numbers: usize) {
        self.numbers.reserve(numbers);
    }

    /// How many stored cells reading a numeric cell looks at or looks up.
    pub(crate) fn work(&self, model: &Model, cube: &Cube, at: &[ElementId]) -> usize {
        let combinations = elements(model, cube, at)
            .try_fold(1usize, |product, e| product.checked_mul(e.leaves.len()));
        combinations.map_or(self.numbers.len(), |n| n.min(self.numbers.len()))
    }

    /// Sums many numeric cells in one pass over the stored cells, as they would be once the
    /// pending writes were stored, giving each stored cell to every one of them it counts
    /// towards. Nothing if the cells are too varied to number.
    pub(crate) fn sum_many(
        &self,
        model: &Model,
        cube: &Cube,
        cells: &[&[ElementId]],
        pending: Option<&Writes>,
    ) -> Option<Vec<f64>> {
        let stored = self.numbers_with(pending);
        Self::share(model, cube, cells, stored, |_, value| Some(value))
    }

    /// Sums many numeric cells of a cube from its leaf cells, each leaf given once: every
    /// leaf is shared out to each of the cells it counts towards, by its weight there. A
    /// leaf comes with whatever is known of it, and `value` makes its number of that, but is
    /// asked only for a leaf that counts towards one of the cells: where a rule has to work
    /// the number out, the leaves beside the point cost nothing. Nothing if the cells are too
    /// varied to number, or if `value` has none to give.
    pub(crate) fn share<Leaf: std::ops::Deref<Target = [ElementId]>, Known>(
        model: &Model,
        cube: &Cube,
        cells: &[&[ElementId]],
        leaves: impl Iterator<Item = (Leaf, Known)>,
        mut value: impl FnMut(&[ElementId], Known) -> Option<f64>,
    ) -> Option<Vec<f64>> {
        // In each dimension the elements asked for are numbered as they are met, and every
        // leaf knows the numbers of those it counts towards, with its weight in each.
        let dimensions: Vec<&[Element]> = cube
            .dimensions
            .iter()
            .map(|&d| model.dimensions()[d as usize].elements.as_slice())
            .collect();
        let mut asked: Vec<FxHashMap<ElementId, u64>> =
            vec![FxHashMap::default(); dimensions.len()];
        let mut towards: Vec<Vec<Vec<(u64, f64)>>> = dimensions
            .iter()
            .map(|d| vec![Vec::new(); d.len()])
            .collect();
        for at in cells {
            for (dimension, &element) in at.iter().enumerate() {
                let number = asked[dimension].len() as u64;
                if let Entry::Vacant(new) = asked[dimension].entry(element) {
                    new.insert(number);
                    for &(leaf, weight) in &dimensions[dimension][element as usize].leaves {
                        towards[dimension][leaf as usize].push((number, weight));
                    }
                }
            }
        }
        // A cell asked for is known by one number: its element's number in each dimension,
        // each dimension counting in units as large as the dimensions before it together.
        let mut units = Vec::with_capacity(dimensions.len());
        let mut size = 1u64;
        for asked in &asked {
            units.push(size);
            size = size.checked_mul(asked.len() as u64)?;
        }
        let number = |at: &[ElementId]| -> u64 {
            let parts = at.iter().zip(&asked).zip(&units);
            parts
                .map(|((element, asked), unit)| asked[element] * unit)
                .sum()
        };
        // Where the numbers do not run too high, every one of them has a place in a row of
        // totals, the cells nobody asked for among them: adding to a place is quicker than
        // looking a cell up, and what is added for nobody is simply not read. Past that,
        // only the cells asked for are kept, each looked up by its number.
        let mut row = vec![
            0.0;
            if size <= MOST_IN_A_ROW {
                size as usize
            } else {
                0
            }
        ];
        let mut totals: FxHashMap<u64, f64> = match row.is_empty() {
            true => cells.iter().map(|at| (number(at), 0.0)).collect(),
            false => FxHashMap::default(),
        };

        let mut lists: Vec<&[(u64, f64)]> = Vec::with_capacity(dimensions.len());
        let mut positions = vec![0; dimensions.len()];
        // Taken as a pass of its own over the cells, which goes quicker than one cell asked
        // for after another; once a leaf has no value, the rest are passed over.
        let mut failed = false;
        leaves.for_each(|(at, known)| {
            if failed {
                return;
            }
            lists.clear();
            lists.extend(
                at.iter()
                    .zip(&towards)
                    .map(|(&leaf, t)| t[leaf as usize].as_slice()),
            );
            if lists.iter().any(|list| list.is_empty()) {
                return;
            }
            let Some(value) = value(&at, known) else {
                failed = true;
                return;
            };
            positions.fill(0);
            // Every combination of one element asked for per dimension that the cell counts
            // towards; most are a cell asked for.
            'combinations: loop {
                let (mut cell, mut weight) = (0, 1.0);
                for ((list, &position), unit) in lists.iter().zip(&positions).zip(&units) {
                    cell += list[position].0 * unit;
                    weight *= list[position].1;
                }
                match row.get_mut(cell as usize) {
                    Some(total) => *total += weight * value,
                    None => {
                        if let Some(total) = totals.get_mut(&cell) {
                            *total += weight * value;
                        }
                    }
                }
                for (position, list) in positions.iter_mut().zip(&lists) {
                    *position += 1;
                    if *position < list.len() {
                        continue 'combinations;
                    }
                    *position = 0;
                }
                break;
            }
        });
        if failed {
            return None;
        }
        let total = |at: &&[ElementId]| match row.get(number(at) as usize) {
            Some(&total) => total,
            None => totals[&number(at)],
        };
        Some(cells.iter().map(total).collect())
    }
}

/// The most totals kept in a row, one beside the next, for the one pass over the cells: four
/// million of them, thirty-two megabytes.
const MOST_IN_A_ROW: u64 = 1 << 22;

/// Moves to the next combination of one leaf per dimension, the last dimension turning
/// fastest. `positions` says which leaf each dimension is at and `at` is the combination.
/// Returns false once every combination has been visited.
pub(crate) fn advance(leaves: &[Leaves], positions: &mut [usize], at: &mut [ElementId]) -> bool {
    for dimension in (0..leaves.len()).rev() {
        positions[dimension] += 1;
        if positions[dimension] == leaves[dimension].len() {
            positions[dimension] = 0;
        }
        at[dimension] = leaves[dimension][positions[dimension]].0;
        if positions[dimension] != 0 {
            return true;
        }
    }
    false
}

/// The weight a leaf cell carries in a consolidation, if it is part of it.
pub(crate) fn weight(leaves: &[Leaves], at: &[ElementId]) -> Option<f64> {
    at.iter().zip(leaves).try_fold(1.0, |weight, (id, l)| {
        let position = l.binary_search_by_key(id, |&(leaf, _)| leaf).ok()?;
        Some(weight * l[position].1)
    })
}

/// Resolves one element name or alias per dimension of the cube into a coordinate.
pub fn coordinate<S: AsRef<str>>(
    model: &Model,
    cube: &Cube,
    names: &[S],
) -> Result<Vec<ElementId>, CellError> {
    if names.len() != cube.dimensions.len() {
        return Err(CellError::Arity {
            expected: cube.dimensions.len(),
            found: names.len(),
        });
    }
    let resolve = |(&dimension, name): (&u32, &S)| {
        let dimension = &model.dimensions()[dimension as usize];
        let found = dimension.element_id(name.as_ref());
        found.ok_or_else(|| CellError::UnknownElement {
            dimension: dimension.name.clone(),
            element: name.as_ref().to_owned(),
        })
    };
    cube.dimensions.iter().zip(names).map(resolve).collect()
}

/// Whether a value may be written to a cell.
pub fn check(
    model: &Model,
    cube: &Cube,
    at: &[ElementId],
    value: &Scalar,
) -> Result<(), CellError> {
    let elements = elements(model, cube, at);
    match (is_text(elements.clone()), value) {
        (true, Scalar::Text(_)) => Ok(()),
        (false, Scalar::Number(number)) if !number.is_finite() => Err(CellError::NotFinite),
        (false, Scalar::Number(_)) if elements.clone().any(is_consolidated) => {
            Err(CellError::Consolidated)
        }
        (false, Scalar::Number(_)) => Ok(()),
        _ => Err(CellError::WrongType),
    }
}

/// The elements of a coordinate, in dimension order.
fn elements<'a>(
    model: &'a Model,
    cube: &'a Cube,
    at: &'a [ElementId],
) -> impl DoubleEndedIterator<Item = &'a Element> + Clone {
    assert_eq!(at.len(), cube.dimensions.len(), "one element per dimension");
    let element =
        move |(&d, &e): (&u32, &ElementId)| &model.dimensions()[d as usize].elements[e as usize];
    cube.dimensions.iter().zip(at).map(element)
}

/// Whether the elements address a text cell: the last one is a string element.
fn is_text<'a>(mut elements: impl DoubleEndedIterator<Item = &'a Element>) -> bool {
    elements
        .next_back()
        .is_some_and(|e| e.kind == ElementType::Text)
}

fn is_consolidated(element: &Element) -> bool {
    element.kind == ElementType::Consolidated
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellError {
    /// The number of names differs from the number of dimensions.
    Arity {
        expected: usize,
        found: usize,
    },
    UnknownElement {
        dimension: String,
        element: String,
    },
    UnknownCube {
        cube: String,
    },
    /// A number was written to a cell with a consolidated element.
    Consolidated,
    /// A number was written to a text cell, or text to a numeric cell.
    WrongType,
    /// A value was written to a cell that a rule calculates.
    Calculated,
    /// The number written was infinite or not a number.
    NotFinite,
    /// A value could not be shared out among the leaves of a cell; the text says why.
    Spread(String),
}

impl CellError {
    /// A stable identifier for the kind of error.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Arity { .. } => "wrong_arity",
            Self::UnknownElement { .. } => "unknown_element",
            Self::UnknownCube { .. } => "unknown_cube",
            Self::Consolidated => "consolidated_cell",
            Self::WrongType => "wrong_type",
            Self::Calculated => "calculated_cell",
            Self::NotFinite => "not_finite",
            Self::Spread(_) => "spread_failed",
        }
    }
}

impl fmt::Display for CellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arity { expected, found } => {
                write!(f, "expected {expected} elements, found {found}")
            }
            Self::UnknownElement { dimension, element } => {
                write!(f, "unknown element '{element}' in dimension '{dimension}'")
            }
            Self::UnknownCube { cube } => write!(f, "unknown cube '{cube}'"),
            Self::Consolidated => write!(f, "a consolidated cell cannot be written"),
            Self::WrongType => write!(f, "the value has the wrong type for the cell"),
            Self::Calculated => write!(f, "the cell is calculated by a rule and cannot be written"),
            Self::NotFinite => write!(f, "the number is not finite"),
            Self::Spread(reason) => reason.fmt(f),
        }
    }
}

impl std::error::Error for CellError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stores each coordinate with a value of its own, in order, then checks that every one
    /// is found, nothing else is, and the table gives each back once; and whether it kept
    /// them packed.
    fn holds(coordinates: &[Vec<ElementId>]) -> bool {
        let mut table = Table::default();
        for (value, at) in coordinates.iter().enumerate() {
            table.insert(at, value);
        }
        for (value, at) in coordinates.iter().enumerate() {
            assert_eq!(table.get(at), Some(&value), "{at:?}");
        }
        let mut absent = coordinates[0].clone();
        absent[0] = ElementId::MAX;
        assert_eq!(table.get(&absent), None);
        let mut given: Vec<(Vec<ElementId>, usize)> = table
            .iter()
            .map(|(at, &value)| (at.to_vec(), value))
            .collect();
        given.sort_by_key(|&(_, value)| value);
        let expected: Vec<(Vec<ElementId>, usize)> = coordinates.iter().cloned().zip(0..).collect();
        assert_eq!(given, expected);
        matches!(table, Table::Packed { .. })
    }

    #[test]
    fn coordinates_are_packed_while_they_fit_and_kept_whole_once_they_do_not() {
        // Elements growing one dimension at a time, each time packed again with more bits.
        let growing: Vec<Vec<ElementId>> = (0..300)
            .map(|i| vec![i % 7, i, (i * 31) % 1000, 0])
            .collect();
        assert!(holds(&growing));
        // Elements needing all 32 bits in two dimensions fill 64 exactly.
        assert!(holds(&[
            vec![0, 0],
            vec![u32::MAX - 1, 5],
            vec![1, u32::MAX - 1]
        ]));
        // A third such dimension does not fit, nor does a ninth dimension.
        assert!(!holds(&[
            vec![1, 2, 3],
            vec![u32::MAX - 1, u32::MAX - 1, 1]
        ]));
        assert!(!holds(&[(0..9).collect()]));
        // Until an element needs more bits, nothing is packed again.
        assert!(holds(&[vec![0, 0, 0], vec![1, 1, 1], vec![0, 1, 0]]));
    }

    #[test]
    fn a_cell_removed_is_gone_however_its_coordinate_is_kept() {
        for wide in [false, true] {
            let mut table = Table::default();
            let far = if wide { u32::MAX - 1 } else { 9 };
            let (near, at) = (vec![1, 2, 3], vec![far, far, far]);
            table.insert(&near, 1.0);
            table.insert(&at, 2.0);
            table.remove(&at);
            table.remove(&[ElementId::MAX, 0, 0]);
            assert_eq!(
                (table.get(&near), table.get(&at), table.len()),
                (Some(&1.0), None, 1)
            );
            assert_eq!(matches!(table, Table::Packed { .. }), !wide);
        }
    }
}
