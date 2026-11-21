use epiphany_core::Scalar;
use epiphany_core::database::{CellWrite, Database, Error};
use epiphany_core::model::{Model, ModelDef, ModelError};
use serde_json::{Value as Json, json};

/// A cube `Sales` over `Period` (Year over two quarters of two months each) and `Region`,
/// with the given subsets on `Period` and views on the cube.
fn definition(subsets: Json, views: Json) -> Json {
    let quarter = |name: &str, months: [&str; 2]| {
        json!({ "name": name, "type": "consolidated",
        "children": [{ "name": months[0] }, { "name": months[1] }] })
    };
    let month = |name: &str, code: &str| json!({ "name": name, "type": "numeric", "attributes": { "Code": code } });
    json!({
        "dimensions": [
            {
                "name": "Period",
                "attributes": { "Code": "alias" },
                "elements": [
                    { "name": "Year", "type": "consolidated", "children": [{ "name": "Q1" }, { "name": "Q2" }] },
                    quarter("Q1", ["Jan", "Feb"]),
                    quarter("Q2", ["Apr", "May"]),
                    month("Jan", "M01"), month("Feb", "M02"), month("Apr", "M04"), month("May", "M05")
                ],
                "subsets": subsets
            },
            { "name": "Region", "elements": [
                { "name": "World", "type": "consolidated", "children": [{ "name": "North" }, { "name": "South" }] },
                { "name": "North", "type": "numeric" },
                { "name": "South", "type": "numeric" }
            ]}
        ],
        "cubes": [{ "name": "Sales", "dimensions": ["Period", "Region"], "views": views }]
    })
}

fn parse(definition: Json) -> ModelDef {
    serde_json::from_value(definition).unwrap()
}

fn compile(subsets: Json, views: Json) -> Result<Model, ModelError> {
    Model::compile(&parse(definition(subsets, views)))
}

fn subset(model: &Model, name: &str) -> Vec<String> {
    let period = model.dimension("Period").unwrap();
    let subset = period.subset(name).expect("subset exists");
    let names = subset
        .elements
        .iter()
        .map(|&id| period.elements[id as usize].name.clone());
    names.collect()
}

fn failure(subsets: Json, views: Json) -> String {
    compile(subsets, views).unwrap_err().to_string()
}

#[test]
fn a_subset_lists_elements_or_selects_them_with_mdx() {
    let model = compile(
        json!({
            "Spring": { "elements": ["Apr", "m05", "Apr"] },
            "Months": { "mdx": "TM1FilterByLevel([Period].Members, 0)" },
            "Quarters": { "mdx": "[Period].[Year].Children" },
            "Remaining": { "mdx": "Except(TM1SubsetToSet([Period], 'Months'), [Period].[Q1].Children)" }
        }),
        json!({}),
    )
    .unwrap();
    // A listed subset keeps its order and its repeats, and may name elements by alias.
    assert_eq!(subset(&model, "spring"), ["Apr", "May", "Apr"]);
    assert_eq!(subset(&model, "Months"), ["Jan", "Feb", "Apr", "May"]);
    assert_eq!(subset(&model, "Quarters"), ["Q1", "Q2"]);
    // Subsets are worked out in name order, so one may build on those before it.
    assert_eq!(subset(&model, "Remaining"), ["Apr", "May"]);
    assert!(
        model
            .dimension("Period")
            .unwrap()
            .subset("Summer")
            .is_none()
    );
}

#[test]
fn a_bad_subset_makes_the_model_invalid() {
    let cases = [
        (
            json!({ "S": { "elements": ["Jan", "Smarch"] } }),
            "unknown element 'Smarch' in subset 'S' of dimension 'Period'",
        ),
        (
            json!({ "S": { "mdx": "[Period].[Smarch].Children" } }),
            "invalid MDX in subset 'S' of dimension 'Period': dimension 'Period' has no member 'Smarch'",
        ),
        (
            json!({ "S": { "mdx": "{[Period].[Jan]" } }),
            "invalid MDX in subset 'S' of dimension 'Period': expected ',' at character 15",
        ),
        (
            json!({ "S": { "mdx": "[Region].Members" } }),
            "invalid MDX in subset 'S' of dimension 'Period': the cube has no dimension 'Region'",
        ),
        (
            json!({ "A": { "mdx": "TM1SubsetToSet([Period], 'B')" }, "B": { "elements": ["Jan"] } }),
            "invalid MDX in subset 'A' of dimension 'Period': dimension 'Period' has no subset 'B'",
        ),
        (
            json!({ "S": { "elements": [] }, "s ": { "elements": [] } }),
            "duplicate subset 's ' in dimension 'Period'",
        ),
    ];
    for (subsets, expected) in cases {
        assert_eq!(failure(subsets, json!({})), expected);
    }
}

