//! Reads cells as rules and feeders make them.
//!
//! A cube without rules is read straight from its stored cells. In a cube with rules, the
//! first rule that covers a cell decides it. A consolidation that no leaf rule can reach is
//! still added up from stored cells alone. One that a leaf rule can reach must also count the
//! calculated leaves beneath it: every one of them if the cube is not fed, or only the stored
//! and fed ones if it is.

use std::borrow::Cow;
use std::cell::RefCell;

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

type Read<T> = Result<T, MdxError>;

pub(crate) struct Reader<'a> {
    model: &'a Model,
    cells: &'a [Cells],
    /// The writes of the transaction being read in, by cube.
    pending: Option<&'a [Writes]>,
    fed: Cow<'a, Fed>,
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
    ) -> Self {
        Self {
            model,
            cells,
            pending,
            fed,
            known: RefCell::default(),
            working: RefCell::default(),
        }
    }

    pub(crate) fn read(&self, cube: usize, at: &[ElementId]) -> Read<Scalar> {
        if self.model.cubes()[cube].rules.is_empty() {
            return Ok(self.stored(cube, at));
        }
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
        let definition = &self.model.cubes()[cube];
        let mut elements = self.elements(definition, at);
        let text = elements
            .next_back()
            .is_some_and(|e| e.kind == ElementType::Text);
        let consolidated = !text
            && self
                .elements(definition, at)
                .any(|e| e.kind == ElementType::Consolidated);
        let read = |cube: usize, at: &[ElementId]| self.read(cube, at);
        for rule in &definition.rules {
            if !rule.covers(at, text, consolidated) {
                continue;
            }
            match evaluate_rule(self.model, cube, rule, at, &read)? {
                Outcome::Value(value) => return Ok(value),
                Outcome::Stet => break,
                Outcome::Continue => {}
            }
        }
        match consolidated {
            true => self.consolidate(cube, at),
            false => Ok(self.stored(cube, at)),
        }
    }

    /// Adds up a consolidated cell that no rule has decided.
    fn consolidate(&self, cube: usize, at: &[ElementId]) -> Read<Scalar> {
        let definition = &self.model.cubes()[cube];
        let leaves: Vec<Leaves> = self
            .elements(definition, at)
            .map(|e| e.leaves.as_slice())
            .collect();
        let holds = |leaves: Leaves, id: ElementId| {
            leaves.binary_search_by_key(&id, |&(leaf, _)| leaf).is_ok()
        };
        let reaches = |area: &[Option<ElementId>]| {
            let fits = |(wanted, leaves): (&Option<ElementId>, &Leaves)| {
                wanted.is_none_or(|w| holds(leaves, w))
            };
            area.iter().zip(&leaves).all(fits)
        };
        let leaf_rules = || definition.rules.iter().filter(|rule| rule.decides_leaves());
        if !leaf_rules().any(|rule| reaches(&rule.area)) {
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
            let within = feeder.source.iter().zip(at.iter()).enumerate().all(
                |(position, (wanted, &element))| {
                    wanted.is_none_or(|wanted| {
                        wanted == element
                            || leaves(cube, position, wanted)
                                .iter()
                                .any(|&(leaf, _)| leaf == element)
                    })
                },
            );
            if !within {
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
