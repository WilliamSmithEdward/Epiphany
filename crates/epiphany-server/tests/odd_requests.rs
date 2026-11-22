//! Every route is sent bodies and parameters that are odd, wrong or extreme. Each must be
//! answered, and never with a 500: what a client sends is the client's mistake to be told
//! of, not the server's to fall over.

use std::path::Path;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use epiphany_core::database::Database;
use epiphany_server::{app, load_model};
use serde_json::{Value, json};
use tower::ServiceExt;

/// The application serving the example model with no data.
fn example() -> Router {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let model = root.join("../epiphany-core/tests/fixtures/model");
    app(Database::new(load_model(&model).unwrap()).unwrap())
}

/// Sends a request and returns the status and the body as text.
async fn send(app: &Router, method: &str, uri: &str, body: Option<String>) -> (StatusCode, String) {
    let request = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body)),
        None => request.body(Body::empty()),
    };
    let response = app.clone().oneshot(request.unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

const RENT: [&str; 4] = ["Rent", "US", "Jan", "Actual"];
const CUBE: &str = "General Ledger";

/// Bodies of every shape a route might be sent, right ones for other routes among them.
fn bodies() -> Vec<String> {
    let at = json!(RENT);
    let cells = |cells: Value| json!({ "cells": cells });
    let spread = |value: Value, method: &str, at: Value| json!({ "spread": { "cube": CUBE, "at": at, "value": value, "method": method } });
    let total = json!(["Operating Expenses", "Total Entity", "H1", "Actual"]);
    let mut bodies: Vec<Value> = vec![
        json!(null),
        json!({}),
        json!([]),
        json!("text"),
        json!(0),
        json!(true),
        cells(json!(null)),
        cells(json!([])),
        cells(json!([{}])),
        cells(json!([null])),
        cells(json!([{ "cube": CUBE, "at": [] }])),
        cells(json!([{ "cube": CUBE, "at": ["Rent"] }])),
        cells(json!([{ "cube": CUBE, "at": ["Rent", "US", "Jan", "Actual", "More"] }])),
        cells(json!([{ "cube": CUBE, "at": [1, 2, 3, 4] }])),
        cells(json!([{ "cube": "", "at": at }])),
        cells(json!([{ "cube": "Nowhere", "at": at }])),
        cells(json!([{ "cube": CUBE, "at": ["", "", "", ""] }])),
        cells(json!([{ "cube": CUBE, "at": at }])),
        cells(json!([{ "cube": CUBE, "at": at, "value": 1 }])),
        cells(json!([{ "cube": CUBE, "at": at, "value": 1e308 }])),
        cells(json!([{ "cube": CUBE, "at": at, "value": -1e308 }])),
        cells(json!([{ "cube": CUBE, "at": at, "value": "text" }])),
        cells(json!([{ "cube": CUBE, "at": at, "value": null }])),
        cells(json!([{ "cube": CUBE, "at": at, "value": [1] }])),
        cells(json!([{ "cube": CUBE, "at": total, "value": 5 }])),
        json!({ "transaction": 0, "cells": [{ "cube": CUBE, "at": at }] }),
        json!({ "transaction": 18446744073709551615u64, "cells": [{ "cube": CUBE, "at": at }] }),
        json!({ "transaction": -1, "cells": [{ "cube": CUBE, "at": at }] }),
        json!({ "transaction": "one", "cells": [] }),
        json!({ "cells": [{ "cube": CUBE, "at": at }], "feeders": true }),
        json!({ "cells": [{ "cube": CUBE, "at": at }], "extra": 1 }),
        json!({ "mdx": "" }),
        json!({ "mdx": "SELECT" }),
        json!({ "mdx": "SELECT {[Period].Members} ON COLUMNS FROM [General Ledger]" }),
        json!({ "mdx": "SELECT FROM [Nowhere]" }),
        json!({ "mdx": 5 }),
        json!({ "mdx": "SELECT {} ON COLUMNS, {} ON ROWS FROM [General Ledger]" }),
        json!({ "mdx": "SELECT {[Period].[Jan]} ON AXIS(99999999999999999999) FROM [General Ledger]" }),
        json!({ "mdx": "SELECT Head([Period].Members, 1e300) ON 0 FROM [General Ledger]" }),
        json!({ "mdx": "SELECT Subset([Period].Members, -5, -5) ON 0 FROM [General Ledger]" }),
        json!({ "name": "ada", "password": "x" }),
        json!({ "name": 1, "password": null }),
        json!({ "cube": CUBE, "mode": "write" }),
        json!({ "cube": CUBE, "mode": "exclusive" }),
        json!({ "cube": "Nowhere", "mode": "read" }),
        json!({ "mode": "sideways" }),
        json!({ "based_on": 0, "model": {} }),
        json!({ "based_on": 1, "model": { "dimensions": [], "cubes": [] } }),
        json!({ "based_on": 1, "model": { "dimensions": [{ "name": "D", "elements": [] }], "cubes": [] } }),
        json!({ "based_on": 1, "model": { "dimensions": [], "cubes": [{ "name": "C", "dimensions": [] }] } }),
        json!({ "based_on": 18446744073709551615u64, "model": { "dimensions": [], "cubes": [] } }),
        json!({ "based_on": 1, "dimension": { "name": "Extra", "elements": [{ "name": "a", "type": "numeric" }] } }),
        json!({ "based_on": 1, "dimension": { "name": "Other name", "elements": [] } }),
        json!({ "based_on": 1, "cube": { "name": "Extra", "dimensions": ["Period"] } }),
        json!({ "based_on": 1, "cube": { "name": "Extra", "dimensions": ["Period", "Period"] } }),
        json!({ "based_on": 1, "cube": { "name": "Extra", "dimensions": ["Period"], "rules": ["nonsense ="] } }),
    ];
    for method in ["proportional", "equal", "repeat", "sideways", ""] {
        for value in [json!(0), json!(100), json!(-100), json!(1e308), json!("x")] {
            bodies.push(spread(value.clone(), method, total.clone()));
            bodies.push(spread(value, method, at.clone()));
        }
    }
    bodies.push(spread(json!(1), "equal", json!([])));
    let mut bodies: Vec<String> = bodies.iter().map(Value::to_string).collect();
    // And what is no JSON at all.
    bodies.extend(["", "{", "{\"cells\": [", "\u{0}", "nonsense"].map(str::to_owned));
    bodies.push("[".repeat(10_000));
    bodies
}

const ROUTES: [(&str, &str); 67] = [
    ("POST", "/sessions"),
    ("DELETE", "/sessions"),
    ("GET", "/model"),
    ("PUT", "/model"),
    ("GET", "/dimensions/Period"),
    ("GET", "/dimensions/Nowhere"),
    ("GET", "/dimensions/%00"),
    ("GET", "/dimensions/%F0%9F%99%82"),
    ("PUT", "/dimensions/Extra"),
    ("PUT", "/dimensions/Period"),
    ("DELETE", "/dimensions/Nowhere"),
    ("DELETE", "/dimensions/Period?based_on=1"),
    ("DELETE", "/dimensions/Period?based_on=x"),
    ("GET", "/cubes/General%20Ledger"),
    ("GET", "/cubes/Nowhere"),
    ("PUT", "/cubes/Extra"),
    ("PUT", "/cubes/General%20Ledger"),
    ("DELETE", "/cubes/Nowhere?based_on=1"),
    ("POST", "/cells/read"),
    ("POST", "/cells/write"),
    ("POST", "/cells/detail"),
    ("POST", "/cells/explain"),
    ("POST", "/cells/history"),
    ("GET", "/admin/changes"),
    ("GET", "/admin/changes?since=x&most=-1"),
    ("GET", "/admin/changes?most=18446744073709551616&cube=%00"),
    ("POST", "/cells/spread"),
    ("POST", "/mdx"),
    ("GET", "/cubes/General%20Ledger/cells"),
    (
        "GET",
        "/cubes/General%20Ledger/cells?offset=18446744073709551615&limit=18446744073709551615",
    ),
    ("GET", "/cubes/General%20Ledger/cells?offset=-1&limit=-1"),
    ("GET", "/cubes/General%20Ledger/cells?offset=x"),
    ("GET", "/cubes/General%20Ledger/cells?limit=0"),
    ("GET", "/cubes/Nowhere/cells"),
    ("GET", "/cubes/General%20Ledger/views/Nowhere"),
    ("GET", "/cubes/Nowhere/views/Nowhere"),
    ("GET", "/cubes/General%20Ledger/feeders/check"),
    ("GET", "/cubes/Nowhere/feeders/check"),
    ("POST", "/transactions"),
    ("POST", "/transactions/0/commit"),
    ("POST", "/transactions/1/commit"),
    ("POST", "/transactions/18446744073709551615/commit"),
    ("POST", "/transactions/-1/rollback"),
    ("POST", "/transactions/x/rollback"),
    ("POST", "/transactions/1/locks"),
    ("POST", "/transactions/99999999999999999999/locks"),
    ("GET", "/transactions/1/point"),
    ("GET", "/transactions/x/point"),
    ("POST", "/transactions/1/undo"),
    ("POST", "/transactions/18446744073709551615/undo"),
    ("GET", "/sandboxes"),
    ("POST", "/sandboxes"),
    ("DELETE", "/sandboxes/Extra"),
    ("DELETE", "/sandboxes/%00"),
    ("POST", "/admin/view-as"),
    ("GET", "/admin/audit"),
    ("GET", "/admin/audit?most=-1"),
    ("GET", "/admin/models"),
    ("GET", "/admin/models/nowhere"),
    ("GET", "/admin/models/%2E%2E"),
    ("GET", "/cubes/General%20Ledger/comments"),
    ("POST", "/cubes/General%20Ledger/comments"),
    ("DELETE", "/cubes/General%20Ledger/comments/1"),
    ("DELETE", "/cubes/Nowhere/comments/x"),
    ("GET", "/nowhere"),
    ("PATCH", "/model"),
    ("GET", "/openapi.json"),
];

#[tokio::test]
async fn nothing_a_client_sends_is_answered_with_a_server_error() {
    let bodies = bodies();
    let mut wrong = Vec::new();
    // One application throughout: what one request leaves behind, the next one meets.
    let app = example();
    for (method, path) in ROUTES {
        for body in bodies.iter().map(Some).chain([None]) {
            let (status, answer) = send(&app, method, path, body.cloned()).await;
            if status.is_server_error() {
                let sent: String = body
                    .map_or("no body", String::as_str)
                    .chars()
                    .take(200)
                    .collect();
                wrong.push(format!("{method} {path} {sent} -> {status} {answer}"));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{} requests were answered with a server error, the first of them:\n{}",
        wrong.len(),
        wrong[..wrong.len().min(8)].join("\n")
    );
}
