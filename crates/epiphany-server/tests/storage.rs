mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::Home;
use epiphany_core::Scalar;
use epiphany_core::database::{Asked, CellRef, CellWrite, Database, Error, Line, Sandbox};
use epiphany_core::model::DimensionDef;
use epiphany_server::open;
use serde_json::{Value as Json, json};

const CUBE: &str = "General Ledger";

fn set(at: [&str; 4], value: f64) -> CellWrite {
    CellWrite {
        cube: CUBE.to_owned(),
        at: at.map(str::to_owned).to_vec(),
        value: Scalar::Number(value),
    }
}

fn get(database: &Database, at: [&str; 4]) -> f64 {
    let cell = CellRef {
        cube: CUBE.to_owned(),
        at: at.map(str::to_owned).to_vec(),
    };
    match database.read(None, &cell) {
        Ok(Scalar::Number(number)) => number,
        other => panic!("expected a number, found {other:?}"),
    }
}

const RENT: [&str; 4] = ["Rent", "US", "Jan", "Actual"];
const SALARIES: [&str; 4] = ["Salaries", "UK", "Feb", "Actual"];
const EXPENSES: [&str; 4] = ["Operating Expenses", "Total Entity", "H1", "Actual"];

#[test]
fn committed_cells_survive_reopening() {
    let home = Home::new("reopen");
    home.open().write(None, &[set(RENT, 25.0)]).unwrap();
    assert!(fs::metadata(home.path("data/log.jsonl")).unwrap().len() > 0);

    let mut database = home.open();
    assert_eq!(get(&database, RENT), 25.0);
    // Opening replaced the log with a snapshot.
    assert_eq!(fs::metadata(home.path("data/log.jsonl")).unwrap().len(), 0);
    assert!(home.path("data/snapshot.json").exists());

    database
        .write(None, &[set(SALARIES, 40.0), set(RENT, 0.0)])
        .unwrap();
    drop(database);
    let database = home.open();
    assert_eq!(
        (get(&database, RENT), get(&database, SALARIES)),
        (0.0, 40.0)
    );
    assert_eq!(get(&database, EXPENSES), 40.0);
}

/// JSON read without care rounds about one number in six to a neighbour, so that a cell held
/// another number after a restart than before it. Every leaf of the cube is given a number
/// that is hard to read back: one that was, or one of any size.
#[test]
fn numbers_come_back_from_the_disk_to_the_last_bit() {
    let home = Home::new("exact");
    let mut leaves: Vec<[&str; 4]> = Vec::new();
    for account in ["Product Revenue", "Service Revenue", "Salaries", "Rent"] {
        for entity in ["US", "UK"] {
            for month in ["Jan", "Feb", "Mar", "Apr", "May", "Jun"] {
                for version in ["Actual", "Budget"] {
                    leaves.push([account, entity, month, version]);
                }
            }
        }
    }
    let mut numbers = vec![
        1892571.4285714286,
        -13745285.714285715,
        0.1 + 0.2,
        1.0715660391465826e-75,
        -1.603964615428183e143,
        5e-324,
        f64::MAX,
        -f64::MIN_POSITIVE,
    ];
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    while numbers.len() < leaves.len() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let number = match numbers.len() % 2 {
            0 => f64::from_bits(seed),
            _ => ((seed % 200_001) as f64 - 100_000.0) / 7.0 * 1e3,
        };
        if number.is_finite() && number != 0.0 {
            numbers.push(number);
        }
    }
    let writes: Vec<CellWrite> = leaves
        .iter()
        .zip(&numbers)
        .map(|(&at, &n)| set(at, n))
        .collect();
    home.open().write(None, &writes).unwrap();
    // First from the log, which opening makes a snapshot of; then from the snapshot.
    for from in ["the log", "the snapshot"] {
        let database = home.open();
        for (&at, number) in leaves.iter().zip(&numbers) {
            let read = get(&database, at);
            assert_eq!(
                read.to_bits(),
                number.to_bits(),
                "{at:?} from {from}: {read:e} for {number:e}"
            );
        }
    }
}

#[test]
fn a_directory_serves_one_database_at_a_time() {
    let home = Home::new("lock");
    let first = home.open();
    let refused = open(&home.0).unwrap_err().to_string();
    assert!(
        refused.ends_with("is already in use by another server"),
        "{refused}"
    );
    drop(first);
    home.open();
}

#[test]
fn only_committed_transactions_are_kept() {
    let home = Home::new("transactions");
    let mut database = home.open();
    let (committed, rolled_back, open) = (database.begin(), database.begin(), database.begin());
    database.write(Some(committed), &[set(RENT, 1.0)]).unwrap();
    database
        .write(Some(rolled_back), &[set(SALARIES, 2.0)])
        .unwrap();
    database
        .write(Some(open), &[set(["Rent", "UK", "Mar", "Actual"], 4.0)])
        .unwrap();
    database.commit(committed).unwrap();
    database.rollback(rolled_back).unwrap();
    drop(database);
    assert_eq!(get(&home.open(), EXPENSES), 1.0);
}

