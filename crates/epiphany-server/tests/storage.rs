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
    let body = body.to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
