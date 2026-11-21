//! A long set on a `NON EMPTY` axis, or in `NonEmpty`, is not written out to have its empty
//! tuples dropped: the tuples that hold something are found from the cells of the cube.

use rustc_hash::FxHashSet;

use super::{
    Context, Evaluated, Evaluator, Id, MOST_TUPLES, Member, Tuple, bounded, cross_join, flag,
};
use crate::mdx::parser::Expr;
use crate::model::ElementId;

/// A cross join on a `NON EMPTY` axis is not written out past this many tuples if its tuples
/// that hold something can be found from the cells instead.
const CROSSED_FROM: usize = 10_000;
/// Looking through the cells costs one pass over them, and writing the cross join out costs
/// working out each of its tuples; a tuple costs about what four cells do. So the cells are
/// looked through only where there are no more than this many to a tuple of the cross join.
/// (Measured 2026-11-21: a pass over 200,000 cells took 5 ms, and 12,769 tuples about 1 ms.)
const CELLS_A_TUPLE: usize = 4;

/// What an axis holds: its tuples, or the sets whose cross join they are, where that is too
/// long to write out before the empty tuples are dropped from it. Such a cross join may
/// have stood inside `Hierarchize`, and is then put in that order once it is cut down:
/// members before what is beneath them, or after if the flag is set.
pub(super) enum Laid {
    Out(Vec<Tuple>),
    Crossed(Vec<Vec<Tuple>>, Option<bool>),
}

