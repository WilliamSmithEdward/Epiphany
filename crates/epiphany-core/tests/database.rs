mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use common::{Rng, Shape, random_model};
use epiphany_core::Scalar;
use epiphany_core::cells::CellError;
use epiphany_core::database::{
    CellRef, CellWrite, Changes, Database, Error, Invalid, Journal, KeptSandbox, LockMode,
    ModelUpdate, NamedCells, NamedWrites, Spread, SpreadMethod,
};
use epiphany_core::model::{MembersDef, ModelDef, ModelError, PicklistDef, SubsetDef};
use serde_json::json;

/// A cube `Sales` over `Product` (All = A + B) and `Measure` (Units, Note as text), and a cube
/// `Stock` over the same dimensions.
fn definition() -> ModelDef {
    serde_json::from_value(json!({
        "dimensions": [
            { "name": "Product", "elements": [
                { "name": "All", "type": "consolidated", "children": [{ "name": "A" }, { "name": "B" }] },
                { "name": "A", "type": "numeric" },
                { "name": "B", "type": "numeric" }
            ]},
            { "name": "Measure", "elements": [
                { "name": "Units", "type": "numeric" },
                { "name": "Note", "type": "string" }
            ]}
        ],
        "cubes": [
            { "name": "Sales", "dimensions": ["Product", "Measure"] },
            { "name": "Stock", "dimensions": ["Product", "Measure"] }
        ]
    }))
    .unwrap()
}

fn database() -> Database {
    Database::new(definition()).unwrap()
}

fn cell(cube: &str, at: [&str; 2]) -> CellRef {
    CellRef {
        cube: cube.to_owned(),
        at: at.map(str::to_owned).to_vec(),
    }
}

fn set(cube: &str, at: [&str; 2], value: f64) -> CellWrite {
    CellWrite {
        cube: cube.to_owned(),
        at: at.map(str::to_owned).to_vec(),
        value: Scalar::Number(value),
    }
}

fn units(database: &Database, transaction: Option<u64>, product: &str) -> f64 {
    match database.read(transaction, &cell("Sales", [product, "Units"])) {
        Ok(Scalar::Number(number)) => number,
        other => panic!("expected a number, found {other:?}"),
    }
}

fn locked(what: &str) -> Result<(), Error> {
    Err(Error::Locked(what.to_owned()))
}

#[test]
fn a_total_read_before_follows_every_change() {
    let mut database = database();
    database
        .write(None, &[set("Sales", ["A", "Units"], 10.0)])
        .unwrap();
    assert_eq!(units(&database, None, "All"), 10.0);
    assert_eq!(units(&database, None, "All"), 10.0);

    // A transaction sees its own writes in the total, and nobody else does.
    let transaction = database.begin();
    assert_eq!(units(&database, Some(transaction), "All"), 10.0);
    database
        .write(Some(transaction), &[set("Sales", ["B", "Units"], 5.0)])
        .unwrap();
    assert_eq!(units(&database, Some(transaction), "All"), 15.0);
    assert_eq!(units(&database, None, "All"), 10.0);
    database.rollback(transaction).unwrap();
    assert_eq!(units(&database, None, "All"), 10.0);

    let transaction = database.begin();
    database
        .write(Some(transaction), &[set("Sales", ["B", "Units"], 7.0)])
        .unwrap();
    database.commit(transaction).unwrap();
    assert_eq!(units(&database, None, "All"), 17.0);

    // A change of the model changes what the total adds up.
    let mut model = definition();
    model.dimensions[0].elements[0].children.pop();
    let version = database.model_version();
    database.update_model(version, model).unwrap();
    assert_eq!(units(&database, None, "All"), 10.0);
}

#[test]
fn cells_read_together_equal_cells_read_one_by_one() {
    const SHAPE: Shape = Shape {
        leaves: 6,
        groups: 4,
        fan: 3,
    };
    for seed in 0..25 {
        let mut rng = Rng::new(seed);
        let model = random_model(&mut rng, 3, &SHAPE);
        let name = |rng: &mut Rng, limit: usize| match rng.below(limit + 1) {
            id if id == limit => "all".to_owned(),
            id => format!("e{id}"),
        };
        let writes: Vec<CellWrite> = (0..60)
            .map(|_| CellWrite {
                cube: "C".to_owned(),
                at: (0..3).map(|_| name(&mut rng, SHAPE.leaves - 1)).collect(),
                value: Scalar::Number(rng.below(1000) as f64 - 500.0),
            })
            .filter(|write| !write.at.contains(&"all".to_owned()))
            .collect();
        // Leaves, consolidations and grand totals, some of them more than once.
        let cells: Vec<CellRef> = (0..300)
            .map(|_| CellRef {
                cube: "C".to_owned(),
                at: (0..3)
                    .map(|_| name(&mut rng, SHAPE.leaves + SHAPE.groups))
                    .collect(),
            })
            .collect();

        let mut together = Database::new(model.clone()).unwrap();
        together.write(None, &writes).unwrap();
        let mut one_by_one = Database::new(model).unwrap();
        one_by_one.write(None, &writes).unwrap();
        let expected: Vec<_> = cells
            .iter()
            .map(|cell| one_by_one.read(None, cell))
            .collect();
        assert_eq!(together.read_many(None, &cells), expected, "seed {seed}");
        // Asked again, they come from what was kept.
        assert_eq!(together.read_many(None, &cells), expected, "seed {seed}");

        // Inside a transaction whose writes replace stored cells, clear some and add others,
        // the cells are what committing those writes would make them, read together or not.
        let pending: Vec<CellWrite> = writes
            .iter()
            .take(20)
            .enumerate()
            .map(|(index, write)| CellWrite {
                value: Scalar::Number(match index % 3 {
                    0 => 0.0,
                    1 => 7.0,
                    _ => -3.5,
                }),
                ..write.clone()
            })
            .chain((0..20).map(|_| {
                CellWrite {
                    cube: "C".to_owned(),
                    at: (0..3)
                        .map(|_| format!("e{}", rng.below(SHAPE.leaves)))
                        .collect(),
                    value: Scalar::Number(rng.below(100) as f64),
                }
            }))
            .collect();
        let within = together.begin();
        together.write(Some(within), &pending).unwrap();
        let one_by_one_within = one_by_one.begin();
        one_by_one.write(Some(one_by_one_within), &pending).unwrap();
        let expected: Vec<_> = cells
            .iter()
            .map(|cell| one_by_one.read(Some(one_by_one_within), cell))
            .collect();
        assert_eq!(
            together.read_many(Some(within), &cells),
            expected,
            "seed {seed}"
        );
        assert_eq!(
            together.read_many(Some(within), &cells),
            expected,
            "seed {seed}"
        );
        // What was worked out inside the transaction does not leak out of it.
        let outside: Vec<_> = cells
            .iter()
            .map(|cell| one_by_one.read(None, cell))
            .collect();
        assert_eq!(together.read_many(None, &cells), outside, "seed {seed}");
        one_by_one.commit(one_by_one_within).unwrap();
        let committed: Vec<_> = cells
            .iter()
            .map(|cell| one_by_one.read(None, cell))
            .collect();
        assert_eq!(expected, committed, "seed {seed}");
    }
}