#[test]
fn a_commit_cut_short_is_ignored_and_does_no_harm() {
    let home = Home::new("torn");
    let mut database = home.open();
    database.write(None, &[set(RENT, 25.0)]).unwrap();
    database.write(None, &[set(SALARIES, 40.0)]).unwrap();
    drop(database);
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(home.path("data/log.jsonl"))
        .unwrap();
    log.write_all(br#"[{"cube":"General Ledger","at":["Rent","UK"#)
        .unwrap();
    drop(log);

    let mut database = home.open();
    assert_eq!(get(&database, EXPENSES), 65.0);
    database
        .write(None, &[set(["Rent", "UK", "Mar", "Actual"], 5.0)])
        .unwrap();
    drop(database);
    assert_eq!(get(&home.open(), EXPENSES), 70.0);
}

#[test]
fn a_damaged_commit_with_others_after_it_is_not_passed_over() {
    let home = Home::new("middle");
    let mut database = home.open();
    for (month, value) in [("Jan", 1.0), ("Feb", 2.0), ("Mar", 4.0)] {
        let cell = set(["Rent", "US", month, "Actual"], value);
        database.write(None, &[cell]).unwrap();
    }
    drop(database);
    let log = home.path("data/log.jsonl");
    let whole = fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = whole.lines().collect();
    assert_eq!(lines.len(), 3);

    // The second of three commits damaged: to open would be to lose the third as well.
    let damaged = [lines[0], &lines[1][..lines[1].len() / 2], lines[2]].join("\n") + "\n";
    fs::write(&log, damaged).unwrap();
    let refusal = open(&home.0).unwrap_err().to_string();
    assert!(refusal.contains("line 2 of 3 cannot be read"), "{refusal}");
    // Refused, the file is as it was left, for whoever comes to mend it.
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 3);

    // The last one cut short is the commit that was being written when the server died.
    let torn = [lines[0], lines[1], &lines[2][..lines[2].len() / 2]].join("\n");
    fs::write(&log, torn).unwrap();
    assert_eq!(get(&home.open(), EXPENSES), 3.0);
}

/// The example model with France added to the entities.
fn with_france(database: &Database) -> epiphany_core::model::ModelDef {
    let mut model = serde_json::to_value(database.definition()).unwrap();
    let entity = model["dimensions"].as_array_mut().unwrap();
    let entity = entity.iter_mut().find(|d| d["name"] == "Entity").unwrap();
    let elements = entity["elements"].as_array_mut().unwrap();
    elements[0]["children"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "name": "FR" }));
    elements.push(json!({ "name": "FR", "type": "numeric" }));
    serde_json::from_value(model).unwrap()
}

#[test]
fn a_model_update_is_kept_along_with_the_model_it_replaced() {
    let home = Home::new("model");
    let mut database = home.open();
    database.write(None, &[set(RENT, 25.0)]).unwrap();
    let updated = with_france(&database);
    assert_eq!(
        database.update_model(1, updated.clone()).unwrap().version,
        2
    );
    database
        .write(None, &[set(["Rent", "FR", "Jan", "Actual"], 5.0)])
        .unwrap();
    drop(database);

    let mut database = home.open();
    assert_eq!(database.model_version(), 2);
    assert!(
        database
            .model()
            .dimension("Entity")
            .unwrap()
            .element("FR")
            .is_some()
    );
    assert_eq!(get(&database, EXPENSES), 30.0);
    let version: Json =
        serde_json::from_str(&fs::read_to_string(home.path("model/model.json")).unwrap()).unwrap();
    assert_eq!(version, json!({ "version": 2 }));
    // The first model had no France.
    let first = fs::read_to_string(home.path("data/models/1/dimensions/entity.json")).unwrap();
    assert!(!first.contains("FR"));

    database.update_model(2, updated).unwrap();
    drop(database);
    assert!(home.path("data/models/2/model.json").exists());
    assert_eq!(home.open().model_version(), 3);
}

#[test]
fn an_interrupted_model_update_is_finished_or_discarded() {
    // Stopped after the old model was moved away: the new one takes its place.
    let home = Home::new("swap");
    fs::rename(home.path("model"), home.path("model.next")).unwrap();
    home.open().write(None, &[set(RENT, 25.0)]).unwrap();
    assert!(home.path("model").exists() && !home.path("model.next").exists());

    // Stopped while the new model was still being written: it is thrown away.
    fs::create_dir_all(home.path("model.next/dimensions")).unwrap();
    assert_eq!(get(&home.open(), RENT), 25.0);
    assert!(!home.path("model.next").exists());
}

#[test]
fn cells_that_no_longer_fit_a_hand_edited_model_are_dropped() {
    let home = Home::new("edited");
    let writes = [set(RENT, 25.0), set(SALARIES, 40.0)];
    home.open().write(None, &writes).unwrap();
    let entity = json!({ "name": "Entity", "elements": [{ "name": "US", "type": "numeric" }] });
    fs::write(
        home.path("model/dimensions/entity.json"),
        entity.to_string(),
    )
    .unwrap();

    let database = home.open();
    assert_eq!(get(&database, RENT), 25.0);
    assert_eq!(database.cells(), [set(RENT, 25.0)]);
}