impl Evaluator<'_> {
    /// The set of an axis. A set of single elements on a `NON EMPTY` axis, or a cross join of
    /// such sets, is left as its sets if it would be long: most of it is likely to be dropped.
    pub(super) fn lay(
        &self,
        expression: &Expr,
        non_empty: bool,
        context: &Context,
    ) -> Evaluated<Laid> {
        // Hierarchize only puts in order, which can as well be done to what is left.
        let (expression, order) = match expression {
            Expr::Call(function, arguments) if non_empty && function == "HIERARCHIZE" => {
                match (arguments.as_slice(), flag(arguments.get(1)).as_deref()) {
                    ([inner], _) => (inner, Some(false)),
                    ([inner, _], Some("POST")) => (inner, Some(true)),
                    _ => (expression, None),
                }
            }
            _ => (expression, None),
        };
        let ordered = |set: Vec<Tuple>| match order {
            Some(post) => self.hierarchized(set, post),
            None => set,
        };
        let mut parts = Vec::new();
        fn crossed<'e>(expression: &'e Expr, parts: &mut Vec<&'e Expr>) {
            match expression {
                Expr::Call(function, arguments) if function == "CROSSJOIN" => {
                    arguments
                        .iter()
                        .for_each(|argument| crossed(argument, parts));
                }
                Expr::Binary(left, "*", right) => {
                    crossed(left, parts);
                    crossed(right, parts);
                }
                other => parts.push(other),
            }
        }
        crossed(expression, &mut parts);
        if !non_empty {
            return self.set(expression, context).map(Laid::Out);
        }
        // A set that is no cross join is one of a single set, and cut down the same way.
        let mut sets = Vec::new();
        for part in parts {
            sets.push(self.set(part, context)?);
        }
        let elements = |set: &Vec<Tuple>| {
            let element = |tuple: &Tuple| {
                matches!(
                    tuple[..],
                    [Member {
                        id: Id::Element(_),
                        ..
                    }]
                )
            };
            set.iter().all(element)
        };
        let long = sets
            .iter()
            .try_fold(1usize, |product, set| product.checked_mul(set.len()))
            .is_none_or(|product| product > CROSSED_FROM);
        let mut positions: Vec<usize> = sets
            .iter()
            .filter_map(|set| Some(set.first()?[0].position))
            .collect();
        positions.sort_unstable();
        positions.dedup();
        if long && sets.iter().all(elements) && positions.len() == sets.len() {
            return Ok(Laid::Crossed(sets, order));
        }
        Ok(Laid::Out(ordered(written_out(&sets)?)))
    }

    /// The tuples of a set that was laid: as they are, or of a cross join left unwritten
    /// those that may hold something beside the sets that stand beside it, if they can be
    /// found from the cells, and else the whole cross join.
    pub(super) fn cut(
        &self,
        laid: &Laid,
        beside: &[&Vec<Tuple>],
        context: &Context,
    ) -> Evaluated<Vec<Tuple>> {
        match laid {
            Laid::Out(set) => Ok(set.clone()),
            Laid::Crossed(crossed, order) => {
                let set = match self.holding(crossed, beside, context)? {
                    Some(set) => set,
                    None => written_out(crossed)?,
                };
                Ok(match *order {
                    Some(post) => self.hierarchized(set, post),
                    None => set,
                })
            }
        }
    }

    /// The tuples of a cross join that may hold something beside the sets that stand beside
    /// it and the cell it is asked at, in the order the cross join would have them: found by
    /// looking at each cell that may hold something and noting the tuples it counts towards,
    /// which never writes the cross join out. Nothing if they cannot be found that way,
    /// because a calculated member stands somewhere, a set beside the cross join is of one
    /// of its own dimensions, or the cube cannot say which of its cells may hold something.
    fn holding(
        &self,
        crossed: &[Vec<Tuple>],
        beside: &[&Vec<Tuple>],
        context: &Context,
    ) -> Evaluated<Option<Vec<Tuple>>> {
        // For each dimension of the cube, the members a cell must stand beneath one of.
        let mut wanted: Vec<Vec<Id>> = context.iter().map(|&id| vec![id]).collect();
        let mut place = |members: &mut dyn Iterator<Item = Member>| {
            let mut first = true;
            for member in members {
                if std::mem::take(&mut first) {
                    wanted[member.position].clear();
                }
                wanted[member.position].push(member.id);
            }
        };
        let own: Vec<usize> = crossed.iter().map(|set| set[0][0].position).collect();
        let shared = |set: &&Vec<Tuple>| {
            let first = set.first().into_iter().flatten();
            first
                .into_iter()
                .any(|member| own.contains(&member.position))
        };
        if beside.iter().any(shared) {
            return Ok(None);
        }
        for set in crossed.iter().chain(beside.iter().copied()) {
            let width = set.first().map_or(0, Vec::len);
            (0..width).for_each(|at| place(&mut set.iter().map(|tuple| tuple[at])));
        }
        // For each dimension, the leaves beneath those members; and for a dimension of the
        // cross join, which of its set's members each leaf stands beneath.
        let mut beneath: Vec<Vec<Option<Vec<u32>>>> = Vec::new();
        for (position, members) in wanted.iter().enumerate() {
            let elements = &self.dimension(position).elements;
            let mut leaves: Vec<Option<Vec<u32>>> = vec![None; elements.len()];
            for (index, id) in members.iter().enumerate() {
                let Id::Element(id) = *id else {
                    return Ok(None);
                };
                for &(leaf, _) in &elements[id as usize].leaves {
                    let above = leaves[leaf as usize].get_or_insert_default();
                    if own.contains(&position) {
                        above.push(index as u32);
                    }
                }
            }
            beneath.push(leaves);
        }
        // A tuple of the cross join is known by one number: its place in the cross join
        // written out, which is each member's place in its set times the length of all the
        // sets after it.
        let tuples = crossed
            .iter()
            .try_fold(1usize, |product, set| product.checked_mul(set.len()));
        let Some(tuples) = tuples else {
            return Ok(None);
        };
        let mut strides = vec![1; crossed.len()];
        for set in (1..crossed.len()).rev() {
            strides[set - 1] = strides[set] * crossed[set].len();
        }
        let set_of: Vec<Option<usize>> = (0..context.len())
            .map(|position| own.iter().position(|&own| own == position))
            .collect();
        let mut found: FxHashSet<usize> = FxHashSet::default();
        let mut above: Vec<&[u32]> = vec![&[]; crossed.len()];
        let mut places = vec![0; crossed.len()];
        let most = tuples.saturating_mul(CELLS_A_TUPLE);
        let shown = (self.occupied)(most, &mut |at: &[ElementId]| {
            for (position, leaf) in at.iter().enumerate() {
                match (&beneath[position][*leaf as usize], set_of[position]) {
                    (None, _) => return,
                    (Some(members), Some(set)) => above[set] = members,
                    (Some(_), None) => {}
                }
            }
            if found.len() > MOST_TUPLES {
                return;
            }
            // Every way of taking one member above the cell from each set of the cross join.
            places.fill(0);
            loop {
                let place = |set: usize| above[set][places[set]] as usize * strides[set];
                found.insert((0..places.len()).map(place).sum());
                let Some(next) = (0..places.len())
                    .rev()
                    .find(|&set| places[set] + 1 < above[set].len())
                else {
                    break;
                };
                places[next] += 1;
                places[next + 1..].fill(0);
            }
        });
        if !shown {
            return Ok(None);
        }
        bounded(Some(found.len()))?;
        let mut found: Vec<usize> = found.into_iter().collect();
        found.sort_unstable();
        let tuple = |place: usize| -> Tuple {
            let member = |set: usize| crossed[set][place / strides[set] % crossed[set].len()][0];
            (0..crossed.len()).map(member).collect()
        };
        Ok(Some(found.into_iter().map(tuple).collect()))
    }
}

/// The cross join of several sets, written out.
fn written_out(sets: &[Vec<Tuple>]) -> Evaluated<Vec<Tuple>> {
    let mut whole = sets.first().cloned().unwrap_or_default();
    for set in sets.iter().skip(1) {
        whole = cross_join(&whole, set)?;
    }
    Ok(whole)
}
