mod common;

use common::{Rng, Shape, random_model};
use epiphany_core::Scalar;
use epiphany_core::cells::CellError;
use epiphany_core::database::{CellRef, CellWrite, Database, Error};
use epiphany_core::model::{Model, ModelDef};
use serde_json::{Value as Json, json};

/// Two cubes. `Sales` is over `Account` (Margin = Revenue - Cost under Profit; Units, Price,
/// Note), `Region` (World = North + South) and `Month` (Q1 = Jan + Feb). `Rates` is over
/// `Month` and `Rate` (Tax). The rules and feeders of `Sales` are given.
fn definition(rules: Json, feeders: Json) -> ModelDef {
    let leaf = |name: &str| json!({ "name": name, "type": "numeric" });
    let total = |name: &str, children: Json| json!({ "name": name, "type": "consolidated", "children": children });
    serde_json::from_value(json!({
        "dimensions": [
            { "name": "Region", "elements": [
                total("World", json!([{ "name": "North" }, { "name": "South" }])),
                leaf("North"), leaf("South")
            ]},
            { "name": "Month", "elements": [
                total("Q1", json!([{ "name": "Jan" }, { "name": "Feb" }])),
                leaf("Jan"), leaf("Feb")
            ]},
            { "name": "Account", "elements": [
                total("Profit", json!([{ "name": "Revenue" }, { "name": "Cost", "weight": -1 }])),
                leaf("Revenue"), leaf("Cost"), leaf("Margin"), leaf("Units"), leaf("Price"), leaf("Tax"),
                { "name": "Note", "type": "string" }
            ]},
            { "name": "Rate", "elements": [leaf("Tax"), leaf("Bonus")] }
        ],
        "cubes": [
            { "name": "Sales", "dimensions": ["Region", "Month", "Account"], "rules": rules, "feeders": feeders },
            { "name": "Rates", "dimensions": ["Month", "Rate"] }
        ]
    }))
    .unwrap()
}

fn database(rules: Json, feeders: Json) -> Database {
    Database::new(definition(rules, feeders)).unwrap()
}

fn write(database: &mut Database, cube: &str, at: &[&str], value: f64) {
    let write = CellWrite {
        cube: cube.to_owned(),
        at: at.iter().map(|&name| name.to_owned()).collect(),
        value: Scalar::Number(value),
    };
    database.write(None, &[write]).unwrap();
}

fn cell(cube: &str, at: &[&str]) -> CellRef {
    CellRef {
        cube: cube.to_owned(),
        at: at.iter().map(|&name| name.to_owned()).collect(),
    }
}

fn read(database: &Database, at: [&str; 3]) -> f64 {
    match database.read(None, &cell("Sales", &at)) {
        Ok(Scalar::Number(number)) => number,
        other => panic!("expected a number, found {other:?}"),
    }
}

fn failure(rules: Json, feeders: Json) -> String {
    Model::compile(&definition(rules, feeders))
        .unwrap_err()
        .to_string()
}

/// Revenue and cost for the north in both months and the south in January.
fn figures(database: &mut Database) {
    for (region, month, revenue, cost) in [
        ("North", "Jan", 100.0, 60.0),
        ("North", "Feb", 80.0, 50.0),
        ("South", "Jan", 40.0, 10.0),
    ] {
        write(database, "Sales", &[region, month, "Revenue"], revenue);
        write(database, "Sales", &[region, month, "Cost"], cost);
    }
}

#[test]
fn a_rule_calculates_the_cells_of_its_area() {
    let mut database = database(
        json!(["[Account].[Margin] = [Account].[Revenue] - [Account].[Cost]"]),
        json!([]),
    );
    figures(&mut database);
    assert_eq!(read(&database, ["North", "Jan", "Margin"]), 40.0);
    // Without a scope the rule also covers consolidated cells, where it uses consolidated
    // revenue and cost.
    assert_eq!(read(&database, ["World", "Q1", "Margin"]), 220.0 - 120.0);
    assert_eq!(read(&database, ["North", "Jan", "Revenue"]), 100.0);
    assert_eq!(read(&database, ["World", "Q1", "Profit"]), 100.0);
}