/// A running server process, killed without warning when dropped.
struct Server(Child);

impl Server {
    fn start(home: &Home, port: u16) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_epiphany-server"));
        let address = format!("127.0.0.1:{port}");
        let command = command.arg("serve").arg(&home.0).arg(address);
        let quiet = command.stdout(Stdio::null()).stderr(Stdio::null());
        let server = Self(quiet.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "the server did not start");
            thread::sleep(Duration::from_millis(50));
        }
        server
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Posts JSON and returns the status and the body.
fn post(port: u16, path: &str, body: Json) -> (u16, String) {
    send(port, "POST", path, body)
}

/// Sends a request with a JSON body and returns the status and the body of the answer.
fn send(port: u16, method: &str, path: &str, body: Json) -> (u16, String) {
    let body = body.to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(format!("{head}{body}").as_bytes())
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    (status, body.to_owned())
}

#[test]
fn a_killed_server_restarts_with_every_acknowledged_write() {
    let home = Home::new("killed");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cell = |at: [&str; 4]| json!({ "cube": CUBE, "at": at });
    let write = |at: [&str; 4], value: f64| json!({ "cube": CUBE, "at": at, "value": value });

    let server = Server::start(&home, port);
    for month in ["Jan", "Feb", "Mar"] {
        let cells = json!({ "cells": [write(["Rent", "US", month, "Actual"], 10.0)] });
        assert_eq!(post(port, "/cells/write", cells).0, 204);
    }
    // A transaction that never commits must leave nothing behind.
    let (_, body) = post(port, "/transactions", json!(null));
    let transaction = serde_json::from_str::<Json>(&body).unwrap()["transaction"].clone();
    let pending = json!({ "transaction": transaction, "cells": [write(SALARIES, 99.0)] });
    assert_eq!(post(port, "/cells/write", pending).0, 204);
    drop(server);

    let _server = Server::start(&home, port);
    let (status, body) = post(port, "/cells/read", json!({ "cells": [cell(EXPENSES)] }));
    assert_eq!(status, 200);
    let body: Json = serde_json::from_str(&body).unwrap();
    assert_eq!(body, json!({ "cells": [{ "value": 30.0 }] }));
}

#[test]
fn slow_queries_do_not_hold_up_a_quick_request() {
    let home = Home::new("slow");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _server = Server::start(&home, port);
    // Calculated members that each add the one before to itself: millions of steps.
    let mut slow = "WITH MEMBER [Account].[M0] AS 1".to_owned();
    for member in 1..=23 {
        let before = member - 1;
        slow += &format!(
            " MEMBER [Account].[M{member}] AS [Account].[M{before}] + [Account].[M{before}]"
        );
    }
    slow += " SELECT {[Account].[M23]} ON COLUMNS FROM [General Ledger]";

    // More of them at once than the machine has processors.
    let finished = std::sync::atomic::AtomicBool::new(false);
    let many = thread::available_parallelism().map_or(4, usize::from) + 2;
    thread::scope(|scope| {
        for _ in 0..many {
            scope.spawn(|| {
                assert_eq!(post(port, "/mdx", json!({ "mdx": slow })).0, 200);
                finished.store(true, std::sync::atomic::Ordering::SeqCst);
            });
        }
        thread::sleep(Duration::from_millis(100));
        let cell = json!({ "cells": [{ "cube": CUBE, "at": RENT }] });
        assert_eq!(post(port, "/cells/read", cell).0, 200);
        assert!(
            !finished.load(std::sync::atomic::Ordering::SeqCst),
            "the quick request waited for a slow one to finish"
        );
    });
}

#[test]
fn requests_that_never_finish_arriving_hold_nobody_up() {
    let home = Home::new("stalled");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _server = Server::start(&home, port);
    // Six hundred requests to sign in, each promising a body and sending the start of one:
    // more than there are threads to handle requests on.
    let head = "POST /sessions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 200\r\n\r\n{\"name\":";
    let stalled: Vec<TcpStream> = (0..600)
        .map(|_| {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.write_all(head.as_bytes()).unwrap();
            stream
        })
        .collect();
    thread::sleep(Duration::from_millis(500));

    // An ordinary request is still answered.
    let (answer, asked) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let cell = json!({ "cells": [{ "cube": CUBE, "at": RENT }] });
        let _ = answer.send(post(port, "/cells/read", cell).0);
    });
    let status = asked.recv_timeout(Duration::from_secs(10));
    assert_eq!(
        status,
        Ok(200),
        "the request waited behind those that never arrived"
    );
    drop(stalled);
}

/// Every file beneath a directory, with what it holds.
fn files(directory: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        match path.is_dir() {
            true => found.extend(files(&path)),
            false => found.push((path.clone(), fs::read(&path).unwrap())),
        }
    }
    found
}