#[test]
fn pending_writes_are_visible_only_inside_their_transaction() {
    let mut database = database();
    database
        .write(None, &[set("Sales", ["A", "Units"], 10.0)])
        .unwrap();
    let transaction = database.begin();
    let writes = [
        set("Sales", ["A", "Units"], 4.0),
        set("Sales", ["B", "Units"], 5.0),
    ];
    database.write(Some(transaction), &writes).unwrap();

    assert_eq!(units(&database, Some(transaction), "A"), 4.0);
    assert_eq!(units(&database, Some(transaction), "All"), 9.0);
    assert_eq!(units(&database, None, "A"), 10.0);
    assert_eq!(units(&database, None, "All"), 10.0);

    database.commit(transaction).unwrap();
    assert_eq!(units(&database, None, "All"), 9.0);
}

#[test]
fn rollback_discards_pending_writes() {
    let mut database = database();
    database
        .write(None, &[set("Sales", ["A", "Units"], 10.0)])
        .unwrap();
    let transaction = database.begin();
    database
        .write(Some(transaction), &[set("Sales", ["A", "Units"], 4.0)])
        .unwrap();
    database.rollback(transaction).unwrap();
    assert_eq!(units(&database, None, "A"), 10.0);
}

#[test]
fn a_transaction_goes_back_to_a_point_it_came_to() {
    let mut database = database();
    database
        .write(None, &[set("Sales", ["A", "Units"], 10.0)])
        .unwrap();
    let transaction = database.begin();
    let start = database.savepoint(transaction).unwrap();
    assert_eq!(start, 0);
    let both = [
        set("Sales", ["A", "Units"], 4.0),
        set("Sales", ["B", "Units"], 1.0),
    ];
    database.write(Some(transaction), &both).unwrap();
    let middle = database.savepoint(transaction).unwrap();
    assert_eq!(middle, 2);
    database
        .write(Some(transaction), &[set("Sales", ["A", "Units"], 7.0)])
        .unwrap();
    assert_eq!(units(&database, Some(transaction), "All"), 8.0);

    // Back to the middle, A is pending at 4 again and B still at 1.
    database.undo_to(transaction, middle).unwrap();
    assert_eq!(units(&database, Some(transaction), "A"), 4.0);
    assert_eq!(units(&database, Some(transaction), "All"), 5.0);
    // Back to the start, nothing is pending and the stored value shows through.
    database.undo_to(transaction, start).unwrap();
    assert_eq!(units(&database, Some(transaction), "All"), 10.0);
    assert_eq!(database.savepoint(transaction), Ok(0));

    // A point not come to is refused, and the transaction stays open with nothing to commit.
    assert!(matches!(
        database.undo_to(transaction, 5),
        Err(Error::NotFound(_))
    ));
    database.commit(transaction).unwrap();
    assert_eq!(units(&database, None, "A"), 10.0);
    assert_eq!(
        database.savepoint(transaction),
        Err(Error::UnknownTransaction(transaction))
    );
}

#[test]
fn a_finished_transaction_is_unknown() {
    let mut database = database();
    let transaction = database.begin();
    database.commit(transaction).unwrap();
    let unknown = Error::UnknownTransaction(transaction);
    assert_eq!(database.commit(transaction), Err(unknown.clone()));
    assert_eq!(database.rollback(transaction), Err(unknown.clone()));
    assert_eq!(database.write(Some(transaction), &[]), Err(unknown.clone()));
    assert_eq!(
        database.read(Some(transaction), &cell("Sales", ["A", "Units"])),
        Err(unknown)
    );
}

#[test]
fn a_batch_with_a_bad_write_stages_nothing() {
    let mut database = database();
    let transaction = database.begin();
    let writes = [
        set("Sales", ["A", "Units"], 1.0),
        set("Sales", ["All", "Units"], 2.0),
    ];
    let error = Error::Write {
        index: 1,
        error: CellError::Consolidated,
    };
    assert_eq!(
        database.write(Some(transaction), &writes),
        Err(error.clone())
    );
    assert_eq!(units(&database, Some(transaction), "A"), 0.0);
    assert_eq!(database.write(None, &writes), Err(error));
    assert_eq!(units(&database, None, "A"), 0.0);
}

#[test]
fn text_cells_take_part_in_transactions() {
    let mut database = database();
    let note = cell("Sales", ["All", "Note"]);
    let write = CellWrite {
        cube: note.cube.clone(),
        at: note.at.clone(),
        value: Scalar::Text("draft".to_owned()),
    };
    let transaction = database.begin();
    database.write(Some(transaction), &[write]).unwrap();
    assert_eq!(
        database.read(Some(transaction), &note),
        Ok(Scalar::Text("draft".to_owned()))
    );
    assert_eq!(database.read(None, &note), Ok(Scalar::Text(String::new())));
    database.commit(transaction).unwrap();
    assert_eq!(
        database.read(None, &note),
        Ok(Scalar::Text("draft".to_owned()))
    );
}

