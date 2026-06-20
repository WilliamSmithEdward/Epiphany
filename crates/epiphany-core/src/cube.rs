//! Cubes: the sparse multidimensional cell store and consolidation.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

use crate::{AttributeKind, AttributeValue, Dimension, ElementKind, Fixed, ModelError};

/// A cube coordinate: one element index per dimension, in dimension order.
pub type Coord = Box<[u32]>;

/// Number of bits needed to represent element indices `0..len` (at least 1).
/// Element counts are `u32`, so this never exceeds 32.
fn bits_for(len: u32) -> u32 {
    if len <= 1 {
        1
    } else {
        u32::BITS - (len - 1).leading_zeros()
    }
}

/// Per-dimension bit packing for narrow coordinates (ADR-0006).
///
/// Each dimension occupies a bit field, sized to its element count, at a fixed
/// offset. When the total width fits in 64 bits the cube packs each coordinate
/// into a single `u64` key; otherwise it falls back to a boxed slice.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Layout {
    offsets: Vec<u32>,
    masks: Vec<u64>,
    narrow: bool,
}

impl Layout {
    fn new(dimensions: &[Dimension]) -> Self {
        let mut offsets = Vec::with_capacity(dimensions.len());
        let mut masks = Vec::with_capacity(dimensions.len());
        let mut offset: u32 = 0;
        for dim in dimensions {
            let width = bits_for(dim.len()); // width <= 32
            offsets.push(offset);
            masks.push((1u64 << width) - 1);
            offset = offset.saturating_add(width);
        }
        Layout {
            offsets,
            masks,
            narrow: offset <= 64,
        }
    }

    fn pack(&self, coord: &[u32]) -> u64 {
        let mut key: u64 = 0;
        for (d, &idx) in coord.iter().enumerate() {
            key |= u64::from(idx) << self.offsets[d];
        }
        key
    }

    fn component(&self, key: u64, d: usize) -> u32 {
        ((key >> self.offsets[d]) & self.masks[d]) as u32
    }

    fn unpack(&self, key: u64, rank: usize) -> Vec<u32> {
        (0..rank).map(|d| self.component(key, d)).collect()
    }
}

/// The sparse cell store, keyed for memory efficiency (ADR-0006), generic over
/// the value type so numeric (`Fixed`) and string (interned id) cells share one
/// keying scheme.
///
/// Splitting the key type at the map level (rather than a per-key enum) keeps a
/// narrow numeric entry a bare `(u64, Fixed)` with no discriminant or alignment
/// padding: 16 bytes plus about one control byte of table overhead, comfortably
/// within the per-cell budget (ROADMAP section 8). Very wide cubes use a boxed-
/// slice key, paying a heap allocation per cell.
#[derive(Clone, Debug)]
enum CellStore<V> {
    Narrow(HashMap<u64, V, FxBuildHasher>),
    Wide(HashMap<Box<[u32]>, V, FxBuildHasher>),
}

impl<V> CellStore<V> {
    fn new(narrow: bool) -> Self {
        if narrow {
            CellStore::Narrow(HashMap::default())
        } else {
            CellStore::Wide(HashMap::default())
        }
    }

    fn len(&self) -> usize {
        match self {
            CellStore::Narrow(m) => m.len(),
            CellStore::Wide(m) => m.len(),
        }
    }

    fn get(&self, layout: &Layout, coord: &[u32]) -> Option<&V> {
        match self {
            CellStore::Narrow(m) => m.get(&layout.pack(coord)),
            CellStore::Wide(m) => m.get(coord),
        }
    }

    fn put(&mut self, layout: &Layout, coord: &[u32], value: V) {
        match self {
            CellStore::Narrow(m) => {
                m.insert(layout.pack(coord), value);
            }
            CellStore::Wide(m) => {
                m.insert(coord.into(), value);
            }
        }
    }

    fn clear(&mut self, layout: &Layout, coord: &[u32]) {
        match self {
            CellStore::Narrow(m) => {
                m.remove(&layout.pack(coord));
            }
            CellStore::Wide(m) => {
                m.remove(coord);
            }
        }
    }

    fn entries<'a>(&'a self, layout: &'a Layout, rank: usize) -> Entries<'a, V> {
        match self {
            CellStore::Narrow(m) => Entries::Narrow {
                iter: m.iter(),
                layout,
                rank,
            },
            CellStore::Wide(m) => Entries::Wide(m.iter()),
        }
    }
}

/// Iterator over populated cells as `(coordinate, &value)`, hiding which key
/// representation the store uses.
enum Entries<'a, V> {
    Narrow {
        iter: std::collections::hash_map::Iter<'a, u64, V>,
        layout: &'a Layout,
        rank: usize,
    },
    Wide(std::collections::hash_map::Iter<'a, Box<[u32]>, V>),
}

impl<'a, V> Iterator for Entries<'a, V> {
    type Item = (Vec<u32>, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Entries::Narrow { iter, layout, rank } => iter
                .next()
                .map(|(&key, value)| (layout.unpack(key, *rank), value)),
            Entries::Wide(iter) => iter.next().map(|(key, value)| (key.to_vec(), value)),
        }
    }
}

/// An interning pool for string cell values (ADR-0006).
///
/// Each distinct string is stored once and referenced by a compact id, so a cell
/// costs one `u32` id, not a heap allocation per cell, and repeated text (status
/// codes, labels) is shared. The pool grows monotonically; ids are assigned in
/// insertion order, which is deterministic.
#[derive(Clone, Debug, Default)]
struct StringPool {
    by_id: Vec<Box<str>>,
    ids: HashMap<Box<str>, u32, FxBuildHasher>,
}

impl StringPool {
    fn intern(&mut self, value: &str) -> u32 {
        if let Some(&id) = self.ids.get(value) {
            return id;
        }
        let id = self.by_id.len() as u32;
        let boxed: Box<str> = value.into();
        self.by_id.push(boxed.clone());
        self.ids.insert(boxed, id);
        id
    }

    fn get(&self, id: u32) -> &str {
        &self.by_id[id as usize]
    }
}

/// A small, fast, dependency-free hasher (FxHash) for the cell store.
///
/// Deterministic (fixed seed), and far cheaper than the default SipHash on the
/// integer keys that dominate the hot path.
#[derive(Default)]
struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.hash
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.add(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        let remainder = chunks.remainder();
        if !remainder.is_empty() {
            let mut buf = [0u8; 8];
            buf[..remainder.len()].copy_from_slice(remainder);
            self.add(u64::from_le_bytes(buf));
        }
    }
}

type FxBuildHasher = BuildHasherDefault<FxHasher>;

/// A cube: an ordered set of dimensions and a sparse store of populated leaf cells.
///
/// Only populated leaf cells are stored (writing zero clears a cell), so memory
/// scales with the data, not with the dense cartesian space. Coordinates are
/// packed into compact integer keys (ADR-0006). Numeric and string cells live in
/// separate stores with disjoint coordinate spaces (a string cell must address a
/// string element; a numeric cell is all numeric leaves). Consolidated values are
/// computed on demand by the sparse consolidation algorithm over numeric cells.
///
/// Note (later increments): consolidated reads currently scan populated cells,
/// which is correct; an index and a calculation cache come later.
#[derive(Clone, Debug)]
pub struct Cube {
    name: String,
    dimensions: Vec<Dimension>,
    layout: Layout,
    cells: CellStore<Fixed>,
    string_cells: CellStore<u32>,
    string_pool: StringPool,
}

/// Combined consolidation weight for a cell, or `None` if the cell does not
/// contribute to the query (some component is not a leaf-descendant of the
/// queried element). `component(d)` returns the cell's element index in dim `d`.
fn cell_weight(per_dim: &[HashMap<u32, i64>], component: impl Fn(usize) -> u32) -> Option<i128> {
    let mut weight: i128 = 1;
    for (d, weights) in per_dim.iter().enumerate() {
        weight *= i128::from(*weights.get(&component(d))?);
    }
    Some(weight)
}

/// A request to add one element to a named dimension. Append-only and
/// idempotent by name: re-adding an existing element is a no-op (its kind must
/// match). Used by flows to build dimension members at run time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElementSpec {
    /// The dimension to add the element to.
    pub dimension: String,
    /// The element name.
    pub name: String,
    /// The element kind (numeric leaf, string leaf, or consolidated).
    pub kind: ElementKind,
}

/// Where to insert a new element relative to a dimension's existing members
/// (ADR-0036): at the end, or immediately before or after a named member. The
/// drag-and-drop editor's "drop before/after" maps to these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Position {
    /// Append after the last existing member.
    AtEnd,
    /// Insert immediately before the named member.
    Before(String),
    /// Insert immediately after the named member.
    After(String),
}

/// A request to add one weighted consolidation edge to a named dimension.
/// Idempotent: an edge that already exists is a no-op.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeSpec {
    /// The dimension the edge belongs to.
    pub dimension: String,
    /// The consolidated parent element name.
    pub parent: String,
    /// The child element name.
    pub child: String,
    /// The consolidation weight.
    pub weight: i64,
}

/// A dimension definition used to build a brand-new cube (ADR-0021): a name, its
/// initial elements as `(name, kind)`, and its consolidation edges as
/// `(parent, child, weight)`. Elements and edges are validated together when the
/// cube is built.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DimensionDef {
    /// The dimension name.
    pub name: String,
    /// Initial elements, in declaration order.
    pub elements: Vec<(String, ElementKind)>,
    /// Initial consolidation edges as `(parent, child, weight)`.
    pub edges: Vec<(String, String, i64)>,
    /// Attribute definitions as `(name, kind)`, in declaration order. Carried so a
    /// cube materialized from a registry dimension keeps its attributes
    /// (ADR-0024/0033). Empty for a plain inline dimension.
    pub attributes: Vec<(String, AttributeKind)>,
    /// Per-element attribute values as `(element, attribute, value)`.
    pub attribute_values: Vec<(String, String, AttributeValue)>,
}

impl Cube {
    /// Build a brand-new cube from dimension definitions (ADR-0021), with full
    /// validation: at least one dimension, no duplicate dimension names, and the
    /// same element/edge checks as [`extend_schema`](Self::extend_schema)
    /// (element-kind conflicts, parent-must-be-consolidated, no cycles, edge
    /// weights). The cube starts with no cell data.
    pub fn build(name: impl Into<String>, dims: &[DimensionDef]) -> Result<Self, ModelError> {
        if dims.is_empty() {
            return Err(ModelError::EmptyCube);
        }
        let mut seen = HashSet::new();
        for d in dims {
            if !seen.insert(d.name.as_str()) {
                return Err(ModelError::DuplicateDimension {
                    dimension: d.name.clone(),
                });
            }
        }
        // Start from empty dimensions, then grow through extend_schema so element
        // and edge validation is the single, shared, well-tested path.
        let empty: Vec<Dimension> = dims.iter().map(|d| Dimension::new(&d.name)).collect();
        let mut cube = Cube::new(name, empty)?;
        let mut elements = Vec::new();
        let mut edges = Vec::new();
        for d in dims {
            for (el_name, kind) in &d.elements {
                elements.push(ElementSpec {
                    dimension: d.name.clone(),
                    name: el_name.clone(),
                    kind: *kind,
                });
            }
            for (parent, child, weight) in &d.edges {
                edges.push(EdgeSpec {
                    dimension: d.name.clone(),
                    parent: parent.clone(),
                    child: child.clone(),
                    weight: *weight,
                });
            }
        }
        cube.extend_schema(&elements, &edges)?;
        // Apply attribute defs + values after the element/edge schema exists, so a
        // cube materialized from a registry dimension keeps its attributes
        // (ADR-0024/0033). define_attribute/set_attribute_values validate and are
        // transactional per dimension.
        for d in dims {
            for (attr, kind) in &d.attributes {
                cube.define_attribute(&d.name, attr, *kind)?;
            }
            for (element, attr, value) in &d.attribute_values {
                cube.set_attribute_values(&d.name, attr, &[(element.clone(), value.clone())])?;
            }
        }
        Ok(cube)
    }