#[test]
fn damaged_files_are_refused_or_recovered_from_and_never_fatal() {
    let home = Home::new("damaged");
    let mut database = home.open();
    // Enough history for there to be something of every kind on disk.
    database.write(None, &[set(RENT, 25.0)]).unwrap();
    let france = with_france(&database);
    database
        .update_model(database.model_version(), france)
        .unwrap();
    database.write(None, &[set(SALARIES, 40.0)]).unwrap();
    drop(database);

    let whole = files(&home.0);
    assert!(whole.len() > 5, "{} files", whole.len());
    let mut down = Vec::new();
    let mut opened = 0;
    for (path, contents) in &whole {
        if path.ends_with("lock") {
            continue;
        }
        let half = contents.len() / 2;
        let mut flipped = contents.clone();
        if let Some(byte) = flipped.get_mut(half) {
            *byte ^= 0xff;
        }
        let damages: [(&str, Vec<u8>); 9] = [
            ("emptied", Vec::new()),
            ("cut in half", contents[..half].to_vec()),
            (
                "without its last byte",
                contents[..contents.len().saturating_sub(1)].to_vec(),
            ),
            ("with a byte turned over", flipped),
            (
                "followed by nonsense",
                [contents.as_slice(), b"\n\x00\xff{[\"nonsense"].concat(),
            ),
            ("made of nulls", vec![0; contents.len()]),
            ("holding only a bracket", b"[".to_vec()),
            ("holding null", b"null".to_vec()),
            (
                "twice over",
                [contents.as_slice(), contents.as_slice()].concat(),
            ),
        ];
        for (damage, damaged) in damages {
            // Everything as it was, but for the one file.
            for (other, contents) in &whole {
                fs::write(other, contents).unwrap();
            }
            fs::write(path, damaged).unwrap();
            let attempt = std::panic::catch_unwind(|| {
                let Ok(mut database) = open(&home.0) else {
                    return false;
                };
                // Opened, it must also answer and take a write.
                drop(database.query(
                    None,
                    "SELECT [Account].Members ON COLUMNS FROM [General Ledger]",
                ));
                drop(database.write(None, &[set(RENT, 1.0)]));
                true
            });
            match attempt {
                Ok(taken) => opened += usize::from(taken),
                Err(_) => down.push(format!("{} {damage}", path.display())),
            }
        }
    }
    assert!(
        down.is_empty(),
        "the server came down on:\n{}",
        down.join("\n")
    );
    // Some damage is recovered from and some is refused: both happen.
    assert!(opened > 0, "nothing damaged was ever opened");
}

/// A small generator of numbers that look random, so that a failure can be had again.
struct Dice(u64);

impl Dice {
    fn roll(&mut self, sides: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % sides as u64) as usize
    }
}

