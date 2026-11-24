//! Times a directory whose model is wide: many dimensions and cubes, each a file of its own.
//! It is opened as a server starting does, first while its files are new, then again, and a
//! dashboard is saved, which writes the model out again.
//!
//! Run with `cargo run --release -p epiphany-server --example bench_wide [dimensions]`; ten
//! thousand dimensions unless told otherwise, of twenty leaves each, and a cube of five of
//! them, with four views, for every twenty.

use std::fs;
use std::time::Instant;

use epiphany_core::model::DashboardDef;
use epiphany_server::open;
use serde_json::json;

const LEAVES: usize = 20;
const CUBE_EVERY: usize = 20;
const CUBE_DIMENSIONS: usize = 5;
const VIEWS: usize = 4;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let count: usize = std::env::args().nth(1).map_or(Ok(10_000), |n| n.parse())?;
    let directory =
        std::env::temp_dir().join(format!("epiphany-bench-wide-{}", std::process::id()));
    let model = directory.join("model");
    fs::create_dir_all(model.join("dimensions"))?;
    fs::create_dir_all(model.join("cubes"))?;
    let name = |d: usize| format!("Dimension {d:05}");
    for d in 0..count {
        let leaves: Vec<String> = (0..LEAVES).map(|e| format!("Leaf {e:02}")).collect();
        let total = json!({
            "name": format!("Total {}", name(d)),
            "type": "consolidated",
            "children": leaves.iter().map(|leaf| json!({ "name": leaf })).collect::<Vec<_>>(),
        });
        let elements: Vec<_> = std::iter::once(total)
            .chain(
                leaves
                    .iter()
                    .map(|leaf| json!({ "name": leaf, "type": "numeric" })),
            )
            .collect();
        let dimension = json!({ "name": name(d), "elements": elements });
        fs::write(
            model.join(format!("dimensions/d{d:05}.json")),
            serde_json::to_string_pretty(&dimension)?,
        )?;
    }
    for c in 0..count / CUBE_EVERY {
        let dimensions: Vec<String> = (0..CUBE_DIMENSIONS)
            .map(|d| name((c * CUBE_DIMENSIONS + d) % count))
            .collect();
        let cube_name = format!("Cube {c:04}");
        let views: serde_json::Map<String, serde_json::Value> = (0..VIEWS)
            .map(|v| {
                let mdx = format!(
                    "SELECT {{[{0}].[Total {0}].Children}} ON COLUMNS FROM [{cube_name}]",
                    dimensions[v % CUBE_DIMENSIONS]
                );
                (format!("View {v}"), json!({ "mdx": mdx }))
            })
            .collect();
        let cube = json!({ "name": cube_name, "dimensions": dimensions, "views": views });
        fs::write(
            model.join(format!("cubes/c{c:04}.json")),
            serde_json::to_string_pretty(&cube)?,
        )?;
    }
    let files = count + count / CUBE_EVERY;

    // Opened first while its files are new, as after a copy, an unpacking or a save; a disk
    // that scans each file as it is first read takes longest then.
    let started = Instant::now();
    let database = open(&directory)?;
    let first = started.elapsed().as_secs_f64();
    drop(database);
    let started = Instant::now();
    let mut database = open(&directory)?;
    let again = started.elapsed().as_secs_f64();
    println!(
        "{count} dimensions, {files} files: opened first in {first:.2} s, again in {again:.2} s"
    );

    // Saving a dashboard writes the model out whole, with every file new.
    let mut definition = database.definition().clone();
    definition.dashboards.push(DashboardDef {
        name: "Saved".to_owned(),
        description: String::new(),
        filters: Vec::new(),
        layout: Default::default(),
        tiles: Vec::new(),
    });
    let started = Instant::now();
    database.update_model(database.model_version(), definition)?;
    println!(
        "a dashboard saved in {:.2} s",
        started.elapsed().as_secs_f64()
    );
    drop(database);
    let started = Instant::now();
    drop(open(&directory)?);
    println!(
        "opened after the save in {:.2} s",
        started.elapsed().as_secs_f64()
    );
    fs::remove_dir_all(&directory)?;
    Ok(())
}