    /// Create a cube from an ordered, non-empty list of pre-built dimensions.
    pub fn new(name: impl Into<String>, dimensions: Vec<Dimension>) -> Result<Self, ModelError> {
        if dimensions.is_empty() {
            return Err(ModelError::EmptyCube);
        }
        let layout = Layout::new(&dimensions);
        let cells = CellStore::new(layout.narrow);
        let string_cells = CellStore::new(layout.narrow);
        Ok(Self {
            name: name.into(),
            dimensions,
            layout,
            cells,
            string_cells,
            string_pool: StringPool::default(),
        })
    }

    /// The cube name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Number of dimensions.
    pub fn rank(&self) -> usize {
        self.dimensions.len()
    }

    /// The dimension at position `i`.
    pub fn dimension(&self, i: usize) -> &Dimension {
        &self.dimensions[i]
    }

    /// All dimensions, in order.
    pub fn dimensions(&self) -> &[Dimension] {
        &self.dimensions
    }

    /// Append elements and consolidation edges to existing dimensions, returning
    /// the number of newly-created elements.
    ///
    /// Append-only and idempotent: re-adding an element or edge that already
    /// exists is a no-op, so a flow can run repeatedly. Existing element indices
    /// are preserved (new elements are appended), so stored cells stay valid; the
    /// coordinate packing is rebuilt only if a dimension's bit-width grew. This is
    /// the runtime "build dimension elements" capability flows use; it never
    /// removes or reorders elements (which would invalidate stored coordinates).
    ///
    /// Transactional: the change is staged on a clone and only swapped in if every
    /// element and edge applies cleanly, so a rejected change (unknown dimension,
    /// element-kind or edge-weight conflict, a cycle) leaves the cube untouched.
    pub fn extend_schema(
        &mut self,
        elements: &[ElementSpec],
        edges: &[EdgeSpec],
    ) -> Result<usize, ModelError> {
        let mut next = self.clone();
        let added = next.apply_growth(elements, edges)?;
        *self = next;
        Ok(added)
    }