#[test]
fn many_clients_at_once_leave_every_cell_as_its_last_writer_left_it() {
    let home = Home::new("crowded");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let server = Server::start(&home, port);

    // Forty-eight leaf cells, shared out among eight clients: each writes only its own.
    let mut cells = Vec::new();
    for account in ["Rent", "Salaries"] {
        for entity in ["US", "UK"] {
            for month in ["Jan", "Feb", "Mar", "Apr", "May", "Jun"] {
                for version in ["Actual", "Budget"] {
                    cells.push([account, entity, month, version]);
                }
            }
        }
    }
    let cell = |at: [&str; 4]| json!({ "cube": CUBE, "at": at });
    let write = |at: [&str; 4], value: f64| json!({ "cube": CUBE, "at": at, "value": value });
    let queries = [
        "SELECT [Account].Members ON COLUMNS, [Period].Members ON ROWS FROM [General Ledger]",
        "SELECT NON EMPTY [Account].Members * [Entity].Members ON ROWS, {[Version].[Actual]} ON COLUMNS FROM [General Ledger]",
        "SELECT TopCount([Period].Members, 3, [Account].[Rent]) ON COLUMNS FROM [General Ledger]",
    ];

    let clients: Vec<_> = (0..8)
        .map(|client| {
            let mine: Vec<[&str; 4]> = cells.iter().copied().skip(client).step_by(8).collect();
            let all = cells.clone();
            thread::spawn(move || {
                let mut dice = Dice(0x9E37_79B9 + client as u64);
                // What each of this client's cells holds, as far as the server has agreed.
                let mut held = vec![0.0; mine.len()];
                let mut errors = Vec::new();
                let mut said = |what: &str, status: u16, body: &str| {
                    if status >= 500 {
                        errors.push(format!("{what}: {status} {body}"));
                    }
                };
                for step in 0..120 {
                    let own = dice.roll(mine.len());
                    let value = (1 + client * 1000 + step) as f64;
                    match dice.roll(8) {
                        // A write on its own.
                        0 | 1 => {
                            let body = json!({ "cells": [write(mine[own], value)] });
                            let (status, answer) = post(port, "/cells/write", body);
                            said("write", status, &answer);
                            if status == 204 {
                                held[own] = value;
                            }
                        }
                        // A transaction, with or without a lock, committed or rolled back.
                        2 | 3 => {
                            let (status, answer) = post(port, "/transactions", json!(null));
                            said("begin", status, &answer);
                            let Ok(begun) = serde_json::from_str::<Json>(&answer) else {
                                continue;
                            };
                            let id = begun["transaction"].clone();
                            if dice.roll(3) == 0 {
                                let mode = ["read", "write", "exclusive"][dice.roll(3)];
                                let lock = json!({ "cube": CUBE, "mode": mode });
                                let (status, answer) =
                                    post(port, &format!("/transactions/{id}/locks"), lock);
                                said("lock", status, &answer);
                            }
                            let body =
                                json!({ "transaction": id, "cells": [write(mine[own], value)] });
                            let (wrote, answer) = post(port, "/cells/write", body);
                            said("write within", wrote, &answer);
                            let end = ["commit", "rollback"][dice.roll(2)];
                            let (ended, answer) =
                                post(port, &format!("/transactions/{id}/{end}"), json!(null));
                            said(end, ended, &answer);
                            if wrote == 204 && end == "commit" && ended < 300 {
                                held[own] = value;
                            }
                        }
                        // Reads of anybody's cells, alone and by the batch.
                        4 | 5 => {
                            let some: Vec<Json> = (0..1 + dice.roll(20))
                                .map(|_| cell(all[dice.roll(all.len())]))
                                .collect();
                            let (status, answer) =
                                post(port, "/cells/read", json!({ "cells": some }));
                            said("read", status, &answer);
                        }
                        6 => {
                            let mdx = queries[dice.roll(queries.len())];
                            let (status, answer) = post(port, "/mdx", json!({ "mdx": mdx }));
                            said("query", status, &answer);
                        }
                        // The model put back as it stands: a new version of it, or a refusal
                        // because it has moved on or a transaction is open.
                        7 if dice.roll(4) == 0 => {
                            let (status, answer) = send(port, "GET", "/model", json!(null));
                            said("model", status, &answer);
                            let Ok(current) = serde_json::from_str::<Json>(&answer) else {
                                continue;
                            };
                            let update = json!({ "based_on": current["version"], "model": current["model"] });
                            let (status, answer) = send(port, "PUT", "/model", update);
                            said("model update", status, &answer);
                        }
                        _ => {
                            let body = json!({ "cells": [cell(EXPENSES), cell(mine[own])] });
                            let (status, answer) = post(port, "/cells/explain", body);
                            said("explain", status, &answer);
                        }
                    }
                }
                (mine, held, errors)
            })
        })
        .collect();

    let mut expected = Vec::new();
    for client in clients {
        let (mine, held, errors) = client.join().unwrap();
        assert!(errors.is_empty(), "{}", errors.join("\n"));
        expected.extend(mine.into_iter().zip(held));
    }
    // Every cell is as its one writer last left it: now, and after the server is killed.
    let check = |port: u16| {
        let asked: Vec<Json> = expected.iter().map(|(at, _)| cell(*at)).collect();
        let (status, answer) = post(port, "/cells/read", json!({ "cells": asked }));
        assert_eq!(status, 200, "{answer}");
        let answer: Json = serde_json::from_str(&answer).unwrap();
        for ((at, value), read) in expected.iter().zip(answer["cells"].as_array().unwrap()) {
            assert_eq!(read["value"].as_f64(), Some(*value), "{at:?}");
        }
    };
    check(port);
    drop(server);
    let _server = Server::start(&home, port);
    check(port);
}

#[test]
fn a_snapshot_laid_out_for_reading_still_loads() {
    // Snapshots were written laid out, an element to a line, until 2026-11-21, and are now
    // written close. One from before must load as it always did.
    let home = Home::new("laid-out");
    drop(home.open());
    let cell = |at: [&str; 4], value: f64| json!({ "cube": CUBE, "at": at, "value": value });
    let snapshot = json!({
        "model_version": 1,
        "cells": [cell(RENT, 25.0), cell(SALARIES, 40.0)]
    });
    let laid_out = serde_json::to_string_pretty(&snapshot).unwrap();
    assert!(laid_out.lines().count() > 10);
    fs::write(home.path("data/snapshot.json"), laid_out + "\n").unwrap();
    fs::write(home.path("data/log.jsonl"), "").unwrap();
    let database = home.open();
    assert_eq!(get(&database, EXPENSES), 65.0);
    drop(database);
    // Opened, the snapshot is written again, close, and loads again.
    assert_eq!(get(&home.open(), EXPENSES), 65.0);
}

