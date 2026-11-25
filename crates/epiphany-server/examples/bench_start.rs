//! Times opening a directory that holds many cells: its model and its snapshot read back,
//! as a server does when it starts.
//!
//! Run with `cargo run --release -p epiphany-server --example bench_start [cells]`; a million
//! cells unless told otherwise.

use std::fs;
use std::time::Instant;

use epiphany_core::Scalar;
use epiphany_core::database::CellWrite;
use epiphany_server::open;
use serde_json::json;

/// A cube of four dimensions of a hundred leaves each, room for a hundred million cells.
const DIMENSIONS: usize = 4;
const LEAVES: usize = 100;
/// Cells written in each commit.
const BATCH: usize = 250_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cells: usize = std::env::args()
        .nth(1)
        .map_or(Ok(1_000_000), |n| n.parse())?;
    let directory =
        std::env::temp_dir().join(format!("epiphany-bench-start-{}", std::process::id()));
    let model = directory.join("model");
    fs::create_dir_all(model.join("dimensions"))?;
    fs::create_dir_all(model.join("cubes"))?;
    let names: Vec<String> = (0..DIMENSIONS).map(|d| format!("d{d}")).collect();
    for name in &names {
        let elements: Vec<_> = (0..LEAVES)
            .map(|e| json!({ "name": format!("e{e}"), "type": "numeric" }))
            .collect();
        let dimension = json!({ "name": name, "elements": elements });
        fs::write(
            model.join(format!("dimensions/{name}.json")),
            dimension.to_string(),
        )?;
    }
    let cube = json!({ "name": "C", "dimensions": names });
    fs::write(model.join("cubes/c.json"), cube.to_string())?;

    // The cells at the first coordinates in order, each its own number: a seventh of its
    // place, written with seventeen digits as worked-out amounts are, which take the longest
    // to read back exactly.
    let mut database = open(&directory)?;
    let started = Instant::now();
    for first in (0..cells).step_by(BATCH) {
        let writes: Vec<CellWrite> = (first..(first + BATCH).min(cells))
            .map(|cell| CellWrite {
                cube: "C".to_owned(),
                at: (0..DIMENSIONS)
                    .map(|d| format!("e{}", cell / LEAVES.pow(d as u32) % LEAVES))
                    .collect(),
                value: Scalar::Number(cell as f64 / 7.0),
            })
            .collect();
        database.write(None, &writes)?;
    }
    println!(
        "{cells} cells written in {:.2} s",
        started.elapsed().as_secs_f64()
    );
    drop(database);

    // Opened again, the log is replaced by a snapshot; opened once more, the snapshot alone
    // is read.
    drop(open(&directory)?);
    let size = fs::metadata(directory.join("data/snapshot.json"))?.len();
    for _ in 0..3 {
        let started = Instant::now();
        let database = open(&directory)?;
        let took = started.elapsed().as_secs_f64();
        println!(
            "opened, a snapshot of {:.0} MB, in {took:.2} s",
            size as f64 / 1e6
        );
        drop(database);
    }
    fs::remove_dir_all(&directory)?;
    Ok(())
}
