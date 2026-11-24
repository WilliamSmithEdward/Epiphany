//! Expressions put together at random, most of them nonsense, are asked of the example
//! model. Whatever each comes to, an answer or a refusal, it must come back: none may
//! bring the evaluator down.

// Only the random numbers are wanted of it here.
#[allow(dead_code)]
mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};

use common::Rng;
use epiphany_core::Scalar;
use epiphany_core::database::{CellWrite, Database};
use epiphany_core::model::ModelDef;

const FUNCTIONS: [&str; 116] = [
    "Sum",
    "Aggregate",
    "Avg",
    "Min",
    "Max",
    "Median",
    "Var",
    "VarP",
    "Stdev",
    "StdevP",
    "Count",
    "Union",
    "Except",
    "Intersect",
    "Distinct",
    "Head",
    "Tail",
    "Subset",
    "TopCount",
    "BottomCount",
    "TopSum",
    "BottomSum",
    "TopPercent",
    "BottomPercent",
    "Generate",
    "LookupCube",
    "TM1SubsetToSet",
    "TM1SubsetAll",
    "Members",
    "TM1FilterByLevel",
    "TM1FilterByPattern",
    "TM1Sort",
    "TM1SortByIndex",
    "TM1Member",
    "TM1TupleSize",
    "IsLeaf",
    "CoalesceEmpty",
    "CrossJoin",
    "Filter",
    "NonEmpty",
    "NonEmptyCrossJoin",
    "AddCalculatedMembers",
    "StripCalculatedMembers",
    "Order",
    "Hierarchize",
    "Descendants",
    "Ancestor",
    "Ancestors",
    "Ascendants",
    "MemberToStr",
    "TupleToStr",
    "SetToStr",
    "IIF",
    "IsEmpty",
    "IsAncestor",
    "IsSibling",
    "Unorder",
    "Extract",
    "Rank",
    "StrToMember",
    "StrToTuple",
    "StrToSet",
    "Abs",
    "Int",
    "Fix",
    "Sqr",
    "Exp",
    "Log",
    "Log10",
    "Sgn",
    "Round",
    "Mod",
    "Power",
    "Len",
    "UCase",
    "LCase",
    "Trim",
    "Val",
    "Str",
    "CStr",
    "Left",
    "Right",
    "Mid",
    "InStr",
    "Replace",
    "Now",
    "Date",
    "DateSerial",
    "CDate",
    "Year",
    "Month",
    "Day",
    "Hour",
    "Minute",
    "Second",
    "Weekday",
    "DateAdd",
    "DateDiff",
    "Format",
    "Cousin",
    "ParallelPeriod",
    "PeriodsToDate",
    "Ytd",
    "Qtd",
    "Mtd",
    "Wtd",
    "LastPeriods",
    "OpeningPeriod",
    "ClosingPeriod",
    "DrilldownMember",
    "TM1DrilldownMember",
    "DrilldownLevel",
    "DrillupMember",
    "ToggleDrillState",
    "Stet",
    "Continue",
];
const METHODS: [&str; 27] = [
    "Dimension",
    "Hierarchy",
    "DefaultMember",
    "FirstSibling",
    "LastSibling",
    "Level",
    "Levels",
    "Ordinal",
    "PrevMember",
    "NextMember",
    "Lag",
    "Lead",
    "AllMembers",
    "Siblings",
    "Count",
    "Item",
    "Members",
    "Children",
    "Parent",
    "CurrentMember",
    "Name",
    "UniqueName",
    "Caption",
    "Value",
    "Properties",
    "FirstChild",
    "LastChild",
];
const ATOMS: [&str; 44] = [
    "0",
    "1",
    "2",
    "-1",
    "0.5",
    "1000000",
    "1e300",
    "'",
    "''",
    "'abc'",
    "'é'",
    "'[Period].[Jan]'",
    "'{[Period].Members}'",
    "'*a*'",
    "'yyyy-mm-dd hh:mm AM/PM'",
    "'#,##0.0%;(0);\"z\"'",
    "'2026-11-22 18:05'",
    "'ww'",
    "\"Code\"",
    "NULL",
    "[Period].[Jan]",
    "[Period].[Jun]",
    "[Period].[Q1]",
    "[Period].[H1]",
    "[Account].[Rent]",
    "[Account].[Revenue]",
    "[Account].[Net Income]",
    "[Entity].[US]",
    "[Entity].[Total Entity]",
    "[Version].[Actual]",
    "[Period]",
    "[Account]",
    "[Period].[Period]",
    "[Period].Members",
    "[Account].Members",
    "{}",
    "ALL",
    "DESC",
    "BDESC",
    "SELF",
    "LEAVES",
    "RECURSIVE",
    "POST",
    "[Nowhere].[Nobody]",
];
const OPERATORS: [&str; 15] = [
    "+", "-", "*", "/", "=", "<>", "<", ">", "<=", ">=", "AND", "OR", "XOR", "IS", ":",
];