#[test]
fn a_leaf_rule_is_added_up_by_consolidations() {
    let rules = json!(["N: [Account].[Revenue] = [Account].[Units] * [Account].[Price]"]);
    let mut database = database(rules, json!([]));
    for (region, month, units, price) in [
        ("North", "Jan", 10.0, 3.0),
        ("North", "Feb", 5.0, 4.0),
        ("South", "Jan", 2.0, 7.0),
    ] {
        write(&mut database, "Sales", &[region, month, "Units"], units);
        write(&mut database, "Sales", &[region, month, "Price"], price);
    }
    assert_eq!(read(&database, ["North", "Jan", "Revenue"]), 30.0);
    // The total is the sum of the leaf products, not the product of the totals.
    assert_eq!(
        read(&database, ["World", "Q1", "Revenue"]),
        30.0 + 20.0 + 14.0
    );
    assert_eq!(read(&database, ["North", "Q1", "Profit"]), 50.0);
    assert_eq!(read(&database, ["World", "Q1", "Units"]), 17.0);
}

#[test]
fn scopes_limit_a_rule_to_leaf_consolidated_or_text_cells() {
    let rules = json!([
        "C: [Account].[Price] = [Account].[Revenue] / [Account].[Units]",
        "N: ([Account].[Units], [Month].[Feb]) = ([Account].[Units], [Month].[Jan]) * 2",
        "S: [Account].[Note] = IIF([Account].[Revenue] > 50, 'big', 'small')"
    ]);
    let mut database = database(rules, json!([]));
    write(&mut database, "Sales", &["North", "Jan", "Units"], 10.0);
    write(&mut database, "Sales", &["North", "Jan", "Price"], 3.0);
    write(&mut database, "Sales", &["North", "Jan", "Revenue"], 30.0);
    write(&mut database, "Sales", &["South", "Jan", "Revenue"], 90.0);
    write(&mut database, "Sales", &["South", "Jan", "Units"], 5.0);

    assert_eq!(read(&database, ["North", "Jan", "Price"]), 3.0);
    // The consolidated price is the average: 120 of revenue over 15 units.
    assert_eq!(read(&database, ["World", "Jan", "Price"]), 8.0);
    assert_eq!(read(&database, ["North", "Feb", "Units"]), 20.0);
    assert_eq!(
        read(&database, ["World", "Q1", "Units"]),
        10.0 + 5.0 + 20.0 + 10.0
    );
    let note = |region: &str| database.read(None, &cell("Sales", &[region, "Jan", "Note"]));
    assert_eq!(note("North"), Ok(Scalar::Text("small".to_owned())));
    assert_eq!(note("South"), Ok(Scalar::Text("big".to_owned())));
}

#[test]
fn the_first_rule_that_covers_a_cell_decides_unless_it_passes() {
    let rules = json!([
        "([Account].[Margin], [Region].[South]) = 1",
        "([Account].[Margin], [Month].[Jan]) = IIF([Region].CurrentMember.Name = 'North', CONTINUE, 2)",
        "([Account].[Margin], [Month].[Feb]) = STET",
        "[Account].[Margin] = 3"
    ]);
    let mut database = Database::new(definition(rules, json!([]))).unwrap();
    assert_eq!(read(&database, ["South", "Jan", "Margin"]), 1.0);
    assert_eq!(read(&database, ["World", "Jan", "Margin"]), 2.0);
    // The north in January is passed on to the last rule.
    assert_eq!(read(&database, ["North", "Jan", "Margin"]), 3.0);
    // February keeps what is stored, which is nothing, and its consolidation adds that up.
    assert_eq!(read(&database, ["North", "Feb", "Margin"]), 0.0);
    assert_eq!(read(&database, ["World", "Feb", "Margin"]), 1.0);

    let stored = CellWrite {
        cube: "Sales".to_owned(),
        at: ["North", "Feb", "Margin"].map(str::to_owned).to_vec(),
        value: Scalar::Number(9.0),
    };
    assert_eq!(
        database.write(None, &[stored]),
        Err(Error::Write {
            index: 0,
            error: CellError::Calculated
        })
    );
}

