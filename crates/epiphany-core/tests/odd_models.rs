//! Definitions that are odd or extreme are compiled and, where they compile, asked a
//! question. Each must be taken or refused; none may bring the engine down.

use std::panic::{AssertUnwindSafe, catch_unwind};

use epiphany_core::database::Database;
use epiphany_core::model::ModelDef;
use serde_json::{Value as Json, json};

fn leaf(name: &str) -> Json {
    json!({ "name": name, "type": "numeric" })
}

fn total(name: &str, children: &[&str]) -> Json {
    let children: Vec<Json> = children
        .iter()
        .map(|child| json!({ "name": child }))
        .collect();
    json!({ "name": name, "type": "consolidated", "children": children })
}

/// A model of one dimension with these elements, and a cube over it and a second, small one.
fn model(elements: Vec<Json>) -> Json {
    json!({
        "dimensions": [
            { "name": "D", "elements": elements },
            { "name": "Other", "elements": [leaf("x"), leaf("y")] }
        ],
        "cubes": [{ "name": "C", "dimensions": ["D", "Other"] }]
    })
}

/// Compiles a definition and, if it is taken, reads the total of its first element. Says
/// whether it was taken, or panics with the label if the engine came down.
fn tried(label: &str, definition: Json) -> bool {
    let attempt = catch_unwind(AssertUnwindSafe(|| {
        let Ok(definition) = serde_json::from_value::<ModelDef>(definition) else {
            return false;
        };
        let Ok(database) = Database::new(definition) else {
            return false;
        };
        let mdx = "SELECT {[D].DefaultMember, Descendants([D].DefaultMember)} ON COLUMNS FROM [C]";
        drop(database.query(None, mdx));
        drop(database.query(None, "SELECT Hierarchize([D].Members) ON COLUMNS FROM [C]"));
        true
    }));
    attempt.unwrap_or_else(|_| panic!("the engine came down on: {label}"))
}

#[test]
fn hierarchies_that_go_round_or_nowhere_are_refused() {
    assert!(tried(
        "a plain hierarchy",
        model(vec![total("all", &["a", "b"]), leaf("a"), leaf("b")])
    ));
    assert!(!tried(
        "an element its own child",
        model(vec![total("a", &["a"])])
    ));
    assert!(!tried(
        "two elements each the other's child",
        model(vec![total("a", &["b"]), total("b", &["a"])])
    ));
    assert!(!tried(
        "a circle of three",
        model(vec![
            total("a", &["b"]),
            total("b", &["c"]),
            total("c", &["a"]),
            leaf("d")
        ])
    ));
    assert!(!tried(
        "a child that is not there",
        model(vec![total("a", &["nobody"])])
    ));
    assert!(!tried(
        "the same name twice",
        model(vec![leaf("a"), leaf("A")])
    ));
    assert!(!tried(
        "a cube over a dimension that is not there",
        json!({
            "dimensions": [], "cubes": [{ "name": "C", "dimensions": ["D"] }]
        })
    ));
    // Odd, and taken or refused as may be: only coming down is wrong.
    tried("a name that is empty", model(vec![leaf("")]));
    tried("a consolidation of nothing", model(vec![total("a", &[])]));
    tried(
        "a child twice",
        model(vec![total("a", &["b", "b"]), leaf("b")]),
    );
    assert!(!tried("no elements", model(vec![])));
    tried(
        "a leaf with children",
        json!({
            "dimensions": [{ "name": "D", "elements": [
                { "name": "a", "type": "numeric", "children": [{ "name": "b" }] }, leaf("b")
            ]}, { "name": "Other", "elements": [leaf("x")] }],
            "cubes": [{ "name": "C", "dimensions": ["D", "Other"] }]
        }),
    );
    tried(
        "a weight too great to add up",
        json!({
            "dimensions": [{ "name": "D", "elements": [
                { "name": "a", "type": "consolidated", "children": [
                    { "name": "b", "weight": 1e308 }, { "name": "c", "weight": 1e308 }
                ]}, leaf("b"), leaf("c")
            ]}, { "name": "Other", "elements": [leaf("x")] }],
            "cubes": [{ "name": "C", "dimensions": ["D", "Other"] }]
        }),
    );
    tried(
        "a cube over one dimension twice",
        json!({
            "dimensions": [{ "name": "D", "elements": [leaf("a")] }],
            "cubes": [{ "name": "C", "dimensions": ["D", "D"] }]
        }),
    );
    tried(
        "a cube over no dimensions",
        json!({
            "dimensions": [{ "name": "D", "elements": [leaf("a")] }],
            "cubes": [{ "name": "C", "dimensions": [] }]
        }),
    );
}

