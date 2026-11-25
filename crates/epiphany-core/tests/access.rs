use std::collections::BTreeMap;

use epiphany_core::Scalar;
use epiphany_core::access::{Access, Permit};
use epiphany_core::cells::CellError;
use epiphany_core::database::{
    CellRef, CellRefs, CellWrite, CellWrites, Database, Error, Spread, SpreadMethod,
};
use epiphany_core::mdx::{Asker, QueryResult};
use epiphany_core::model::ModelDef;
use serde_json::json;

/// A cube `Sales` over `Region` (World over Europe and the Americas, and EU over Germany and
/// Spain beside it) and `Product` (All over A and B), with units sold in every country.
fn database() -> Database {
    let leaf = |name: &str| json!({ "name": name, "type": "numeric" });
    let total = |name: &str, children: &[&str]| {
        let children: Vec<_> = children
            .iter()
            .map(|name| json!({ "name": name }))
            .collect();
        json!({ "name": name, "type": "consolidated", "children": children })
    };
    let definition: ModelDef = serde_json::from_value(json!({
        "dimensions": [
            { "name": "Region", "elements": [
                total("World", &["Europe", "Americas"]),
                total("Europe", &["France", "Germany"]),
                total("Americas", &["US", "Canada"]),
                total("EU", &["Germany", "Spain"]),
                leaf("France"), leaf("Germany"), leaf("US"), leaf("Canada"), leaf("Spain")
            ]},
            { "name": "Product", "elements": [total("All", &["A", "B"]), leaf("A"), leaf("B")] }
        ],
        "cubes": [{ "name": "Sales", "dimensions": ["Region", "Product"] }]
    }))
    .unwrap();
    let mut database = Database::new(definition).unwrap();
    let writes: Vec<CellWrite> = [
        ("France", 1.0),
        ("Germany", 2.0),
        ("US", 4.0),
        ("Canada", 8.0),
        ("Spain", 16.0),
    ]
    .iter()
    .map(|&(country, units)| CellWrite {
        cube: "Sales".to_owned(),
        at: vec![country.to_owned(), "A".to_owned()],
        value: Scalar::Number(units),
    })
    .collect();
    database.write(None, &writes).unwrap();
    database
}

fn access(database: &Database, given: &[(&str, &str, Permit)]) -> Access {
    let mut permits: BTreeMap<String, BTreeMap<String, Permit>> = BTreeMap::new();
    for &(dimension, element, permit) in given {
        let elements = permits.entry(dimension.to_owned()).or_default();
        elements.insert(element.to_owned(), permit);
    }
    Access::new(database.model(), &permits)
}

fn permit(database: &Database, access: &Access, element: &str) -> Permit {
    let model = database.model();
    let region = model.dimension_id("Region").unwrap();
    let element = model
        .dimension("Region")
        .unwrap()
        .element_id(element)
        .unwrap();
    access.of(region, element)
}

fn query(database: &Database, access: &Access, mdx: &str) -> Result<QueryResult, Error> {
    let asker = Asker {
        access: Some(access),
        ..Asker::ANYONE
    };
    database.query_as(None, mdx, &asker)
}

fn names(result: &QueryResult) -> Vec<&str> {
    let tuples = result.axes[0].tuples.iter();
    tuples.map(|tuple| tuple[0].name.as_str()).collect()
}

#[test]
fn a_permit_holds_beneath_its_element_unless_a_nearer_one_has_its_own() {
    let database = database();
    let given = [
        ("Region", "Europe", Permit::Hidden),
        ("Region", "France", Permit::Read),
        ("Region", "EU", Permit::Read),
        ("Nowhere", "Nothing", Permit::Read),
        ("Region", "Atlantis", Permit::Read),
    ];
    let access = access(&database, &given);
    let of = |element| permit(&database, &access, element);
    // Europe's permit holds beneath it, where France has its own; Germany, beneath both
    // Europe and the EU, takes the higher of theirs.
    assert_eq!(of("Europe"), Permit::Hidden);
    assert_eq!(of("France"), Permit::Read);
    assert_eq!(of("Germany"), Permit::Read);
    assert_eq!(of("Spain"), Permit::Read);
    // Elements with no permit above them are bound by none.
    assert_eq!(of("World"), Permit::Write);
    assert_eq!(of("US"), Permit::Write);
    assert!(access.binds());
    assert!(!Access::new(database.model(), &BTreeMap::new()).binds());

    // A cell is open to what the lowest of its elements allows, and a spread to the leaves
    // beneath it only where every one of them may be written.
    let model = database.model();
    let cube = model.cube("Sales").unwrap();
    let at = |region: &str, product: &str| {
        let region = model
            .dimension("Region")
            .unwrap()
            .element_id(region)
            .unwrap();
        let product = model
            .dimension("Product")
            .unwrap()
            .element_id(product)
            .unwrap();
        vec![region, product]
    };
    assert_eq!(access.cell(cube, &at("France", "A")), Permit::Read);
    assert_eq!(access.cell(cube, &at("US", "A")), Permit::Write);
    let germany = model
        .dimension("Region")
        .unwrap()
        .element_id("Germany")
        .unwrap();
    let region = model.dimension_id("Region").unwrap();
    let europe = access.unwritable_beneath(model, cube, &at("Europe", "All"));
    assert!(matches!(europe, Some((dimension, _)) if dimension == region));
    assert_eq!(
        access.unwritable_beneath(model, cube, &at("Americas", "All")),
        None
    );
    assert_eq!(
        access.unwritable_beneath(model, cube, &at("Germany", "A")),
        Some((region, germany))
    );
}

