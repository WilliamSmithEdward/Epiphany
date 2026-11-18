mod common;

use std::collections::HashMap;

use common::{Rng, Shape, random_model};
use epiphany_core::Scalar;
use epiphany_core::cells::{CellError, Cells, coordinate};
use epiphany_core::model::{Cube, Element, ElementId, Model};
use serde_json::json;

fn element<'a>(model: &'a Model, cube: &Cube, dimension: usize, id: ElementId) -> &'a Element {
    &model.dimensions()[cube.dimensions[dimension] as usize].elements[id as usize]
}

/// The value of a numeric cell computed the slow way: straight from the hierarchy, one child
/// at a time, without the flattened leaf weights the store uses.
fn reference(
    model: &Model,
    cube: &Cube,
    stored: &HashMap<Vec<ElementId>, f64>,
    at: &mut Vec<ElementId>,
) -> f64 {
    let consolidated = (0..at.len()).find(|&d| !element(model, cube, d, at[d]).children.is_empty());
    let Some(dimension) = consolidated else {
        return stored.get(at).copied().unwrap_or(0.0);
    };
    let id = at[dimension];
    let mut total = 0.0;
    for &(child, weight) in &element(model, cube, dimension, id).children {
        at[dimension] = child;
        total += weight * reference(model, cube, stored, at);
    }
    at[dimension] = id;
    total
}

/// Every coordinate of a cube whose dimensions have the given numbers of elements.
fn every_coordinate(sizes: &[usize]) -> Vec<Vec<ElementId>> {
    sizes.iter().fold(vec![Vec::new()], |coordinates, &size| {
        let extend = |at: &Vec<ElementId>| {
            let at = at.clone();
            (0..size as ElementId).map(move |id| [at.as_slice(), &[id]].concat())
        };
        coordinates.iter().flat_map(extend).collect()
    })
}

#[test]
fn every_cell_of_random_cubes_matches_the_reference() {
    for seed in 0..48 {
        let mut rng = Rng::new(seed);
        let dimensions = 2 + rng.below(3);
        let shape = Shape {
            leaves: 2 + rng.below(4),
            groups: 1 + rng.below(4),
            fan: 3,
        };
        let model = Model::compile(&random_model(&mut rng, dimensions, &shape)).unwrap();
        let cube = model.cube("C").unwrap();

        // Few writes leave most consolidations larger than the store; many make them smaller.
        // The two cases take the two different summing strategies.
        let writes = [1, 6, 60, 600][seed as usize % 4];
        let mut cells = Cells::default();
        let mut stored = HashMap::new();
        for _ in 0..writes {
            let at: Vec<ElementId> = (0..dimensions)
                .map(|_| rng.below(shape.leaves) as ElementId)
                .collect();
            let value = (rng.below(2001) as f64 - 1000.0) / 8.0;
            cells
                .write(&model, cube, &at, Scalar::Number(value))
                .unwrap();
            stored.insert(at, value);
        }

        let sizes = vec![shape.leaves + shape.groups + 1; dimensions];
        for mut at in every_coordinate(&sizes) {
            let expected = reference(&model, cube, &stored, &mut at);
            let found = cells.read(&model, cube, &at);
            assert_eq!(found, Scalar::Number(expected), "seed {seed} at {at:?}");
        }
    }
}

/// A cube `C` over `Account` (Total = Sales - Cost) and `Field` (Amount, Comment, Both).
fn ledger() -> Model {
    let def = json!({
        "dimensions": [
            {
                "name": "Account",
                "attributes": { "Code": "alias" },
                "elements": [
                    { "name": "Total", "type": "consolidated", "children": [
                        { "name": "Sales" },
                        { "name": "Cost", "weight": -1 }
                    ]},
                    { "name": "Sales", "type": "numeric", "attributes": { "Code": "4000" } },
                    { "name": "Cost", "type": "numeric" }
                ]
            },
            {
                "name": "Field",
                "elements": [
                    { "name": "Amount", "type": "numeric" },
                    { "name": "Comment", "type": "string" },
                    { "name": "Both", "type": "consolidated", "children": [
                        { "name": "Amount" },
                        { "name": "Comment" }
                    ]}
                ]
            }
        ],
        "cubes": [{ "name": "C", "dimensions": ["Account", "Field"] }]
    });
    Model::compile(&serde_json::from_value(def).unwrap()).unwrap()
}