#[test]
fn a_bad_view_makes_the_model_invalid() {
    let cases = [
        (
            json!({ "V": { "mdx": "SELECT [Period].[Smarch] ON COLUMNS FROM [Sales]" } }),
            "invalid MDX in view 'V' of cube 'Sales': dimension 'Period' has no member 'Smarch'",
        ),
        (
            json!({ "V": { "mdx": "SELECT FROM [Stock]" } }),
            "invalid MDX in view 'V' of cube 'Sales': the view selects from 'Stock'",
        ),
        (
            json!({ "V": { "mdx": "SELECT FROM" } }),
            "invalid MDX in view 'V' of cube 'Sales': expected a cube name at character 11",
        ),
    ];
    for (views, expected) in cases {
        assert_eq!(failure(json!({}), views), expected);
    }
}

fn database() -> Database {
    let subsets = json!({ "First half": { "elements": ["Jan", "Feb"] } });
    let views = json!({ "By region": { "mdx": "
        SELECT [Region].Members ON COLUMNS,
               TM1SubsetToSet([Period], 'First half') ON ROWS
        FROM [Sales]" } });
    let mut database = Database::new(parse(definition(subsets, views))).unwrap();
    let write = |at: [&str; 2], value: f64| CellWrite {
        cube: "Sales".to_owned(),
        at: at.map(str::to_owned).to_vec(),
        value: Scalar::Number(value),
    };
    let writes = [write(["Jan", "North"], 5.0), write(["Feb", "South"], 7.0)];
    database.write(None, &writes).unwrap();
    database
}

#[test]
fn a_view_runs_its_query_and_may_use_subsets() {
    let mut database = database();
    let result = database.view(None, "sales", "by region").unwrap();
    let cells: Vec<Option<Scalar>> = [5.0, 5.0, 0.0, 7.0, 0.0, 7.0]
        .map(|n| Some(Scalar::Number(n)))
        .to_vec();
    assert_eq!(result.cells, cells);
    assert_eq!(result.axes[1].tuples.len(), 2);

    // Inside a transaction a view shows that transaction's pending writes.
    let transaction = database.begin();
    let write = CellWrite {
        cube: "Sales".to_owned(),
        at: vec!["Jan".to_owned(), "South".to_owned()],
        value: Scalar::Number(1.0),
    };
    database.write(Some(transaction), &[write]).unwrap();
    let pending = database
        .view(Some(transaction), "Sales", "By region")
        .unwrap();
    assert_eq!(pending.cells[0], Some(Scalar::Number(6.0)));

    let missing = database.view(None, "Sales", "Nope");
    assert_eq!(
        missing,
        Err(Error::NotFound(
            "cube 'Sales' has no view 'Nope'".to_owned()
        ))
    );
}

#[test]
fn changing_only_subsets_and_views_keeps_every_cell() {
    let mut database = database();
    let subsets = json!({ "Second half": { "mdx": "[Period].[Q2].Children" } });
    let update = database
        .update_model(1, parse(definition(subsets, json!({}))))
        .unwrap();
    assert_eq!((update.version, update.dropped_cells), (2, 0));
    assert_eq!(database.cells().len(), 2);
    assert!(
        database
            .model()
            .dimension("Period")
            .unwrap()
            .subset("First half")
            .is_none()
    );
    assert!(matches!(
        database.view(None, "Sales", "By region"),
        Err(Error::NotFound(_))
    ));
    // Whether a subset is public or private may be said, and makes no difference here.
    let query =
        "SELECT TM1SubsetToSet([Period].[Period], 'Second half', 'public') ON COLUMNS FROM [Sales]";
    assert_eq!(database.query(None, query).unwrap().axes[0].tuples.len(), 2);
}

#[test]
fn subsets_and_views_survive_a_round_trip_through_json() {
    let subsets = json!({ "Spring": { "elements": ["Apr", "May"] }, "Months": { "mdx": "[Period].Members" } });
    let views = json!({ "All": { "mdx": "SELECT FROM [Sales]" } });
    let original = definition(subsets, views);
    assert_eq!(
        serde_json::to_value(parse(original.clone())).unwrap()["dimensions"][0]["subsets"],
        original["dimensions"][0]["subsets"]
    );
    assert_eq!(
        serde_json::to_value(parse(original.clone())).unwrap()["cubes"][0]["views"],
        original["cubes"][0]["views"]
    );
}
