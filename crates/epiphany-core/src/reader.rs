//! Reads cells as rules and feeders make them.
//!
//! A cube without rules is read straight from its stored cells. In a cube with rules, the
//! first rule that covers a cell decides it. A consolidation that no leaf rule can reach is
//! still added up from stored cells alone. One that a leaf rule can reach must also count the
//! calculated leaves beneath it: every one of them if the cube is not fed, or only the stored
//! and fed ones if it is.

use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::Scalar;
use crate::cells::{Cells, Coordinate, Leaves, Writes, advance, weight};
use crate::mdx::{MdxError, Outcome, evaluate_rule, feeder_targets, matches};
use crate::model::{Cube, Element, ElementId, ElementType, Model};

/// The fed leaf cells of each cube, for the cubes that are fed.
pub(crate) type Fed = Vec<Option<FxHashSet<Coordinate>>>;

/// The most leaf cells one consolidation may visit in a cube that is not fed.
const MOST_VISITED: usize = 10_000_000;
/// The most cells the feeders of a model may feed.
const MOST_FED: usize = 10_000_000;
/// How deeply cells may depend on one another through rules.
const DEEPEST: usize = 256;

/// The most consolidated cells kept for one cube; past it they are forgotten and kept afresh.
const MOST_KEPT: usize = 1_000_000;

type Read<T> = Result<T, MdxError>;

/// Consolidated cells worked out from the committed cells of each cube. They hold only until
/// a cell or the model changes, when whoever owns them starts again with none.
#[derive(Debug, Default)]
pub(crate) struct Totals(Mutex<Vec<FxHashMap<Coordinate, Scalar>>>);

impl Totals {
    fn kept(&self) -> MutexGuard<'_, Vec<FxHashMap<Coordinate, Scalar>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self, cube: usize, at: &[ElementId]) -> Option<Scalar> {
        self.kept().get(cube)?.get(at).cloned()
    }

    fn keep(&self, cube: usize, at: &[ElementId], value: &Scalar) {
        let mut kept = self.kept();
        if kept.len() <= cube {
            kept.resize_with(cube + 1, FxHashMap::default);
        }
        if kept[cube].len() >= MOST_KEPT {
            kept[cube].clear();
        }
        kept[cube].insert(Coordinate::from(at), value.clone());
    }
}

