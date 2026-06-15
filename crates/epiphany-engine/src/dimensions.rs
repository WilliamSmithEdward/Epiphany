//! Shared, independent dimensions (ADR-0024), Phase 0: the registry data model.
//!
//! A `DimensionRegistry` owns dimension *identity*: each `SharedDimension` is a
//! core [`Dimension`] plus a stable, server-unique [`DimensionId`] (never
//! positional) and a `generation` bumped on every append. Cubes will reference
//! these by id and keep their own per-cube packing (ADR-0006). This module is
//! additive and not yet wired into the live read/commit path; Phase 1 threads it
//! through `Cube`/`Published`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use epiphany_core::{Dimension, DimensionDef, EdgeSpec, ElementKind, ElementSpec, ModelError};

/// A server-unique dimension identifier, minted from the engine's `IdGen`. It is
/// stable for the life of the dimension and is never a positional index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DimensionId(pub u64);

/// A registry-owned dimension: the core [`Dimension`] (element identity, edges,
/// attribute definitions) plus a stable id and a monotonic generation.
#[derive(Debug, Clone)]
pub struct SharedDimension {
    pub id: DimensionId,
    /// Bumped on every append; a cube records which generation its packed cells
    /// correspond to (`attached_generation`).
    pub generation: u64,
    pub dimension: Dimension,
}

impl SharedDimension {
    /// A new shared dimension at generation 0.
    pub fn new(id: DimensionId, dimension: Dimension) -> Self {
        Self {
            id,
            generation: 0,
            dimension,
        }
    }

    /// A `DimensionDef` capturing this dimension's current elements and edges, so
    /// a cube can materialize a copy of it at attach time (ADR-0024 v1).
    pub fn to_dimension_def(&self) -> DimensionDef {
        let d = &self.dimension;
        let elements = d
            .iter_elements()
            .map(|el| (el.name.clone(), el.kind))
            .collect();
        let edges = d
            .edges()
            .into_iter()
            .map(|(parent, child, weight)| {
                (
                    d.element(parent).expect("valid index").name.clone(),
                    d.element(child).expect("valid index").name.clone(),
                    weight,
                )
            })
            .collect();
        DimensionDef {
            name: d.name().to_string(),
            elements,
            edges,
        }
    }

    /// Append elements and consolidation edges (append-only, idempotent), and
    /// return the grown dimension with `generation` bumped only when something
    /// actually changed. Transactional: it stages on a clone, so a rejected
    /// change (kind conflict, unknown edge endpoint, edge-weight conflict, cycle,
    /// non-consolidated parent) returns an error and the original is untouched.
    /// Mirrors `Cube::extend_schema` semantics, scoped to one dimension.
    pub fn grown(
        &self,
        elements: &[ElementSpec],
        edges: &[EdgeSpec],
    ) -> Result<SharedDimension, ModelError> {
        let name = self.dimension.name().to_string();
        let mut next = self.dimension.clone();
        let mut changed = false;

        for spec in elements {
            match next.index_of(&spec.name) {
                Some(existing) => {
                    if next.element(existing)?.kind != spec.kind {
                        return Err(ModelError::ElementKindConflict {
                            dimension: name.clone(),
                            element: spec.name.clone(),
                        });
                    }
                }
                None => {
                    match spec.kind {
                        ElementKind::Leaf => next.add_leaf(spec.name.clone()),
                        ElementKind::String => next.add_string(spec.name.clone()),
                        ElementKind::Consolidated => next.add_consolidated(spec.name.clone()),
                    };
                    changed = true;
                }
            }
        }

        for edge in edges {
            let parent =
                next.index_of(&edge.parent)
                    .ok_or_else(|| ModelError::ElementNotFound {
                        dimension: name.clone(),
                        element: edge.parent.clone(),
                    })?;
            let child = next
                .index_of(&edge.child)
                .ok_or_else(|| ModelError::ElementNotFound {
                    dimension: name.clone(),
                    element: edge.child.clone(),
                })?;
            if let Some(&(_, _, w)) = next
                .edges()
                .iter()
                .find(|&&(p, c, _)| p == parent && c == child)
            {
                if w != edge.weight {
                    return Err(ModelError::EdgeWeightConflict {
                        dimension: name.clone(),
                        parent: edge.parent.clone(),
                        child: edge.child.clone(),
                    });
                }
                continue;
            }
            next.add_child(parent, child, edge.weight)?;
            changed = true;
        }

        Ok(SharedDimension {
            id: self.id,
            generation: if changed {
                self.generation + 1
            } else {
                self.generation
            },
            dimension: next,
        })
    }
}

/// The server-level dimension registry (ADR-0024): id -> shared dimension, plus a
/// reverse index of which cubes reference each dimension (so a referenced
/// dimension cannot be deleted). Cheap to clone for the copy-on-write swap behind
/// the engine's `ArcSwap` (the `SharedDimension`s are shared by `Arc`).
#[derive(Debug, Clone, Default)]
pub struct DimensionRegistry {
    by_id: BTreeMap<DimensionId, Arc<SharedDimension>>,
    refs: BTreeMap<DimensionId, BTreeSet<String>>,
}