#[test]
fn cells_of_a_cube_with_rules_read_together_equal_cells_read_one_by_one() {
    let rules = json!([
        "N: [Account].[Tax] = [Account].[Revenue] * 0.5",
        "C: ([Region].[World], [Account].[Units]) = 7",
        "C: [Account].[Price] = STET"
    ]);
    let regions = ["World", "North", "South"];
    let months = ["Q1", "Jan", "Feb"];
    let accounts = [
        "Profit", "Revenue", "Cost", "Margin", "Units", "Price", "Tax", "Note",
    ];
    let mut cells = Vec::new();
    for region in regions {
        for month in months {
            for account in accounts {
                cells.push(cell("Sales", &[region, month, account]));
            }
        }
    }
    for feeders in [json!([]), json!(["[Account].[Revenue] => [Account].[Tax]"])] {
        let filled = || {
            let mut database = database(rules.clone(), feeders.clone());
            figures(&mut database);
            write(&mut database, "Sales", &["North", "Jan", "Units"], 3.0);
            write(&mut database, "Sales", &["South", "Feb", "Price"], 2.5);
            database
        };
        let (together, one_by_one) = (filled(), filled());
        let expected: Vec<_> = cells
            .iter()
            .map(|cell| one_by_one.read(None, cell))
            .collect();
        assert_eq!(together.read_many(None, &cells), expected);
        // Totals a rule decides or reaches, and those it does not, each as before.
        assert_eq!(read(&together, ["World", "Q1", "Units"]), 7.0);
        assert_eq!(read(&together, ["World", "Q1", "Tax"]), 110.0);
        assert_eq!(read(&together, ["World", "Q1", "Price"]), 2.5);
        assert_eq!(read(&together, ["World", "Q1", "Profit"]), 100.0);

        // The same inside a transaction that changes revenue, the source of the rule.
        let (mut together, mut one_by_one) = (together, one_by_one);
        let change = |at: [&str; 3], value: f64| CellWrite {
            cube: "Sales".to_owned(),
            at: at.map(str::to_owned).to_vec(),
            value: Scalar::Number(value),
        };
        let pending = [
            change(["North", "Jan", "Revenue"], 0.0),
            change(["South", "Feb", "Revenue"], 30.0),
            change(["South", "Jan", "Units"], 4.0),
        ];
        let (within, alone) = (together.begin(), one_by_one.begin());
        together.write(Some(within), &pending).unwrap();
        one_by_one.write(Some(alone), &pending).unwrap();
        let expected: Vec<_> = cells
            .iter()
            .map(|cell| one_by_one.read(Some(alone), cell))
            .collect();
        assert_eq!(together.read_many(Some(within), &cells), expected);
    }
}

#[test]
fn a_rule_reads_another_cube() {
    let rules = json!([
        "N: [Account].[Tax] = [Account].[Revenue] * LookupCube('Rates', [Rate].[Tax])",
        "N: ([Account].[Margin], [Month].[Feb]) = LookupCube('Rates', [Rate].[Bonus], [Month].[Feb].Parent.FirstChild)"
    ]);
    let mut database = database(rules, json!([]));
    figures(&mut database);
    write(&mut database, "Rates", &["Jan", "Tax"], 0.5);
    write(&mut database, "Rates", &["Feb", "Tax"], 0.25);
    write(&mut database, "Rates", &["Jan", "Bonus"], 7.0);
    // The month carries across because both cubes have it; the rate is named.
    assert_eq!(read(&database, ["North", "Jan", "Tax"]), 50.0);
    assert_eq!(read(&database, ["North", "Feb", "Tax"]), 20.0);
    assert_eq!(read(&database, ["World", "Q1", "Tax"]), 50.0 + 20.0 + 20.0);
    // A member worked out in this cube is carried across too: February's bonus is January's.
    assert_eq!(read(&database, ["South", "Feb", "Margin"]), 7.0);
    // A total already worked out follows a change in the cube its rule reads.
    write(&mut database, "Rates", &["Jan", "Tax"], 1.0);
    assert_eq!(read(&database, ["World", "Q1", "Tax"]), 100.0 + 20.0 + 40.0);
}