#[test]
fn a_query_finds_no_element_hidden_from_its_asker_but_totals_still_count_them() {
    let database = database();
    let access = access(&database, &[("Region", "Europe", Permit::Hidden)]);
    let columns = |set: &str| {
        let mdx = format!("SELECT {set} ON COLUMNS FROM [Sales]");
        query(&database, &access, &mdx).map(|result| {
            names(&result)
                .iter()
                .map(|&name| name.to_owned())
                .collect::<Vec<_>>()
        })
    };
    // Europe and what is beneath it are not there, however the set is made.
    assert_eq!(
        columns("[Region].Members").unwrap(),
        ["World", "Americas", "EU", "US", "Canada", "Spain"]
    );
    assert_eq!(columns("[Region].[World].Children").unwrap(), ["Americas"]);
    assert_eq!(
        columns("Descendants([Region].[World], 2)").unwrap(),
        ["US", "Canada"]
    );
    assert_eq!(
        columns("TM1FilterByLevel(TM1SubsetAll([Region]), 0)").unwrap(),
        ["US", "Canada", "Spain"]
    );
    // By name it is as if it had never been made.
    let missing = columns("{[Region].[France]}").unwrap_err().to_string();
    assert_eq!(
        missing,
        columns("{[Region].[Atlantis]}")
            .unwrap_err()
            .to_string()
            .replace("Atlantis", "France")
    );
    assert_eq!(
        columns("{StrToMember('[Region].[Europe]')}")
            .unwrap_err()
            .to_string(),
        "dimension 'Region' has no member 'Europe'"
    );
    // A walk to it comes to no member, and what counts children counts those seen.
    let value = |expression: &str| {
        let mdx = format!(
            "WITH MEMBER [Product].[X] AS {expression} SELECT {{[Product].[X]}} ON COLUMNS FROM [Sales]"
        );
        query(&database, &access, &mdx).unwrap().cells[0].clone()
    };
    assert_eq!(
        value("IIF([Region].[World].FirstChild IS NULL, 'none', 'some')"),
        Some(Scalar::Text("none".to_owned()))
    );
    assert_eq!(
        value("[Region].[World].Properties('CHILDREN_CARDINALITY')"),
        Some(Scalar::Number(1.0))
    );
    // The world's total still adds up Europe's units, as it does for everyone.
    let world = "SELECT {[Region].[World]} ON COLUMNS FROM [Sales] WHERE [Product].[A]";
    let everyone = database.query(None, world).unwrap().cells;
    assert_eq!(query(&database, &access, world).unwrap().cells, everyone);
    assert_eq!(everyone, [Some(Scalar::Number(15.0))]);
}

#[test]
fn a_query_stands_at_the_first_element_its_asker_may_see() {
    let database = database();
    // With the world hidden, the Americas and the EU are the tops they see, and a query
    // stands at the first.
    let access = access(
        &database,
        &[
            ("Region", "World", Permit::Hidden),
            ("Region", "Americas", Permit::Read),
        ],
    );
    let mdx = "SELECT {[Product].[A]} ON COLUMNS FROM [Sales]";
    assert_eq!(
        query(&database, &access, mdx).unwrap().cells,
        [Some(Scalar::Number(12.0))]
    );
    let tops = "SELECT TM1FilterByLevel([Region].Members, 1) ON COLUMNS FROM [Sales]";
    assert_eq!(
        names(&query(&database, &access, tops).unwrap()),
        ["Americas", "EU"]
    );
    // Where every element of a dimension is hidden, there is nowhere to stand.
    let nothing = access_all_hidden(&database);
    assert_eq!(
        query(&database, &nothing, mdx).unwrap_err().to_string(),
        "you may see no element of dimension 'Region'"
    );
}

/// Every top of `Region` hidden, and so everything beneath them.
fn access_all_hidden(database: &Database) -> Access {
    access(
        database,
        &[
            ("Region", "World", Permit::Hidden),
            ("Region", "EU", Permit::Hidden),
        ],
    )
}