pub(crate) struct Reader<'a> {
    model: &'a Model,
    cells: &'a [Cells],
    /// The writes of the transaction being read in, by cube.
    pending: Option<&'a [Writes]>,
    fed: Cow<'a, Fed>,
    /// Consolidated cells others have worked out; none when reading a transaction's own
    /// writes, which only it can see.
    totals: Option<&'a Totals>,
    /// Cells of cubes with rules that have been worked out already.
    known: RefCell<FxHashMap<(usize, Coordinate), Scalar>>,
    /// The cells being worked out, each because the one before it needs it.
    working: RefCell<Vec<(usize, Coordinate)>>,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(
        model: &'a Model,
        cells: &'a [Cells],
        pending: Option<&'a [Writes]>,
        fed: Cow<'a, Fed>,
        totals: Option<&'a Totals>,
    ) -> Self {
        Self {
            model,
            cells,
            pending,
            fed,
            totals,
            known: RefCell::default(),
            working: RefCell::default(),
        }
    }

    pub(crate) fn read(&self, cube: usize, at: &[ElementId]) -> Read<Scalar> {
        let ruled = !self.model.cubes()[cube].rules.is_empty();
        // A leaf is quicker to look up where it is stored than among the totals.
        let totals = self.totals.filter(|_| self.kind(cube, at).1);
        if let Some(kept) = totals.and_then(|totals| totals.get(cube, at)) {
            return Ok(kept);
        }
        let value = match ruled {
            true => self.work_out(cube, at)?,
            false => self.stored(cube, at),
        };
        if let Some(totals) = totals {
            totals.keep(cube, at, &value);
        }
        Ok(value)
    }

    /// Says which cells of a cube are about to be read. Where adding up the totals among them
    /// one by one would pass over the stored cells more than once, they are all added up in a
    /// single pass and kept, and the reads that follow find them there.
    pub(crate) fn expect(&self, cube: usize, cells: &[&[ElementId]]) {
        let definition = &self.model.cubes()[cube];
        let Some(totals) = self.totals else {
            return;
        };
        let stored = &self.cells[cube];
        let mut wanted: Vec<&[ElementId]> = Vec::new();
        let mut seen = FxHashSet::default();
        let mut work = 0usize;
        // Totals that a leaf rule reaches are another matter, taken up below.
        let mut reached: Vec<&[ElementId]> = Vec::new();
        for &at in cells {
            let Some(plain) = self.total(cube, at) else {
                continue;
            };
            if totals.get(cube, at).is_some() || !seen.insert(at) {
                continue;
            }
            match plain {
                true => {
                    wanted.push(at);
                    work = work.saturating_add(stored.work(self.model, definition, at));
                }
                false => reached.push(at),
            }
        }
        let keep = |cells: &[&[ElementId]], sums: Option<Vec<f64>>| {
            for (at, sum) in cells.iter().zip(sums.into_iter().flatten()) {
                totals.keep(cube, at, &Scalar::Number(sum));
            }
        };
        if work > stored.stored() {
            keep(&wanted, stored.sum_many(self.model, definition, &wanted));
        }

        // In a fed cube such a total counts each stored cell as it is, or as its rule makes
        // it, and each fed cell that a rule covers. Read by itself, every total passes over
        // all of them; here each is worked out once and shared out among the totals.
        let (Some(fed), true) = (&self.fed[cube], reached.len() > 1) else {
            return;
        };
        let leaf_rules = || definition.rules.iter().filter(|rule| rule.decides_leaves());
        let ruled = |leaf: &[ElementId]| leaf_rules().any(|rule| matches(&rule.area, leaf));
        // The stored cells with what they hold, then the fed cells that hold nothing.
        let held = stored
            .numbers_with(None)
            .map(|(leaf, held)| (leaf, Some(held)));
        let empty = fed
            .iter()
            .map(|leaf| &**leaf)
            .filter(|leaf| stored.number_with(leaf, None) == 0.0);
        let leaves = held.chain(empty.map(|leaf| (leaf, None)));
        // A leaf a rule covers is what the rule makes it; any other is what it holds. A rule
        // is only asked about a leaf beneath one of the totals, and one that fails is left to
        // fail where its cell is read: nothing is kept then.
        let value = |leaf: &[ElementId], held: Option<f64>| match ruled(leaf) {
            true => self.number(cube, leaf).ok(),
            false => Some(held.unwrap_or(0.0)),
        };
        let sums = Cells::share(self.model, definition, &reached, leaves, value);
        keep(&reached, sums);
    }

    /// Whether a leaf rule covers any leaf cell beneath a consolidation, given the leaves of
    /// its element in each dimension.
    fn reached(&self, cube: &Cube, leaves: &[Leaves]) -> bool {
        let holds = |leaves: Leaves, id: ElementId| {
            leaves.binary_search_by_key(&id, |&(leaf, _)| leaf).is_ok()
        };
        let reaches = |area: &[Option<ElementId>]| {
            let fits = |(wanted, leaves): (&Option<ElementId>, &Leaves)| {
                wanted.is_none_or(|w| holds(leaves, w))
            };
            area.iter().zip(leaves).all(fits)
        };
        let mut leaf_rules = cube.rules.iter().filter(|rule| rule.decides_leaves());
        leaf_rules.any(|rule| reaches(&rule.area))
    }

    /// For a consolidated cell that no rule covers, whether it is a sum of stored cells and
    /// nothing else: true if no leaf rule covers a cell beneath it, false if one does.
    /// Nothing for any other cell.
    fn total(&self, cube: usize, at: &[ElementId]) -> Option<bool> {
        let definition = &self.model.cubes()[cube];
        if !self.kind(cube, at).1 {
            return None;
        }
        if definition.rules.is_empty() {
            return Some(true);
        }
        let leaves: Vec<Leaves> = self
            .elements(definition, at)
            .map(|e| e.leaves.as_slice())
            .collect();
        let covered = definition
            .rules
            .iter()
            .any(|rule| rule.covers(at, false, true));
        (!covered).then(|| !self.reached(definition, &leaves))
    }

    /// Reads a cell of a cube with rules, remembering it for as long as this reader lasts.
    fn work_out(&self, cube: usize, at: &[ElementId]) -> Read<Scalar> {
        let key = (cube, Coordinate::from(at));
        if let Some(known) = self.known.borrow().get(&key) {
            return Ok(known.clone());
        }
        let circular = {
            let working = self.working.borrow();
            working.len() >= DEEPEST || working.contains(&key)
        };
        if circular {
            let (name, at) = (&self.model.cubes()[cube].name, self.names(cube, at));
            return Err(MdxError::new(format!(
                "cell ({at}) of cube '{name}' depends on itself"
            )));
        }
        self.working.borrow_mut().push(key.clone());
        let value = self.decide(cube, at);
        self.working.borrow_mut().pop();
        let value = value?;
        self.known.borrow_mut().insert(key, value.clone());
        Ok(value)
    }

    /// The leaf cells that a rule gives a value but that are neither stored nor fed, and so
    /// are missing from the consolidations of a fed cube. At most `limit` are returned.
    pub(crate) fn unfed(&self, cube: usize, limit: usize) -> Read<Vec<Vec<ElementId>>> {
        let definition = &self.model.cubes()[cube];
        let Some(fed) = &self.fed[cube] else {
            return Ok(Vec::new());
        };
        let last = definition.dimensions.len() - 1;
        let mut found = Vec::new();
        for (index, rule) in definition.rules.iter().enumerate() {
            if !rule.decides_leaves() {
                continue;
            }
            // Every leaf cell of the rule's area: the element it names in a dimension, or
            // every leaf of the dimension where it names none.
            let leaves: Vec<Vec<(ElementId, f64)>> = (0..=last)
                .map(|position| {
                    let elements = self.dimension(definition, position).iter().enumerate();
                    let leaf = |(id, element): (usize, &Element)| {
                        let wanted = rule.area[position].is_none_or(|wanted| wanted as usize == id);
                        let numeric = element.kind != ElementType::Consolidated
                            && (position != last || element.kind != ElementType::Text);
                        (wanted && numeric).then_some((id as ElementId, 1.0))
                    };
                    elements.filter_map(leaf).collect()
                })
                .collect();
            let leaves: Vec<Leaves> = leaves.iter().map(Vec::as_slice).collect();
            let Some(mut positions) = self.start(&leaves)? else {
                continue;
            };
            let mut at: Vec<ElementId> = leaves.iter().map(|l| l[0].0).collect();
            let earlier = &definition.rules[..index];
            loop {
                let shadowed = earlier
                    .iter()
                    .any(|rule| rule.decides_leaves() && matches(&rule.area, &at));
                let counted = fed.contains(at.as_slice())
                    || self.cells[cube].number_with(&at, self.pending.map(|p| &p[cube])) != 0.0;
                if !shadowed && !counted && self.number(cube, &at)? != 0.0 {
                    found.push(at.clone());
                    if found.len() == limit {
                        return Ok(found);
                    }
                }
                if !advance(&leaves, &mut positions, &mut at) {
                    break;
                }
            }
        }
        Ok(found)
    }

    fn decide(&self, cube: usize, at: &[ElementId]) -> Read<Scalar> {
        if let Some((_, value)) = self.ruling(cube, at)? {
            return Ok(value);
        }
        match self.kind(cube, at) {
            (_, true) => self.consolidate(cube, at),
            (_, false) => Ok(self.stored(cube, at)),
        }
    }

    /// Whether a cell is a text cell, and whether it is a consolidated one.
    pub(crate) fn kind(&self, cube: usize, at: &[ElementId]) -> (bool, bool) {
        // Asked of every cell read, so the elements are gone through once.
        let (mut text, mut consolidated) = (false, false);
        for element in self.elements(&self.model.cubes()[cube], at) {
            // Left standing by the last element, which is the one that makes a cell text.
            text = element.kind == ElementType::Text;
            consolidated |= element.kind == ElementType::Consolidated;
        }
        (text, consolidated && !text)
    }

    /// The rule that decides a cell, by its position among the cube's rules, and the value it
    /// gives. Nothing if no rule covers the cell or the one that does leaves it as it is.
    pub(crate) fn ruling(&self, cube: usize, at: &[ElementId]) -> Read<Option<(usize, Scalar)>> {
        let (text, consolidated) = self.kind(cube, at);
        let read = |cube: usize, at: &[ElementId]| self.read(cube, at);
        for (index, rule) in self.model.cubes()[cube].rules.iter().enumerate() {
            if !rule.covers(at, text, consolidated) {
                continue;
            }
            match evaluate_rule(self.model, cube, rule, at, &read)? {
                Outcome::Value(value) => return Ok(Some((index, value))),
                Outcome::Stet => break,
                Outcome::Continue => {}
            }
        }
        Ok(None)
    }

    /// Whether a leaf cell is fed, in a cube that is fed at all.
    pub(crate) fn is_fed(&self, cube: usize, at: &[ElementId]) -> Option<bool> {
        self.fed[cube].as_ref().map(|fed| fed.contains(at))
    }

    /// The feeders that feed a leaf cell, each with the cell it fed it from: the cube, the
    /// feeder's position among that cube's feeders, and the source cell. At most `limit`.
    pub(crate) fn fed_by(
        &self,
        cube: usize,
        at: &[ElementId],
        limit: usize,
    ) -> Read<Vec<(usize, usize, Vec<ElementId>)>> {
        let mut found = Vec::new();
        for (id, source_cube) in self.model.cubes().iter().enumerate() {
            if source_cube.feeders.is_empty() {
                continue;
            }
            // Feeders fire from stored cells and from cells that are themselves fed.
            let stored = self.cells[id].numbers_with(self.pending.map(|pending| &pending[id]));
            let fed = self.fed[id].iter().flatten().map(|at| &**at);
            let sources: FxHashSet<&[ElementId]> = stored.map(|(at, _)| at).chain(fed).collect();
            for source in sources {
                for (number, feeder) in source_cube.feeders.iter().enumerate() {
                    if !within(self.model, source_cube, &feeder.source, source) {
                        continue;
                    }
                    for (target_id, target) in feeder_targets(self.model, id, feeder, source)? {
                        let target: Vec<Option<ElementId>> = target.into_iter().map(Some).collect();
                        if target_id == cube
                            && within(self.model, &self.model.cubes()[cube], &target, at)
                        {
                            found.push((id, number, source.to_vec()));
                            if found.len() == limit {
                                return Ok(found);
                            }
                        }
                    }
                }
            }
        }
        Ok(found)
    }

    /// Adds up a consolidated cell that no rule has decided.
    fn consolidate(&self, cube: usize, at: &[ElementId]) -> Read<Scalar> {
        let definition = &self.model.cubes()[cube];
        let leaves: Vec<Leaves> = self
            .elements(definition, at)
            .map(|e| e.leaves.as_slice())
            .collect();
        let leaf_rules = || definition.rules.iter().filter(|rule| rule.decides_leaves());
        if !self.reached(definition, &leaves) {
            return Ok(self.stored(cube, at));
        }

        let ruled = |leaf: &[ElementId]| leaf_rules().any(|rule| matches(&rule.area, leaf));
        let pending = self.pending.map(|pending| &pending[cube]);
        let mut total = 0.0;
        if let Some(fed) = &self.fed[cube] {
            // A stored cell counts as it is unless a rule covers it; a fed cell counts if a
            // rule covers it. A cell that is both is counted once, as stored.
            for (leaf, stored) in self.cells[cube].numbers_with(pending) {
                if let Some(weight) = weight(&leaves, leaf) {
                    total += weight
                        * match ruled(leaf) {
                            true => self.number(cube, leaf)?,
                            false => stored,
                        };
                }
            }
            for leaf in fed {
                let stored = self.cells[cube].number_with(leaf, pending) != 0.0;
                if let (false, Some(weight), true) = (stored, weight(&leaves, leaf), ruled(leaf)) {
                    total += weight * self.number(cube, leaf)?;
                }
            }
        } else if let Some(mut positions) = self.start(&leaves)? {
            let mut leaf: Vec<ElementId> = leaves.iter().map(|l| l[0].0).collect();
            loop {
                let weights = positions.iter().zip(&leaves).map(|(&p, l)| l[p].1);
                total += weights.product::<f64>() * self.number(cube, &leaf)?;
                if !advance(&leaves, &mut positions, &mut leaf) {
                    break;
                }
            }
        }
        Ok(Scalar::Number(total))
    }

    /// Where a walk over every combination of leaves starts, or nothing if there are none.
    /// Refuses a walk too long to finish in reasonable time.
    fn start(&self, leaves: &[Leaves]) -> Read<Option<Vec<usize>>> {
        let count = leaves
            .iter()
            .try_fold(1usize, |count, l| count.checked_mul(l.len()));
        match count {
            Some(0) => Ok(None),
            Some(count) if count <= MOST_VISITED => Ok(Some(vec![0; leaves.len()])),
            _ => Err(MdxError::new(format!(
                "a consolidation over rules would visit more than {MOST_VISITED} cells; add feeders"
            ))),
        }
    }

    fn number(&self, cube: usize, at: &[ElementId]) -> Read<f64> {
        Ok(match self.read(cube, at)? {
            Scalar::Number(number) => number,
            Scalar::Text(_) => 0.0,
        })
    }

    /// A cell as stored: a leaf's own value, or a consolidation of stored leaves.
    fn stored(&self, cube: usize, at: &[ElementId]) -> Scalar {
        let (cells, definition) = (&self.cells[cube], &self.model.cubes()[cube]);
        match self.pending {
            Some(pending) => cells.read_with(self.model, definition, at, &pending[cube]),
            None => cells.read(self.model, definition, at),
        }
    }

    fn dimension(&self, cube: &Cube, position: usize) -> &'a [Element] {
        &self.model.dimensions()[cube.dimensions[position] as usize].elements
    }

    fn elements(
        &self,
        cube: &Cube,
        at: &[ElementId],
    ) -> impl DoubleEndedIterator<Item = &'a Element> {
        let model = self.model;
        let pairs = cube.dimensions.iter().zip(at);
        pairs.map(move |(&d, &e)| &model.dimensions()[d as usize].elements[e as usize])
    }

    fn names(&self, cube: usize, at: &[ElementId]) -> String {
        let names: Vec<&str> = self
            .elements(&self.model.cubes()[cube], at)
            .map(|element| element.name.as_str())
            .collect();
        names.join(", ")
    }
}

