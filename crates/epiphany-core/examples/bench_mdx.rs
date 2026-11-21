//! Times MDX queries on a random cube.
//!
//! Run with `cargo run --release --example bench_mdx`.

#[path = "../tests/common/mod.rs"]
mod common;

use std::time::Instant;

use common::{Rng, Shape, random_model};
use epiphany_core::Scalar;
use epiphany_core::database::{CellWrite, Database};

const DIMENSIONS: usize = 4;
const SHAPE: Shape = Shape {
    leaves: 100,
    groups: 12,
    fan: 10,
};
const WRITES: usize = 200_000;

fn main() {
    let mut rng = Rng::new(1);
    let mut database = Database::new(random_model(&mut rng, DIMENSIONS, &SHAPE)).unwrap();
    let write = |index: usize| CellWrite {
        cube: "C".to_owned(),
        at: (0..DIMENSIONS)
            .map(|_| format!("e{}", rng.below(SHAPE.leaves)))
            .collect(),
        value: Scalar::Number(1.0 + index as f64),
    };
    let writes: Vec<CellWrite> = (0..WRITES).map(write).collect();
    let start = Instant::now();
    database.write(None, &writes).unwrap();
    println!(
        "{:<52} {:>9.1} ms",
        "write 200,000 cells in one batch",
        millis(start)
    );

    let leaves = |dimension: usize| format!("TM1FilterByLevel([d{dimension}].Members, 0)");
    let first = |dimension: usize, count: usize| format!("Head({}, {count})", leaves(dimension));
    let queries = [
        (
            "10,000 leaf cells",
            format!(
                "SELECT {} ON COLUMNS, {} ON ROWS FROM [C] WHERE ([d2].[e0], [d3].[e0])",
                leaves(0),
                leaves(1)
            ),
        ),
        (
            "10,000 cells of a calculated member",
            format!(
                "WITH MEMBER [d2].[share] AS IIF([d2].[e1] = 0, 0, ([d2].[e0] - [d2].[e1]) / [d2].[e1]) \
                 SELECT {} ON COLUMNS, {} ON ROWS FROM [C] WHERE ([d2].[share], [d3].[e0])",
                leaves(0),
                leaves(1)
            ),
        ),
        (
            "10,000 tuples filtered by a comparison of two cells",
            format!(
                "SELECT Filter({} * {}, [d2].[e0] > [d2].[e1]) ON COLUMNS FROM [C] WHERE [d3].[e0]",
                leaves(0),
                leaves(1)
            ),
        ),
        (
            "400 cells, each totalling two dimensions",
            format!(
                "SELECT {} ON COLUMNS, {} ON ROWS FROM [C] WHERE ([d2].[all], [d3].[all])",
                first(0, 20),
                first(1, 20)
            ),
        ),
        (
            "400 cells of a calculated member over such totals",
            format!(
                "WITH MEMBER [d3].[share] AS IIF([d3].[all] = 0, 0, [d3].[e0] / [d3].[all]) \
                 SELECT {} ON COLUMNS, {} ON ROWS FROM [C] WHERE ([d2].[all], [d3].[share])",
                first(0, 20),
                first(1, 20)
            ),
        ),
        (
            "113 cells, each totalling three dimensions",
            "SELECT [d0].Members ON COLUMNS FROM [C] WHERE ([d1].[all], [d2].[all], [d3].[all])"
                .to_owned(),
        ),
        (
            "the 10 largest of 100 totals",
            format!(
                "SELECT TopCount({}, 10, ([d1].[all], [d2].[all], [d3].[all])) ON COLUMNS FROM [C] WHERE ([d1].[all], [d2].[all], [d3].[all])",
                leaves(0)
            ),
        ),
        // Last: whatever is asked straight after the long one takes about twice its usual
        // time (measured 2026-11-21), as the first query of a run does.
        (
            "non-empty rows of 12,769",
            "SELECT NON EMPTY [d0].Members * [d1].Members ON ROWS, \
             {[d2].[e0]} ON COLUMNS FROM [C] WHERE [d3].[e0]"
                .to_owned(),
        ),
        (
            "non-empty rows of 1,442,897, found from the cells",
            "SELECT NON EMPTY [d0].Members * [d1].Members * [d2].Members ON ROWS, \
             {[d3].[e0]} ON COLUMNS FROM [C]"
                .to_owned(),
        ),
    ];
    for (label, mdx) in &queries {
        // Each query starts with no totals kept from the one before.
        database.write(None, &writes[..1]).unwrap();
        for label in [label.to_string(), format!("{label}, again")] {
            let start = Instant::now();
            let result = database.query(None, mdx).unwrap();
            let took = millis(start);
            let each = took * 1000.0 / result.cells.len() as f64;
            println!("{label:<52} {took:>9.1} ms  {each:>9.1} us/cell");
        }
    }
}

fn millis(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}
