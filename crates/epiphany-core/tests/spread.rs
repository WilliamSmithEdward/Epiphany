mod common;

use common::{Rng, Shape, random_model};
use epiphany_core::Scalar;
use epiphany_core::cells::CellError;
use epiphany_core::database::{CellRef, CellWrite, Database, Error, Spread, SpreadMethod};
use serde_json::{Value as Json, json};

/// A cube `Sales` over `Region` (World = North + South), `Month` (Q1 = Jan + Feb) and
/// `Account` (Profit = Revenue - Cost; Units; Note as text), with the given rules.
fn database(rules: Json) -> Database {
    let leaf = |name: &str| json!({ "name": name, "type": "numeric" });
    let total = |name: &str, children: Json| json!({ "name": name, "type": "consolidated", "children": children });
    let definition = json!({
        "dimensions": [
            { "name": "Region", "elements": [
                total("World", json!([{ "name": "North" }, { "name": "South" }])), leaf("North"), leaf("South")
            ]},
            { "name": "Month", "elements": [
                total("Q1", json!([{ "name": "Jan" }, { "name": "Feb" }])), leaf("Jan"), leaf("Feb")
            ]},
            { "name": "Account", "elements": [
                total("Profit", json!([{ "name": "Revenue" }, { "name": "Cost", "weight": -1 }])),
                leaf("Revenue"), leaf("Cost"), leaf("Units"),
                { "name": "Note", "type": "string" }
            ]}
        ],
        "cubes": [{ "name": "Sales", "dimensions": ["Region", "Month", "Account"], "rules": rules }]
    });
    Database::new(serde_json::from_value(definition).unwrap()).unwrap()
}

fn cell(at: [&str; 3]) -> CellRef {
    CellRef {
        cube: "Sales".to_owned(),
        at: at.map(str::to_owned).to_vec(),
    }
}

fn set(database: &mut Database, at: [&str; 3], value: f64) {
    let write = CellWrite {
        cube: "Sales".to_owned(),
        at: at.map(str::to_owned).to_vec(),
        value: Scalar::Number(value),
    };
    database.write(None, &[write]).unwrap();
}

fn get(database: &Database, at: [&str; 3]) -> f64 {
    match database.read(None, &cell(at)) {
        Ok(Scalar::Number(number)) => number,
        other => panic!("expected a number, found {other:?}"),
    }
}

fn spread(at: [&str; 3], value: f64, method: SpreadMethod) -> Spread {
    Spread {
        cube: "Sales".to_owned(),
        at: at.map(str::to_owned).to_vec(),
        value,
        method,
        holds: Vec::new(),
    }
}

/// A spread that leaves the cells given as they are.
fn holding(spread: Spread, holds: &[[&str; 3]]) -> Spread {
    Spread {
        holds: holds
            .iter()
            .map(|at| at.map(str::to_owned).to_vec())
            .collect(),
        ..spread
    }
}

fn refused(error: CellError) -> Result<usize, Error> {
    Err(Error::Write { index: 0, error })
}

const QUARTER: [&str; 3] = ["World", "Q1", "Units"];
const LEAVES: [[&str; 3]; 4] = [
    ["North", "Jan", "Units"],
    ["North", "Feb", "Units"],
    ["South", "Jan", "Units"],
    ["South", "Feb", "Units"],
];

#[test]
fn proportional_keeps_the_proportions_of_what_is_there() {
    let mut database = database(json!([]));
    set(&mut database, LEAVES[0], 10.0);
    set(&mut database, LEAVES[2], 30.0);
    let written = database.spread(
        None,
        &spread(QUARTER, 100.0, SpreadMethod::Proportional),
        None,
    );
    // Only the two leaves that held something are written; the empty ones stay empty.
    assert_eq!(written, Ok(2));
    assert_eq!(LEAVES.map(|at| get(&database, at)), [25.0, 0.0, 75.0, 0.0]);
    assert_eq!(get(&database, QUARTER), 100.0);
    assert_eq!(get(&database, ["North", "Q1", "Units"]), 25.0);
}

#[test]
fn proportional_over_nothing_spreads_equally() {
    let mut database = database(json!([]));
    assert_eq!(
        database.spread(
            None,
            &spread(QUARTER, 100.0, SpreadMethod::Proportional),
            None
        ),
        Ok(4)
    );
    assert_eq!(LEAVES.map(|at| get(&database, at)), [25.0; 4]);
}

#[test]
fn equal_gives_every_leaf_the_same_share() {
    let mut database = database(json!([]));
    set(&mut database, LEAVES[0], 10.0);
    assert_eq!(
        database.spread(None, &spread(QUARTER, 60.0, SpreadMethod::Equal), None),
        Ok(4)
    );
    assert_eq!(LEAVES.map(|at| get(&database, at)), [15.0; 4]);
    assert_eq!(get(&database, QUARTER), 60.0);

    // A cell consolidated in one dimension only has just that dimension's leaves.
    assert_eq!(
        database.spread(
            None,
            &spread(["North", "Q1", "Units"], 8.0, SpreadMethod::Equal),
            None
        ),
        Ok(2)
    );
    assert_eq!(LEAVES.map(|at| get(&database, at)), [4.0, 4.0, 15.0, 15.0]);
}