fn cell(region: &str, product: &str) -> CellRef {
    CellRef {
        cube: "Sales".to_owned(),
        at: vec![region.to_owned(), product.to_owned()],
    }
}

#[test]
fn cells_hidden_from_one_are_as_if_missing_and_those_they_may_read_are_not_theirs_to_change() {
    let mut database = database();
    // Europe hidden, but France beneath it to be read; the Americas to be read.
    let given = [
        ("Region", "Europe", Permit::Hidden),
        ("Region", "France", Permit::Read),
        ("Region", "Americas", Permit::Read),
    ];
    let access = access(&database, &given);
    let missing = |name: &str| {
        Error::Cell(CellError::UnknownElement {
            dimension: "Region".to_owned(),
            element: name.to_owned(),
        })
    };
    // Read: a hidden cell is refused as a cell of an element there is not, by its name as
    // asked for.
    let cells = CellRefs::from(
        &[
            cell("germany", "A"),
            cell("Atlantis", "A"),
            cell("France", "A"),
        ][..],
    );
    let read = database.read_batch(None, &cells, Some(&access));
    assert_eq!(read[0], Err(missing("germany")));
    assert_eq!(read[1], Err(missing("Atlantis")));
    assert_eq!(read[2], Ok(Scalar::Number(1.0)));
    assert_eq!(
        database.permit(&cell("France", "A"), &access),
        Ok(Permit::Read)
    );
    assert_eq!(
        database.permit(&cell("Spain", "B"), &access),
        Ok(Permit::Write)
    );
    assert_eq!(
        database.permit(&cell("Germany", "B"), &access),
        Err(missing("Germany"))
    );

    // Written: a cell one may only read is refused as forbidden, saying which element.
    let write = |at: [&str; 2], value: f64| CellWrite {
        cube: "Sales".to_owned(),
        at: at.map(str::to_owned).to_vec(),
        value: Scalar::Number(value),
    };
    let writes = [write(["Spain", "B"], 5.0), write(["US", "B"], 5.0)];
    let refused = database.write_batch(None, &CellWrites::from(&writes[..]), Some(&access));
    let forbidden = "you may only read element 'US' of dimension 'Region'".to_owned();
    assert_eq!(
        refused,
        Err(Error::Write {
            index: 1,
            error: CellError::Forbidden(forbidden)
        })
    );
    let writes = [write(["Germany", "B"], 5.0)];
    let refused = database.write_batch(None, &CellWrites::from(&writes[..]), Some(&access));
    assert!(matches!(
        refused,
        Err(Error::Write {
            index: 0,
            error: CellError::UnknownElement { .. }
        })
    ));
    let writes = [write(["Spain", "B"], 5.0)];
    database
        .write_batch(None, &CellWrites::from(&writes[..]), Some(&access))
        .unwrap();

    // Explained: a cell one may only read is not writable for them.
    let explained = database
        .explain(None, &cell("France", "A"), false, Some(&access))
        .unwrap();
    assert!(!explained.writable);
    assert!(
        database
            .explain(None, &cell("Spain", "A"), false, Some(&access))
            .unwrap()
            .writable
    );
    let hidden = database.explain(None, &cell("Germany", "A"), false, Some(&access));
    assert_eq!(hidden.map(|_| ()), Err(missing("Germany")));

    // Spread: only where every leaf beneath may be written, and nothing hidden is named.
    let spread = |region: &str| Spread {
        cube: "Sales".to_owned(),
        at: vec![region.to_owned(), "All".to_owned()],
        value: 100.0,
        method: SpreadMethod::Equal,
        holds: Vec::new(),
    };
    let mut refusal = |region: &str| match database.spread(None, &spread(region), Some(&access)) {
        Err(Error::Write {
            error: CellError::Forbidden(reason),
            ..
        }) => reason,
        other => panic!("expected a refusal, found {other:?}"),
    };
    assert_eq!(
        refusal("Americas"),
        "you may only read element 'US' of dimension 'Region', beneath the cell"
    );
    assert_eq!(
        refusal("EU"),
        "the cell adds up cells hidden from you, which a spread would change"
    );
    assert_eq!(
        database.spread(None, &spread("Europe"), Some(&access)),
        Err(missing("Europe"))
    );
    assert_eq!(
        database.spread(None, &spread("Spain"), Some(&access)),
        Ok(2)
    );

    // Listed: the stored cells one may see, and how many of them there are.
    let (total, page) = database.stored("Sales", 0, 10, Some(&access)).unwrap();
    let listed: Vec<&str> = page.iter().map(|cell| cell.at[0].as_str()).collect();
    assert_eq!(
        (total, listed),
        (5, vec!["France", "US", "Canada", "Spain", "Spain"])
    );
    assert_eq!(database.stored("Sales", 0, 10, None).unwrap().0, 6);
}