#[test]
fn two_transactions_cannot_write_the_same_cell() {
    let mut database = database();
    let (first, second) = (database.begin(), database.begin());
    database
        .write(Some(first), &[set("Sales", ["A", "Units"], 1.0)])
        .unwrap();
    let same_cell = [
        set("Sales", ["B", "Units"], 2.0),
        set("Sales", ["a", "units"], 2.0),
    ];
    assert_eq!(
        database.write(Some(second), &same_cell),
        locked("cell (a, units) of cube 'Sales'")
    );
    assert_eq!(
        database.write(None, &same_cell),
        locked("cell (a, units) of cube 'Sales'")
    );
    assert_eq!(units(&database, Some(second), "B"), 0.0);

    database
        .write(Some(second), &[set("Sales", ["B", "Units"], 2.0)])
        .unwrap();
    database.commit(first).unwrap();
    database
        .write(Some(second), &[set("Sales", ["A", "Units"], 3.0)])
        .unwrap();
    database.commit(second).unwrap();
    assert_eq!(units(&database, None, "All"), 5.0);
}

#[test]
fn an_exclusive_cube_lock_keeps_other_writers_out() {
    let mut database = database();
    let (owner, other) = (database.begin(), database.begin());
    database
        .lock(owner, Some("Sales"), LockMode::Exclusive)
        .unwrap();
    let write = [set("Sales", ["A", "Units"], 1.0)];
    assert_eq!(database.write(Some(other), &write), locked("cube 'Sales'"));
    assert_eq!(database.write(None, &write), locked("cube 'Sales'"));
    assert_eq!(
        database.lock(other, Some("Sales"), LockMode::Shared),
        locked("cube 'Sales'")
    );
    database
        .write(Some(other), &[set("Stock", ["A", "Units"], 1.0)])
        .unwrap();
    database.write(Some(owner), &write).unwrap();
    database.commit(owner).unwrap();
    database.write(Some(other), &write).unwrap();
}

#[test]
fn a_shared_cube_lock_allows_readers_and_blocks_writers() {
    let mut database = database();
    let (first, second, writer) = (database.begin(), database.begin(), database.begin());
    database
        .lock(first, Some("Sales"), LockMode::Shared)
        .unwrap();
    database
        .lock(second, Some("Sales"), LockMode::Shared)
        .unwrap();
    let write = [set("Sales", ["A", "Units"], 1.0)];
    assert_eq!(database.write(Some(writer), &write), locked("cube 'Sales'"));
    assert_eq!(database.write(Some(first), &write), locked("cube 'Sales'"));
    assert_eq!(units(&database, Some(writer), "A"), 0.0);
    database.rollback(second).unwrap();
    database.write(Some(first), &write).unwrap();
}

#[test]
fn a_writer_keeps_exclusive_locks_out() {
    let mut database = database();
    let (writer, other) = (database.begin(), database.begin());
    database
        .write(Some(writer), &[set("Sales", ["A", "Units"], 1.0)])
        .unwrap();
    assert_eq!(
        database.lock(other, Some("Sales"), LockMode::Exclusive),
        locked("cube 'Sales'")
    );
    assert_eq!(
        database.lock(other, Some("Sales"), LockMode::Shared),
        locked("cube 'Sales'")
    );
    assert_eq!(
        database.lock(other, None, LockMode::Exclusive),
        locked("the server")
    );
    assert_eq!(
        database.lock(other, None, LockMode::Shared),
        locked("the server")
    );
    database
        .lock(other, Some("Stock"), LockMode::Exclusive)
        .unwrap();
}

#[test]
fn an_exclusive_server_lock_keeps_everyone_out() {
    let mut database = database();
    let (owner, other) = (database.begin(), database.begin());
    database.lock(owner, None, LockMode::Exclusive).unwrap();
    assert_eq!(
        database.write(Some(other), &[set("Stock", ["A", "Units"], 1.0)]),
        locked("the server")
    );
    assert_eq!(
        database.lock(other, Some("Stock"), LockMode::Shared),
        locked("the server")
    );
    assert!(matches!(
        database.lock(owner, Some("Nope"), LockMode::Shared),
        Err(Error::Cell(CellError::UnknownCube { .. }))
    ));
}

#[test]
fn an_idle_transaction_expires_and_frees_its_locks() {
    let mut database = database();
    database.set_transaction_timeout(Duration::from_millis(20));
    let idle = database.begin();
    database
        .lock(idle, Some("Sales"), LockMode::Exclusive)
        .unwrap();
    database
        .write(Some(idle), &[set("Sales", ["A", "Units"], 9.0)])
        .unwrap();
    thread::sleep(Duration::from_millis(60));

    assert_eq!(
        database.read(Some(idle), &cell("Sales", ["A", "Units"])),
        Err(Error::UnknownTransaction(idle))
    );
    database
        .write(None, &[set("Sales", ["A", "Units"], 1.0)])
        .unwrap();
    assert_eq!(database.commit(idle), Err(Error::UnknownTransaction(idle)));
    assert_eq!(units(&database, None, "A"), 1.0);
}

#[test]
fn using_a_transaction_keeps_it_alive() {
    let mut database = database();
    database.set_transaction_timeout(Duration::from_millis(150));
    let transaction = database.begin();
    database
        .write(Some(transaction), &[set("Sales", ["A", "Units"], 9.0)])
        .unwrap();
    for _ in 0..4 {
        thread::sleep(Duration::from_millis(60));
        assert_eq!(units(&database, Some(transaction), "A"), 9.0);
    }
    database.commit(transaction).unwrap();
    assert_eq!(units(&database, None, "A"), 9.0);
}