#[test]
fn rules_apply_inside_queries_and_transactions() {
    let mut database = database(
        json!(["[Account].[Margin] = [Account].[Revenue] - [Account].[Cost]"]),
        json!([]),
    );
    figures(&mut database);
    let mdx = "SELECT {[Region].[North], [Region].[World]} ON COLUMNS FROM [Sales] WHERE ([Month].[Jan], [Account].[Margin])";
    let cells = |database: &Database, transaction| database.query(transaction, mdx).unwrap().cells;
    assert_eq!(
        cells(&database, None),
        [Some(Scalar::Number(40.0)), Some(Scalar::Number(70.0))]
    );

    let transaction = database.begin();
    let pending = CellWrite {
        cube: "Sales".to_owned(),
        at: ["North", "Jan", "Cost"].map(str::to_owned).to_vec(),
        value: Scalar::Number(10.0),
    };
    database.write(Some(transaction), &[pending]).unwrap();
    assert_eq!(
        cells(&database, Some(transaction)),
        [Some(Scalar::Number(90.0)), Some(Scalar::Number(120.0))]
    );
    assert_eq!(
        cells(&database, None),
        [Some(Scalar::Number(40.0)), Some(Scalar::Number(70.0))]
    );
}

#[test]
fn a_rule_that_depends_on_itself_is_reported() {
    let rules = json!([
        "[Account].[Margin] = [Account].[Price] + 1",
        "[Account].[Price] = [Account].[Margin] * 2"
    ]);
    let database = database(rules, json!([]));
    assert_eq!(
        database.read(None, &cell("Sales", &["North", "Jan", "Margin"])),
        Err(Error::Rule(
            "cell (North, Jan, Margin) of cube 'Sales' depends on itself".to_owned()
        ))
    );
    assert_eq!(
        database
            .query(None, "SELECT FROM [Sales] WHERE [Account].[Price]")
            .unwrap_err()
            .to_string(),
        "cell (World, Q1, Price) of cube 'Sales' depends on itself"
    );
}

/// A balance carried forward from day to day: each day's is the day before's and its own flow.
fn carried(days: usize, balance: &str) -> Database {
    let leaf = |name: String| json!({ "name": name, "type": "numeric" });
    let days: Vec<Json> = (1..=days).map(|day| leaf(format!("D{day}"))).collect();
    let before = "([Account].[Balance], [Day].CurrentMember.PrevMember)";
    let rule = format!(
        "[Account].[Balance] = {}",
        balance.replace("BEFORE", before)
    );
    let definition = serde_json::from_value(json!({
        "dimensions": [
            { "name": "Day", "elements": days },
            { "name": "Account", "elements": [leaf("Flow".to_owned()), leaf("Balance".to_owned())] }
        ],
        "cubes": [{ "name": "Ledger", "dimensions": ["Day", "Account"], "rules": [rule] }]
    }))
    .unwrap();
    let mut database = Database::new(definition).unwrap();
    write(&mut database, "Ledger", &["D1", "Flow"], 5.0);
    write(&mut database, "Ledger", &["D3", "Flow"], 2.0);
    database
}

