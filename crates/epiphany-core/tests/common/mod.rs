//! Shared by the randomised tests and the benchmark: a small generator and random models.

use std::collections::BTreeSet;

use epiphany_core::model::{
    ChildDef, CubeDef, DimensionDef, DimensionType, ElementDef, ElementType, ModelDef,
};

/// A deterministic xorshift generator, so failures can be replayed from their seed.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, limit: usize) -> usize {
        (self.next_u64() % limit as u64) as usize
    }
}

/// The shape of each dimension of a random model.
pub struct Shape {
    /// Numeric elements, with ids `0..leaves`.
    pub leaves: usize,
    /// Consolidations over random earlier elements, with ids following the leaves.
    pub groups: usize,
    /// The most children a group may have.
    pub fan: usize,
}

/// A model with one cube `C` over `dimensions` dimensions of the given shape. Each dimension
/// ends with a consolidation `all` over every leaf. Weights are powers of two, so sums of
/// values that are multiples of a power of two are exact in any order.
pub fn random_model(rng: &mut Rng, dimensions: usize, shape: &Shape) -> ModelDef {
    const WEIGHTS: [f64; 5] = [1.0, 1.0, -1.0, 0.5, 2.0];
    let element = |name: String, children: Vec<ChildDef>| ElementDef {
        kind: match children.is_empty() {
            true => ElementType::Numeric,
            false => ElementType::Consolidated,
        },
        name,
        children,
        attributes: Default::default(),
    };
    let child = |id: usize, weight: f64| ChildDef {
        name: format!("e{id}"),
        weight,
    };

    let mut dimension = |index: usize| {
        let mut elements: Vec<ElementDef> = (0..shape.leaves)
            .map(|id| element(format!("e{id}"), Vec::new()))
            .collect();
        for id in shape.leaves..shape.leaves + shape.groups {
            let picks: BTreeSet<usize> = (0..shape.fan).map(|_| rng.below(id)).collect();
            let children = picks
                .into_iter()
                .map(|pick| child(pick, WEIGHTS[rng.below(5)]));
            elements.push(element(format!("e{id}"), children.collect()));
        }
        let leaves = (0..shape.leaves).map(|id| child(id, 1.0));
        elements.push(element("all".to_owned(), leaves.collect()));
        DimensionDef {
            name: format!("d{index}"),
            kind: DimensionType::Standard,
            elements,
            attributes: Default::default(),
            subsets: Default::default(),
            levels: Default::default(),
        }
    };

    ModelDef {
        dimensions: (0..dimensions).map(&mut dimension).collect(),
        cubes: vec![CubeDef {
            name: "C".to_owned(),
            dimensions: (0..dimensions).map(|index| format!("d{index}")).collect(),
            views: Default::default(),
            rules: Vec::new(),
            feeders: Vec::new(),
        }],
        ..ModelDef::default()
    }
}