fn pick<'a>(rng: &mut Rng, from: &[&'a str]) -> &'a str {
    from[rng.below(from.len())]
}

fn list(rng: &mut Rng, depth: usize, most: usize) -> String {
    let count = rng.below(most + 1);
    let items: Vec<String> = (0..count).map(|_| expression(rng, depth)).collect();
    items.join(", ")
}

fn expression(rng: &mut Rng, depth: usize) -> String {
    if depth == 0 {
        return pick(rng, &ATOMS).to_owned();
    }
    let depth = depth - 1;
    match rng.below(12) {
        0 | 1 => pick(rng, &ATOMS).to_owned(),
        2..=4 => format!("{}({})", pick(rng, &FUNCTIONS), list(rng, depth, 4)),
        5 => format!("{}.{}", expression(rng, depth), pick(rng, &METHODS)),
        6 => format!(
            "{}.{}({})",
            expression(rng, depth),
            pick(rng, &METHODS),
            list(rng, depth, 2)
        ),
        7 => format!(
            "{} {} {}",
            expression(rng, depth),
            pick(rng, &OPERATORS),
            expression(rng, depth)
        ),
        8 => format!("{{{}}}", list(rng, depth, 3)),
        9 => format!("({})", list(rng, depth, 3)),
        10 => format!("NOT {}", expression(rng, depth)),
        _ => format!(
            "CASE {} WHEN {} THEN {} ELSE {} END",
            expression(rng, depth),
            expression(rng, depth),
            expression(rng, depth),
            expression(rng, depth)
        ),
    }
}

/// The example model.
fn definition() -> ModelDef {
    let dimensions = [
        include_str!("fixtures/model/dimensions/account.json"),
        include_str!("fixtures/model/dimensions/entity.json"),
        include_str!("fixtures/model/dimensions/period.json"),
        include_str!("fixtures/model/dimensions/version.json"),
    ];
    let cube = include_str!("fixtures/model/cubes/general-ledger.json");
    ModelDef {
        dimensions: dimensions
            .iter()
            .map(|text| serde_json::from_str(text).unwrap())
            .collect(),
        cubes: vec![serde_json::from_str(cube).unwrap()],
        ..ModelDef::default()
    }
}

/// A few cells of the example model.
fn cells() -> [CellWrite; 4] {
    let cell = |at: [&str; 4], value: f64| CellWrite {
        cube: "General Ledger".to_owned(),
        at: at.map(str::to_owned).to_vec(),
        value: Scalar::Number(value),
    };
    [
        cell(["Product Revenue", "US", "Jan", "Actual"], 100.0),
        cell(["Product Revenue", "UK", "Feb", "Actual"], -100.0),
        cell(["Rent", "US", "Jan", "Actual"], 10.0),
        cell(["Salaries", "UK", "Jun", "Budget"], 40.0),
    ]
}

/// The example model with those cells in it.
fn database() -> Database {
    let mut database = Database::new(definition()).unwrap();
    database.write(None, &cells()).unwrap();
    database
}