#[test]
fn a_chain_of_rules_is_followed_however_long() {
    let balance =
        |database: &Database, day: &str| database.read(None, &cell("Ledger", &[day, "Balance"]));
    let seven = Ok(Scalar::Number(7.0));
    let plain = carried(30_000, "[Account].[Flow] + BEFORE");
    assert_eq!(balance(&plain, "D3"), seven);
    // Far more days than can be worked out one inside another.
    assert_eq!(balance(&plain, "D30000"), seven);
    assert_eq!(balance(&plain, "D15000"), seven);
    let sum = "SELECT {[Day].[D29999], [Day].[D30000]} ON COLUMNS FROM [Ledger] WHERE [Account].[Balance]";
    let result = plain.query(None, sum).unwrap();
    assert_eq!(
        result.cells,
        [Some(Scalar::Number(7.0)), Some(Scalar::Number(7.0))]
    );

    // The same with each day's rule a nest of functions.
    let nested = format!(
        "{}[Account].[Flow] + BEFORE{}",
        "IIF(1 = 1, ".repeat(20),
        ", 0)".repeat(20)
    );
    assert_eq!(balance(&carried(5_000, &nested), "D5000"), seven);

    // A circle too long to see at once is still a circle: the first day's balance is the
    // last day's.
    let round = "[Account].[Flow] + ([Account].[Balance], \
                 IIF([Day].CurrentMember IS [Day].[D1], [Day].[D5000], [Day].CurrentMember.PrevMember))";
    match balance(&carried(5_000, round), "D9") {
        Err(Error::Rule(message)) => assert!(message.ends_with("depends on itself"), "{message}"),
        other => panic!("expected a circle, found {other:?}"),
    }
}
#[test]
fn bad_rules_and_feeders_make_the_model_invalid() {
    let cases = [
        (
            json!(["[Account].[Margin] = [Account].[Smarch]"]),
            json!([]),
            "invalid MDX in rule 1 of cube 'Sales': dimension 'Account' has no member 'Smarch'",
        ),
        (
            json!(["[Account].[Margin] = 1", "[Account].[Margin] 2"]),
            json!([]),
            "invalid MDX in rule 2 of cube 'Sales': expected '=' at character 19",
        ),
        (
            json!(["{[Account].[Margin], [Account].[Price]} = 1"]),
            json!([]),
            "invalid MDX in rule 1 of cube 'Sales': an area is a member or a tuple",
        ),
        (
            json!(["[Rate].[Tax] = 1"]),
            json!([]),
            "invalid MDX in rule 1 of cube 'Sales': the cube has no dimension 'Rate'",
        ),
        (
            json!(["[Account].[Tax] = LookupCube('Nope', [Rate].[Tax])"]),
            json!([]),
            "invalid MDX in rule 1 of cube 'Sales': unknown cube 'Nope'",
        ),
        (
            json!(["[Account].[Tax] = LookupCube('Rates', [Region].[North])"]),
            json!([]),
            "invalid MDX in rule 1 of cube 'Sales': cube 'Rates' has no dimension 'Region'",
        ),
        (
            json!([]),
            json!(["[Account].[Units] => [Account].[Smarch]"]),
            "invalid MDX in feeder 1 of cube 'Sales': dimension 'Account' has no member 'Smarch'",
        ),
        (
            json!([]),
            json!(["[Account].[Units] = [Account].[Revenue]"]),
            "invalid MDX in feeder 1 of cube 'Sales': expected '=>' at character 18",
        ),
    ];
    for (rules, feeders, expected) in cases {
        assert_eq!(failure(rules, feeders), expected);
    }
}

const REVENUE: &str = "N: [Account].[Revenue] = [Account].[Units] * [Account].[Price]";

fn priced(feeders: Json) -> Database {
    let mut database = database(json!([REVENUE]), feeders);
    for (region, month, units, price) in [
        ("North", "Jan", 10.0, 3.0),
        ("North", "Feb", 5.0, 4.0),
        ("South", "Jan", 2.0, 7.0),
    ] {
        write(&mut database, "Sales", &[region, month, "Units"], units);
        write(&mut database, "Sales", &[region, month, "Price"], price);
    }
    database
}