#[test]
fn a_snapshot_loads_whatever_order_its_fields_come_in_and_drops_what_no_longer_fits() {
    let home = Home::new("odd-snapshot");
    drop(home.open());
    let snapshot = json!({
        "model_version": 1,
        // More than the file could hold is not made room for.
        "stored": { CUBE: 1_000_000_000_000_u64, "Gone": 5 },
        "cells": [
            { "at": RENT, "cube": CUBE, "value": 25.0 },
            { "value": 40.0, "cube": "general ledger", "at": SALARIES },
            { "cube": "Gone", "at": RENT, "value": 1.0 },
            { "cube": CUBE, "at": ["Rent", "Mars", "Jan", "Actual"], "value": 1.0 },
            { "cube": CUBE, "at": ["Rent", "US", "Jan"], "value": 1.0 },
            { "cube": CUBE, "at": ["Rent", "US", "Jan", "Actual", "More"], "value": 1.0 },
            { "at": ["Rent", "US", "Jan", "Actual", "More"], "cube": CUBE, "value": 1.0 },
            { "cube": CUBE, "at": EXPENSES, "value": 1.0 }
        ]
    });
    fs::write(home.path("data/snapshot.json"), snapshot.to_string()).unwrap();
    fs::write(home.path("data/log.jsonl"), "").unwrap();
    let database = home.open();
    assert_eq!(get(&database, EXPENSES), 65.0);
    assert_eq!(database.cells().len(), 2);
    drop(database);

    // A cell without its value is damage, and the directory is not opened.
    let damaged = json!({ "model_version": 1, "cells": [{ "cube": CUBE, "at": RENT }] });
    fs::write(home.path("data/snapshot.json"), damaged.to_string()).unwrap();
    let error = epiphany_server::open(&home.0).unwrap_err().to_string();
    assert!(error.contains("value"), "{error}");
}