#[test]
fn a_cube_of_many_dimensions_keeps_its_cells_as_one_of_few_does() {
    use epiphany_core::Scalar;
    use epiphany_core::database::{CellRef, CellWrite};

    // Seven, eight, nine and twelve dimensions: a coordinate keeps up to eight elements in
    // place and more of them apart, and neither way may show.
    for count in [7, 8, 9, 12] {
        let dimension = |index: usize| json!({ "name": format!("D{index}"), "elements": [total("all", &["a", "b"]), leaf("a"), leaf("b")] });
        let names: Vec<String> = (0..count).map(|index| format!("D{index}")).collect();
        let definition = json!({
            "dimensions": (0..count).map(dimension).collect::<Vec<_>>(),
            "cubes": [{ "name": "C", "dimensions": names }]
        });
        let mut database = Database::new(serde_json::from_value(definition).unwrap()).unwrap();
        let at = |last: &str, rest: &str| -> Vec<String> {
            let mut at = vec![rest.to_owned(); count - 1];
            at.push(last.to_owned());
            at
        };
        let write = |at: Vec<String>, value: f64| CellWrite {
            cube: "C".to_owned(),
            at,
            value: Scalar::Number(value),
        };
        let writes = [
            write(at("a", "a"), 1.0),
            write(at("b", "a"), 2.0),
            write(at("b", "b"), 4.0),
        ];
        database.write(None, &writes).unwrap();
        let read = |at: Vec<String>| {
            database.read(
                None,
                &CellRef {
                    cube: "C".to_owned(),
                    at,
                },
            )
        };
        assert_eq!(read(at("b", "a")), Ok(Scalar::Number(2.0)), "{count}");
        assert_eq!(read(at("all", "a")), Ok(Scalar::Number(3.0)), "{count}");
        assert_eq!(read(at("all", "all")), Ok(Scalar::Number(7.0)), "{count}");
        // Written over and cleared, as any cell is.
        let mut database = database;
        database
            .write(None, &[write(at("a", "a"), 0.0), write(at("b", "b"), 5.0)])
            .unwrap();
        let read = |at: Vec<String>| {
            database.read(
                None,
                &CellRef {
                    cube: "C".to_owned(),
                    at,
                },
            )
        };
        assert_eq!(read(at("all", "all")), Ok(Scalar::Number(7.0)), "{count}");
        assert_eq!(read(at("a", "a")), Ok(Scalar::Number(0.0)), "{count}");
        let mdx = format!(
            "SELECT NON EMPTY [D0].Members * [D{}].Members ON COLUMNS FROM [C]",
            count - 1
        );
        // Two cells are left, both at b in the last dimension: a and b in the first, each
        // with the total above it and the total of the last.
        assert_eq!(
            database.query(None, &mdx).unwrap().axes[0].tuples.len(),
            6,
            "{count}"
        );
    }
}

#[test]
fn a_hierarchy_may_be_wide_but_only_so_deep() {
    // Each element the only child of the one before it, and a leaf at the bottom.
    let chain = |deep: usize, downwards: bool| {
        let mut elements: Vec<Json> = (0..deep)
            .map(|level| total(&format!("t{level}"), &[&format!("t{}", level + 1)]))
            .collect();
        elements.push(leaf(&format!("t{deep}")));
        if !downwards {
            elements.reverse();
        }
        model(elements)
    };
    assert!(tried("a chain a hundred deep", chain(100, true)));
    // Fifty thousand deep is refused, whichever end of it is written first.
    assert!(!tried("a chain fifty thousand deep", chain(50_000, true)));
    assert!(!tried("the same, the leaf first", chain(50_000, false)));
    assert!(!tried("one level too many", chain(101, false)));

    // As wide as may be: one total over fifty thousand leaves.
    let names: Vec<String> = (0..50_000).map(|leaf| format!("l{leaf}")).collect();
    let children: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut elements = vec![total("all", &children)];
    elements.extend(names.iter().map(|name| leaf(name)));
    assert!(tried("a total over fifty thousand leaves", model(elements)));
}