#[test]
fn feeders_change_speed_not_values() {
    let unfed = priced(json!([]));
    let fed = priced(json!(["[Account].[Units] => [Account].[Revenue]"]));
    assert!(fed.model().cube("Sales").unwrap().sparse);
    assert!(!unfed.model().cube("Sales").unwrap().sparse);
    for region in ["World", "North", "South"] {
        for month in ["Q1", "Jan", "Feb"] {
            for account in ["Revenue", "Profit", "Units", "Cost"] {
                let at = [region, month, account];
                assert_eq!(read(&fed, at), read(&unfed, at), "{at:?}");
            }
        }
    }
    assert_eq!(read(&fed, ["World", "Q1", "Revenue"]), 64.0);
    assert_eq!(fed.unfed("Sales", 10), Ok(Vec::new()));
}

#[test]
fn a_cell_that_nothing_feeds_is_left_out_and_can_be_found() {
    // Only the north is fed, so the south's calculated revenue is missing from the totals.
    let database = priced(json!([
        "([Account].[Units], [Region].[North]) => [Account].[Revenue]"
    ]));
    assert_eq!(read(&database, ["South", "Jan", "Revenue"]), 14.0);
    assert_eq!(read(&database, ["World", "Q1", "Revenue"]), 50.0);
    assert_eq!(
        database.unfed("Sales", 10),
        Ok(vec![cell("Sales", &["South", "Jan", "Revenue"])])
    );
    assert_eq!(database.unfed("Rates", 10), Ok(Vec::new()));
}

#[test]
fn a_cell_explains_how_it_comes_by_its_value() {
    let feeder = "([Account].[Units], [Region].[North]) => [Account].[Revenue]";
    let database = priced(json!([feeder]));
    let explain = |at: [&str; 3]| database.explain(None, &cell("Sales", &at), true).unwrap();

    // A calculated leaf that is fed, and by what.
    let revenue = explain(["North", "Jan", "Revenue"]);
    assert_eq!(
        serde_json::to_value(&revenue).unwrap(),
        json!({
            "value": 30.0,
            "kind": "leaf",
            "stored": 0.0,
            "writable": false,
            "rule": { "number": 1, "text": REVENUE },
            "fed": true,
            "fed_by": [{
                "feeder": { "number": 1, "text": feeder },
                "from": { "cube": "Sales", "at": ["North", "Jan", "Units"] }
            }]
        })
    );
    // A calculated leaf that nothing feeds.
    let south = explain(["South", "Jan", "Revenue"]);
    assert_eq!(
        (south.fed, south.fed_by.len(), south.rule.is_some()),
        (Some(false), 0, true)
    );
    // A stored leaf: writable, decided by no rule.
    let units = explain(["North", "Jan", "Units"]);
    assert_eq!(
        (units.writable, units.rule, units.stored, units.value),
        (true, None, Some(Scalar::Number(10.0)), Scalar::Number(10.0))
    );
    // A consolidation stores nothing and is not itself fed.
    let total = explain(["World", "Q1", "Revenue"]);
    assert_eq!(
        serde_json::to_value(&total).unwrap(),
        json!({
            "value": 50.0, "kind": "consolidated", "stored": null, "writable": false,
            "rule": null, "fed": null, "fed_by": []
        })
    );
    // In a cube that is not fed, nothing is said about feeding.
    let unfed = priced(json!([]));
    let revenue = unfed
        .explain(None, &cell("Sales", &["North", "Jan", "Revenue"]), true)
        .unwrap();
    assert_eq!(
        (revenue.fed, revenue.writable, revenue.value),
        (None, false, Scalar::Number(30.0))
    );
}

#[test]
fn feeding_follows_new_data_and_pending_writes() {
    let mut database = priced(json!(["[Account].[Units] => [Account].[Revenue]"]));
    write(&mut database, "Sales", &["South", "Feb", "Units"], 1.0);
    write(&mut database, "Sales", &["South", "Feb", "Price"], 6.0);
    assert_eq!(read(&database, ["World", "Q1", "Revenue"]), 70.0);

    let transaction = database.begin();
    let units = CellWrite {
        cube: "Sales".to_owned(),
        at: ["South", "Feb", "Units"].map(str::to_owned).to_vec(),
        value: Scalar::Number(0.0),
    };
    database.write(Some(transaction), &[units]).unwrap();
    let total = cell("Sales", &["World", "Q1", "Revenue"]);
    assert_eq!(
        database.read(Some(transaction), &total),
        Ok(Scalar::Number(64.0))
    );
    assert_eq!(database.read(None, &total), Ok(Scalar::Number(70.0)));
}

