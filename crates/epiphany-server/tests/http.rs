use std::path::Path;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use epiphany_core::database::Database;
use epiphany_server::{app, load_model, openapi};
use serde_json::{Value, json};
use tower::ServiceExt;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// The application serving the example model with no data.
fn example() -> Router {
    let model = Path::new(ROOT).join("../epiphany-core/tests/fixtures/model");
    app(Database::new(load_model(&model).unwrap()).unwrap())
}

/// Sends a request and returns the status and the JSON body, or null for an empty body.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    };
    let response = app.clone().oneshot(request.unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = match bytes.is_empty() {
        true => Value::Null,
        false => serde_json::from_slice(&bytes).unwrap(),
    };
    (status, body)
}

fn cell(at: [&str; 4]) -> Value {
    json!({ "cube": "General Ledger", "at": at })
}

fn write(at: [&str; 4], value: Value) -> Value {
    json!({ "cube": "General Ledger", "at": at, "value": value })
}

#[tokio::test]
async fn written_cells_are_read_back_with_consolidations() {
    let app = example();
    let writes = json!({ "cells": [
        write(["Product Revenue", "US", "Jan", "Actual"], json!(100)),
        write(["Service Revenue", "UK", "Feb", "Actual"], json!(40)),
        write(["Rent", "US", "M03", "Actual"], json!(25)),
        write(["Product Revenue", "US", "Jan", "Budget"], json!(90)),
    ] });
    let (status, body) = call(&app, "POST", "/cells/write", Some(writes)).await;
    assert_eq!((status, body), (StatusCode::NO_CONTENT, Value::Null));

    let reads = json!({ "cells": [
        cell(["Product Revenue", "US", "Jan", "Actual"]),
        cell(["revenue", "total entity", "q1", "actual"]),
        cell(["Net Income", "Total Entity", "H1", "Actual"]),
        cell(["Product Revenue", "US", "Jan", "Variance"]),
        cell(["Salaries", "US", "Jan", "Actual"]),
    ] });
    let (status, body) = call(&app, "POST", "/cells/read", Some(reads)).await;
    assert_eq!(status, StatusCode::OK);
    let expected = [100.0, 140.0, 115.0, 10.0, 0.0].map(|value| json!({ "value": value }));
    assert_eq!(body, json!({ "cells": expected }));
}

#[tokio::test]
async fn a_bad_cell_in_a_read_reports_its_own_error() {
    let app = example();
    let reads = json!({ "cells": [
        cell(["Rent", "US", "Jan", "Actual"]),
        cell(["Rent", "Mars", "Jan", "Actual"]),
        json!({ "cube": "Nope", "at": [] }),
        json!({ "cube": "General Ledger", "at": ["Rent"] }),
    ] });
    let (status, body) = call(&app, "POST", "/cells/read", Some(reads)).await;
    assert_eq!(status, StatusCode::OK);
    let error =
        |code: &str, message: &str| json!({ "error": { "code": code, "message": message } });
    assert_eq!(
        body,
        json!({ "cells": [
            { "value": 0.0 },
            error("unknown_element", "unknown element 'Mars' in dimension 'Entity'"),
            error("unknown_cube", "unknown cube 'Nope'"),
            error("wrong_arity", "expected 4 elements, found 1"),
        ] })
    );
}

#[tokio::test]
async fn a_bad_cell_in_a_write_rejects_the_whole_batch() {
    let app = example();
    let writes = json!({ "cells": [
        write(["Rent", "US", "Jan", "Actual"], json!(25)),
        write(["Revenue", "US", "Jan", "Actual"], json!(1)),
    ] });
    let (status, body) = call(&app, "POST", "/cells/write", Some(writes)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body,
        json!({
            "code": "consolidated_cell",
            "message": "a consolidated cell cannot be written",
            "index": 1
        })
    );

    let reads = json!({ "cells": [cell(["Rent", "US", "Jan", "Actual"])] });
    let (_, body) = call(&app, "POST", "/cells/read", Some(reads)).await;
    assert_eq!(body, json!({ "cells": [{ "value": 0.0 }] }));
}

#[tokio::test]
async fn the_model_is_served_as_defined() {
    let (status, body) = call(&example(), "GET", "/model", None).await;
    assert_eq!(status, StatusCode::OK);
    let names = |list: &Value| -> Vec<String> {
        let items = list.as_array().unwrap().iter();
        items
            .map(|item| item["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        names(&body["dimensions"]),
        ["Account", "Entity", "Period", "Version"]
    );
    assert_eq!(
        body["cubes"],
        json!([{ "name": "General Ledger", "dimensions": ["Account", "Entity", "Period", "Version"] }])
    );
}

#[tokio::test]
async fn a_dimension_is_served_with_its_hierarchy() {
    let (status, body) = call(&example(), "GET", "/dimensions/period", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Period");
    assert_eq!(
        body["attributes"],
        json!({ "Code": "alias", "Days": "numeric" })
    );
    assert_eq!(
        body["elements"][1],
        json!({
            "name": "Q1",
            "type": "consolidated",
            "level": 1,
            "parents": ["H1"],
            "children": [
                { "name": "Jan", "weight": 1.0 },
                { "name": "Feb", "weight": 1.0 },
                { "name": "Mar", "weight": 1.0 }
            ],
            "attributes": {}
        })
    );
    assert_eq!(
        body["elements"][4],
        json!({
            "name": "Feb",
            "type": "numeric",
            "level": 0,
            "parents": ["Q1"],
            "children": [],
            "attributes": { "Code": "M02", "Days": 28.0 }
        })
    );
}

#[tokio::test]
async fn an_unknown_dimension_is_not_found() {
    let (status, body) = call(&example(), "GET", "/dimensions/Colour", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        json!({ "code": "unknown_dimension", "message": "unknown dimension 'Colour'" })
    );
}

#[tokio::test]
async fn a_malformed_body_uses_the_common_error_shape() {
    let body = json!({ "cells": [{ "cube": "General Ledger", "at": [], "colour": "red" }] });
    let (status, body) = call(&example(), "POST", "/cells/read", Some(body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "invalid_request");
    assert!(body["message"].as_str().unwrap().contains("colour"));
}

/// The committed contract must equal the document generated from the routes. Regenerate it
/// with `cargo run -p epiphany-server -- openapi > openapi.json`.
#[tokio::test]
async fn the_committed_contract_matches_the_routes() {
    let committed = std::fs::read_to_string(Path::new(ROOT).join("../../openapi.json")).unwrap();
    let committed: Value = serde_json::from_str(committed.trim_start_matches('\u{feff}')).unwrap();
    let generated = serde_json::to_value(openapi()).unwrap();
    assert_eq!(committed, generated);

    let (status, served) = call(&example(), "GET", "/openapi.json", None).await;
    assert_eq!((status, served), (StatusCode::OK, generated));
}
