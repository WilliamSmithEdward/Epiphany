use epiphany_core::Scalar;
use epiphany_core::model::{Dimension, Model, ModelDef, ModelError};
use serde_json::{Value as Json, json};

fn compile(def: Json) -> Result<Model, ModelError> {
    Model::compile(&serde_json::from_value(def).expect("definition parses"))
}

/// A model of one dimension named `D` with the given elements.
fn dimension(elements: Json) -> Json {
    json!({ "dimensions": [{ "name": "D", "elements": elements }] })
}

fn example() -> Model {
    let dimensions = [
        include_str!("fixtures/model/dimensions/account.json"),
        include_str!("fixtures/model/dimensions/entity.json"),
        include_str!("fixtures/model/dimensions/period.json"),
        include_str!("fixtures/model/dimensions/version.json"),
    ];
    let cube = include_str!("fixtures/model/cubes/general-ledger.json");
    let def = ModelDef {
        dimensions: dimensions
            .iter()
            .map(|text| serde_json::from_str(text).expect("dimension parses"))
            .collect(),
        cubes: vec![serde_json::from_str(cube).expect("cube parses")],
        ..ModelDef::default()
    };
    Model::compile(&def).expect("example model is valid")
}

fn names(dimension: &Dimension, ids: &[u32]) -> Vec<String> {
    ids.iter()
        .map(|&id| dimension.elements[id as usize].name.clone())
        .collect()
}

fn leaves(dimension: &Dimension, element: &str) -> Vec<(String, f64)> {
    let element = dimension.element(element).expect("element exists");
    element
        .leaves
        .iter()
        .map(|&(id, weight)| (dimension.elements[id as usize].name.clone(), weight))
        .collect()
}

fn pairs(expected: &[(&str, f64)]) -> Vec<(String, f64)> {
    expected.iter().map(|&(n, w)| (n.to_owned(), w)).collect()
}

#[test]
fn example_cube_lists_its_dimensions_in_order() {
    let model = example();
    let cube = model.cube("general ledger").expect("cube exists");
    assert_eq!(cube.name, "General Ledger");
    let dimensions: Vec<&str> = cube
        .dimensions
        .iter()
        .map(|&id| model.dimensions()[id as usize].name.as_str())
        .collect();
    assert_eq!(dimensions, ["Account", "Entity", "Period", "Version"]);
}

#[test]
fn consolidations_flatten_to_weighted_leaves() {
    let model = example();
    let account = model.dimension("Account").unwrap();
    assert_eq!(
        leaves(account, "Revenue"),
        pairs(&[("Product Revenue", 1.0), ("Service Revenue", 1.0)])
    );
    assert_eq!(
        leaves(account, "Net Income"),
        pairs(&[
            ("Product Revenue", 1.0),
            ("Service Revenue", 1.0),
            ("Salaries", -1.0),
            ("Rent", -1.0),
        ])
    );
    assert_eq!(leaves(account, "Rent"), pairs(&[("Rent", 1.0)]));
}

#[test]
fn levels_count_up_from_leaves() {
    let model = example();
    let period = model.dimension("Period").unwrap();
    let level = |name| period.element(name).unwrap().level;
    assert_eq!((level("Jan"), level("Q1"), level("H1")), (0, 1, 2));
}

#[test]
fn children_and_parents_keep_declaration_order() {
    let model = example();
    let period = model.dimension("Period").unwrap();
    let children: Vec<u32> = period
        .element("Q1")
        .unwrap()
        .children
        .iter()
        .map(|c| c.0)
        .collect();
    assert_eq!(names(period, &children), ["Jan", "Feb", "Mar"]);
    assert_eq!(
        names(period, &period.element("Jan").unwrap().parents),
        ["Q1"]
    );
    assert!(period.element("H1").unwrap().parents.is_empty());
}

#[test]
fn names_ignore_case_and_spaces() {
    let model = example();
    let account = model.dimension("ACCOUNT").unwrap();
    let id = account.element_id("Product Revenue");
    assert!(id.is_some());
    assert_eq!(account.element_id("productrevenue"), id);
    assert_eq!(account.element_id(" PRODUCT  revenue "), id);
    assert_eq!(account.element_id("Product Revenues"), None);
}

#[test]
fn aliases_resolve_to_their_elements() {
    let model = example();
    let period = model.dimension("Period").unwrap();
    assert_eq!(period.element_id("m02"), period.element_id("Feb"));
}

#[test]
fn attributes_are_read_by_name() {
    let model = example();
    let account = model.dimension("Account").unwrap();
    let period = model.dimension("Period").unwrap();
    let rent = account.element_id("Rent").unwrap();
    let feb = period.element_id("Feb").unwrap();
    assert_eq!(
        account.attribute_value(rent, "accountcode"),
        Some(&Scalar::Text("6100".to_owned()))
    );
    assert_eq!(
        period.attribute_value(feb, "Days"),
        Some(&Scalar::Number(28.0))
    );
    assert_eq!(
        account.attribute_value(account.element_id("Revenue").unwrap(), "AccountCode"),
        None
    );
    assert_eq!(account.attribute_value(rent, "Missing"), None);
}

#[test]
fn a_leaf_reached_twice_sums_its_weights() {
    let model = compile(dimension(json!([
        { "name": "Top", "type": "consolidated", "children": [
            { "name": "Left", "weight": 2 },
            { "name": "Right", "weight": -1 },
            { "name": "A" }
        ]},
        { "name": "Left", "type": "consolidated", "children": [{ "name": "A" }, { "name": "B" }] },
        { "name": "Right", "type": "consolidated", "children": [{ "name": "A", "weight": 0.5 }] },
        { "name": "A", "type": "numeric" },
        { "name": "B", "type": "numeric" }
    ])))
    .unwrap();
    let d = model.dimension("D").unwrap();
    assert_eq!(leaves(d, "Top"), pairs(&[("A", 2.5), ("B", 2.0)]));
}