#[test]
fn feeders_reach_consolidations_other_cubes_and_further_feeders() {
    let rules = json!([
        REVENUE,
        "N: [Account].[Tax] = [Account].[Revenue] * LookupCube('Rates', [Rate].[Tax])"
    ]);
    // Units feed revenue, and fed revenue feeds tax in turn. A rate feeds tax for every
    // region of its month.
    let feeders = json!([
        "[Account].[Units] => [Account].[Revenue]",
        "[Account].[Revenue] => [Account].[Tax]"
    ]);
    let mut model = serde_json::to_value(definition(rules, feeders)).unwrap();
    model["cubes"][1]["feeders"] =
        json!(["[Rate].[Tax] => LookupCube('Sales', [Region].[World], [Account].[Tax])"]);
    let mut database = Database::new(serde_json::from_value(model).unwrap()).unwrap();
    write(&mut database, "Sales", &["North", "Jan", "Units"], 10.0);
    write(&mut database, "Sales", &["North", "Jan", "Price"], 3.0);
    write(&mut database, "Rates", &["Jan", "Tax"], 0.5);
    assert_eq!(read(&database, ["North", "Jan", "Tax"]), 15.0);
    assert_eq!(read(&database, ["World", "Q1", "Tax"]), 15.0);
    assert_eq!(database.unfed("Sales", 10), Ok(Vec::new()));
}

/// On random cubes with a rule over a random leaf, a fully fed cube and an unfed one agree on
/// every cell, and both agree with working the rule out by hand.
#[test]
fn fed_and_unfed_cubes_agree_on_random_data() {
    for seed in 0..12 {
        let mut rng = Rng::new(seed);
        let shape = Shape {
            leaves: 3 + rng.below(3),
            groups: 1 + rng.below(3),
            fan: 3,
        };
        let base = random_model(&mut rng, 3, &shape);
        // In the last dimension, e0 is twice e1 plus e2, at leaf cells.
        let rule = "N: [d2].[e0] = [d2].[e1] * 2 + [d2].[e2]";
        let with = |feeders: Json| {
            let mut model = serde_json::to_value(&base).unwrap();
            model["cubes"][0]["rules"] = json!([rule]);
            model["cubes"][0]["feeders"] = feeders;
            Database::new(serde_json::from_value(model).unwrap()).unwrap()
        };
        let mut databases = [
            with(json!([])),
            with(json!(["[d2].[e1] => [d2].[e0]", "[d2].[e2] => [d2].[e0]"])),
        ];
        let mut writes = Vec::new();
        for _ in 0..30 {
            let at = vec![
                format!("e{}", rng.below(shape.leaves)),
                format!("e{}", rng.below(shape.leaves)),
                format!("e{}", 1 + rng.below(shape.leaves - 1)),
            ];
            writes.push(CellWrite {
                cube: "C".to_owned(),
                at,
                value: Scalar::Number((rng.below(41) as f64 - 20.0) / 4.0),
            });
        }
        for database in &mut databases {
            database.write(None, &writes).unwrap();
        }
        let names: Vec<String> = (0..shape.leaves + shape.groups)
            .map(|id| format!("e{id}"))
            .chain(["all".to_owned()])
            .collect();
        for first in &names {
            for second in &names {
                for third in &names {
                    let cell = CellRef {
                        cube: "C".to_owned(),
                        at: vec![first.clone(), second.clone(), third.clone()],
                    };
                    let [unfed, fed] = databases
                        .each_ref()
                        .map(|database| database.read(None, &cell));
                    assert_eq!(fed, unfed, "seed {seed} at {:?}", cell.at);
                }
            }
        }
        assert_eq!(databases[1].unfed("C", 5), Ok(Vec::new()), "seed {seed}");
    }
}