impl DimensionRegistry {
    /// Look up a shared dimension by id.
    pub fn get(&self, id: DimensionId) -> Option<&Arc<SharedDimension>> {
        self.by_id.get(&id)
    }

    /// Insert or replace a shared dimension (e.g. a grown generation).
    pub fn put(&mut self, dim: Arc<SharedDimension>) {
        let id = dim.id;
        self.by_id.insert(id, dim);
        self.refs.entry(id).or_default();
    }

    /// Remove a shared dimension and its reference set (callers must verify it is
    /// unreferenced first; cubes keep their own materialized copies).
    pub fn remove(&mut self, id: DimensionId) {
        self.by_id.remove(&id);
        self.refs.remove(&id);
    }

    /// Record that `cube` references `id`.
    pub fn attach(&mut self, id: DimensionId, cube: &str) {
        self.refs.entry(id).or_default().insert(cube.to_string());
    }

    /// Drop a cube's reference to `id`.
    pub fn detach(&mut self, id: DimensionId, cube: &str) {
        if let Some(set) = self.refs.get_mut(&id) {
            set.remove(cube);
        }
    }

    /// Whether any cube references `id` (a referenced dimension cannot be deleted).
    pub fn is_referenced(&self, id: DimensionId) -> bool {
        self.refs.get(&id).is_some_and(|set| !set.is_empty())
    }

    /// The cubes referencing `id`, sorted.
    pub fn referencing(&self, id: DimensionId) -> Vec<String> {
        self.refs.get(&id).into_iter().flatten().cloned().collect()
    }

    /// All dimension ids, in sorted order.
    pub fn ids(&self) -> Vec<DimensionId> {
        self.by_id.keys().copied().collect()
    }

    /// All shared dimensions, in id order (for persistence).
    pub fn all(&self) -> Vec<Arc<SharedDimension>> {
        self.by_id.values().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epiphany_core::Dimension;

    fn region() -> Dimension {
        let mut d = Dimension::new("Region");
        d.add_leaf("North");
        d
    }

    fn leaf(name: &str) -> ElementSpec {
        ElementSpec {
            dimension: "Region".into(),
            name: name.into(),
            kind: ElementKind::Leaf,
        }
    }

    #[test]
    fn grow_appends_and_bumps_generation_idempotently() {
        let dim = SharedDimension::new(DimensionId(1), region());
        assert_eq!(dim.generation, 0);

        // Appending a new leaf bumps the generation and preserves stable indices.
        let g1 = dim.grown(&[leaf("South")], &[]).unwrap();
        assert_eq!(g1.generation, 1);
        assert_eq!(g1.dimension.index_of("North"), Some(0));
        assert_eq!(g1.dimension.index_of("South"), Some(1));

        // Re-appending the same element is a no-op: no generation bump.
        let g2 = g1.grown(&[leaf("South")], &[]).unwrap();
        assert_eq!(g2.generation, 1);

        // A kind conflict on an existing element is rejected, original untouched.
        let conflict = g1.grown(
            &[ElementSpec {
                dimension: "Region".into(),
                name: "South".into(),
                kind: ElementKind::Consolidated,
            }],
            &[],
        );
        assert!(matches!(
            conflict,
            Err(ModelError::ElementKindConflict { .. })
        ));
    }

    #[test]
    fn grow_adds_consolidation_edges() {
        let dim = SharedDimension::new(DimensionId(1), region());
        let grown = dim
            .grown(
                &[
                    leaf("South"),
                    ElementSpec {
                        dimension: "Region".into(),
                        name: "Total".into(),
                        kind: ElementKind::Consolidated,
                    },
                ],
                &[
                    EdgeSpec {
                        dimension: "Region".into(),
                        parent: "Total".into(),
                        child: "North".into(),
                        weight: 1,
                    },
                    EdgeSpec {
                        dimension: "Region".into(),
                        parent: "Total".into(),
                        child: "South".into(),
                        weight: 1,
                    },
                ],
            )
            .unwrap();
        assert_eq!(grown.generation, 1);
        assert_eq!(grown.dimension.edges().len(), 2);
    }

    #[test]
    fn registry_tracks_references_and_prevents_orphan_delete_check() {
        let mut reg = DimensionRegistry::default();
        let id = DimensionId(7);
        reg.put(Arc::new(SharedDimension::new(id, region())));
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_referenced(id));

        reg.attach(id, "Sales");
        reg.attach(id, "Budget");
        assert!(reg.is_referenced(id));
        assert_eq!(
            reg.referencing(id),
            vec!["Budget".to_string(), "Sales".to_string()]
        );

        reg.detach(id, "Sales");
        assert!(reg.is_referenced(id)); // Budget still references it
        reg.detach(id, "Budget");
        assert!(!reg.is_referenced(id)); // now unreferenced -> deletable
    }
}