/// Each thread moves units between two cells in transactions, retrying when it meets a lock.
/// Whatever the interleaving, the total never changes and no transfer is half applied.
#[test]
fn concurrent_transfers_preserve_the_total() {
    let mut database = database();
    let opening = [
        set("Sales", ["A", "Units"], 1000.0),
        set("Sales", ["B", "Units"], 1000.0),
    ];
    database.write(None, &opening).unwrap();
    let database = Arc::new(RwLock::new(database));

    let transfer =
        |database: &RwLock<Database>, from: &str, to: &str, amount: f64| -> Result<(), Error> {
            let transaction = database.write().unwrap().begin();
            let staged = (|| {
                // Reads take no locks, so a change computed from a read needs the cube locked.
                let exclusive = LockMode::Exclusive;
                database
                    .write()
                    .unwrap()
                    .lock(transaction, Some("Sales"), exclusive)?;
                let before = |product| units(&database.read().unwrap(), Some(transaction), product);
                let debit = set("Sales", [from, "Units"], before(from) - amount);
                database
                    .write()
                    .unwrap()
                    .write(Some(transaction), &[debit])?;
                let credit = set("Sales", [to, "Units"], before(to) + amount);
                database
                    .write()
                    .unwrap()
                    .write(Some(transaction), &[credit])
            })();
            let mut database = database.write().unwrap();
            match staged {
                Ok(()) => database.commit(transaction),
                Err(error) => database.rollback(transaction).and(Err(error)),
            }
        };

    let workers: Vec<_> = (0..8u64)
        .map(|seed| {
            let database = Arc::clone(&database);
            thread::spawn(move || {
                let mut rng = Rng::new(seed);
                let mut done = 0;
                while done < 200 {
                    let (from, to) = if rng.below(2) == 0 {
                        ("A", "B")
                    } else {
                        ("B", "A")
                    };
                    match transfer(&database, from, to, 1.0 + rng.below(5) as f64) {
                        Ok(()) => done += 1,
                        Err(Error::Locked(_)) => thread::yield_now(),
                        Err(error) => panic!("{error}"),
                    }
                    assert_eq!(units(&database.read().unwrap(), None, "All"), 2000.0);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(units(&database.read().unwrap(), None, "All"), 2000.0);
}

/// What a transaction reads before committing is what everyone reads after it commits.
#[test]
fn pending_reads_match_committed_reads_on_random_cubes() {
    for seed in 0..24 {
        let mut rng = Rng::new(seed);
        let shape = Shape {
            leaves: 2 + rng.below(4),
            groups: 1 + rng.below(4),
            fan: 3,
        };
        let dimensions = 2 + rng.below(2);
        let mut database = Database::new(random_model(&mut rng, dimensions, &shape)).unwrap();
        let batch = |rng: &mut Rng, count: usize| -> Vec<CellWrite> {
            let write = |_| CellWrite {
                cube: "C".to_owned(),
                at: (0..dimensions)
                    .map(|_| format!("e{}", rng.below(shape.leaves)))
                    .collect(),
                value: Scalar::Number((rng.below(2001) as f64 - 1000.0) / 8.0),
            };
            (0..count).map(write).collect()
        };
        let (stored, pending) = (batch(&mut rng, 40), batch(&mut rng, 40));
        database.write(None, &stored).unwrap();
        let transaction = database.begin();
        database.write(Some(transaction), &pending).unwrap();

        let names: Vec<String> = (0..shape.leaves + shape.groups)
            .map(|id| format!("e{id}"))
            .chain(["all".to_owned()])
            .collect();
        let cells: Vec<CellRef> = (0..names.len().pow(dimensions as u32))
            .map(|index| CellRef {
                cube: "C".to_owned(),
                at: (0..dimensions)
                    .map(|d| names[index / names.len().pow(d as u32) % names.len()].clone())
                    .collect(),
            })
            .collect();
        let read = |database: &Database, transaction| -> Vec<Scalar> {
            cells
                .iter()
                .map(|cell| database.read(transaction, cell).unwrap())
                .collect()
        };
        let before = read(&database, Some(transaction));
        database.commit(transaction).unwrap();
        assert_eq!(before, read(&database, None), "seed {seed}");
    }
}

fn with_products(products: serde_json::Value) -> ModelDef {
    let mut definition = serde_json::to_value(definition()).unwrap();
    definition["dimensions"][0]["elements"] = products;
    serde_json::from_value(definition).unwrap()
}

#[test]
fn a_model_update_keeps_the_cells_that_still_fit() {
    let mut database = database();
    let note = CellWrite {
        cube: "Sales".to_owned(),
        at: vec!["B".to_owned(), "Note".to_owned()],
        value: Scalar::Text("keep".to_owned()),
    };
    let writes = [
        set("Sales", ["A", "Units"], 1.0),
        set("Sales", ["B", "Units"], 2.0),
        set("Stock", ["A", "Units"], 4.0),
        note,
    ];
    database.write(None, &writes).unwrap();

    // C is added before the others, A is removed, and B moves under a new parent.
    let updated = with_products(json!([
        { "name": "C", "type": "numeric" },
        { "name": "All", "type": "consolidated", "children": [{ "name": "Old" }, { "name": "C" }] },
        { "name": "Old", "type": "consolidated", "children": [{ "name": "B" }] },
        { "name": "B", "type": "numeric" }
    ]));
    let update = database.update_model(1, updated.clone()).unwrap();
    assert_eq!(
        update,
        ModelUpdate {
            version: 2,
            dropped_cells: 2
        }
    );
    assert_eq!(database.model_version(), 2);
    assert_eq!(database.definition(), &updated);
    assert_eq!(units(&database, None, "B"), 2.0);
    assert_eq!(units(&database, None, "All"), 2.0);
    assert_eq!(units(&database, None, "C"), 0.0);
    assert_eq!(
        database.read(None, &cell("Sales", ["B", "Note"])),
        Ok(Scalar::Text("keep".to_owned()))
    );
}

#[test]
fn a_leaf_that_becomes_a_consolidation_loses_its_cells() {
    let mut database = database();
    database
        .write(None, &[set("Sales", ["A", "Units"], 7.0)])
        .unwrap();
    let updated = with_products(json!([
        { "name": "A", "type": "consolidated", "children": [{ "name": "B" }] },
        { "name": "B", "type": "numeric" }
    ]));
    assert_eq!(database.update_model(1, updated).unwrap().dropped_cells, 1);
    assert_eq!(units(&database, None, "A"), 0.0);
}

#[test]
fn a_cube_with_changed_dimensions_loses_its_cells() {
    let mut database = database();
    let writes = [
        set("Sales", ["A", "Units"], 1.0),
        set("Stock", ["A", "Units"], 2.0),
    ];
    database.write(None, &writes).unwrap();
    let mut updated = serde_json::to_value(definition()).unwrap();
    updated["cubes"] = json!([
        { "name": "Stock", "dimensions": ["Product", "Measure"] },
        { "name": "Sales", "dimensions": ["Measure", "Product"] }
    ]);
    let update = database
        .update_model(1, serde_json::from_value(updated).unwrap())
        .unwrap();
    assert_eq!(update.dropped_cells, 1);
    assert_eq!(
        database.read(None, &cell("Stock", ["A", "Units"])),
        Ok(Scalar::Number(2.0))
    );
    assert_eq!(
        database.read(None, &cell("Sales", ["Units", "A"])),
        Ok(Scalar::Number(0.0))
    );
}

#[test]
fn a_refused_model_update_changes_nothing() {
    let mut database = database();
    database
        .write(None, &[set("Sales", ["A", "Units"], 1.0)])
        .unwrap();

    let stale = Error::StaleVersion {
        based_on: 7,
        current: 1,
    };
    assert_eq!(database.update_model(7, definition()), Err(stale));

    let invalid = with_products(json!([{ "name": "All", "type": "consolidated" }]));
    assert!(matches!(
        database.update_model(1, invalid),
        Err(Error::Model(_))
    ));

    let writer = database.begin();
    database
        .write(Some(writer), &[set("Sales", ["B", "Units"], 1.0)])
        .unwrap();
    assert_eq!(
        database.update_model(1, definition()).map(drop),
        locked("the server")
    );

    assert_eq!(database.model_version(), 1);
    assert_eq!(units(&database, None, "A"), 1.0);
    database.commit(writer).unwrap();
    assert_eq!(database.update_model(1, definition()).unwrap().version, 2);
    assert_eq!(units(&database, None, "All"), 2.0);
}

#[test]
fn an_open_transaction_survives_a_model_update_that_adds_a_cube() {
    let mut database = database();
    let transaction = database.begin();
    let mut updated = serde_json::to_value(definition()).unwrap();
    let plan = json!({ "name": "Plan", "dimensions": ["Product", "Measure"] });
    updated["cubes"].as_array_mut().unwrap().push(plan);
    database
        .update_model(1, serde_json::from_value(updated).unwrap())
        .unwrap();
    database
        .write(Some(transaction), &[set("Plan", ["A", "Units"], 3.0)])
        .unwrap();
    database.commit(transaction).unwrap();
    assert_eq!(
        database.read(None, &cell("Plan", ["All", "Units"])),
        Ok(Scalar::Number(3.0))
    );
}

/// A journal that counts what it is asked to do, and is full after a given number of commits.
#[derive(Debug, Default)]
struct Counted {
    full_after: usize,
    commits: AtomicUsize,
    snapshots: Arc<AtomicUsize>,
}

impl Journal for Counted {
    fn commit(&mut self, _: &NamedWrites<'_>) -> std::io::Result<()> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn model(&mut self, _: u64, _: &ModelDef) -> std::io::Result<()> {
        Ok(())
    }

    fn is_full(&self) -> bool {
        self.commits.load(Ordering::SeqCst) >= self.full_after
    }

    fn snapshot(&mut self, _: u64, cells: &NamedCells<'_>) -> std::io::Result<()> {
        // The cells are written out as a snapshot would write them.
        drop(serde_json::to_string(cells).unwrap());
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        self.commits.store(0, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn a_snapshot_is_taken_in_the_commit_or_left_for_later() {
    let taken = |snapshots: &Arc<AtomicUsize>| snapshots.load(Ordering::SeqCst);
    for deferred in [false, true] {
        let snapshots = Arc::new(AtomicUsize::new(0));
        let mut database = database();
        database.set_journal(Box::new(Counted {
            full_after: 2,
            snapshots: Arc::clone(&snapshots),
            ..Counted::default()
        }));
        if deferred {
            database.defer_snapshots();
        }
        database
            .write(None, &[set("Sales", ["A", "Units"], 1.0)])
            .unwrap();
        assert!(!database.snapshot_if_wanted(), "not yet full");
        database
            .write(None, &[set("Sales", ["B", "Units"], 2.0)])
            .unwrap();
        match deferred {
            // Taken inside the commit that filled the log.
            false => {
                assert_eq!(taken(&snapshots), 1);
                assert!(!database.snapshot_if_wanted());
            }
            // Left until asked for, and taken once.
            true => {
                assert_eq!(taken(&snapshots), 0);
                assert!(database.snapshot_if_wanted());
                assert_eq!(taken(&snapshots), 1);
                assert!(!database.snapshot_if_wanted());
            }
        }
        assert_eq!(units(&database, None, "All"), 3.0);
    }
}

#[test]
fn every_cell_is_written_out_as_a_cell_write_is() {
    let mut database = database();
    let note = CellWrite {
        value: Scalar::Text("a \"quoted\"\nline".to_owned()),
        ..set("Sales", ["A", "Note"], 0.0)
    };
    let writes = [
        set("Sales", ["A", "Units"], 1.5),
        set("Sales", ["B", "Units"], -2.0),
        note,
    ];
    database.write(None, &writes).unwrap();
    let written = serde_json::to_string(&database.named_cells()).unwrap();
    assert_eq!(written, serde_json::to_string(&database.cells()).unwrap());
    assert_eq!(database.cells().len(), 3);
}

/// A journal that keeps each commit as it would be written out.
#[derive(Debug, Default)]
struct Written(Arc<std::sync::Mutex<Vec<String>>>);

impl Journal for Written {
    fn commit(&mut self, writes: &NamedWrites<'_>) -> std::io::Result<()> {
        self.0.lock().unwrap().push(serde_json::to_string(writes)?);
        Ok(())
    }

    fn model(&mut self, _: u64, _: &ModelDef) -> std::io::Result<()> {
        Ok(())
    }

    fn is_full(&self) -> bool {
        false
    }

    fn snapshot(&mut self, _: u64, _: &NamedCells<'_>) -> std::io::Result<()> {
        Ok(())
    }
}

/// A journal that keeps what each commit changed, as the history of cells would write it.
#[derive(Debug, Default)]
struct Changed(Arc<std::sync::Mutex<Vec<serde_json::Value>>>);

impl Journal for Changed {
    fn commit(&mut self, _: &NamedWrites<'_>) -> std::io::Result<()> {
        Ok(())
    }

    fn model(&mut self, _: u64, _: &ModelDef) -> std::io::Result<()> {
        Ok(())
    }

    fn is_full(&self) -> bool {
        false
    }

    fn snapshot(&mut self, _: u64, _: &NamedCells<'_>) -> std::io::Result<()> {
        Ok(())
    }

    fn changes(&mut self, changes: &Changes<'_>) {
        self.0
            .lock()
            .unwrap()
            .push(serde_json::to_value(changes).unwrap());
    }
}

#[test]
fn a_text_cell_with_a_picklist_takes_only_what_it_offers() {
    let note = |value: &str| CellWrite {
        value: Scalar::Text(value.to_owned()),
        ..set("Sales", ["A", "Note"], 0.0)
    };
    let mut database = database();
    // Written before the picklist, a text stays when one is given.
    database.write(None, &[note("Odd")]).unwrap();
    let mut definition = definition();
    let offered = ["Low", "High"].map(str::to_owned).to_vec();
    definition.dimensions[1].elements[1].picklist = Some(PicklistDef::Values(offered));
    database.update_model(1, definition.clone()).unwrap();
    let read = |database: &Database| database.read(None, &cell("Sales", ["A", "Note"]));
    assert_eq!(read(&database), Ok(Scalar::Text("Odd".to_owned())));

    // A value offered is taken; another is refused, saying what is offered; emptying is allowed.
    database.write(None, &[note("High")]).unwrap();
    let refused = database.write(None, &[note("Medium")]).unwrap_err();
    assert!(
        matches!(&refused, Error::Write { index: 0, error: CellError::NotOffered { offered } } if offered == &["Low", "High"]),
        "{refused:?}"
    );
    assert!(refused.to_string().contains("Low, High"), "{refused}");
    assert_eq!(read(&database), Ok(Scalar::Text("High".to_owned())));
    database.write(None, &[note("")]).unwrap();

    // A picklist may offer a dimension's elements, or a subset's.
    let members = |subset: Option<&str>| {
        PicklistDef::Members(MembersDef {
            dimension: "product".to_owned(),
            subset: subset.map(str::to_owned),
        })
    };
    definition.dimensions[1].elements[1].picklist = Some(members(None));
    database.update_model(2, definition.clone()).unwrap();
    database.write(None, &[note("B")]).unwrap();
    let last = SubsetDef::Elements(vec!["B".to_owned()]);
    definition.dimensions[0]
        .subsets
        .insert("Last".to_owned(), last);
    definition.dimensions[1].elements[1].picklist = Some(members(Some("last")));
    database.update_model(3, definition.clone()).unwrap();
    let refused = database.write(None, &[note("A")]).unwrap_err();
    assert!(
        matches!(&refused, Error::Write { error: CellError::NotOffered { offered }, .. } if offered == &["B"]),
        "{refused:?}"
    );

    // A source that is not there is refused, and only a string element takes a picklist.
    definition.dimensions[1].elements[1].picklist = Some(members(Some("Nowhere")));
    assert!(matches!(
        Database::new(definition.clone()),
        Err(ModelError::Unknown { .. })
    ));
    definition.dimensions[1].elements[1].picklist = None;
    definition.dimensions[1].elements[0].picklist = Some(members(None));
    assert!(matches!(
        Database::new(definition),
        Err(ModelError::Picklist { .. })
    ));
}

#[test]
fn a_commit_records_who_changed_each_cell_from_what_to_what() {
    let changed = Changed::default();
    let lines = Arc::clone(&changed.0);
    let mut database = database();
    database.set_journal(Box::new(changed));
    // Unsigned, and at once.
    database
        .write(None, &[set("Sales", ["A", "Units"], 5.0)])
        .unwrap();
    // Begun signed, the transaction keeps the name; writing nothing where nothing was changes nothing.
    database.sign(Some("ada"));
    let transaction = database.begin();
    database.sign(None);
    let writes = [
        set("Sales", ["A", "Units"], 7.0),
        set("Sales", ["B", "Units"], 0.0),
    ];
    database.write(Some(transaction), &writes).unwrap();
    database.commit(transaction).unwrap();
    // A value written again as it was is no change at all.
    database
        .write(None, &[set("Sales", ["A", "Units"], 7.0)])
        .unwrap();
    let lines = lines.lock().unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["author"], serde_json::Value::Null);
    let cell = |before: f64, after: f64| json!([{ "cube": "Sales", "at": ["A", "Units"], "before": before, "after": after }]);
    assert_eq!(lines[0]["cells"], cell(0.0, 5.0));
    assert_eq!(lines[1]["author"], "ada");
    assert_eq!(lines[1]["cells"], cell(5.0, 7.0));
    assert!(lines[1]["time"].as_u64().unwrap() > 1_700_000_000);
}

#[test]
fn a_commit_is_recorded_as_the_cell_writes_it_holds() {
    let written = Written::default();
    let lines = Arc::clone(&written.0);
    let mut database = database();
    database.set_journal(Box::new(written));
    let note = CellWrite {
        value: Scalar::Text("a \"quoted\"\nline".to_owned()),
        ..set("Sales", ["A", "Note"], 0.0)
    };
    let mut writes = vec![
        set("Sales", ["A", "Units"], 1.5),
        set("Sales", ["B", "Units"], 0.0),
        note,
    ];
    database.write(None, &writes).unwrap();
    let lines = lines.lock().unwrap();
    assert_eq!(lines.len(), 1);
    let mut recorded: Vec<CellWrite> = serde_json::from_str(&lines[0]).unwrap();
    recorded.sort_by(|a, b| a.at.cmp(&b.at));
    writes.sort_by(|a, b| a.at.cmp(&b.at));
    assert_eq!(recorded, writes);
}

#[test]
fn a_sandbox_keeps_its_values_to_itself_until_they_are_published() {
    let mut database = database();
    let sandbox = database.create_sandbox("ada", " Plan ").unwrap();
    assert_eq!((sandbox.name.as_str(), sandbox.pending), ("Plan", 0));
    let id = sandbox.transaction;
    // Another of the same name, in any case, is refused; another user may have one.
    assert!(matches!(
        database.create_sandbox("Ada", "plan"),
        Err(Error::Sandbox(_))
    ));
    database.create_sandbox("lin", "Plan").unwrap();

    database
        .write(Some(id), &[set("Sales", ["A", "Units"], 5.0)])
        .unwrap();
    assert_eq!(units(&database, Some(id), "A"), 5.0);
    assert_eq!(units(&database, None, "A"), 0.0);
    let theirs = database.sandboxes("ADA");
    assert_eq!((theirs.len(), theirs[0].pending), (1, 1));

    // It holds no locks and waits on nobody, and is no open transaction: another writes
    // the same cell and locks the cube while it holds its value.
    assert!(database.open_transactions().is_empty());
    assert!(matches!(
        database.lock(id, Some("Sales"), LockMode::Exclusive),
        Err(Error::Sandbox(_))
    ));
    let other = database.begin();
    database
        .write(Some(other), &[set("Sales", ["A", "Units"], 7.0)])
        .unwrap();
    database
        .lock(other, Some("Sales"), LockMode::Exclusive)
        .unwrap();
    // Published while the cube is locked, it is refused, and keeps its value.
    assert!(matches!(database.commit(id), Err(Error::Locked(_))));
    database.commit(other).unwrap();
    assert_eq!(units(&database, None, "A"), 7.0);
    assert_eq!(units(&database, Some(id), "A"), 5.0);

    // Published, its values are stored, and it stays, empty; rolled back, it is emptied.
    database.commit(id).unwrap();
    assert_eq!(units(&database, None, "A"), 5.0);
    assert_eq!(database.sandboxes("ada")[0].pending, 0);
    database
        .write(Some(id), &[set("Sales", ["B", "Units"], 3.0)])
        .unwrap();
    database.rollback(id).unwrap();
    assert_eq!(units(&database, Some(id), "B"), 0.0);
    assert_eq!(database.sandboxes("ada")[0].transaction, id);

    // However long it waits, it is kept; a plain transaction is not.
    database.set_transaction_timeout(Duration::ZERO);
    let idle = database.begin();
    thread::sleep(Duration::from_millis(5));
    database
        .write(Some(id), &[set("Sales", ["B", "Units"], 4.0)])
        .unwrap();
    assert!(matches!(
        database.rollback(idle),
        Err(Error::UnknownTransaction(_))
    ));
    assert_eq!(units(&database, Some(id), "B"), 4.0);

    database.delete_sandbox("ada", "PLAN").unwrap();
    assert!(database.sandboxes("ada").is_empty());
    let gone = database.read(Some(id), &cell("Sales", ["B", "Units"]));
    assert!(matches!(gone, Err(Error::UnknownTransaction(_))));
    assert!(matches!(
        database.delete_sandbox("ada", "Plan"),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn a_sandbox_is_published_whole_or_not_at_all() {
    let mut database = database();
    let id = database.create_sandbox("ada", "Plan").unwrap().transaction;
    let note = CellWrite {
        value: Scalar::Text("High".to_owned()),
        ..set("Sales", ["A", "Note"], 0.0)
    };
    let writes = [set("Sales", ["B", "Units"], 2.0), note];
    database.write(Some(id), &writes).unwrap();
    // Since it was written, the model has come to offer only "Low" there.
    let mut definition = definition();
    let offered = PicklistDef::Values(vec!["Low".to_owned()]);
    definition.dimensions[1].elements[1].picklist = Some(offered);
    database.update_model(1, definition).unwrap();
    let refused = database.commit(id).unwrap_err();
    let names_the_cell = |message: &str| message.contains("cell (A, Note) of cube 'Sales'");
    assert!(
        matches!(&refused, Error::Sandbox(message) if names_the_cell(message) && message.contains("Low")),
        "{refused}"
    );
    assert_eq!(units(&database, None, "B"), 0.0);
    assert_eq!(database.sandboxes("ada")[0].pending, 2);
}

/// A journal that keeps each sandbox as it would be written out, by its transaction.
#[derive(Debug, Default)]
struct Sandboxes(Arc<std::sync::Mutex<std::collections::BTreeMap<u64, Option<serde_json::Value>>>>);

impl Journal for Sandboxes {
    fn commit(&mut self, _: &NamedWrites<'_>) -> std::io::Result<()> {
        Ok(())
    }

    fn model(&mut self, _: u64, _: &ModelDef) -> std::io::Result<()> {
        Ok(())
    }

    fn is_full(&self) -> bool {
        false
    }

    fn snapshot(&mut self, _: u64, _: &NamedCells<'_>) -> std::io::Result<()> {
        Ok(())
    }

    fn sandbox(&mut self, id: u64, sandbox: Option<&KeptSandbox<'_>>) -> std::io::Result<()> {
        let kept = sandbox.map(|sandbox| serde_json::to_value(sandbox).unwrap());
        self.0.lock().unwrap().insert(id, kept);
        Ok(())
    }
}

#[test]
fn a_sandbox_is_kept_by_the_journal_and_carried_into_a_new_model() {
    let journal = Sandboxes::default();
    let kept = Arc::clone(&journal.0);
    let mut database = database();
    database.set_journal(Box::new(journal));
    let id = database.create_sandbox("ada", "Plan").unwrap().transaction;
    let writes = [
        set("Sales", ["A", "Units"], 5.0),
        set("Sales", ["B", "Units"], 6.0),
    ];
    database.write(Some(id), &writes).unwrap();
    let saved = || kept.lock().unwrap()[&id].clone().unwrap();
    assert_eq!(
        (&saved()["name"], &saved()["owner"]),
        (&json!("Plan"), &json!("ada"))
    );
    assert_eq!(saved()["cells"].as_array().unwrap().len(), 2);

    // In a model where B is gone and C comes before A, the value at A goes where A is now;
    // the one at B has no place.
    let mut definition = definition();
    let product = &mut definition.dimensions[0];
    product.elements.retain(|element| element.name != "B");
    product.elements[0]
        .children
        .retain(|child| child.name != "B");
    let c = serde_json::from_value(json!({ "name": "C", "type": "numeric" })).unwrap();
    product.elements.insert(1, c);
    database.update_model(1, definition.clone()).unwrap();
    assert_eq!(units(&database, Some(id), "A"), 5.0);
    assert_eq!(units(&database, Some(id), "C"), 0.0);
    assert_eq!(database.sandboxes("ada")[0].pending, 1);
    assert_eq!(saved()["cells"].as_array().unwrap().len(), 1);

    // Taken in as the journal kept it, it is the same sandbox, under the same transaction.
    let cells: Vec<CellWrite> = serde_json::from_value(saved()["cells"].clone()).unwrap();
    let mut restored = Database::new(definition).unwrap();
    assert_eq!(restored.restore_sandbox(id, "ada", "Plan", &cells), 0);
    assert_eq!(units(&restored, Some(id), "A"), 5.0);
    assert_eq!(restored.sandboxes("ada")[0].transaction, id);

    database.delete_sandbox("ada", "Plan").unwrap();
    assert_eq!(kept.lock().unwrap()[&id], None);
}

/// A check of the cube `Sales`, as the model writes one.
fn check(
    cells: &str,
    valid: &str,
    message: &str,
    refuse: bool,
) -> epiphany_core::model::ValidationDef {
    let check = json!({ "cells": cells, "valid": valid, "message": message, "refuse": refuse });
    serde_json::from_value(check).unwrap()
}

#[test]
fn a_check_refuses_a_value_it_does_not_hold_for_or_warns_of_it() {
    let mut checked = definition();
    checked.cubes[0].validations = vec![
        check(
            "N: [Measure].[Units]",
            "[Measure].[Units] >= 0",
            "Units are never negative.",
            true,
        ),
        check(
            "[Measure].[Units]",
            "[Measure].[Units] <= 100",
            "Over 100 units is unusual.",
            false,
        ),
    ];
    let mut database = Database::new(checked).unwrap();

    // Refused, a value takes its batch with it, and the error names the check.
    let writes = [
        set("Sales", ["A", "Units"], 5.0),
        set("Sales", ["B", "Units"], -1.0),
    ];
    let refused = database.write(None, &writes).unwrap_err();
    let negative = |message: &String| message == "Units are never negative.";
    assert!(
        matches!(&refused, Error::Write { index: 1, error: CellError::Invalid(message) } if negative(message)),
        "{refused:?}"
    );
    assert_eq!(refused.code(), "invalid_value");
    assert_eq!(units(&database, None, "A"), 0.0);
    // In a transaction, the batch refused is taken back, and what came before it stays.
    let id = database.begin();
    database
        .write(Some(id), &[set("Sales", ["A", "Units"], 5.0)])
        .unwrap();
    let again = database.write(Some(id), &[set("Sales", ["A", "Units"], -5.0)]);
    assert!(again.is_err());
    assert_eq!(units(&database, Some(id), "A"), 5.0);
    assert_eq!(database.savepoint(id).unwrap(), 1);
    database.commit(id).unwrap();
    // A spread is checked as the writes it makes are.
    let spread = Spread {
        cube: "Sales".to_owned(),
        at: vec!["All".to_owned(), "Units".to_owned()],
        value: -10.0,
        method: SpreadMethod::Equal,
        holds: Vec::new(),
    };
    assert!(matches!(
        database.spread(None, &spread),
        Err(Error::Write {
            error: CellError::Invalid(_),
            ..
        })
    ));

    // A value that only breaks a warning is kept, and explained as breaking it, as is a
    // total it makes break it too.
    database
        .write(None, &[set("Sales", ["B", "Units"], 150.0)])
        .unwrap();
    let invalid = |product: &str| {
        let explained = database.explain(None, &cell("Sales", [product, "Units"]), false);
        explained.unwrap().invalid
    };
    let unusual = Invalid {
        message: "Over 100 units is unusual.".to_owned(),
        refuses: false,
    };
    assert_eq!(invalid("B"), [unusual]);
    assert_eq!(invalid("All"), invalid("B"));
    assert!(invalid("A").is_empty());

    // A check that names what is not there is refused with the model, which says which.
    let mut broken = definition();
    broken.cubes[0].validations = vec![check("[Nowhere]", "1", "", false)];
    assert!(matches!(
        Database::new(broken),
        Err(ModelError::Mdx { within, .. }) if within == "validation 1 of cube 'Sales'"
    ));
}