struct Ledger {
    model: Model,
    cells: Cells,
}

impl Ledger {
    fn new() -> Self {
        Self {
            model: ledger(),
            cells: Cells::default(),
        }
    }

    fn at(&self, names: [&str; 2]) -> Vec<ElementId> {
        coordinate(&self.model, self.model.cube("C").unwrap(), &names).unwrap()
    }

    fn read(&self, names: [&str; 2]) -> Scalar {
        let cube = self.model.cube("C").unwrap();
        self.cells.read(&self.model, cube, &self.at(names))
    }

    fn write(&mut self, names: [&str; 2], value: Scalar) -> Result<(), CellError> {
        let at = self.at(names);
        let cube = self.model.cube("C").unwrap();
        self.cells.write(&self.model, cube, &at, value)
    }
}

fn text(text: &str) -> Scalar {
    Scalar::Text(text.to_owned())
}

#[test]
fn leaf_cells_are_read_back_and_default_to_zero() {
    let mut ledger = Ledger::new();
    assert_eq!(ledger.read(["Sales", "Amount"]), Scalar::Number(0.0));
    ledger
        .write(["Sales", "Amount"], Scalar::Number(100.0))
        .unwrap();
    assert_eq!(ledger.read(["Sales", "Amount"]), Scalar::Number(100.0));
    ledger
        .write(["Sales", "Amount"], Scalar::Number(0.0))
        .unwrap();
    assert_eq!(ledger.read(["Sales", "Amount"]), Scalar::Number(0.0));
}

#[test]
fn consolidated_cells_apply_weights() {
    let mut ledger = Ledger::new();
    ledger
        .write(["Sales", "Amount"], Scalar::Number(100.0))
        .unwrap();
    ledger
        .write(["Cost", "Amount"], Scalar::Number(30.0))
        .unwrap();
    assert_eq!(ledger.read(["Total", "Amount"]), Scalar::Number(70.0));
    assert_eq!(ledger.read(["Total", "Both"]), Scalar::Number(70.0));
}

#[test]
fn consolidated_cells_cannot_be_written() {
    let mut ledger = Ledger::new();
    let result = ledger.write(["Total", "Amount"], Scalar::Number(1.0));
    assert_eq!(result, Err(CellError::Consolidated));
}

#[test]
fn text_cells_hold_strings_and_default_to_empty() {
    let mut ledger = Ledger::new();
    assert_eq!(ledger.read(["Sales", "Comment"]), text(""));
    ledger.write(["Sales", "Comment"], text("late")).unwrap();
    ledger
        .write(["Total", "Comment"], text("reviewed"))
        .unwrap();
    assert_eq!(ledger.read(["Sales", "Comment"]), text("late"));
    assert_eq!(ledger.read(["Total", "Comment"]), text("reviewed"));
    ledger.write(["Sales", "Comment"], text("")).unwrap();
    assert_eq!(ledger.read(["Sales", "Comment"]), text(""));
}

#[test]
fn values_must_match_the_cell_type() {
    let mut ledger = Ledger::new();
    let number_in_text = ledger.write(["Sales", "Comment"], Scalar::Number(1.0));
    let text_in_number = ledger.write(["Sales", "Amount"], text("x"));
    assert_eq!(number_in_text, Err(CellError::WrongType));
    assert_eq!(text_in_number, Err(CellError::WrongType));
}

#[test]
fn numbers_must_be_finite() {
    let mut ledger = Ledger::new();
    for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let result = ledger.write(["Sales", "Amount"], Scalar::Number(number));
        assert_eq!(result, Err(CellError::NotFinite));
    }
}

#[test]
fn coordinates_resolve_names_and_aliases() {
    let ledger = Ledger::new();
    assert_eq!(
        ledger.at(["4000", "amount"]),
        ledger.at(["Sales", "Amount"])
    );
}

#[test]
fn bad_coordinates_are_rejected() {
    let model = ledger();
    let cube = model.cube("C").unwrap();
    assert_eq!(
        coordinate(&model, cube, &["Sales"]),
        Err(CellError::Arity {
            expected: 2,
            found: 1
        })
    );
    assert_eq!(
        coordinate(&model, cube, &["Sales", "Nope"]),
        Err(CellError::UnknownElement {
            dimension: "Field".to_owned(),
            element: "Nope".to_owned()
        })
    );
}