#[test]
fn a_snapshot_cell_named_twice_is_damage_and_one_given_as_a_list_loads() {
    let home = Home::new("snapshot-shapes");
    drop(home.open());
    let listed =
        json!({ "model_version": 1, "cells": [[CUBE, RENT, 25.0], [CUBE, SALARIES, 40.0]] });
    fs::write(home.path("data/snapshot.json"), listed.to_string()).unwrap();
    fs::write(home.path("data/log.jsonl"), "").unwrap();
    assert_eq!(get(&home.open(), EXPENSES), 65.0);

    for twice in [
        r#"{"cube":"General Ledger","at":["Rent","US","Jan","Actual"],"cube":"Other","value":1}"#,
        r#"{"cube":"General Ledger","at":["Rent","US","Jan","Actual"],"at":["Rent"],"value":1}"#,
        r#"{"cube":"General Ledger","at":["Rent","US","Jan","Actual"],"value":1,"value":2}"#,
    ] {
        let damaged = format!(r#"{{"model_version":1,"cells":[{twice}]}}"#);
        fs::write(home.path("data/snapshot.json"), damaged).unwrap();
        let error = epiphany_server::open(&home.0).unwrap_err().to_string();
        assert!(error.contains("duplicate field"), "{error}");
    }
}

#[test]
fn comments_outlast_the_process_and_so_do_those_taken_away() {
    let home = Home::new("comments-kept");
    let mut database = home.open();
    let at = ["Rent", "US", "Jan", "Actual"].map(str::to_owned);
    let first = database
        .comment(CUBE, &at, "Ada", 10, "Rent rose.")
        .unwrap();
    let second = database.comment(CUBE, &at, "Grace", 20, "It did.").unwrap();
    database.uncomment(first.id).unwrap();
    drop(database);

    let mut reopened = home.open();
    assert_eq!(
        reopened.comments(CUBE).unwrap(),
        std::slice::from_ref(&second)
    );
    // Ids go on from the last one given, never again.
    let third = reopened.comment(CUBE, &at, "Ada", 30, "Fixed.").unwrap();
    assert!(third.id > second.id);
    assert!(matches!(
        reopened.comment(CUBE, &at, "Ada", 40, ""),
        Err(Error::Comment(_))
    ));
}

#[test]
fn the_history_of_a_cell_outlasts_snapshots_and_the_process() {
    let home = Home::new("history-kept");
    let mut database = home.open();
    database.sign(Some("Ada"));
    let transaction = database.begin();
    database.sign(None);
    database
        .write(Some(transaction), &[set(RENT, 25.0)])
        .unwrap();
    database.commit(transaction).unwrap();
    database
        .write(None, &[set(RENT, 30.0), set(SALARIES, 40.0)])
        .unwrap();
    drop(database);

    // Opening again replaces the log with a snapshot; the history stays as it was.
    let database = home.open();
    let read = |asked: Asked, most: usize| database.history(asked).unwrap().read(0, most).unwrap();
    let cell = |at: [&str; 4]| Asked {
        cube: Some(CUBE.to_owned()),
        at: Some(at.map(str::to_owned).to_vec()),
        ..Asked::default()
    };
    let changes = read(cell(RENT), 10);
    let seen: Vec<_> = changes
        .iter()
        .map(|change| {
            (
                change.author.as_deref(),
                change.before.clone(),
                change.after.clone(),
            )
        })
        .collect();
    let number = Scalar::Number;
    assert_eq!(
        seen,
        [
            (None, number(25.0), number(30.0)),
            (Some("Ada"), number(0.0), number(25.0)),
        ]
    );
    // The newest, at most as many as asked for, after any passed over; none for a cell never
    // written.
    assert_eq!(read(cell(RENT), 1), changes[..1]);
    let older = database.history(cell(RENT)).unwrap().read(1, 10).unwrap();
    assert_eq!(older, changes[1..]);
    assert!(read(cell(EXPENSES), 10).is_empty());

    // Across the cube, every change; by a user, in any case, theirs; and by time.
    let cube = Asked {
        cube: Some("general ledger".to_owned()),
        ..Asked::default()
    };
    assert_eq!(read(cube.clone(), 10).len(), 3);
    let by_ada = read(
        Asked {
            author: Some("ADA".to_owned()),
            ..cube.clone()
        },
        10,
    );
    assert_eq!(by_ada, changes[1..]);
    let later = Asked {
        since: Some(changes[0].time + 1),
        ..cube
    };
    assert!(read(later, 10).is_empty());
}

#[test]
fn a_cells_detail_outlives_the_process_with_its_value() {
    let home = Home::new("details-kept");
    let mut database = home.open();
    let rent = CellRef {
        cube: CUBE.to_owned(),
        at: RENT.map(str::to_owned).to_vec(),
    };
    let line = |label: &str, value: f64| Line {
        label: label.to_owned(),
        value,
    };
    let lines = vec![line("Office", 30.0), line("Store", 12.5)];
    database
        .write_detail(None, &rent, lines.clone(), None)
        .unwrap();
    drop(database);
    // Opened again, the commit in the log gives it back; opened once more, the snapshot
    // that took the log's place does.
    for _ in 0..2 {
        let database = home.open();
        assert_eq!(database.detail(None, &rent).unwrap(), lines);
        assert_eq!(get(&database, RENT), 42.5);
    }
    // A value written plainly takes it away for good.
    let mut database = home.open();
    database.write(None, &[set(RENT, 40.0)]).unwrap();
    drop(database);
    assert!(home.open().detail(None, &rent).unwrap().is_empty());
}

#[test]
fn a_sandbox_outlives_the_process_until_it_is_deleted() {
    let home = Home::new("sandboxes-kept");
    let mut database = home.open();
    let stored = get(&database, RENT);
    let id = database.create_sandbox("ada", "Plan").unwrap().transaction;
    let writes = [set(RENT, stored + 25.0), set(SALARIES, 40.0)];
    database.write(Some(id), &writes).unwrap();
    // Supporting detail waits in it with the value it adds up to.
    let salaries = CellRef {
        cube: CUBE.to_owned(),
        at: SALARIES.map(str::to_owned).to_vec(),
    };
    let detail = vec![Line {
        label: "Two hires".to_owned(),
        value: 40.0,
    }];
    database
        .write_detail(Some(id), &salaries, detail.clone(), None)
        .unwrap();
    drop(database);

    // Opened again, it is there under its transaction, holding its values apart.
    let mut database = home.open();
    let sandbox = |pending| Sandbox {
        name: "Plan".to_owned(),
        transaction: id,
        pending,
    };
    assert_eq!(database.sandboxes("ada"), [sandbox(2)]);
    let cell = CellRef {
        cube: CUBE.to_owned(),
        at: RENT.map(str::to_owned).to_vec(),
    };
    let held = database.read(Some(id), &cell).unwrap();
    assert_eq!(held, Scalar::Number(stored + 25.0));
    assert_eq!(get(&database, RENT), stored);
    assert_eq!(database.detail(Some(id), &salaries).unwrap(), detail);
    assert!(database.detail(None, &salaries).unwrap().is_empty());

    // Emptied, it stays so; deleted, it is gone for good, and so is its file.
    database.rollback(id).unwrap();
    drop(database);
    let mut database = home.open();
    assert_eq!(database.sandboxes("ada"), [sandbox(0)]);
    database.delete_sandbox("ada", "plan").unwrap();
    drop(database);
    assert!(home.open().sandboxes("ada").is_empty());
    let files = fs::read_dir(home.path("data/sandboxes")).unwrap().count();
    assert_eq!(files, 0);
}

#[test]
fn earlier_models_are_kept_and_can_be_read_back() {
    let home = Home::new("earlier-models");
    let mut database = home.open();
    let before = database.journal_bytes().unwrap();
    assert!(before.model > 0 && before.earlier_models == 0, "{before:?}");
    let first = database.definition().clone();
    let mut second = first.clone();
    second.cubes[0].rules = vec!["[Account].[Rent] = 1".to_owned()];
    database.update_model(1, second.clone()).unwrap();
    let mut third = second.clone();
    third.cubes[0].rules.clear();
    database.update_model(2, third).unwrap();
    // What the models kept take on disk is counted: what each holds alone, its cube and its
    // version, and not the dimensions it shares with the model after it.
    let after = database.journal_bytes().unwrap();
    assert!(
        after.earlier_models > 0 && after.earlier_models < before.model,
        "{before:?} then {after:?}"
    );

    let kept: Vec<(String, u64)> = database
        .earlier_models()
        .into_iter()
        .map(|earlier| (earlier.name, earlier.version))
        .collect();
    assert_eq!(kept, [("2".to_owned(), 2), ("1".to_owned(), 1)]);
    assert!(
        database
            .earlier_models()
            .iter()
            .all(|earlier| earlier.saved.is_some())
    );
    assert_eq!(database.earlier_model("1").unwrap(), first);
    assert_eq!(database.earlier_model("2").unwrap(), second);

    // Only a name the history lists is read: no other folder can be asked for.
    for name in ["3", "..", "../model", "1/../2"] {
        assert!(
            matches!(database.earlier_model(name), Err(Error::NotFound(_))),
            "{name}"
        );
    }
}

#[test]
fn a_save_writes_only_what_changed_and_shares_the_rest_with_the_model_kept() {
    let home = Home::new("shared-saves");
    let changed = |path: &str| fs::metadata(home.path(path)).unwrap().modified().unwrap();
    let rules = |database: &Database, rule: &str| {
        let mut next = database.definition().clone();
        next.cubes[0].rules = vec![rule.to_owned()];
        next
    };
    let account = "model/dimensions/account.json";
    let cube = "model/cubes/general-ledger.json";
    let mut database = home.open();
    let (account_was, cube_was) = (changed(account), changed(cube));
    thread::sleep(Duration::from_millis(50));

    // A rule changed: the cube's file is written, the dimensions' are shared with the model
    // kept, the same files, as they were.
    database
        .update_model(1, rules(&database, "[Account].[Rent] = 1"))
        .unwrap();
    assert_eq!(changed(account), account_was);
    assert_eq!(
        changed("data/models/1/dimensions/account.json"),
        account_was
    );
    assert!(changed(cube) > cube_was);
    let kept = database.journal_bytes().unwrap().earlier_models;
    assert!(kept > 0 && kept < database.journal_bytes().unwrap().model);

    // Opened again, it knows its files as it read them, and shares them still; what each
    // model kept holds alone is counted as it was.
    drop(database);
    let mut database = home.open();
    assert_eq!(database.journal_bytes().unwrap().earlier_models, kept);
    database
        .update_model(2, rules(&database, "[Account].[Rent] = 2"))
        .unwrap();
    assert_eq!(changed(account), account_was);

    // A file changed by hand since it was written is the model's again at the next save,
    // written from the model as the server holds it.
    let read = fs::read_to_string(home.path(account)).unwrap();
    fs::write(home.path(account), format!("{read} ")).unwrap();
    database
        .update_model(3, rules(&database, "[Account].[Rent] = 3"))
        .unwrap();
    let written = fs::read_to_string(home.path(account)).unwrap();
    let held = database
        .definition()
        .dimensions
        .iter()
        .find(|d| d.name == "Account");
    assert!(written.ends_with("}\n"), "{written:?}");
    assert_eq!(
        serde_json::from_str::<DimensionDef>(&written).unwrap(),
        **held.unwrap()
    );
    drop(database);
    assert_eq!(
        home.open().definition().cubes[0].rules,
        ["[Account].[Rent] = 3"]
    );
}

#[test]
fn a_dimension_too_large_to_edit_by_hand_is_written_close() {
    let home = Home::new("close");
    let mut database = home.open();
    let mut definition = database.definition().clone();
    // 100,001 leaves under one total: more than a person edits in a file.
    let names = (0..100_001).map(|leaf| format!("l{leaf}"));
    let children: Vec<Json> = names.clone().map(|name| json!({ "name": name })).collect();
    let leaves = names.map(|name| json!({ "name": name, "type": "numeric" }));
    let total = json!({ "name": "All", "type": "consolidated", "children": children });
    let elements: Vec<Json> = std::iter::once(total).chain(leaves).collect();
    let big = json!({ "name": "Big", "elements": elements });
    definition.dimensions.push(
        serde_json::from_value::<epiphany_core::model::DimensionDef>(big)
            .unwrap()
            .into(),
    );
    database.update_model(1, definition).unwrap();
    drop(database);
    // That one on a line of its own; the others laid out, to be read and edited.
    let lines = |file: &str| fs::read_to_string(home.path(file)).unwrap().lines().count();
    assert_eq!(lines("model/dimensions/big.json"), 1);
    assert!(lines("model/dimensions/account.json") > 1);
    let reopened = home.open();
    let big = reopened.model().dimension("Big").unwrap();
    assert_eq!(big.elements.len(), 100_002);
}

#[test]
fn a_dashboard_is_kept_with_the_model_on_disk() {
    let home = Home::new("dashboards");
    let mut database = home.open();
    let mut definition = database.definition().clone();
    definition.cubes[0].views.insert(
        "All".to_owned(),
        serde_json::from_value(
            json!({ "mdx": "SELECT {[Entity].Members} ON COLUMNS FROM [General Ledger]" }),
        )
        .unwrap(),
    );
    definition.dashboards.push(
        serde_json::from_value(json!({ "name": "Finance", "tiles": [{ "cube": CUBE, "view": "All", "show": "lines" }] })).unwrap(),
    );
    database.update_model(1, definition).unwrap();
    drop(database);
    // Files are named in lower case, whatever the file system makes of case.
    let names: Vec<_> = fs::read_dir(home.path("model/dashboards"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["finance.json"]);
    let reopened = home.open();
    assert_eq!(reopened.definition().dashboards[0].name, "Finance");
}
