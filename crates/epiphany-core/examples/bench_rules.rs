//! Times a consolidation over rule-calculated leaves, without and with feeders.
//!
//! Run with `cargo run --release --example bench_rules`.

#[path = "../tests/common/mod.rs"]
mod common;

use std::time::Instant;

use common::{Rng, Shape, random_model};
use epiphany_core::Scalar;
use epiphany_core::database::{CellRef, CellWrite, Database};

const DIMENSIONS: usize = 3;
const SHAPE: Shape = Shape {
    leaves: 100,
    groups: 12,
    fan: 10,
};
const WRITES: usize = 50_000;

fn main() {
    let mut rng = Rng::new(1);
    let base = random_model(&mut rng, DIMENSIONS, &SHAPE);
    // In the last dimension, e0 is twice e1, at leaf cells. Only e1 is ever written.
    let writes: Vec<CellWrite> = (0..WRITES)
        .map(|index| CellWrite {
            cube: "C".to_owned(),
            at: vec![
                format!("e{}", rng.below(SHAPE.leaves)),
                format!("e{}", rng.below(SHAPE.leaves)),
                "e1".to_owned(),
            ],
            value: Scalar::Number(1.0 + index as f64),
        })
        .collect();
    let total = CellRef {
        cube: "C".to_owned(),
        at: vec!["all".to_owned(), "all".to_owned(), "e0".to_owned()],
    };
    let grand = CellRef {
        cube: "C".to_owned(),
        at: vec!["all".to_owned(); DIMENSIONS],
    };

    for (label, feeders) in [
        ("without feeders", vec![]),
        ("with feeders", vec!["[d2].[e1] => [d2].[e0]"]),
    ] {
        let mut model = base.clone();
        model.cubes[0].rules = vec!["N: [d2].[e0] = [d2].[e1] * 2".to_owned()];
        model.cubes[0].feeders = feeders.into_iter().map(str::to_owned).collect();
        let mut database = Database::new(model).unwrap();
        database.write(None, &writes).unwrap();
        for (what, cell) in [
            ("total of the calculated element", &total),
            ("grand total", &grand),
        ] {
            let start = Instant::now();
            let value = database.read(None, cell).unwrap();
            let took = start.elapsed().as_secs_f64() * 1000.0;
            println!("{label:<16} {what:<34} {took:>9.1} ms  {value:?}");
        }
    }
}
