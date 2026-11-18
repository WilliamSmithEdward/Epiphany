//! Times cell writes and reads on a random cube.
//!
//! Run with `cargo run --release --example bench_cells`.

#[path = "../tests/common/mod.rs"]
mod common;

use std::hint::black_box;
use std::time::Instant;

use common::{Rng, Shape, random_model};
use epiphany_core::Scalar;
use epiphany_core::cells::Cells;
use epiphany_core::model::{ElementId, Model};

const DIMENSIONS: usize = 4;
const SHAPE: Shape = Shape {
    leaves: 100,
    groups: 12,
    fan: 10,
};
const WRITES: usize = 200_000;

fn time(label: &str, operations: usize, mut run: impl FnMut()) {
    let start = Instant::now();
    run();
    let nanos = start.elapsed().as_nanos() as f64 / operations as f64;
    println!("{label:<44} {nanos:>14.1} ns/op  ({operations} ops)");
}

fn main() {
    let mut rng = Rng::new(1);
    let model = Model::compile(&random_model(&mut rng, DIMENSIONS, &SHAPE)).unwrap();
    let cube = model.cube("C").unwrap();
    let leaf = |rng: &mut Rng| -> Vec<ElementId> {
        (0..DIMENSIONS)
            .map(|_| rng.below(SHAPE.leaves) as ElementId)
            .collect()
    };
    let coordinates: Vec<Vec<ElementId>> = (0..WRITES).map(|_| leaf(&mut rng)).collect();
    let mut cells = Cells::default();

    time("write leaf", WRITES, || {
        for (index, at) in coordinates.iter().enumerate() {
            let value = Scalar::Number(1.0 + index as f64);
            cells.write(&model, cube, at, value).unwrap();
        }
    });
    time("read leaf", WRITES, || {
        for at in &coordinates {
            black_box(cells.read(&model, cube, at));
        }
    });

    // The first group of every dimension has at most ten leaves, so its combinations number
    // fewer than the stored cells and are looked up one by one. `all` has a hundred leaves
    // per dimension, so every stored cell is weighed instead.
    let group = vec![SHAPE.leaves as ElementId; DIMENSIONS];
    let all = vec![(SHAPE.leaves + SHAPE.groups) as ElementId; DIMENSIONS];
    time("read consolidation by combinations", 200, || {
        for _ in 0..200 {
            black_box(cells.read(&model, cube, black_box(&group)));
        }
    });
    time("read consolidation by stored cells", 50, || {
        for _ in 0..50 {
            black_box(cells.read(&model, cube, black_box(&all)));
        }
    });
}