    /// Define an attribute on a dimension (idempotent by name). Re-declaring an
    /// existing attribute with the same kind is a no-op; a different kind is a
    /// conflict. An error leaves the cube untouched.
    pub fn define_attribute(
        &mut self,
        dimension: &str,
        name: &str,
        kind: AttributeKind,
    ) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension)?;
        let dim = &mut self.dimensions[d];
        if let Some(existing) = dim.attribute_index(name) {
            if dim.attribute_defs()[existing as usize].kind != kind {
                return Err(ModelError::AttributeKindConflict {
                    dimension: dimension.to_string(),
                    attribute: name.to_string(),
                });
            }
            return Ok(());
        }
        dim.add_attribute(name.to_string(), kind);
        Ok(())
    }

    /// Set an attribute's value for one or more elements (by element name).
    /// Transactional: it stages on a clone of the dimension, so any rejected
    /// value (unknown element, kind mismatch, alias collision) leaves the cube
    /// untouched.
    pub fn set_attribute_values(
        &mut self,
        dimension: &str,
        attribute: &str,
        values: &[(String, AttributeValue)],
    ) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension)?;
        let mut next = self.dimensions[d].clone();
        for (element_name, value) in values {
            let element =
                next.index_of(element_name)
                    .ok_or_else(|| ModelError::ElementNotFound {
                        dimension: dimension.to_string(),
                        element: element_name.clone(),
                    })?;
            next.set_attribute(element, attribute, value.clone())?;
        }
        self.dimensions[d] = next;
        Ok(())
    }

    // ---- structural editing (ADR-0036) ----
    //
    // Each op stages on a clone, validates fully through the dimension primitive,
    // remaps stored cells for the index change it reports, and only then swaps the
    // clone in. A rejected edit returns the model error and changes nothing.

    /// Remap every stored numeric and string cell's coordinate component for
    /// dimension `d` through `to_new` (old element index -> new element index),
    /// dropping any cell whose component for `d` maps to `u32::MAX` (a removed
    /// element). The other coordinate components are untouched. Both cell stores
    /// are rebuilt under the (possibly re-widened) layout, so a reorder/insert/
    /// delete that changes the dimension's bit-width re-packs correctly.
    ///
    /// Deterministic: cells are re-inserted into a fresh store, and the result is
    /// independent of iteration order (each remapped coordinate is unique because
    /// `to_new` is injective on the surviving elements). Runs after the dimension
    /// edit is already staged on `self`, so the new layout reflects the edit.
    fn remap_cells_for_dimension(&mut self, d: usize, to_new: &[u32]) {
        // Re-pack both stores under the (possibly re-widened) layout, mapping each
        // cell's component for `d` through `to_new` and dropping any cell whose
        // component maps to `u32::MAX` (a removed element).
        let new_layout = Layout::new(&self.dimensions);
        self.rebuild_stores(new_layout, true, true, |coord| {
            let mapped = to_new[coord[d] as usize];
            if mapped == u32::MAX {
                return false; // a removed element: drop the cell
            }
            coord[d] = mapped;
            true
        });
    }

    /// Rebuild the numeric and/or string cell store(s) under `new_layout`, applying
    /// `transform` to each stored coordinate: a returned `false` drops the cell, and
    /// a `true` keeps the (possibly mutated) coordinate, re-put under the new layout.
    ///
    /// This is the single rebuild primitive shared by the cell-store reshaping ops
    /// ([`remap_cells_for_dimension`](Self::remap_cells_for_dimension),
    /// [`relayout`](Self::relayout), and
    /// [`drop_cells_for_element`](Self::drop_cells_for_element)): each collects every
    /// cell from both stores via `entries()`, builds a fresh store under the new
    /// layout, and re-puts each surviving (and optionally transformed) cell. Cells
    /// are collected under the OLD layout (the one the current keys were packed
    /// with), then re-packed under `new_layout`.
    ///
    /// Deterministic: cells are re-inserted into a fresh store and the result is
    /// independent of iteration order, because each retained coordinate is unique
    /// (the callers' transforms preserve that). When `numeric`/`strings` is false
    /// the matching store is left untouched (and `new_layout` must equal the current
    /// layout, since that store keeps its existing keys).
    fn rebuild_stores(
        &mut self,
        new_layout: Layout,
        numeric: bool,
        strings: bool,
        mut transform: impl FnMut(&mut Vec<u32>) -> bool,
    ) {
        let rank = self.rank();
        if numeric {
            let collected: Vec<(Vec<u32>, Fixed)> = self
                .cells
                .entries(&self.layout, rank)
                .map(|(coord, &value)| (coord, value))
                .collect();
            let mut new_cells = CellStore::new(new_layout.narrow);
            for (mut coord, value) in collected {
                if transform(&mut coord) {
                    new_cells.put(&new_layout, &coord, value);
                }
            }
            self.cells = new_cells;
        }
        if strings {
            let collected: Vec<(Vec<u32>, u32)> = self
                .string_cells
                .entries(&self.layout, rank)
                .map(|(coord, &id)| (coord, id))
                .collect();
            let mut new_strings = CellStore::new(new_layout.narrow);
            for (mut coord, id) in collected {
                if transform(&mut coord) {
                    new_strings.put(&new_layout, &coord, id);
                }
            }
            self.string_cells = new_strings;
        }
        self.layout = new_layout;
    }

    /// Reorder a dimension's members to `new_order` (a permutation of its current
    /// member names), remapping every stored cell so each value follows its
    /// member to its new index. Transactional: a non-permutation is rejected and
    /// the cube is unchanged.
    pub fn reorder_elements(
        &mut self,
        dimension: &str,
        new_order: &[String],
    ) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension)?;
        let mut next = self.clone();
        let to_new = next.dimensions[d].reorder(new_order)?;
        next.remap_cells_for_dimension(d, &to_new);
        *self = next;
        Ok(())
    }

    /// Reparent `child` under `new_parent` (or detach to a root when `None`) in a
    /// dimension, converting a numeric/string `new_parent` to a consolidation
    /// first. An edge-only change, so no cell remap. Transactional: a self-parent
    /// or a cycle is rejected and the cube is unchanged.
    pub fn reparent_element(
        &mut self,
        dimension: &str,
        child: &str,
        new_parent: Option<&str>,
        weight: i64,
    ) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension)?;
        let mut next = self.clone();
        let dim = &mut next.dimensions[d];
        let child_idx = dim
            .index_of(child)
            .ok_or_else(|| ModelError::ElementNotFound {
                dimension: dimension.to_string(),
                element: child.to_string(),
            })?;
        let parent_idx = match new_parent {
            Some(p) => Some(dim.index_of(p).ok_or_else(|| ModelError::ElementNotFound {
                dimension: dimension.to_string(),
                element: p.to_string(),
            })?),
            None => None,
        };
        // If the new parent is a numeric/string member (it holds a stored value),
        // gaining a child converts it to a consolidation, whose own stored cell
        // must then be dropped (a consolidation is computed, not stored), mirroring
        // set_element_kind. Without this the orphan cell would later fail to reload
        // (set_leaf rejects a consolidated coordinate) and could resurrect if the
        // member were converted back to a leaf.
        let parent_was_leaf = match parent_idx {
            Some(p) => matches!(
                dim.element(p)?.kind,
                ElementKind::Leaf | ElementKind::String
            ),
            None => false,
        };
        dim.reparent(child_idx, parent_idx, weight)?;
        if parent_was_leaf {
            if let Some(p) = parent_idx {
                next.drop_cells_for_element(d, p, true, true);
            }
        }
        *self = next;
        Ok(())
    }

    /// Add `child` to the consolidation `parent` ADDITIVELY: the new
    /// `parent -> child` edge is created while every existing edge of `child`
    /// (its membership in other consolidations, or its place as a root) is kept.
    /// This is the OLAP "add to a consolidation" operation, where a member may
    /// roll up to multiple consolidations (alternate hierarchies). Unlike
    /// [`Self::reparent_element`], it never detaches the child from anything.
    ///
    /// A numeric/string `parent` is converted to a consolidation first (so it can
    /// hold a child), and its own stored cell is dropped (a consolidation is
    /// computed, not stored), mirroring [`Self::reparent_element`]. An edge-only
    /// change otherwise, so no cell remap.
    ///
    /// Idempotent: re-adding an edge that already exists is a no-op (it does not
    /// error and does not duplicate the edge). Transactional: a self-parent or a
    /// cycle is rejected and the cube is unchanged.
    pub fn add_child_element(
        &mut self,
        dimension: &str,
        parent: &str,
        child: &str,
        weight: i64,
    ) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension)?;
        let mut next = self.clone();
        let dim = &mut next.dimensions[d];
        let parent_idx = dim
            .index_of(parent)
            .ok_or_else(|| ModelError::ElementNotFound {
                dimension: dimension.to_string(),
                element: parent.to_string(),
            })?;
        let child_idx = dim
            .index_of(child)
            .ok_or_else(|| ModelError::ElementNotFound {
                dimension: dimension.to_string(),
                element: child.to_string(),
            })?;
        // Idempotent: an existing parent -> child edge is left untouched (no
        // duplicate, no error). The kind/weight stay as they are.
        if dim.children_of(parent_idx)?.contains(&child_idx) {
            return Ok(());
        }
        // A numeric/string parent gains a child, which converts it to a
        // consolidation (so add_child accepts it). Its stored cell is then dropped
        // (a consolidation is computed, not stored), mirroring reparent_element.
        let parent_was_leaf = matches!(
            dim.element(parent_idx)?.kind,
            ElementKind::Leaf | ElementKind::String
        );
        if parent_was_leaf {
            dim.set_kind(parent_idx, ElementKind::Consolidated)?;
        }
        // Additive: add_child appends the edge and keeps the child's other edges,
        // validating self-parent, cycle, and a non-consolidated parent.
        dim.add_child(parent_idx, child_idx, weight)?;
        if parent_was_leaf {
            next.drop_cells_for_element(d, parent_idx, true, true);
        }
        *self = next;
        Ok(())
    }

    /// Remove the single `parent -> child` consolidation edge in a dimension,
    /// keeping the child element, its stored cells, and its OTHER parent edges. The
    /// `parent` stays a consolidation even if it becomes childless (no
    /// auto-convert). If the removed edge was the child's last incoming edge it
    /// simply becomes a root. An edge-only change, so no cell remap.
    ///
    /// This is the "remove from one consolidation" operation (ADR-0036), distinct
    /// from [`Self::reparent_element`] with `None` (which detaches the child from
    /// EVERY parent) and from [`Self::delete_element`] (which removes the member).
    ///
    /// Idempotent: if the `parent -> child` edge is absent it is a no-op and
    /// returns `Ok`. Transactional: an unknown parent or child is rejected and the
    /// cube is unchanged.
    pub fn remove_child_element(
        &mut self,
        dimension: &str,
        parent: &str,
        child: &str,
    ) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension)?;
        let mut next = self.clone();
        let dim = &mut next.dimensions[d];
        let parent_idx = dim
            .index_of(parent)
            .ok_or_else(|| ModelError::ElementNotFound {
                dimension: dimension.to_string(),
                element: parent.to_string(),
            })?;
        let child_idx = dim
            .index_of(child)
            .ok_or_else(|| ModelError::ElementNotFound {
                dimension: dimension.to_string(),
                element: child.to_string(),
            })?;
        // Edge-only: drop just this one parent -> child edge. The child, its cells,
        // and its other parent edges are untouched; the parent keeps its kind.
        dim.remove_child(parent_idx, child_idx)?;
        *self = next;
        Ok(())
    }

    /// Pin `element` to the top level in a dimension (ADR-0038), so it shows as a
    /// display root regardless of its parents. A display-only marker: no rollup
    /// edge, value, or element index changes, so no cell remap. Idempotent (pinning
    /// an already-pinned or no-parent member is a no-op). Transactional: an unknown
    /// element is rejected and the cube is unchanged.
    pub fn pin_element_to_top(&mut self, dimension: &str, element: &str) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension)?;
        let mut next = self.clone();
        let element_idx =
            next.dimensions[d]
                .index_of(element)
                .ok_or_else(|| ModelError::ElementNotFound {
                    dimension: dimension.to_string(),
                    element: element.to_string(),
                })?;
        next.dimensions[d].pin_to_top(element_idx)?;
        *self = next;
        Ok(())
    }

    /// Unpin `element` from the top level in a dimension (ADR-0038). It reverts to a
    /// display root only if it has no parent. Display-only, so no cell remap.
    /// Idempotent (unpinning an unpinned member is a no-op). Transactional: an
    /// unknown element is rejected and the cube is unchanged.
    pub fn unpin_element_from_top(
        &mut self,
        dimension: &str,
        element: &str,
    ) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension)?;
        let mut next = self.clone();
        let element_idx =
            next.dimensions[d]
                .index_of(element)
                .ok_or_else(|| ModelError::ElementNotFound {
                    dimension: dimension.to_string(),
                    element: element.to_string(),
                })?;
        next.dimensions[d].unpin_from_top(element_idx)?;
        *self = next;
        Ok(())
    }

    /// Convert a dimension element's kind. A numeric/string change re-types the
    /// element's stored cells (an incompatible value is cleared, not crashed); a
    /// change to consolidated drops the element's stored leaf value (a
    /// consolidation is computed). Converting away from consolidated requires no
    /// children. Transactional: a rejected conversion leaves the cube unchanged.
    pub fn set_element_kind(
        &mut self,
        dimension: &str,
        element: &str,
        kind: ElementKind,
    ) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension)?;
        let mut next = self.clone();
        let element_idx =
            next.dimensions[d]
                .index_of(element)
                .ok_or_else(|| ModelError::ElementNotFound {
                    dimension: dimension.to_string(),
                    element: element.to_string(),
                })?;
        let previous = next.dimensions[d].set_kind(element_idx, kind)?;
        next.retype_cells_for_kind(d, element_idx, previous, kind);
        *self = next;
        Ok(())
    }

    /// Re-type or clear an element's stored cells after its kind changed (no index
    /// change). Numeric<->string moves the value to the matching store when it
    /// transfers cleanly and clears it otherwise; any change to or from
    /// consolidated clears the element's stored cells (a consolidation is computed,
    /// and a fresh leaf/string starts empty). Operates on cells whose component
    /// for `d` equals `element`.
    fn retype_cells_for_kind(
        &mut self,
        d: usize,
        element: u32,
        previous: ElementKind,
        kind: ElementKind,
    ) {
        use ElementKind::{Consolidated, Leaf, String as Str};
        match (previous, kind) {
            (Leaf, Str) => {
                // Numeric leaf -> string: a number has no faithful text cell, so the
                // incompatible stored value is cleared.
                self.drop_cells_for_element(d, element, true, false);
            }
            (Str, Leaf) => {
                // String leaf -> numeric: text has no numeric value, so clear it.
                self.drop_cells_for_element(d, element, false, true);
            }
            (Leaf, Consolidated) | (Str, Consolidated) => {
                // A consolidation holds no stored value of its own.
                self.drop_cells_for_element(d, element, true, true);
            }
            // Consolidated -> leaf/string starts with no stored value (there was
            // none), and leaf<->leaf or string<->string keeps its cells.
            _ => {}
        }
    }

    /// Drop stored cells whose component for dimension `d` equals `element`, from
    /// the numeric store (`numeric`) and/or the string store (`strings`). Used by
    /// kind conversion; no index change, so the layout is untouched.
    fn drop_cells_for_element(&mut self, d: usize, element: u32, numeric: bool, strings: bool) {
        // No index change, so the layout is unchanged; keep only cells whose
        // component for `d` differs from `element` in the selected store(s).
        let layout = self.layout.clone();
        self.rebuild_stores(layout, numeric, strings, |coord| coord[d] != element);
    }

    /// Delete a dimension element, its edges (as parent and as child), and its
    /// stored cells, then reindex the remaining elements and remap their cells.
    /// Transactional: deleting a consolidation that still has children is rejected
    /// (detach or delete the children first) and the cube is unchanged.
    pub fn delete_element(&mut self, dimension: &str, element: &str) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension)?;
        let mut next = self.clone();
        let element_idx =
            next.dimensions[d]
                .index_of(element)
                .ok_or_else(|| ModelError::ElementNotFound {
                    dimension: dimension.to_string(),
                    element: element.to_string(),
                })?;
        let (_removed, to_new) = next.dimensions[d].delete(element_idx)?;
        next.remap_cells_for_dimension(d, &to_new);
        *self = next;
        Ok(())
    }

    /// Insert a new element `name` of `kind` into a dimension at `position`
    /// (`Position::AtEnd`, or before/after an existing member), remapping cells for
    /// the index shift. Transactional: a duplicate name or an unknown anchor is
    /// rejected and the cube is unchanged.
    pub fn insert_element_at(
        &mut self,
        dimension: &str,
        name: &str,
        kind: ElementKind,
        position: Position,
    ) -> Result<(), ModelError> {
        let d = self.dimension_index(dimension)?;
        let mut next = self.clone();
        let insert_index = match &position {
            Position::AtEnd => next.dimensions[d].len(),
            Position::Before(anchor) | Position::After(anchor) => {
                let anchor_idx = next.dimensions[d].index_of(anchor).ok_or_else(|| {
                    ModelError::ElementNotFound {
                        dimension: dimension.to_string(),
                        element: anchor.clone(),
                    }
                })?;
                match position {
                    Position::Before(_) => anchor_idx,
                    Position::After(_) => anchor_idx + 1,
                    Position::AtEnd => unreachable!(),
                }
            }
        };
        let (_at, to_new) = next.dimensions[d].insert_at(name, kind, insert_index)?;
        next.remap_cells_for_dimension(d, &to_new);
        *self = next;
        Ok(())
    }

    /// Apply element/edge additions in place (not transactional; callers go
    /// through [`extend_schema`](Self::extend_schema), which stages on a clone).
    fn apply_growth(
        &mut self,
        elements: &[ElementSpec],
        edges: &[EdgeSpec],
    ) -> Result<usize, ModelError> {
        let mut added = 0;
        for spec in elements {
            let d = self.dimension_index(&spec.dimension)?;
            let dim = &mut self.dimensions[d];
            match dim.index_of(&spec.name) {
                Some(existing) => {
                    if dim.element(existing)?.kind != spec.kind {
                        return Err(ModelError::ElementKindConflict {
                            dimension: spec.dimension.clone(),
                            element: spec.name.clone(),
                        });
                    }
                }
                None => {
                    match spec.kind {
                        ElementKind::Leaf => dim.add_leaf(spec.name.clone()),
                        ElementKind::String => dim.add_string(spec.name.clone()),
                        ElementKind::Consolidated => dim.add_consolidated(spec.name.clone()),
                    };
                    added += 1;
                }
            }
        }
        for edge in edges {
            let d = self.dimension_index(&edge.dimension)?;
            let dim = &mut self.dimensions[d];
            let parent = dim
                .index_of(&edge.parent)
                .ok_or_else(|| ModelError::ElementNotFound {
                    dimension: edge.dimension.clone(),
                    element: edge.parent.clone(),
                })?;
            let child = dim
                .index_of(&edge.child)
                .ok_or_else(|| ModelError::ElementNotFound {
                    dimension: edge.dimension.clone(),
                    element: edge.child.clone(),
                })?;
            // Idempotent: an edge that already exists is a no-op, but re-declaring
            // it with a different weight is a conflict (silently keeping the old
            // weight would corrupt rollups).
            if let Some(&(_, _, w)) = dim
                .edges()
                .iter()
                .find(|&&(p, c, _)| p == parent && c == child)
            {
                if w != edge.weight {
                    return Err(ModelError::EdgeWeightConflict {
                        dimension: edge.dimension.clone(),
                        parent: edge.parent.clone(),
                        child: edge.child.clone(),
                    });
                }
                continue;
            }
            dim.add_child(parent, child, edge.weight)?;
        }
        self.relayout();
        Ok(added)
    }

    /// The position of a dimension by name.
    fn dimension_index(&self, name: &str) -> Result<usize, ModelError> {
        self.dimensions
            .iter()
            .position(|d| d.name() == name)
            .ok_or_else(|| ModelError::UnknownDimension {
                cube: self.name.clone(),
                dimension: name.to_string(),
            })
    }

    /// Recompute the coordinate packing and re-pack all cells if a dimension's
    /// bit-width grew (so existing packed keys would otherwise be misaligned).
    /// A no-op when the layout is unchanged (the common case for appends within
    /// the same bit-width).
    fn relayout(&mut self) {
        let new_layout = Layout::new(&self.dimensions);
        if new_layout == self.layout {
            return;
        }
        // Re-pack both stores under the wider layout; coordinates are unchanged.
        self.rebuild_stores(new_layout, true, true, |_coord| true);
    }

    /// Number of populated numeric leaf cells.
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Number of populated string cells.
    pub fn string_cell_count(&self) -> usize {
        self.string_cells.len()
    }

    /// Iterate populated numeric leaf cells as `(coordinate, value)`.
    pub fn cell_entries(&self) -> impl Iterator<Item = (Vec<u32>, Fixed)> + '_ {
        self.cells
            .entries(&self.layout, self.rank())
            .map(|(coord, &value)| (coord, value))
    }

    /// Iterate populated string cells as `(coordinate, value)`.
    pub fn string_cell_entries(&self) -> impl Iterator<Item = (Vec<u32>, &str)> + '_ {
        let pool = &self.string_pool;
        self.string_cells
            .entries(&self.layout, self.rank())
            .map(move |(coord, &id)| (coord, pool.get(id)))
    }

    fn check_coord(&self, coord: &[u32]) -> Result<(), ModelError> {
        if coord.len() != self.rank() {
            return Err(ModelError::RankMismatch {
                expected: self.rank(),
                got: coord.len(),
            });
        }
        for (d, &idx) in coord.iter().enumerate() {
            self.dimensions[d].element(idx)?;
        }
        Ok(())
    }

    /// Write a numeric value to a leaf cell. Every coordinate element must be a
    /// numeric leaf. Writing [`Fixed::ZERO`] clears the cell, keeping the store
    /// sparse.
    pub fn set_leaf(&mut self, coord: &[u32], value: Fixed) -> Result<(), ModelError> {
        self.check_coord(coord)?;
        for (d, &idx) in coord.iter().enumerate() {
            let element = self.dimensions[d].element(idx)?;
            match element.kind {
                ElementKind::Leaf => {}
                ElementKind::String => {
                    return Err(ModelError::CellTypeMismatch {
                        dimension: self.dimensions[d].name().to_string(),
                        element: element.name.clone(),
                    })
                }
                ElementKind::Consolidated => {
                    return Err(ModelError::WriteToNonLeaf {
                        dimension: self.dimensions[d].name().to_string(),
                        element: element.name.clone(),
                    })
                }
            }
        }
        if value.is_zero() {
            self.cells.clear(&self.layout, coord);
        } else {
            self.cells.put(&self.layout, coord, value);
        }
        Ok(())
    }

    /// Read a numeric leaf cell directly (zero if unpopulated).
    pub fn get_leaf(&self, coord: &[u32]) -> Result<Fixed, ModelError> {
        self.check_coord(coord)?;
        Ok(self.leaf_value(coord))
    }

    /// Direct numeric cell lookup, assuming `coord` is already validated.
    fn leaf_value(&self, coord: &[u32]) -> Fixed {
        self.cells
            .get(&self.layout, coord)
            .copied()
            .unwrap_or(Fixed::ZERO)
    }

    /// Write a string value to a string cell. Every coordinate element must be a
    /// leaf, and at least one must be a string element. Writing an empty string
    /// clears the cell.
    pub fn set_string(&mut self, coord: &[u32], value: &str) -> Result<(), ModelError> {
        self.check_coord(coord)?;
        let mut addresses_string = false;
        for (d, &idx) in coord.iter().enumerate() {
            let element = self.dimensions[d].element(idx)?;
            match element.kind {
                ElementKind::String => addresses_string = true,
                ElementKind::Leaf => {}
                ElementKind::Consolidated => {
                    return Err(ModelError::WriteToNonLeaf {
                        dimension: self.dimensions[d].name().to_string(),
                        element: element.name.clone(),
                    })
                }
            }
        }
        if !addresses_string {
            return Err(ModelError::StringCellRequiresStringElement {
                cube: self.name.clone(),
            });
        }
        if value.is_empty() {
            self.string_cells.clear(&self.layout, coord);
        } else {
            let id = self.string_pool.intern(value);
            self.string_cells.put(&self.layout, coord, id);
        }
        Ok(())
    }

    /// Read a string cell, if populated.
    pub fn get_string(&self, coord: &[u32]) -> Result<Option<&str>, ModelError> {
        self.check_coord(coord)?;
        Ok(self
            .string_cells
            .get(&self.layout, coord)
            .map(|&id| self.string_pool.get(id)))
    }

    /// Read a value at any coordinate, consolidating across consolidated elements.
    ///
    /// Exact and deterministic: contributions are summed in a 128-bit accumulator
    /// over exact integer values, so the result is independent of iteration order.
    /// Only numeric cells contribute; string cells never aggregate.
    pub fn get(&self, coord: &[u32]) -> Result<Fixed, ModelError> {
        self.check_coord(coord)?;

        let mut per_dim: Vec<HashMap<u32, i64>> = Vec::with_capacity(self.rank());
        let mut all_leaf = true;
        for (d, &idx) in coord.iter().enumerate() {
            if self.dimensions[d].element(idx)?.kind != ElementKind::Leaf {
                all_leaf = false;
            }
            per_dim.push(self.dimensions[d].leaf_weights(idx)?.into_iter().collect());
        }

        // Fast path: a pure numeric-leaf cell is a direct lookup.
        if all_leaf {
            return Ok(self.leaf_value(coord));
        }

        // Sparse consolidation: include each populated cell whose every component
        // is a leaf-descendant of the corresponding query element. Summation is
        // exact and order-independent.
        let acc: i128 = match &self.cells {
            CellStore::Narrow(m) => m
                .iter()
                .filter_map(|(&key, value)| {
                    cell_weight(&per_dim, |d| self.layout.component(key, d))
                        .map(|w| w * i128::from(value.to_scaled()))
                })
                .sum(),
            CellStore::Wide(m) => m
                .iter()
                .filter_map(|(key, value)| {
                    cell_weight(&per_dim, |d| key[d]).map(|w| w * i128::from(value.to_scaled()))
                })
                .sum(),
        };
        i64::try_from(acc)
            .map(Fixed::from_scaled)
            .map_err(|_| ModelError::Overflow)
    }

    /// Consolidate a coordinate, sourcing each contributing leaf's value through
    /// `leaf_value` instead of reading the stored cell store directly.
    ///
    /// This is the rule-aware consolidation entry point (the seam the calc engine
    /// uses): with a stored-cell source it equals [`get`](Self::get) exactly; with
    /// a rule overlay it folds rule-derived leaf values into the rollup through
    /// the same exact weighted `i128` algebra. An all-leaf coordinate
    /// short-circuits to a single `leaf_value` call (no enumeration). A
    /// consolidated coordinate enumerates its contributing leaf coordinates (the
    /// cartesian product of each dimension's net-weighted leaves) and sums
    /// `weight * value` exactly. The error type is generic so a caller can thread
    /// either a `ModelError` or a richer calc error through the closure.
    ///
    /// The enumeration is dense over the consolidation's leaf space, so callers
    /// that can be sparse (feeder-driven) should restrict the queried region; the
    /// no-rules read path stays on the sparse [`get`](Self::get).
    pub fn consolidate_with<E, F>(&self, coord: &[u32], leaf_value: F) -> Result<Fixed, E>
    where
        E: From<ModelError>,
        F: Fn(&[u32]) -> Result<Fixed, E>,
    {
        self.check_coord(coord).map_err(E::from)?;

        let mut per_dim: Vec<Vec<(u32, i64)>> = Vec::with_capacity(self.rank());
        let mut all_leaf = true;
        for (d, &idx) in coord.iter().enumerate() {
            if self.dimensions[d].element(idx).map_err(E::from)?.kind != ElementKind::Leaf {
                all_leaf = false;
            }
            per_dim.push(self.dimensions[d].leaf_weights(idx).map_err(E::from)?);
        }

        // Fast path: a pure leaf coordinate is a single source lookup.
        if all_leaf {
            return leaf_value(coord);
        }

        // Dense enumeration of the contributing leaf coordinates: a mixed-radix
        // walk over the per-dimension weighted-leaf lists. Any empty list (a
        // net-zero rollup) means no contributing leaves.
        let total: usize = per_dim.iter().map(|w| w.len()).product();
        if total == 0 {
            return Ok(Fixed::ZERO);
        }
        let rank = self.rank();
        let mut combo = vec![0u32; rank];
        let mut acc: i128 = 0;
        for n in 0..total {
            let mut rem = n;
            let mut weight: i128 = 1;
            for d in 0..rank {
                let len = per_dim[d].len();
                let (leaf, w) = per_dim[d][rem % len];
                rem /= len;
                combo[d] = leaf;
                weight *= i128::from(w);
            }
            acc += weight * i128::from(leaf_value(&combo)?.to_scaled());
        }
        let scaled = i64::try_from(acc).map_err(|_| E::from(ModelError::Overflow))?;
        Ok(Fixed::from_scaled(scaled))
    }

    /// Consolidate a coordinate over a SPARSE union of the stored populated cells
    /// and the supplied `fed` leaf coordinates, sourcing each value through
    /// `leaf_value`. This is the feeder-driven sparse-skip path (ADR-0005): the
    /// `fed` set names rule-derived leaves that are not in the stored cell store,
    /// so a rollup includes them without enumerating the dense leaf space.
    ///
    /// Each contributing coordinate is counted exactly once (a coordinate that is
    /// both stored and fed is summed a single time, with its `leaf_value`), so a
    /// rule that overrides a stored leaf is not double counted. The result equals
    /// the dense [`consolidate_with`](Self::consolidate_with) exactly when `fed`
    /// covers every non-zero rule-derived leaf; with an incomplete `fed` set it
    /// under-counts (which feeder validation flags), and the dense path is always
    /// the correct fallback. The sum is order-independent (i128), so the result
    /// is deterministic regardless of iteration order.
    pub fn consolidate_fed<E, F>(
        &self,
        coord: &[u32],
        fed: &[Box<[u32]>],
        leaf_value: F,
    ) -> Result<Fixed, E>
    where
        E: From<ModelError>,
        F: Fn(&[u32]) -> Result<Fixed, E>,
    {
        self.check_coord(coord).map_err(E::from)?;

        let mut per_dim: Vec<HashMap<u32, i64>> = Vec::with_capacity(self.rank());
        let mut all_leaf = true;
        for (d, &idx) in coord.iter().enumerate() {
            if self.dimensions[d].element(idx).map_err(E::from)?.kind != ElementKind::Leaf {
                all_leaf = false;
            }
            per_dim.push(
                self.dimensions[d]
                    .leaf_weights(idx)
                    .map_err(E::from)?
                    .into_iter()
                    .collect(),
            );
        }
        if all_leaf {
            return leaf_value(coord);
        }

        let mut seen: HashSet<Box<[u32]>> = HashSet::new();
        let mut acc: i128 = 0;
        // Stored populated cells whose every component is a leaf-descendant of the
        // query element. The value comes through `leaf_value` (so a rule override
        // of a stored leaf uses the rule value, not the stored one).
        for (cell_coord, _) in self.cell_entries() {
            if let Some(weight) = cell_weight(&per_dim, |d| cell_coord[d]) {
                let key: Box<[u32]> = cell_coord.clone().into_boxed_slice();
                if seen.insert(key) {
                    acc += weight * i128::from(leaf_value(&cell_coord)?.to_scaled());
                }
            }
        }
        // Fed (rule-derived) leaves in the region, not already counted.
        for fc in fed {
            if let Some(weight) = cell_weight(&per_dim, |d| fc[d]) {
                if seen.insert(fc.clone()) {
                    acc += weight * i128::from(leaf_value(fc)?.to_scaled());
                }
            }
        }
        let scaled = i64::try_from(acc).map_err(|_| E::from(ModelError::Overflow))?;
        Ok(Fixed::from_scaled(scaled))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dimension;
    use epiphany_determinism::DeterministicRng;

    fn fix(n: i32) -> Fixed {
        Fixed::from(n)
    }

    /// A dimension with `n` leaves and one consolidated "Total" summing them.
    /// Returns `(dimension, total_index, leaf_indices)`.
    fn sum_dim(name: &str, n: u32) -> (Dimension, u32, Vec<u32>) {
        let mut d = Dimension::new(name);
        let leaves: Vec<u32> = (0..n).map(|i| d.add_leaf(format!("{name}_l{i}"))).collect();
        let total = d.add_consolidated(format!("{name}_Total"));
        for &leaf in &leaves {
            d.add_child(total, leaf, 1).unwrap();
        }
        (d, total, leaves)
    }

    #[test]
    fn build_constructs_dimensions_elements_and_consolidations() {
        let cube = Cube::build(
            "Sales",
            &[
                DimensionDef {
                    name: "Region".into(),
                    elements: vec![
                        ("North".into(), ElementKind::Leaf),
                        ("South".into(), ElementKind::Leaf),
                        ("Total".into(), ElementKind::Consolidated),
                    ],
                    edges: vec![
                        ("Total".into(), "North".into(), 1),
                        ("Total".into(), "South".into(), 1),
                    ],
                    ..Default::default()
                },
                DimensionDef {
                    name: "Measure".into(),
                    elements: vec![("Amount".into(), ElementKind::Leaf)],
                    edges: vec![],
                    ..Default::default()
                },
            ],
        )
        .unwrap();
        assert_eq!(cube.rank(), 2);
        assert_eq!(cube.dimension(0).name(), "Region");
        assert_eq!(cube.dimension(0).len(), 3);
        assert_eq!(cube.dimension(1).len(), 1);
        // The consolidation is live: writing leaves rolls up under Total.
        let mut cube = cube;
        let north = cube.dimension(0).index_of("North").unwrap();
        let south = cube.dimension(0).index_of("South").unwrap();
        let amount = cube.dimension(1).index_of("Amount").unwrap();
        cube.set_leaf(&[north, amount], fix(10)).unwrap();
        cube.set_leaf(&[south, amount], fix(15)).unwrap();
        let total = cube.dimension(0).index_of("Total").unwrap();
        assert_eq!(cube.get(&[total, amount]).unwrap(), fix(25));
    }

    #[test]
    fn build_rejects_empty_duplicate_and_bad_edges() {
        assert_eq!(Cube::build("X", &[]).unwrap_err(), ModelError::EmptyCube);

        let dup = Cube::build(
            "X",
            &[
                DimensionDef {
                    name: "D".into(),
                    elements: vec![("a".into(), ElementKind::Leaf)],
                    edges: vec![],
                    ..Default::default()
                },
                DimensionDef {
                    name: "D".into(),
                    elements: vec![("b".into(), ElementKind::Leaf)],
                    edges: vec![],
                    ..Default::default()
                },
            ],
        )
        .unwrap_err();
        assert!(matches!(dup, ModelError::DuplicateDimension { .. }));

        // A leaf parent cannot have children.
        let bad_parent = Cube::build(
            "X",
            &[DimensionDef {
                name: "D".into(),
                elements: vec![
                    ("p".into(), ElementKind::Leaf),
                    ("c".into(), ElementKind::Leaf),
                ],
                edges: vec![("p".into(), "c".into(), 1)],
                ..Default::default()
            }],
        )
        .unwrap_err();
        assert!(matches!(
            bad_parent,
            ModelError::ParentNotConsolidated { .. }
        ));
    }

    #[test]
    fn define_and_set_attributes() {
        let mut cube = Cube::build(
            "Sales",
            &[DimensionDef {
                name: "Region".into(),
                elements: vec![("North".into(), ElementKind::Leaf)],
                edges: vec![],
                ..Default::default()
            }],
        )
        .unwrap();

        cube.define_attribute("Region", "Currency", AttributeKind::Text)
            .unwrap();
        // Idempotent for the same kind, conflict for a different kind.
        cube.define_attribute("Region", "Currency", AttributeKind::Text)
            .unwrap();
        assert!(matches!(
            cube.define_attribute("Region", "Currency", AttributeKind::Numeric),
            Err(ModelError::AttributeKindConflict { .. })
        ));

        cube.set_attribute_values(
            "Region",
            "Currency",
            &[("North".into(), AttributeValue::Text("USD".into()))],
        )
        .unwrap();
        let north = cube.dimension(0).index_of("North").unwrap();
        assert_eq!(
            cube.dimension(0).attribute(north, "Currency"),
            Some(&AttributeValue::Text("USD".into()))
        );

        // An unknown element rolls the whole change back (transactional).
        let before = cube.dimension(0).attribute_values().len();
        let err = cube
            .set_attribute_values(
                "Region",
                "Currency",
                &[
                    ("North".into(), AttributeValue::Text("EUR".into())),
                    ("Nowhere".into(), AttributeValue::Text("GBP".into())),
                ],
            )
            .unwrap_err();
        assert!(matches!(err, ModelError::ElementNotFound { .. }));
        assert_eq!(cube.dimension(0).attribute_values().len(), before);
        assert_eq!(
            cube.dimension(0).attribute(north, "Currency"),
            Some(&AttributeValue::Text("USD".into())),
            "the rolled-back batch left the prior value intact"
        );
    }

    #[test]
    fn leaf_write_read_and_sparsity() {
        let (region, _total, r) = sum_dim("Region", 3);
        let mut cube = Cube::new("Sales", vec![region]).unwrap();
        assert_eq!(cube.cell_count(), 0);

        cube.set_leaf(&[r[0]], fix(10)).unwrap();
        cube.set_leaf(&[r[1]], fix(20)).unwrap();
        assert_eq!(cube.get_leaf(&[r[0]]).unwrap(), fix(10));
        assert_eq!(cube.get_leaf(&[r[2]]).unwrap(), Fixed::ZERO);
        assert_eq!(cube.cell_count(), 2);

        // Writing zero clears the cell.
        cube.set_leaf(&[r[0]], Fixed::ZERO).unwrap();
        assert_eq!(cube.cell_count(), 1);
        assert_eq!(cube.get_leaf(&[r[0]]).unwrap(), Fixed::ZERO);
    }

    #[test]
    fn consolidation_two_dimensions() {
        let (region, region_total, r) = sum_dim("Region", 2);
        let (period, period_total, p) = sum_dim("Period", 2);
        let mut cube = Cube::new("Sales", vec![region, period]).unwrap();
        cube.set_leaf(&[r[0], p[0]], fix(10)).unwrap();
        cube.set_leaf(&[r[1], p[0]], fix(20)).unwrap();
        cube.set_leaf(&[r[0], p[1]], fix(30)).unwrap();
        cube.set_leaf(&[r[1], p[1]], fix(40)).unwrap();

        assert_eq!(cube.get(&[region_total, p[0]]).unwrap(), fix(30));
        assert_eq!(cube.get(&[r[0], period_total]).unwrap(), fix(40));
        assert_eq!(cube.get(&[region_total, period_total]).unwrap(), fix(100));
        assert_eq!(cube.get(&[r[0], p[0]]).unwrap(), fix(10));
    }

    #[test]
    fn extend_schema_grows_dimension_and_repacks_cells() {
        // Dim A: 2 leaves (1 bit). Dim B: 2 leaves, sitting at bit offset 1.
        let mut a = Dimension::new("A");
        let a0 = a.add_leaf("a0");
        a.add_leaf("a1");
        let mut b = Dimension::new("B");
        let b0 = b.add_leaf("b0");
        b.add_leaf("b1");
        let mut cube = Cube::new("C", vec![a, b]).unwrap();
        cube.set_leaf(&[a0, b0], fix(42)).unwrap();

        // Add 3 leaves to A -> 5 elements -> 3 bits, shifting B's bit offset and
        // forcing a re-pack of the existing cell's key.
        let specs: Vec<ElementSpec> = (0..3)
            .map(|i| ElementSpec {
                dimension: "A".into(),
                name: format!("a{}", i + 2),
                kind: ElementKind::Leaf,
            })
            .collect();
        assert_eq!(cube.extend_schema(&specs, &[]).unwrap(), 3);

        // The pre-existing cell survived the re-pack.
        assert_eq!(cube.get_leaf(&[a0, b0]).unwrap(), fix(42));
        // A newly-added element is addressable and writable.
        let a4 = cube.dimension(0).resolve("a4").unwrap();
        cube.set_leaf(&[a4, b0], fix(7)).unwrap();
        assert_eq!(cube.get_leaf(&[a4, b0]).unwrap(), fix(7));
        assert_eq!(cube.get_leaf(&[a0, b0]).unwrap(), fix(42));

        // Idempotent: re-adding the same elements creates nothing new.
        assert_eq!(cube.extend_schema(&specs, &[]).unwrap(), 0);
    }

    #[test]
    fn extend_schema_adds_consolidation_and_is_edge_idempotent() {
        let mut region = Dimension::new("Region");
        region.add_leaf("North");
        let mut cube = Cube::new("Sales", vec![region]).unwrap();
        let north = cube.dimension(0).resolve("North").unwrap();
        cube.set_leaf(&[north], fix(100)).unwrap();

        let els = vec![
            ElementSpec {
                dimension: "Region".into(),
                name: "South".into(),
                kind: ElementKind::Leaf,
            },
            ElementSpec {
                dimension: "Region".into(),
                name: "Total".into(),
                kind: ElementKind::Consolidated,
            },
        ];
        let edges = vec![
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
        ];
        cube.extend_schema(&els, &edges).unwrap();
        let south = cube.dimension(0).resolve("South").unwrap();
        let total = cube.dimension(0).resolve("Total").unwrap();
        cube.set_leaf(&[south], fix(50)).unwrap();
        assert_eq!(cube.get(&[total]).unwrap(), fix(150));

        // Re-applying the same elements and edges must not double-count.
        cube.extend_schema(&els, &edges).unwrap();
        assert_eq!(cube.get(&[total]).unwrap(), fix(150));
    }

    #[test]
    fn extend_schema_rejects_kind_conflict_and_unknown_dimension() {
        let mut region = Dimension::new("Region");
        region.add_leaf("North");
        let mut cube = Cube::new("Sales", vec![region]).unwrap();
        let conflict = cube.extend_schema(
            &[ElementSpec {
                dimension: "Region".into(),
                name: "North".into(),
                kind: ElementKind::Consolidated,
            }],
            &[],
        );
        assert!(matches!(
            conflict,
            Err(ModelError::ElementKindConflict { .. })
        ));
        let bad = cube.extend_schema(
            &[ElementSpec {
                dimension: "Nope".into(),
                name: "X".into(),
                kind: ElementKind::Leaf,
            }],
            &[],
        );
        assert!(matches!(bad, Err(ModelError::UnknownDimension { .. })));
    }

    #[test]
    fn extend_schema_rejects_edge_weight_conflict() {
        let mut region = Dimension::new("Region");
        region.add_leaf("North");
        region.add_consolidated("Total");
        let mut cube = Cube::new("Sales", vec![region]).unwrap();
        let edge = |w: i64| EdgeSpec {
            dimension: "Region".into(),
            parent: "Total".into(),
            child: "North".into(),
            weight: w,
        };
        cube.extend_schema(&[], &[edge(1)]).unwrap();
        // Re-adding with the same weight is idempotent.
        cube.extend_schema(&[], &[edge(1)]).unwrap();
        // A different weight is a conflict, not a silent discard.
        assert!(matches!(
            cube.extend_schema(&[], &[edge(-1)]),
            Err(ModelError::EdgeWeightConflict { .. })
        ));
    }

    #[test]
    fn extend_schema_is_transactional_on_rejection() {
        let mut region = Dimension::new("Region");
        region.add_leaf("North");
        let mut cube = Cube::new("Sales", vec![region]).unwrap();
        let before = cube.dimension(0).len();
        // A batch whose first element is valid but second conflicts (North exists
        // as a leaf) must leave the dimension entirely unchanged.
        let err = cube.extend_schema(
            &[
                ElementSpec {
                    dimension: "Region".into(),
                    name: "South".into(),
                    kind: ElementKind::Leaf,
                },
                ElementSpec {
                    dimension: "Region".into(),
                    name: "North".into(),
                    kind: ElementKind::Consolidated,
                },
            ],
            &[],
        );
        assert!(matches!(err, Err(ModelError::ElementKindConflict { .. })));
        assert_eq!(cube.dimension(0).len(), before, "no partial mutation");
        assert!(
            cube.dimension(0).resolve("South").is_none(),
            "South not added"
        );
    }

    #[test]
    fn weighted_consolidation_variance() {
        let mut version = Dimension::new("Version");
        let actual = version.add_leaf("Actual");
        let budget = version.add_leaf("Budget");
        let variance = version.add_consolidated("Variance");
        version.add_child(variance, actual, 1).unwrap();
        version.add_child(variance, budget, -1).unwrap();

        let mut cube = Cube::new("PnL", vec![version]).unwrap();
        cube.set_leaf(&[actual], fix(100)).unwrap();
        cube.set_leaf(&[budget], fix(80)).unwrap();
        assert_eq!(cube.get(&[variance]).unwrap(), fix(20));
    }

    #[test]
    fn alternate_rollups() {
        let mut d = Dimension::new("Region");
        let north = d.add_leaf("North");
        let south = d.add_leaf("South");
        let east = d.add_leaf("East");
        let total = d.add_consolidated("Total");
        let coastal = d.add_consolidated("Coastal");
        for leaf in [north, south, east] {
            d.add_child(total, leaf, 1).unwrap();
        }
        d.add_child(coastal, north, 1).unwrap();
        d.add_child(coastal, east, 1).unwrap();

        let mut cube = Cube::new("Sales", vec![d]).unwrap();
        cube.set_leaf(&[north], fix(1)).unwrap();
        cube.set_leaf(&[south], fix(2)).unwrap();
        cube.set_leaf(&[east], fix(3)).unwrap();
        assert_eq!(cube.get(&[total]).unwrap(), fix(6));
        assert_eq!(cube.get(&[coastal]).unwrap(), fix(4));
    }

    #[test]
    fn diamond_rollup_counts_a_leaf_once() {
        // Total rolls up East, South, North directly AND East again via Coastal
        // (Total -> Coastal -> East). East reaches Total by two paths but must be
        // counted ONCE (ADR-0039), so Total = 10 + 5 + 3 = 18, NOT 28.
        let mut d = Dimension::new("Region");
        let east = d.add_leaf("East");
        let south = d.add_leaf("South");
        let north = d.add_leaf("North");
        let total = d.add_consolidated("Total");
        let coastal = d.add_consolidated("Coastal");
        d.add_child(total, east, 1).unwrap();
        d.add_child(total, south, 1).unwrap();
        d.add_child(total, north, 1).unwrap();
        d.add_child(total, coastal, 1).unwrap();
        d.add_child(coastal, east, 1).unwrap();

        let mut cube = Cube::new("Sales", vec![d]).unwrap();
        cube.set_leaf(&[east], fix(10)).unwrap();
        cube.set_leaf(&[south], fix(5)).unwrap();
        cube.set_leaf(&[north], fix(3)).unwrap();
        assert_eq!(cube.get(&[total]).unwrap(), fix(18));
    }

    #[test]
    fn write_to_consolidated_is_rejected() {
        let (region, region_total, _r) = sum_dim("Region", 2);
        let mut cube = Cube::new("Sales", vec![region]).unwrap();
        assert!(matches!(
            cube.set_leaf(&[region_total], fix(5)).unwrap_err(),
            ModelError::WriteToNonLeaf { .. }
        ));
    }

    #[test]
    fn rank_mismatch_is_rejected() {
        let (region, _total, _r) = sum_dim("Region", 2);
        let cube = Cube::new("Sales", vec![region]).unwrap();
        assert!(matches!(
            cube.get(&[0, 0]).unwrap_err(),
            ModelError::RankMismatch { .. }
        ));
    }

    #[test]
    fn cycles_are_rejected() {
        let mut d = Dimension::new("D");
        let a = d.add_consolidated("A");
        let b = d.add_consolidated("B");
        let leaf = d.add_leaf("L");
        d.add_child(a, b, 1).unwrap();
        d.add_child(b, leaf, 1).unwrap();
        // a -> b -> L; adding b -> a would close a cycle.
        assert!(matches!(
            d.add_child(b, a, 1).unwrap_err(),
            ModelError::CycleDetected { .. }
        ));
    }

    #[test]
    fn sparse_consolidation_matches_bruteforce_randomized() {
        let mut rng = DeterministicRng::new(2024);
        for _ in 0..200 {
            let nr = 1 + (rng.next_u64() % 5) as u32;
            let np = 1 + (rng.next_u64() % 5) as u32;
            let (region, region_total, r) = sum_dim("Region", nr);
            let (period, period_total, p) = sum_dim("Period", np);
            let mut cube = Cube::new("C", vec![region, period]).unwrap();

            let mut expected_total: i64 = 0;
            for &ri in &r {
                for &pj in &p {
                    if rng.next_u64().is_multiple_of(2) {
                        let v = (rng.next_u64() % 1000) as i32;
                        cube.set_leaf(&[ri, pj], fix(v)).unwrap();
                        expected_total += i64::from(v);
                    }
                }
            }
            assert_eq!(
                cube.get(&[region_total, period_total]).unwrap(),
                Fixed::from_int(expected_total).unwrap()
            );
        }
    }

    #[test]
    fn consolidation_is_deterministic_across_runs() {
        let build = || {
            let (region, region_total, r) = sum_dim("Region", 3);
            let mut cube = Cube::new("C", vec![region]).unwrap();
            cube.set_leaf(&[r[0]], fix(7)).unwrap();
            cube.set_leaf(&[r[2]], fix(5)).unwrap();
            cube.get(&[region_total]).unwrap()
        };
        assert_eq!(build(), build());
        assert_eq!(build(), fix(12));
    }

    #[test]
    fn small_cube_uses_a_narrow_key() {
        let (region, _t, _r) = sum_dim("Region", 5);
        let (period, _t2, _p) = sum_dim("Period", 12);
        let cube = Cube::new("C", vec![region, period]).unwrap();
        assert!(cube.layout.narrow, "small cubes should pack into a u64 key");
        assert!(matches!(cube.cells, CellStore::Narrow(_)));
    }

    #[test]
    fn wide_cube_falls_back_to_boxed_keys() {
        // 65 single-leaf dimensions need 65 bits, past the 64-bit narrow ceiling.
        let dims: Vec<Dimension> = (0..65)
            .map(|i| {
                let mut d = Dimension::new(format!("D{i}"));
                d.add_leaf("only");
                d
            })
            .collect();
        let mut cube = Cube::new("Wide", dims).unwrap();
        assert!(!cube.layout.narrow);
        assert!(matches!(cube.cells, CellStore::Wide(_)));

        let coord = vec![0u32; 65];
        cube.set_leaf(&coord, fix(7)).unwrap();
        assert_eq!(cube.get_leaf(&coord).unwrap(), fix(7));
        assert_eq!(cube.cell_count(), 1);
        // Round-trips through the entry iterator too.
        let entries: Vec<_> = cube.cell_entries().collect();
        assert_eq!(entries, vec![(coord, fix(7))]);
    }

    #[test]
    fn layout_packs_and_unpacks() {
        let coord = [3u32, 9u32];
        let layout = Layout {
            offsets: vec![0, 4],
            masks: vec![0xF, 0xF],
            narrow: true,
        };
        let key = layout.pack(&coord);
        assert_eq!(layout.component(key, 0), 3);
        assert_eq!(layout.component(key, 1), 9);
        assert_eq!(layout.unpack(key, 2), vec![3, 9]);
    }

    /// A Region x Measure cube where Measure has a numeric leaf, a string leaf,
    /// and a numeric Total over the numerics.
    fn string_cube() -> (Cube, u32, u32, u32, u32) {
        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        let south = region.add_leaf("South");
        region.add_consolidated("Total");
        region.add_child(2, north, 1).unwrap();
        region.add_child(2, south, 1).unwrap();

        let mut measure = Dimension::new("Measure");
        let sales = measure.add_leaf("Sales");
        let comment = measure.add_string("Comment");

        let cube = Cube::new("Sales", vec![region, measure]).unwrap();
        (cube, north, south, sales, comment)
    }

    #[test]
    fn string_cells_write_read_and_intern() {
        let (mut cube, north, south, _sales, comment) = string_cube();
        cube.set_string(&[north, comment], "ok").unwrap();
        cube.set_string(&[south, comment], "ok").unwrap(); // same text -> interned once

        assert_eq!(cube.get_string(&[north, comment]).unwrap(), Some("ok"));
        assert_eq!(cube.get_string(&[south, comment]).unwrap(), Some("ok"));
        assert_eq!(cube.string_cell_count(), 2);
        assert_eq!(
            cube.string_pool.by_id.len(),
            1,
            "equal strings share one id"
        );

        // An empty write clears the string cell.
        cube.set_string(&[north, comment], "").unwrap();
        assert_eq!(cube.get_string(&[north, comment]).unwrap(), None);
        assert_eq!(cube.string_cell_count(), 1);
    }

    #[test]
    fn numeric_write_to_string_leaf_is_rejected() {
        let (mut cube, north, _south, _sales, comment) = string_cube();
        assert!(matches!(
            cube.set_leaf(&[north, comment], fix(1)).unwrap_err(),
            ModelError::CellTypeMismatch { .. }
        ));
    }

    #[test]
    fn string_write_to_numeric_coordinate_is_rejected() {
        let (mut cube, north, _south, sales, _comment) = string_cube();
        assert!(matches!(
            cube.set_string(&[north, sales], "x").unwrap_err(),
            ModelError::StringCellRequiresStringElement { .. }
        ));
    }

    #[test]
    fn string_cells_do_not_affect_consolidation() {
        let (mut cube, north, south, sales, comment) = string_cube();
        cube.set_leaf(&[north, sales], fix(100)).unwrap();
        cube.set_leaf(&[south, sales], fix(50)).unwrap();
        cube.set_string(&[north, comment], "note").unwrap();

        let region_total = cube.dimension(0).index_of("Total").unwrap();
        // Total Sales = 150; the string cell contributes nothing.
        assert_eq!(cube.get(&[region_total, sales]).unwrap(), fix(150));
        // A numeric read over the string measure is zero (no numeric cells there).
        assert_eq!(cube.get(&[region_total, comment]).unwrap(), Fixed::ZERO);
    }

    #[test]
    fn consolidate_with_stored_source_equals_get() {
        let (region, region_total, r) = sum_dim("Region", 3);
        let (period, period_total, p) = sum_dim("Period", 2);
        let mut cube = Cube::new("Sales", vec![region, period]).unwrap();
        cube.set_leaf(&[r[0], p[0]], fix(10)).unwrap();
        cube.set_leaf(&[r[1], p[0]], fix(20)).unwrap();
        cube.set_leaf(&[r[0], p[1]], fix(30)).unwrap();
        let coords = [
            [r[0], p[0]],
            [r[2], p[1]],
            [region_total, p[0]],
            [r[0], period_total],
            [region_total, period_total],
        ];
        for c in coords {
            let via = cube
                .consolidate_with::<ModelError, _>(&c, |lc| Ok(cube.leaf_value(lc)))
                .unwrap();
            assert_eq!(via, cube.get(&c).unwrap(), "mismatch at {c:?}");
        }
    }

    #[test]
    fn consolidate_with_fast_path_does_not_enumerate() {
        let (region, region_total, r) = sum_dim("Region", 3);
        let cube = Cube::new("Sales", vec![region]).unwrap();
        // A pure-leaf coordinate calls the source exactly once.
        let calls = std::cell::Cell::new(0usize);
        cube.consolidate_with::<ModelError, _>(&[r[0]], |_| {
            calls.set(calls.get() + 1);
            Ok(Fixed::ZERO)
        })
        .unwrap();
        assert_eq!(calls.get(), 1, "a leaf read must not enumerate");
        // A consolidated coordinate enumerates its three contributing leaves.
        let calls = std::cell::Cell::new(0usize);
        cube.consolidate_with::<ModelError, _>(&[region_total], |_| {
            calls.set(calls.get() + 1);
            Ok(Fixed::ZERO)
        })
        .unwrap();
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn consolidate_fed_unions_stored_and_fed_leaves() {
        let (region, region_total, r) = sum_dim("Region", 2);
        let mut cube = Cube::new("Sales", vec![region]).unwrap();
        cube.set_leaf(&[r[0]], fix(10)).unwrap();
        // r[1] is not stored but is supplied as a fed (rule-derived) leaf = 5.
        let fed: Vec<Box<[u32]>> = vec![vec![r[1]].into_boxed_slice()];
        let leaf = |c: &[u32]| -> Result<Fixed, ModelError> {
            if c == [r[1]] {
                Ok(fix(5))
            } else {
                Ok(cube.leaf_value(c))
            }
        };
        let v = cube
            .consolidate_fed::<ModelError, _>(&[region_total], &fed, leaf)
            .unwrap();
        assert_eq!(v, fix(15));
    }

    #[test]
    fn consolidate_fed_counts_an_overridden_stored_leaf_once() {
        let (region, region_total, r) = sum_dim("Region", 2);
        let mut cube = Cube::new("Sales", vec![region]).unwrap();
        cube.set_leaf(&[r[0]], fix(10)).unwrap();
        cube.set_leaf(&[r[1]], fix(20)).unwrap();
        // r[0] is stored AND fed (a rule overrides it to 100); count once.
        let fed: Vec<Box<[u32]>> = vec![vec![r[0]].into_boxed_slice()];
        let leaf = |c: &[u32]| -> Result<Fixed, ModelError> {
            if c == [r[0]] {
                Ok(fix(100))
            } else {
                Ok(cube.leaf_value(c))
            }
        };
        let v = cube
            .consolidate_fed::<ModelError, _>(&[region_total], &fed, leaf)
            .unwrap();
        assert_eq!(v, fix(120));
    }

    #[test]
    fn consolidate_with_equals_get_randomized() {
        let (region, _rt, r) = sum_dim("Region", 4);
        let (period, _pt, p) = sum_dim("Period", 3);
        let mut cube = Cube::new("Sales", vec![region, period]).unwrap();
        let mut rng = DeterministicRng::new(7);
        for &ri in &r {
            for &pi in &p {
                if rng.next_u64().is_multiple_of(2) {
                    let v = Fixed::from_scaled((rng.next_u64() % 1000) as i64);
                    cube.set_leaf(&[ri, pi], v).unwrap();
                }
            }
        }
        let rlen = cube.dimension(0).len();
        let plen = cube.dimension(1).len();
        for ri in 0..rlen {
            for pi in 0..plen {
                let c = [ri, pi];
                let via = cube
                    .consolidate_with::<ModelError, _>(&c, |lc| Ok(cube.leaf_value(lc)))
                    .unwrap();
                assert_eq!(via, cube.get(&c).unwrap(), "mismatch at {c:?}");
            }
        }
    }

    // ---- structural editing (ADR-0036) ----

    /// A Region x Measure cube. Region: North, South, East leaves under Total.
    /// Measure: Sales numeric leaf and Comment string leaf. Seeded with numeric
    /// cells per (region, Sales) and a string cell at (North, Comment).
    fn edit_cube() -> Cube {
        let mut region = Dimension::new("Region");
        let north = region.add_leaf("North");
        let south = region.add_leaf("South");
        let east = region.add_leaf("East");
        let total = region.add_consolidated("Total");
        region.add_child(total, north, 1).unwrap();
        region.add_child(total, south, 1).unwrap();
        region.add_child(total, east, 1).unwrap();

        let mut measure = Dimension::new("Measure");
        measure.add_leaf("Sales");
        measure.add_string("Comment");

        let mut cube = Cube::new("Sales", vec![region, measure]).unwrap();
        let sales = cube.dimension(1).index_of("Sales").unwrap();
        let comment = cube.dimension(1).index_of("Comment").unwrap();
        cube.set_leaf(&[north, sales], fix(10)).unwrap();
        cube.set_leaf(&[south, sales], fix(20)).unwrap();
        cube.set_leaf(&[east, sales], fix(30)).unwrap();
        cube.set_string(&[north, comment], "hi").unwrap();
        cube
    }

    /// Read a numeric cell by element names, resolving indices freshly each call
    /// (so the read follows a member across structural edits).
    fn read(cube: &Cube, region: &str, measure: &str) -> Fixed {
        let r = cube.dimension(0).index_of(region).unwrap();
        let m = cube.dimension(1).index_of(measure).unwrap();
        cube.get(&[r, m]).unwrap()
    }

    #[test]
    fn reorder_moves_values_with_their_members() {
        let mut cube = edit_cube();
        // Reverse the leaves but keep Total last.
        cube.reorder_elements(
            "Region",
            &[
                "East".into(),
                "South".into(),
                "North".into(),
                "Total".into(),
            ],
        )
        .unwrap();
        // The member list is in the new order.
        let names: Vec<&str> = cube
            .dimension(0)
            .iter_elements()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec!["East", "South", "North", "Total"]);
        // Each value followed its member, not its old slot.
        assert_eq!(read(&cube, "North", "Sales"), fix(10));
        assert_eq!(read(&cube, "South", "Sales"), fix(20));
        assert_eq!(read(&cube, "East", "Sales"), fix(30));
        // The consolidation still sums correctly and the string cell followed too.
        assert_eq!(read(&cube, "Total", "Sales"), fix(60));
        let north = cube.dimension(0).index_of("North").unwrap();
        let comment = cube.dimension(1).index_of("Comment").unwrap();
        assert_eq!(cube.get_string(&[north, comment]).unwrap(), Some("hi"));
    }

    #[test]
    fn reorder_rejects_non_permutation_and_is_unchanged() {
        let mut cube = edit_cube();
        // Wrong count.
        assert!(matches!(
            cube.reorder_elements("Region", &["North".into(), "South".into()]),
            Err(ModelError::InvalidReorder { .. })
        ));
        // A duplicate name.
        assert!(matches!(
            cube.reorder_elements(
                "Region",
                &[
                    "North".into(),
                    "North".into(),
                    "East".into(),
                    "Total".into(),
                ],
            ),
            Err(ModelError::InvalidReorder { .. })
        ));
        // An unknown name.
        assert!(matches!(
            cube.reorder_elements(
                "Region",
                &["North".into(), "South".into(), "East".into(), "Nope".into(),],
            ),
            Err(ModelError::InvalidReorder { .. })
        ));
        // Untouched: original order and values intact.
        let names: Vec<&str> = cube
            .dimension(0)
            .iter_elements()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec!["North", "South", "East", "Total"]);
        assert_eq!(read(&cube, "South", "Sales"), fix(20));
    }

    #[test]
    fn reorder_is_deterministic_and_identity_is_a_no_op() {
        let order = vec![
            "East".to_string(),
            "North".to_string(),
            "Total".to_string(),
            "South".to_string(),
        ];
        let run = || {
            let mut cube = edit_cube();
            cube.reorder_elements("Region", &order).unwrap();
            cube.cell_entries().collect::<Vec<_>>()
        };
        let a = run();
        let b = run();
        assert_eq!(a, b, "reorder is deterministic across runs");

        // A no-op reorder (current order) leaves cells byte-identical.
        let mut cube = edit_cube();
        let before: std::collections::BTreeMap<Vec<u32>, Fixed> = cube.cell_entries().collect();
        cube.reorder_elements(
            "Region",
            &[
                "North".into(),
                "South".into(),
                "East".into(),
                "Total".into(),
            ],
        )
        .unwrap();
        let after: std::collections::BTreeMap<Vec<u32>, Fixed> = cube.cell_entries().collect();
        assert_eq!(before, after);
    }

    #[test]
    fn reparent_recomputes_rollup_and_converts_leaf_parent() {
        let mut cube = edit_cube();
        // Move East out from under Total and under a brand-new... actually convert
        // the leaf South into a consolidation by parenting East under it.
        cube.reparent_element("Region", "East", Some("South"), 1)
            .unwrap();
        // South was a leaf; gaining a child converts it to a consolidation.
        let south = cube.dimension(0).index_of("South").unwrap();
        assert_eq!(
            cube.dimension(0).element(south).unwrap().kind,
            ElementKind::Consolidated
        );
        // South is now a consolidation summing its own (none) plus child East = 30.
        assert_eq!(read(&cube, "South", "Sales"), fix(30));
        // reparent detaches East from its old parent (Total) before re-attaching it
        // under South, so East now reaches Total only through South. Total =
        // North(10) + South[East 30] = 40.
        assert_eq!(read(&cube, "Total", "Sales"), fix(40));
        // Detach East to a root: it stops contributing to South.
        cube.reparent_element("Region", "East", None, 1).unwrap();
        assert_eq!(read(&cube, "South", "Sales"), Fixed::ZERO);
    }

    #[test]
    fn reparent_rejects_cycle_and_self_parent() {
        let mut cube = edit_cube();
        // Self-parent.
        assert!(matches!(
            cube.reparent_element("Region", "North", Some("North"), 1),
            Err(ModelError::SelfParent { .. })
        ));
        // Total -> North already; parenting Total under North would cycle.
        assert!(matches!(
            cube.reparent_element("Region", "Total", Some("North"), 1),
            Err(ModelError::CycleDetected { .. })
        ));
        // Unchanged: Total still sums its three leaves.
        assert_eq!(read(&cube, "Total", "Sales"), fix(60));
    }

    #[test]
    fn reparent_converting_a_populated_leaf_drops_its_cell_and_round_trips() {
        // South is a numeric leaf holding Sales=20. Parenting East under it
        // converts South to a consolidation, whose stored value must be dropped:
        // otherwise the snapshot serializes a numeric cell at a now-consolidated
        // coordinate that fails to reload (set_leaf rejects it). ADR-0036 review.
        let mut cube = edit_cube();
        cube.reparent_element("Region", "East", Some("South"), 1)
            .unwrap();
        let model = crate::Model::new(cube);
        let text = model.to_model_text().unwrap();
        let reloaded = crate::Model::from_model_text(&text).expect("reloads after reparent");
        // South now reads as a pure consolidation of its child East (30), with no
        // stale 20 lingering.
        assert_eq!(read(&reloaded.cube, "South", "Sales"), fix(30));
    }

    #[test]
    fn add_child_is_additive_and_keeps_other_parents() {
        // ADR-0036 + UAT: adding a member to a consolidation must NOT remove it
        // from any other consolidation it already rolls up to (alternate
        // hierarchies are valid), unlike reparent which detaches first.
        let mut cube = edit_cube(); // North/South/East are leaves under Total.
                                    // A second consolidation; North will roll up to BOTH Total and Coastal.
        cube.insert_element_at(
            "Region",
            "Coastal",
            ElementKind::Consolidated,
            Position::AtEnd,
        )
        .unwrap();
        cube.add_child_element("Region", "Coastal", "North", 1)
            .unwrap();
        // North still contributes to Total (it was NOT detached): 10+20+30 = 60.
        assert_eq!(read(&cube, "Total", "Sales"), fix(60));
        // And it now also rolls up to Coastal = North's 10.
        assert_eq!(read(&cube, "Coastal", "Sales"), fix(10));
        // Idempotent: re-adding the same edge changes nothing.
        cube.add_child_element("Region", "Coastal", "North", 1)
            .unwrap();
        assert_eq!(read(&cube, "Coastal", "Sales"), fix(10));
    }

    #[test]
    fn remove_child_drops_one_edge_keeps_member_and_other_parents() {
        // ADR-0036 #136: "remove from one consolidation" drops only the named
        // parent -> child edge. East rolls up to BOTH Total and Coastal; removing
        // it from Coastal leaves it under Total, still holding its value.
        let mut cube = edit_cube(); // North/South/East leaves under Total.
        cube.insert_element_at(
            "Region",
            "Coastal",
            ElementKind::Consolidated,
            Position::AtEnd,
        )
        .unwrap();
        cube.add_child_element("Region", "Coastal", "East", 1)
            .unwrap();
        // Sanity: East under both -> Total = 60, Coastal = East's 30.
        assert_eq!(read(&cube, "Total", "Sales"), fix(60));
        assert_eq!(read(&cube, "Coastal", "Sales"), fix(30));

        cube.remove_child_element("Region", "Coastal", "East")
            .unwrap();
        // East still exists with its value, still under Total, no longer under
        // Coastal (which stays a consolidation even though it is now childless).
        assert!(cube.dimension(0).index_of("East").is_some());
        assert_eq!(read(&cube, "East", "Sales"), fix(30));
        assert_eq!(read(&cube, "Total", "Sales"), fix(60));
        assert_eq!(read(&cube, "Coastal", "Sales"), Fixed::ZERO);
        let coastal = cube.dimension(0).index_of("Coastal").unwrap();
        assert_eq!(
            cube.dimension(0).element(coastal).unwrap().kind,
            ElementKind::Consolidated
        );

        // Idempotent: re-removing the now-absent edge is Ok and changes nothing.
        cube.remove_child_element("Region", "Coastal", "East")
            .unwrap();
        assert_eq!(read(&cube, "Total", "Sales"), fix(60));
    }

    #[test]
    fn set_element_kind_retypes_and_clears_values() {
        let mut cube = edit_cube();
        let sales = cube.dimension(1).index_of("Sales").unwrap();

        // Numeric Sales -> string: the stored numeric cells are cleared.
        cube.set_element_kind("Measure", "Sales", ElementKind::String)
            .unwrap();
        assert_eq!(
            cube.dimension(1).element(sales).unwrap().kind,
            ElementKind::String
        );
        assert_eq!(cube.cell_count(), 0, "numeric cells cleared on re-type");
        // Sales is now a string leaf and a string write addresses it.
        let north = cube.dimension(0).index_of("North").unwrap();
        cube.set_string(&[north, sales], "note").unwrap();
        assert_eq!(cube.get_string(&[north, sales]).unwrap(), Some("note"));

        // String Sales -> numeric: the string cell is cleared.
        cube.set_element_kind("Measure", "Sales", ElementKind::Leaf)
            .unwrap();
        assert_eq!(cube.get_string(&[north, sales]).unwrap(), None);
    }

    #[test]
    fn set_element_kind_to_consolidated_drops_leaf_value() {
        let mut cube = edit_cube();
        // North holds Sales = 10; converting it to a consolidation drops that value.
        cube.set_element_kind("Region", "North", ElementKind::Consolidated)
            .unwrap();
        assert_eq!(read(&cube, "North", "Sales"), Fixed::ZERO);
        // Total no longer gets North's old leaf value (South 20 + East 30).
        assert_eq!(read(&cube, "Total", "Sales"), fix(50));
    }

    #[test]
    fn set_element_kind_consolidated_to_leaf_requires_no_children() {
        let mut cube = edit_cube();
        // Total has children; converting it to a leaf is rejected.
        assert!(matches!(
            cube.set_element_kind("Region", "Total", ElementKind::Leaf),
            Err(ModelError::ConsolidationHasChildren { .. })
        ));
        // After detaching all children, the convert succeeds.
        for child in ["North", "South", "East"] {
            cube.reparent_element("Region", child, None, 1).unwrap();
        }
        cube.set_element_kind("Region", "Total", ElementKind::Leaf)
            .unwrap();
        let total = cube.dimension(0).index_of("Total").unwrap();
        assert_eq!(
            cube.dimension(0).element(total).unwrap().kind,
            ElementKind::Leaf
        );
    }

    #[test]
    fn delete_removes_member_and_reindexes_remaining_cells() {
        let mut cube = edit_cube();
        // Detach South from Total (a leaf delete is fine but Total would still
        // reference it; delete only removes edges where the member is the child,
        // which delete handles), then delete South.
        cube.delete_element("Region", "South").unwrap();
        // South is gone; the other members and their cells are intact.
        assert!(cube.dimension(0).index_of("South").is_none());
        assert_eq!(read(&cube, "North", "Sales"), fix(10));
        assert_eq!(read(&cube, "East", "Sales"), fix(30));
        // Total now sums only North + East = 40 (South's edge was removed).
        assert_eq!(read(&cube, "Total", "Sales"), fix(40));
        // The string cell at North/Comment survived the reindex.
        let north = cube.dimension(0).index_of("North").unwrap();
        let comment = cube.dimension(1).index_of("Comment").unwrap();
        assert_eq!(cube.get_string(&[north, comment]).unwrap(), Some("hi"));
    }

    #[test]
    fn delete_rejects_parent_with_children_and_is_unchanged() {
        let mut cube = edit_cube();
        assert!(matches!(
            cube.delete_element("Region", "Total"),
            Err(ModelError::ConsolidationHasChildren { .. })
        ));
        // Unchanged.
        assert_eq!(cube.dimension(0).len(), 4);
        assert_eq!(read(&cube, "Total", "Sales"), fix(60));
    }

    #[test]
    fn insert_element_at_places_correctly_and_keeps_other_cells() {
        let mut cube = edit_cube();
        // Insert West before East.
        cube.insert_element_at(
            "Region",
            "West",
            ElementKind::Leaf,
            Position::Before("East".into()),
        )
        .unwrap();
        let names: Vec<&str> = cube
            .dimension(0)
            .iter_elements()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec!["North", "South", "West", "East", "Total"]);
        // Existing cells are intact after the index shift.
        assert_eq!(read(&cube, "North", "Sales"), fix(10));
        assert_eq!(read(&cube, "South", "Sales"), fix(20));
        assert_eq!(read(&cube, "East", "Sales"), fix(30));
        // The new member is writable and rolls up if parented.
        cube.reparent_element("Region", "West", Some("Total"), 1)
            .unwrap();
        let west = cube.dimension(0).index_of("West").unwrap();
        let sales = cube.dimension(1).index_of("Sales").unwrap();
        cube.set_leaf(&[west, sales], fix(5)).unwrap();
        assert_eq!(read(&cube, "Total", "Sales"), fix(65));

        // Insert after a member, and at end.
        cube.insert_element_at(
            "Region",
            "NE",
            ElementKind::Leaf,
            Position::After("North".into()),
        )
        .unwrap();
        cube.insert_element_at("Region", "Far", ElementKind::Leaf, Position::AtEnd)
            .unwrap();
        let names: Vec<&str> = cube
            .dimension(0)
            .iter_elements()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["North", "NE", "South", "West", "East", "Total", "Far"]
        );
        assert_eq!(read(&cube, "North", "Sales"), fix(10));
    }

    #[test]
    fn insert_rejects_duplicate_name() {
        let mut cube = edit_cube();
        assert!(matches!(
            cube.insert_element_at("Region", "North", ElementKind::Leaf, Position::AtEnd),
            Err(ModelError::DuplicateElement { .. })
        ));
        assert_eq!(cube.dimension(0).len(), 4);
    }
}