/// Whether a leaf cell lies in an area, where an area element that is a consolidation stands
/// for every leaf beneath it.
fn within(model: &Model, cube: &Cube, area: &[Option<ElementId>], at: &[ElementId]) -> bool {
    let fits = |(position, (wanted, &element)): (usize, (&Option<ElementId>, &ElementId))| {
        let dimension = &model.dimensions()[cube.dimensions[position] as usize];
        wanted.is_none_or(|wanted| {
            let beneath = &dimension.elements[wanted as usize].leaves;
            wanted == element || beneath.iter().any(|&(leaf, _)| leaf == element)
        })
    };
    area.iter().zip(at).enumerate().all(fits)
}

/// Works out which leaf cells the feeders feed, starting from every stored cell and following
/// feeders from fed cells onward.
pub(crate) fn feed(model: &Model, cells: &[Cells], pending: Option<&[Writes]>) -> Read<Fed> {
    let cubes = model.cubes();
    let mut fed: Fed = cubes
        .iter()
        .map(|cube| cube.sparse.then(FxHashSet::default))
        .collect();
    let mut active: Vec<(usize, Coordinate)> = Vec::new();
    for (id, cube) in cubes.iter().enumerate() {
        if !cube.feeders.is_empty() {
            let stored = cells[id].numbers_with(pending.map(|pending| &pending[id]));
            active.extend(stored.map(|(at, _)| (id, Coordinate::from(at))));
        }
    }
    let leaves = |cube: &Cube, position: usize, element: ElementId| -> Leaves {
        let dimension = &model.dimensions()[cube.dimensions[position] as usize];
        &dimension.elements[element as usize].leaves
    };
    let mut count = 0;
    while let Some((id, at)) = active.pop() {
        let cube = &cubes[id];
        for feeder in &cube.feeders {
            // A source that names a consolidation stands for every leaf beneath it.
            if !within(model, cube, &feeder.source, &at) {
                continue;
            }
            for (target_id, target) in feeder_targets(model, id, feeder, &at)? {
                let target_cube = &cubes[target_id];
                // A target that names a consolidation feeds every leaf beneath it.
                let leaves: Vec<Leaves> = target
                    .iter()
                    .enumerate()
                    .map(|(position, &element)| leaves(target_cube, position, element))
                    .collect();
                if leaves.iter().any(|leaves| leaves.is_empty()) {
                    continue;
                }
                let cells = fed[target_id]
                    .as_mut()
                    .expect("a fed cube is marked sparse");
                let mut positions = vec![0; leaves.len()];
                let mut leaf: Vec<ElementId> = leaves.iter().map(|l| l[0].0).collect();
                loop {
                    if cells.insert(Coordinate::from(leaf.as_slice())) {
                        count += 1;
                        if count > MOST_FED {
                            return Err(MdxError::new(format!(
                                "the feeders feed more than {MOST_FED} cells"
                            )));
                        }
                        if !target_cube.feeders.is_empty() {
                            active.push((target_id, Coordinate::from(leaf.as_slice())));
                        }
                    }
                    if !advance(&leaves, &mut positions, &mut leaf) {
                        break;
                    }
                }
            }
        }
    }
    Ok(fed)
}