#[test]
fn repeat_gives_every_leaf_the_value_itself() {
    let mut database = database(json!([]));
    assert_eq!(
        database.spread(None, &spread(QUARTER, 7.0, SpreadMethod::Repeat), None),
        Ok(4)
    );
    assert_eq!(LEAVES.map(|at| get(&database, at)), [7.0; 4]);
    assert_eq!(get(&database, QUARTER), 28.0);
}

#[test]
fn weights_are_respected() {
    let mut database = database(json!([]));
    set(&mut database, ["North", "Jan", "Revenue"], 100.0);
    set(&mut database, ["North", "Jan", "Cost"], 60.0);
    // Profit is revenue less cost, 40 now. Doubling it doubles both.
    let profit = ["North", "Jan", "Profit"];
    assert_eq!(
        database.spread(
            None,
            &spread(profit, 80.0, SpreadMethod::Proportional),
            None
        ),
        Ok(2)
    );
    assert_eq!(get(&database, ["North", "Jan", "Revenue"]), 200.0);
    assert_eq!(get(&database, ["North", "Jan", "Cost"]), 120.0);
    assert_eq!(get(&database, profit), 80.0);

    // One part revenue less one part cost comes to nothing whatever the share, so no equal
    // share can add up to a value.
    let outcome = database.spread(None, &spread(profit, 80.0, SpreadMethod::Equal), None);
    assert!(
        matches!(
            outcome,
            Err(Error::Write {
                error: CellError::Spread(_),
                ..
            })
        ),
        "{outcome:?}"
    );
    assert_eq!(get(&database, profit), 80.0);
}

#[test]
fn cells_held_keep_their_values_and_the_rest_take_the_difference() {
    let filled = || {
        let mut database = database(json!([]));
        for (leaf, value) in LEAVES.iter().zip([10.0, 20.0, 30.0, 40.0]) {
            set(&mut database, *leaf, value);
        }
        database
    };
    let values = |database: &Database| LEAVES.map(|leaf| get(database, leaf));

    // In proportion: North in January keeps its 10, and the other three share 190 as they
    // shared 90.
    let mut database = filled();
    let proportional = spread(QUARTER, 200.0, SpreadMethod::Proportional);
    database
        .spread(None, &holding(proportional, &[LEAVES[0]]), None)
        .unwrap();
    let [kept, rest @ ..] = values(&database);
    assert_eq!(kept, 10.0);
    let expected = [20.0, 30.0, 40.0].map(|value| value * 190.0 / 90.0);
    for (got, want) in rest.iter().zip(expected) {
        assert!((got - want).abs() < 1e-9, "{got} against {want}");
    }
    assert!((get(&database, QUARTER) - 200.0).abs() < 1e-9);

    // Equally, with all of the North held through its quarter: the South shares what is left.
    let mut database = filled();
    let equal = spread(QUARTER, 100.0, SpreadMethod::Equal);
    database
        .spread(None, &holding(equal, &[["North", "Q1", "Units"]]), None)
        .unwrap();
    assert_eq!(values(&database), [10.0, 20.0, 35.0, 35.0]);

    // Repeated, every leaf but the one held takes the value itself.
    let mut database = filled();
    let repeat = spread(QUARTER, 5.0, SpreadMethod::Repeat);
    database
        .spread(None, &holding(repeat, &[LEAVES[3]]), None)
        .unwrap();
    assert_eq!(values(&database), [5.0, 5.0, 5.0, 40.0]);

    // With every leaf held, nothing can take a share.
    let mut database = filled();
    let everything = holding(spread(QUARTER, 1.0, SpreadMethod::Equal), &[QUARTER]);
    assert!(matches!(
        database.spread(None, &everything, None),
        Err(Error::Write {
            error: CellError::Spread(_),
            ..
        })
    ));
    assert_eq!(values(&database), [10.0, 20.0, 30.0, 40.0]);
}