/// How many expressions to try. More when asked for by `RANDOM_MDX`, as when looking for
/// trouble by hand.
fn tries() -> u64 {
    let asked = std::env::var("RANDOM_MDX").ok();
    asked.and_then(|count| count.parse().ok()).unwrap_or(1500)
}

#[test]
fn nothing_put_together_at_random_brings_the_evaluator_down() {
    let database = database();
    let mut fallen = Vec::new();
    for seed in 1..=tries() {
        let mut rng = Rng::new(seed);
        let depth = 1 + rng.below(3);
        let written = expression(&mut rng, depth);
        let queries = [
            format!(
                "WITH MEMBER [Account].[X] AS {written} SELECT {{[Account].[X]}} ON COLUMNS \
                 FROM [General Ledger] WHERE ([Period].[Feb], [Entity].[US])"
            ),
            format!("SELECT {written} ON COLUMNS FROM [General Ledger]"),
            format!(
                "SELECT NON EMPTY {written} ON ROWS, {{[Version].[Actual]}} ON COLUMNS \
                 FROM [General Ledger]"
            ),
        ];
        for mdx in queries {
            // An answer or a refusal, either will do.
            let asked = catch_unwind(AssertUnwindSafe(|| drop(database.query(None, &mdx))));
            if asked.is_err() {
                fallen.push(mdx);
            }
        }
    }
    assert!(
        fallen.is_empty(),
        "{} queries brought it down, the first of them:\n{}",
        fallen.len(),
        fallen[..fallen.len().min(5)].join("\n")
    );
}

const AREAS: [&str; 8] = [
    "[Account].[Rent]",
    "N: [Account].[Salaries]",
    "C: [Account].[Revenue]",
    "[Period].[Jan]",
    "([Account].[Rent], [Period].[Jan])",
    "N: ([Account].[Rent], [Entity].[US])",
    "[Version].[Budget]",
    "C: [Entity].[Total Entity]",
];
const FEEDERS: [&str; 4] = [
    "[Account].[Product Revenue] => [Account].[Rent]",
    "[Account].[Rent] => [Account].[Salaries]",
    "[Period].[Feb] => [Period].[Jan]",
    "[Account].[Rent] => ([Account].[Rent], [Period].[Jun])",
];

#[test]
fn no_rule_put_together_at_random_brings_the_engine_down() {
    let grid = "SELECT [Account].Members ON COLUMNS, [Period].Members ON ROWS FROM [General Ledger] \
                WHERE ([Entity].[Total Entity], [Version].[Actual])";
    let sparse = "SELECT NON EMPTY [Account].Members * [Period].Members ON ROWS, \
                  [Version].Members ON COLUMNS FROM [General Ledger]";
    let mut fallen = Vec::new();
    for seed in 1..=tries() / 5 {
        let mut rng = Rng::new(seed);
        let depth = 1 + rng.below(3);
        let mut definition = definition();
        let rules = 1 + rng.below(2);
        definition.cubes[0].rules = (0..rules)
            .map(|_| {
                format!(
                    "{} = {}",
                    pick(&mut rng, &AREAS),
                    expression(&mut rng, depth)
                )
            })
            .collect();
        if rng.below(2) == 0 {
            definition.cubes[0].feeders = vec![pick(&mut rng, &FEEDERS).to_owned()];
        }
        let written = format!(
            "{:?} {:?}",
            definition.cubes[0].rules, definition.cubes[0].feeders
        );
        // The model is taken or refused; taken, its cells are written and read every way.
        let tried = catch_unwind(AssertUnwindSafe(|| {
            let Ok(mut database) = Database::new(definition) else {
                return;
            };
            for cell in cells() {
                drop(database.write(None, &[cell]));
            }
            drop(database.query(None, grid));
            drop(database.query(None, sparse));
            drop(database.unfed("General Ledger", 10));
        }));
        if tried.is_err() {
            fallen.push(written);
        }
    }
    assert!(
        fallen.is_empty(),
        "{} sets of rules brought it down, the first of them:\n{}",
        fallen.len(),
        fallen[..fallen.len().min(5)].join("\n")
    );
}
