//! Times reading and compiling a model with one very large dimension.
//!
//! Run with `cargo run --release --example bench_model`.

use std::time::Instant;

use epiphany_core::model::{Model, ModelDef};
use serde_json::{Value, json};

fn main() {
    for (leaves, fan) in [
        (100_000usize, 100usize),
        (1_000_000, 100),
        (1_000_000, 1_000_000),
    ] {
        // Leaves under groups of `fan`, the groups under one total.
        let groups = leaves.div_ceil(fan);
        let mut elements: Vec<Value> = Vec::with_capacity(leaves + groups + 1);
        let group_names: Vec<Value> = (0..groups)
            .map(|g| json!({ "name": format!("g{g}") }))
            .collect();
        elements.push(json!({ "name": "all", "type": "consolidated", "children": group_names }));
        for g in 0..groups {
            let children: Vec<Value> = (g * fan..((g + 1) * fan).min(leaves))
                .map(|leaf| json!({ "name": format!("l{leaf}") }))
                .collect();
            elements.push(
                json!({ "name": format!("g{g}"), "type": "consolidated", "children": children }),
            );
        }
        for leaf in 0..leaves {
            elements.push(json!({ "name": format!("l{leaf}"), "type": "numeric" }));
        }
        let definition = json!({
            "dimensions": [
                { "name": "Big", "elements": elements },
                { "name": "Small", "elements": [{ "name": "x", "type": "numeric" }] }
            ],
            "cubes": [{ "name": "C", "dimensions": ["Big", "Small"] }]
        });
        let text = definition.to_string();
        let start = Instant::now();
        let definition: ModelDef = serde_json::from_str(&text).unwrap();
        let parsed = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        let model = Model::compile(&definition).unwrap();
        let compiled = start.elapsed().as_secs_f64() * 1000.0;
        let size = model.dimension("Big").unwrap().elements.len();
        // A dashboard more, as saving one changes the model: its dimensions are as they were.
        let mut changed = definition.clone();
        let notes = json!({ "name": "Notes", "tiles": [{ "show": "text", "text": "Hello" }] });
        changed
            .dashboards
            .push(serde_json::from_value(notes).unwrap());
        let start = Instant::now();
        Model::compile_change(&changed, &definition, &model).unwrap();
        let dashboard = start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{leaves:>9} leaves in groups of {fan:>7}: {:>5.0} MB of JSON, parsed {parsed:>7.0} ms, compiled {compiled:>7.0} ms, a dashboard more {dashboard:>5.2} ms, {size} elements",
            text.len() as f64 / 1e6
        );
    }
}
