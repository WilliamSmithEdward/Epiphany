mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::Home;
use epiphany_core::Scalar;
use epiphany_core::database::{CellRef, CellWrite, Database};
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
    // Another test starting a server at this moment holds a copy of every open file of
    // this process, the lock among them, until the server's own program takes its place,
    // so the lock may outlast the database by an instant.
    let reopened = (0..50).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(20));
        open(&home.0).is_ok()
    });
    assert!(
        reopened,
        "the directory stayed locked after its database was dropped"
    );
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