#[test]
fn a_spread_is_part_of_its_transaction() {
    let mut database = database(json!([]));
    let transaction = database.begin();
    database
        .spread(
            Some(transaction),
            &spread(QUARTER, 100.0, SpreadMethod::Equal),
            None,
        )
        .unwrap();
    assert_eq!(
        database.read(Some(transaction), &cell(QUARTER)),
        Ok(Scalar::Number(100.0))
    );
    assert_eq!(get(&database, QUARTER), 0.0);

    // A second spread in the same transaction works from the first one's pending values.
    let after_first = database.savepoint(transaction).unwrap();
    database
        .spread(
            Some(transaction),
            &spread(QUARTER, 40.0, SpreadMethod::Proportional),
            None,
        )
        .unwrap();
    assert_eq!(
        database.read(Some(transaction), &cell(LEAVES[0])),
        Ok(Scalar::Number(10.0))
    );
    // Going back past the second spread takes back every leaf it wrote, at once.
    database.undo_to(transaction, after_first).unwrap();
    assert_eq!(
        database.read(Some(transaction), &cell(QUARTER)),
        Ok(Scalar::Number(100.0))
    );
    database.rollback(transaction).unwrap();
    assert_eq!(get(&database, QUARTER), 0.0);

    // Another transaction writing one of the leaves makes the whole spread fail.
    let (first, second) = (database.begin(), database.begin());
    let write = CellWrite {
        cube: "Sales".to_owned(),
        at: LEAVES[3].map(str::to_owned).to_vec(),
        value: Scalar::Number(1.0),
    };
    database.write(Some(first), &[write]).unwrap();
    let blocked = database.spread(
        Some(second),
        &spread(QUARTER, 100.0, SpreadMethod::Equal),
        None,
    );
    assert!(matches!(blocked, Err(Error::Locked(_))), "{blocked:?}");
    assert_eq!(
        database.read(Some(second), &cell(LEAVES[0])),
        Ok(Scalar::Number(0.0))
    );
}

#[test]
fn cells_that_rules_calculate_are_left_alone() {
    // The south's units are calculated; a rule also decides the world's February total.
    let rules = json!([
        "N: ([Account].[Units], [Region].[South]) = ([Account].[Units], [Region].[North]) * 2",
        "C: ([Account].[Units], [Region].[World], [Month].[Feb]) = 5"
    ]);
    let mut database = database(rules);
    assert_eq!(
        database.spread(None, &spread(QUARTER, 30.0, SpreadMethod::Equal), None),
        Ok(2)
    );
    assert_eq!(
        LEAVES.map(|at| get(&database, at)),
        [15.0, 15.0, 30.0, 30.0]
    );
    let decided = spread(["World", "Feb", "Units"], 1.0, SpreadMethod::Equal);
    assert_eq!(
        database.spread(None, &decided, None),
        refused(CellError::Calculated)
    );
}

#[test]
fn a_leaf_is_simply_written_and_bad_targets_are_refused() {
    let mut database = database(json!([]));
    assert_eq!(
        database.spread(None, &spread(LEAVES[0], 9.0, SpreadMethod::Repeat), None),
        Ok(1)
    );
    assert_eq!(get(&database, LEAVES[0]), 9.0);

    let note = spread(["World", "Q1", "Note"], 1.0, SpreadMethod::Equal);
    assert_eq!(
        database.spread(None, &note, None),
        refused(CellError::WrongType)
    );
    let infinite = spread(QUARTER, f64::INFINITY, SpreadMethod::Equal);
    assert_eq!(
        database.spread(None, &infinite, None),
        refused(CellError::NotFinite)
    );
    let unknown = spread(["Mars", "Q1", "Units"], 1.0, SpreadMethod::Equal);
    assert!(matches!(
        database.spread(None, &unknown, None),
        Err(Error::Cell(CellError::UnknownElement { .. }))
    ));
    assert!(matches!(
        database.spread(Some(99), &spread(QUARTER, 1.0, SpreadMethod::Equal), None),
        Err(Error::UnknownTransaction(99))
    ));
}

#[test]
fn a_spread_too_wide_is_refused_unless_it_only_scales_what_is_there() {
    let mut rng = Rng::new(3);
    let shape = Shape {
        leaves: 100,
        groups: 1,
        fan: 2,
    };
    let mut database = Database::new(random_model(&mut rng, 4, &shape)).unwrap();
    let grand = |value: f64, method: SpreadMethod| Spread {
        cube: "C".to_owned(),
        at: vec!["all".to_owned(); 4],
        value,
        method,
        holds: Vec::new(),
    };
    // A hundred million leaves are too many to write one by one.
    let outcome = database.spread(None, &grand(1.0, SpreadMethod::Equal), None);
    assert!(
        matches!(
            outcome,
            Err(Error::Write {
                error: CellError::Spread(_),
                ..
            })
        ),
        "{outcome:?}"
    );

    let write = CellWrite {
        cube: "C".to_owned(),
        at: vec!["e1".to_owned(); 4],
        value: Scalar::Number(4.0),
    };
    database.write(None, &[write]).unwrap();
    assert_eq!(
        database.spread(None, &grand(10.0, SpreadMethod::Proportional), None),
        Ok(1)
    );
    let total = CellRef {
        cube: "C".to_owned(),
        at: vec!["all".to_owned(); 4],
    };
    assert_eq!(database.read(None, &total), Ok(Scalar::Number(10.0)));
}