#[test]
fn string_children_are_not_aggregated() {
    let model = compile(dimension(json!([
        { "name": "Total", "type": "consolidated", "children": [{ "name": "Amount" }, { "name": "Note" }] },
        { "name": "Amount", "type": "numeric" },
        { "name": "Note", "type": "string" }
    ])))
    .unwrap();
    let d = model.dimension("D").unwrap();
    assert_eq!(leaves(d, "Total"), pairs(&[("Amount", 1.0)]));
    assert_eq!(d.element("Total").unwrap().level, 1);
}

#[test]
fn unknown_fields_are_rejected() {
    let cube = json!({ "name": "C", "dimensions": ["D"], "measures": {} });
    assert!(serde_json::from_value::<ModelDef>(json!({ "cubes": [cube] })).is_err());
}

#[test]
fn invalid_models_are_rejected() {
    let leaf = |name: &str| json!({ "name": name, "type": "numeric" });
    let parent = |name: &str, children: &[&str]| {
        let children: Vec<Json> = children.iter().map(|c| json!({ "name": c })).collect();
        json!({ "name": name, "type": "consolidated", "children": children })
    };
    let attributed = |declared: Json, values: Json| {
        json!({ "dimensions": [{
            "name": "D",
            "attributes": declared,
            "elements": [
                { "name": "a", "type": "numeric", "attributes": values },
                { "name": "b", "type": "numeric", "attributes": { "Alias": "bee" } }
            ]
        }] })
    };
    let aliases = json!({ "Alias": "alias", "Other": "alias", "Size": "numeric" });
    let cubes = |cubes: Json| json!({ "dimensions": [{ "name": "D", "elements": [leaf("a")] }], "cubes": cubes });

    let cases = [
        (
            json!({ "dimensions": [
                { "name": "D", "elements": [leaf("a")] },
                { "name": "d ", "elements": [leaf("a")] }
            ] }),
            "duplicate dimension 'd ' in the model",
        ),
        (
            json!({ "dimensions": [{ "name": " ", "elements": [leaf("a")] }] }),
            "empty dimension name in the model",
        ),
        (
            dimension(json!([leaf("a"), leaf("A")])),
            "duplicate element 'A' in dimension 'D'",
        ),
        (
            dimension(json!([leaf("")])),
            "empty element name in dimension 'D'",
        ),
        (
            dimension(json!([parent("p", &["a", "x"]), leaf("a")])),
            "unknown child 'x' in element 'p' of dimension 'D'",
        ),
        (
            dimension(json!([parent("p", &["a", "A"]), leaf("a")])),
            "duplicate child 'A' in element 'p' of dimension 'D'",
        ),
        (
            dimension(json!([{ "name": "p", "type": "consolidated" }])),
            "consolidated element 'p' in dimension 'D' has no children",
        ),
        (
            dimension(
                json!([{ "name": "a", "type": "numeric", "children": [{ "name": "b" }] }, leaf("b")]),
            ),
            "element 'a' in dimension 'D' has children but is not consolidated",
        ),
        (
            dimension(json!([parent("p", &["p"])])),
            "element 'p' in dimension 'D' is its own descendant",
        ),
        (
            dimension(json!([
                parent("p", &["q"]),
                parent("q", &["r"]),
                parent("r", &["p"])
            ])),
            "element 'p' in dimension 'D' is its own descendant",
        ),
        (
            attributed(aliases.clone(), json!({ "Colour": "red" })),
            "unknown attribute 'Colour' in element 'a' of dimension 'D'",
        ),
        (
            attributed(aliases.clone(), json!({ "Size": "large" })),
            "attribute 'Size' of element 'a' in dimension 'D' has the wrong type",
        ),
        (
            attributed(aliases.clone(), json!({ "Alias": 7 })),
            "attribute 'Alias' of element 'a' in dimension 'D' has the wrong type",
        ),
        (
            attributed(aliases.clone(), json!({ "Alias": "B" })),
            "alias 'B' in dimension 'D' is already the name or alias of another element",
        ),
        (
            attributed(aliases.clone(), json!({ "Other": "Bee" })),
            "alias 'bee' in dimension 'D' is already the name or alias of another element",
        ),
        (
            attributed(aliases.clone(), json!({ "Alias": " " })),
            "empty alias name in dimension 'D'",
        ),
        (
            attributed(json!({ "Alias": "alias", "ALIAS": "string" }), json!({})),
            "duplicate attribute 'Alias' in dimension 'D'",
        ),
        (
            cubes(json!([{ "name": "C", "dimensions": [] }])),
            "cube 'C' has no dimensions",
        ),
        (
            cubes(json!([{ "name": "C", "dimensions": ["D", "E"] }])),
            "unknown dimension 'E' in cube 'C'",
        ),
        (
            cubes(json!([{ "name": "C", "dimensions": ["D", "d"] }])),
            "duplicate dimension 'd' in cube 'C'",
        ),
        (
            cubes(
                json!([{ "name": "C", "dimensions": ["D"] }, { "name": "c", "dimensions": ["D"] }]),
            ),
            "duplicate cube 'c' in the model",
        ),
    ];

    for (def, expected) in cases {
        let error = compile(def).expect_err(expected);
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn an_alias_may_repeat_its_own_element_name() {
    let model = compile(json!({ "dimensions": [{
        "name": "D",
        "attributes": { "Alias": "alias" },
        "elements": [{ "name": "a", "type": "numeric", "attributes": { "Alias": "A" } }]
    }] }));
    assert!(model.is_ok());
}
