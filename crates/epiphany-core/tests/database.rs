mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use common::{Rng, Shape, random_model};
use epiphany_core::Scalar;
use epiphany_core::cells::CellError;
use epiphany_core::database::{
    CellRef, CellWrite, Database, Error, Journal, LockMode, ModelUpdate, NamedCells,
};
use epiphany_core::model::ModelDef;
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
    fn commit(&mut self, _: &[CellWrite]) -> std::io::Result<()> {
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
