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

        // Every element of the first dimension at the total of the second, for the calculated
        // element: 113 totals a leaf rule reaches, read one at a time and then together.
        let names = (0..SHAPE.leaves + SHAPE.groups).map(|id| format!("e{id}"));
        let cells: Vec<CellRef> = names
            .chain(["all".to_owned()])
            .map(|name| CellRef {
                cube: "C".to_owned(),
                at: vec![name, "all".to_owned(), "e0".to_owned()],
            })
            .collect();
        // Every consolidation of the first two dimensions by every other, at the calculated
        // element and at the total of the last: 338 totals, nested and overlapping.
        let consolidations: Vec<String> = (SHAPE.leaves..SHAPE.leaves + SHAPE.groups)
            .map(|id| format!("e{id}"))
            .chain(["all".to_owned()])
            .collect();
        let mut grid = Vec::new();
        for first in &consolidations {
            for second in &consolidations {
                for last in ["e0", "all"] {
                    grid.push(CellRef {
                        cube: "C".to_owned(),
                        at: vec![first.clone(), second.clone(), last.to_owned()],
                    });
                }
            }
        }
        for (what, cells) in [("113 such totals", &cells), ("a grid of 338 totals", &grid)] {
            for together in [false, true] {
                // A write empties what was kept, so each way starts with nothing worked out.
                database.write(None, &writes[..1]).unwrap();
                let start = Instant::now();
                let read = match together {
                    true => database.read_many(None, cells),
                    false => cells.iter().map(|cell| database.read(None, cell)).collect(),
                };
                let took = start.elapsed().as_secs_f64() * 1000.0;
                assert!(read.iter().all(Result::is_ok));
                let how = match together {
                    true => "read together",
                    false => "one at a time",
                };
                let what = format!("{what}, {how}");
                println!("{label:<16} {what:<34} {took:>9.1} ms");
            }
        }
    }
}
