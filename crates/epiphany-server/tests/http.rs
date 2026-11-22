use std::path::Path;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use epiphany_core::database::{Database, Journal, NamedCells, NamedWrites};
use epiphany_core::model::ModelDef;
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
async fn a_check_refuses_a_value_or_marks_it_as_breaking_the_check() {
    let app = example();
    let (_, mut cube) = call(&app, "GET", "/cubes/General%20Ledger", None).await;
    cube["validations"] = json!([
        { "cells": "N: [Account].[Rent]", "valid": "[Account].[Rent] >= 0", "message": "Rent is never negative.", "refuse": true },
        { "cells": "[Account].[Rent]", "valid": "[Account].[Rent] < 1000", "message": "Rent of 1,000 or more is unusual." },
    ]);
    let update = json!({ "based_on": 1, "cube": cube });
    let (status, _) = call(&app, "PUT", "/cubes/General%20Ledger", Some(update)).await;
    assert!(status.is_success(), "{status}");

    let rent = ["Rent", "US", "Jan", "Actual"];
    let writing = |value: i32| Some(json!({ "cells": [write(rent, json!(value))] }));
    let (status, body) = call(&app, "POST", "/cells/write", writing(-5)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let refused =
        json!({ "code": "invalid_value", "message": "Rent is never negative.", "index": 0 });
    assert_eq!(body, refused);
    let (status, _) = call(&app, "POST", "/cells/write", writing(1500)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let explain = json!({ "cells": [cell(rent)] });
    let (_, body) = call(&app, "POST", "/cells/explain", Some(explain)).await;
    let unusual = json!([{ "message": "Rent of 1,000 or more is unusual.", "refuses": false }]);
    assert_eq!(body["cells"][0]["cell"]["invalid"], unusual);
}

#[tokio::test]
async fn a_cells_detail_is_written_with_its_value_and_explained() {
    let app = example();
    let rent = ["Rent", "US", "Jan", "Actual"];
    let lines = json!([{ "label": "Office", "value": 30.0 }, { "label": "Store", "value": 12.5 }]);
    let detail = json!({ "cube": "General Ledger", "at": rent, "lines": lines });
    let (status, _) = call(&app, "POST", "/cells/detail", Some(detail)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let explain = json!({ "cells": [cell(rent)] });
    let (_, body) = call(&app, "POST", "/cells/explain", Some(explain)).await;
    let explained = &body["cells"][0]["cell"];
    assert_eq!(
        (&explained["value"], &explained["detail"]),
        (&json!(42.5), &lines)
    );

    // Lines that are not a detail a cell can have are refused, saying why.
    let bad = json!({ "cube": "General Ledger", "at": rent, "lines": [{ "label": "\u{7}", "value": 1 }] });
    let (status, body) = call(&app, "POST", "/cells/detail", Some(bad)).await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid_detail"))
    );
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
        names(&body["model"]["dimensions"]),
        ["Account", "Entity", "Period", "Version"]
    );
    assert_eq!(
        body["model"]["cubes"],
        json!([{ "name": "General Ledger", "dimensions": ["Account", "Entity", "Period", "Version"] }])
    );
}

#[tokio::test]
async fn the_outline_gives_the_model_but_its_dimensions_elements() {
    let app = example();
    let (_, whole) = call(&app, "GET", "/model", None).await;
    let (status, outline) = call(&app, "GET", "/model/outline", None).await;
    assert_eq!(status, StatusCode::OK);
    // Each dimension by its name, type and size; the cubes and dashboards whole.
    let sized: Vec<Value> = whole["model"]["dimensions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|dimension| {
            let kind = dimension.get("type").cloned().unwrap_or(json!("standard"));
            let count = dimension["elements"].as_array().unwrap().len();
            json!({ "name": dimension["name"], "type": kind, "element_count": count })
        })
        .collect();
    assert_eq!(outline["dimensions"], json!(sized));
    assert_eq!(outline["cubes"], whole["model"]["cubes"]);
    assert_eq!(outline["dashboards"], json!([]));
    assert_eq!(outline["version"], whole["version"]);
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
            ]
        })
    );
    // What an element has none of is left out: here, a leaf's children.
    assert_eq!(
        body["elements"][4],
        json!({
            "name": "Feb",
            "type": "numeric",
            "level": 0,
            "parents": ["Q1"],
            "attributes": { "Code": "M02", "Days": 28.0 }
        })
    );
    assert_eq!(body["elements"][0]["parents"], Value::Null);
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
async fn a_transaction_keeps_its_writes_to_itself_until_it_commits() {
    let app = example();
    let at = ["Rent", "US", "Jan", "Actual"];
    let read = |transaction: Value| json!({ "transaction": transaction, "cells": [cell(at)] });
    let value = |value: f64| json!({ "cells": [{ "value": value }] });

    let (status, body) = call(&app, "POST", "/transactions", None).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = body["transaction"].clone();
    let write = json!({ "transaction": id, "cells": [write(at, json!(25))] });
    let (status, _) = call(&app, "POST", "/cells/write", Some(write)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, inside) = call(&app, "POST", "/cells/read", Some(read(id.clone()))).await;
    let (_, outside) = call(&app, "POST", "/cells/read", Some(read(Value::Null))).await;
    assert_eq!((inside, outside), (value(25.0), value(0.0)));

    let (status, _) = call(&app, "POST", &format!("/transactions/{id}/commit"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, outside) = call(&app, "POST", "/cells/read", Some(read(Value::Null))).await;
    assert_eq!(outside, value(25.0));

    let (status, body) = call(&app, "POST", &format!("/transactions/{id}/commit"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "unknown_transaction");
}

#[tokio::test]
async fn a_transaction_goes_back_to_a_point_it_came_to() {
    let app = example();
    let at = ["Rent", "US", "Jan", "Actual"];
    let (_, body) = call(&app, "POST", "/transactions", None).await;
    let id = body["transaction"].clone();
    let point = || async {
        let (status, body) = call(&app, "GET", &format!("/transactions/{id}/point"), None).await;
        assert_eq!(status, StatusCode::OK);
        body["point"].clone()
    };
    assert_eq!(point().await, json!(0));
    let write = |value: f64| json!({ "transaction": id, "cells": [write(at, json!(value))] });
    call(&app, "POST", "/cells/write", Some(write(25.0))).await;
    let kept = point().await;
    assert_eq!(kept, json!(1));
    call(&app, "POST", "/cells/write", Some(write(40.0))).await;

    let undo = |to: Value| json!({ "to": to });
    let (status, _) = call(
        &app,
        "POST",
        &format!("/transactions/{id}/undo"),
        Some(undo(kept)),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let read = json!({ "transaction": id, "cells": [cell(at)] });
    let (_, body) = call(&app, "POST", "/cells/read", Some(read)).await;
    assert_eq!(body, json!({ "cells": [{ "value": 25.0 }] }));

    let (status, body) = call(
        &app,
        "POST",
        &format!("/transactions/{id}/undo"),
        Some(undo(json!(9))),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::NOT_FOUND, json!("not_found"))
    );
    let (status, _) = call(&app, "GET", "/transactions/77/point", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_rolled_back_transaction_leaves_no_trace() {
    let app = example();
    let at = ["Rent", "US", "Jan", "Actual"];
    let (_, body) = call(&app, "POST", "/transactions", None).await;
    let id = body["transaction"].clone();
    let write = json!({ "transaction": id, "cells": [write(at, json!(25))] });
    call(&app, "POST", "/cells/write", Some(write)).await;
    let (status, _) = call(&app, "POST", &format!("/transactions/{id}/rollback"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = call(
        &app,
        "POST",
        "/cells/read",
        Some(json!({ "cells": [cell(at)] })),
    )
    .await;
    assert_eq!(body, json!({ "cells": [{ "value": 0.0 }] }));
}

#[tokio::test]
async fn a_locked_cube_refuses_other_writers_with_a_conflict() {
    let app = example();
    let (_, body) = call(&app, "POST", "/transactions", None).await;
    let id = body["transaction"].clone();
    let lock = json!({ "cube": "General Ledger", "mode": "exclusive" });
    let (status, _) = call(
        &app,
        "POST",
        &format!("/transactions/{id}/locks"),
        Some(lock),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let cells = json!({ "cells": [write(["Rent", "US", "Jan", "Actual"], json!(25))] });
    let (status, body) = call(&app, "POST", "/cells/write", Some(cells.clone())).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body,
        json!({
            "code": "locked",
            "message": "cube 'General Ledger' is locked by another transaction"
        })
    );

    call(&app, "POST", &format!("/transactions/{id}/rollback"), None).await;
    let (status, _) = call(&app, "POST", "/cells/write", Some(cells)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn the_model_is_updated_against_its_version() {
    let app = example();
    let cells = json!({ "cells": [write(["Rent", "US", "Jan", "Actual"], json!(25))] });
    call(&app, "POST", "/cells/write", Some(cells)).await;
    let (_, served) = call(&app, "GET", "/model", None).await;
    assert_eq!(served["version"], 1);

    // Add an entity; everything already stored still fits.
    let mut model = served["model"].clone();
    let entity = &mut model["dimensions"][1]["elements"];
    entity[0]["children"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "name": "FR" }));
    entity
        .as_array_mut()
        .unwrap()
        .push(json!({ "name": "FR", "type": "numeric" }));

    let update = json!({ "based_on": 1, "model": model });
    let (status, body) = call(&app, "PUT", "/model", Some(update.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "version": 2, "dropped_cells": 0 }));

    let reads = json!({ "cells": [
        cell(["Rent", "Total Entity", "Jan", "Actual"]),
        cell(["Rent", "FR", "Jan", "Actual"]),
    ] });
    let (_, body) = call(&app, "POST", "/cells/read", Some(reads)).await;
    assert_eq!(
        body,
        json!({ "cells": [{ "value": 25.0 }, { "value": 0.0 }] })
    );

    let (status, body) = call(&app, "PUT", "/model", Some(update)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "stale_version");

    let broken =
        json!({ "based_on": 2, "model": { "cubes": [{ "name": "C", "dimensions": [] }] } });
    let (status, body) = call(&app, "PUT", "/model", Some(broken)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "invalid_model");
    assert_eq!(call(&app, "GET", "/model", None).await.1["version"], 2);
}
#[tokio::test]
async fn an_mdx_query_is_answered_with_axes_and_cells() {
    let app = example();
    let cells = json!({ "cells": [
        write(["Product Revenue", "US", "Jan", "Actual"], json!(100)),
        write(["Product Revenue", "UK", "Jan", "Actual"], json!(50)),
    ] });
    call(&app, "POST", "/cells/write", Some(cells)).await;

    let mdx = "SELECT {[Entity].[US], [Entity].[Total Entity]} ON COLUMNS,
                      {[Account].[Revenue]} ON ROWS
               FROM [General Ledger] WHERE ([Period].[Jan], [Version].[Actual])";
    let (status, body) = call(&app, "POST", "/mdx", Some(json!({ "mdx": mdx }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["cube"], "General Ledger");
    assert_eq!(body["cells"], json!([100.0, 150.0]));
    assert_eq!(
        body["axes"][0]["tuples"][1],
        json!([{
            "dimension": "Entity",
            "name": "Total Entity",
            "unique_name": "[Entity].[Total Entity]",
            "type": "consolidated",
            "level": 1,
            "properties": {}
        }])
    );

    let broken = json!({ "mdx": "SELECT {[Entity].[Mars]} ON COLUMNS FROM [General Ledger]" });
    let (status, body) = call(&app, "POST", "/mdx", Some(broken)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body,
        json!({ "code": "invalid_mdx", "message": "dimension 'Entity' has no member 'Mars'" })
    );

    // The same query stood at other members: the UK in place of the axis's own is ignored,
    // and February in place of January, where nothing is stored, totals nothing.
    let context = json!([{ "dimension": "Period", "member": "Feb" }, { "dimension": "Entity", "member": "UK" }]);
    let (status, body) = call(
        &app,
        "POST",
        "/mdx",
        Some(json!({ "mdx": mdx, "context": context })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["cells"], json!([0.0, 0.0]));
    let slicer: Vec<&str> = body["slicer"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["unique_name"].as_str().unwrap())
        .collect();
    assert_eq!(slicer, ["[Period].[Feb]", "[Version].[Actual]"]);
    let unknown = json!({ "mdx": mdx, "context": [{ "dimension": "Period", "member": "Smarch" }] });
    assert_eq!(
        call(&app, "POST", "/mdx", Some(unknown)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn subsets_and_views_are_saved_with_the_model_and_served() {
    let app = example();
    let (_, served) = call(&app, "GET", "/model", None).await;
    let mut model = served["model"].clone();
    model["dimensions"][2]["subsets"] = json!({
        "Months": { "mdx": "TM1FilterByLevel([Period].Members, 0)" },
        "Winter": { "elements": ["Jan", "Feb"] }
    });
    model["cubes"][0]["views"] = json!({ "Winter rent": { "mdx":
        "SELECT TM1SubsetToSet([Period], 'Winter') ON COLUMNS FROM [General Ledger]
         WHERE ([Account].[Rent], [Entity].[US], [Version].[Actual])" } });
    let update = json!({ "based_on": 1, "model": model });
    let (status, body) = call(&app, "PUT", "/model", Some(update)).await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({ "version": 2, "dropped_cells": 0 }))
    );

    let cells = json!({ "cells": [write(["Rent", "US", "Feb", "Actual"], json!(25))] });
    call(&app, "POST", "/cells/write", Some(cells)).await;
    let (status, body) = call(
        &app,
        "GET",
        "/cubes/General%20Ledger/views/winter%20rent",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["cells"], json!([0.0, 25.0]));

    let (_, body) = call(&app, "GET", "/dimensions/Period", None).await;
    assert_eq!(
        body["subsets"],
        json!({ "Months": ["Jan", "Feb", "Mar", "Apr", "May", "Jun"], "Winter": ["Jan", "Feb"] })
    );
    let (_, body) = call(&app, "GET", "/model", None).await;
    assert_eq!(
        body["model"]["dimensions"][2]["subsets"]["Winter"],
        json!({ "elements": ["Jan", "Feb"] })
    );

    let (status, body) = call(&app, "GET", "/cubes/General%20Ledger/views/Nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        json!({ "code": "not_found", "message": "cube 'General Ledger' has no view 'Nope'" })
    );

    // A view keeps the sizes of its grid and how it shows its numbers; a unit not known is
    // refused.
    let (_, mut cube) = call(&app, "GET", "/cubes/General%20Ledger", None).await;
    let shown = json!({
        "geometry": { "row_height": 40, "widths": { "col:[Period].[Jan]": 120 } },
        "format": { "decimals": 1, "scale": "thousands", "parentheses": true, "dash_zeros": true },
        "highlights": {
            "bars": true,
            "shades": true,
            "rules": [
                { "test": "at_least", "value": 100.0, "tone": "good" },
                { "test": "below", "value": 0.0, "tone": "bad" }
            ]
        }
    });
    let mut view = shown.clone();
    view["mdx"] = cube["views"]["Winter rent"]["mdx"].clone();
    cube["views"]["Winter rent"] = view;
    let put = json!({ "based_on": 2, "cube": cube.clone() });
    assert_eq!(
        call(&app, "PUT", "/cubes/General%20Ledger", Some(put))
            .await
            .0,
        StatusCode::OK
    );
    let (_, served) = call(&app, "GET", "/cubes/General%20Ledger", None).await;
    assert_eq!(served["views"]["Winter rent"]["format"], shown["format"]);
    assert_eq!(
        served["views"]["Winter rent"]["geometry"],
        shown["geometry"]
    );
    assert_eq!(
        served["views"]["Winter rent"]["highlights"],
        shown["highlights"]
    );
    cube["views"]["Winter rent"]["format"]["scale"] = json!("dozens");
    let put = json!({ "based_on": 3, "cube": cube });
    assert_eq!(
        call(&app, "PUT", "/cubes/General%20Ledger", Some(put))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn rules_and_feeders_are_part_of_the_model() {
    let app = example();
    let (_, served) = call(&app, "GET", "/model", None).await;
    let mut model = served["model"].clone();
    // Budget is a tenth more than actual, and only US actuals feed it.
    model["cubes"][0]["rules"] = json!(["N: [Version].[Budget] = [Version].[Actual] * 1.1"]);
    model["cubes"][0]["feeders"] =
        json!(["([Version].[Actual], [Entity].[US]) => [Version].[Budget]"]);
    let update = json!({ "based_on": 1, "model": model });
    assert_eq!(
        call(&app, "PUT", "/model", Some(update)).await.0,
        StatusCode::OK
    );

    let cells = json!({ "cells": [
        write(["Rent", "US", "Jan", "Actual"], json!(100)),
        write(["Rent", "UK", "Jan", "Actual"], json!(200)),
    ] });
    call(&app, "POST", "/cells/write", Some(cells)).await;
    let reads = json!({ "cells": [
        cell(["Rent", "UK", "Jan", "Budget"]),
        cell(["Rent", "Total Entity", "Jan", "Budget"]),
    ] });
    let (_, body) = call(&app, "POST", "/cells/read", Some(reads)).await;
    // The UK budget is calculated, but nothing feeds it, so the total leaves it out.
    let values: Vec<f64> = body["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["value"].as_f64().unwrap())
        .collect();
    assert!(
        (values[0] - 220.0).abs() < 1e-9 && (values[1] - 110.0).abs() < 1e-9,
        "{values:?}"
    );

    let (status, body) = call(&app, "GET", "/cubes/General%20Ledger/feeders/check", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "unfed": [{ "cube": "General Ledger", "at": ["Rent", "UK", "Jan", "Budget"] }] })
    );

    let calculated = json!({ "cells": [write(["Rent", "US", "Jan", "Budget"], json!(1))] });
    let (status, body) = call(&app, "POST", "/cells/write", Some(calculated)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body,
        json!({
            "code": "calculated_cell",
            "message": "the cell is calculated by a rule and cannot be written",
            "index": 0
        })
    );
}

#[tokio::test]
async fn a_value_is_spread_over_the_leaves_of_a_consolidation() {
    let app = example();
    let quarter = ["Rent", "US", "Q1", "Actual"];
    let spread = |value: f64, method: &str| {
        json!({ "spread": {
        "cube": "General Ledger", "at": quarter, "value": value, "method": method
    } })
    };
    let (status, body) = call(&app, "POST", "/cells/spread", Some(spread(90.0, "equal"))).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "written": 3 })));
    let months = json!({ "cells": [
        cell(["Rent", "US", "Jan", "Actual"]),
        cell(["Rent", "US", "Mar", "Actual"]),
        cell(quarter),
    ] });
    let (_, body) = call(&app, "POST", "/cells/read", Some(months.clone())).await;
    assert_eq!(
        body,
        json!({ "cells": [{ "value": 30.0 }, { "value": 30.0 }, { "value": 90.0 }] })
    );

    let (status, _) = call(
        &app,
        "POST",
        "/cells/spread",
        Some(spread(45.0, "proportional")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = call(&app, "POST", "/cells/read", Some(months)).await;
    assert_eq!(
        body,
        json!({ "cells": [{ "value": 15.0 }, { "value": 15.0 }, { "value": 45.0 }] })
    );

    let (status, body) = call(&app, "POST", "/cells/spread", Some(spread(1.0, "sideways"))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "invalid_request");
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
    assert_eq!((status, served.clone()), (StatusCode::OK, generated));

    // The contract says what every value is: no schema is left to mean "anything", and every
    // response is described.
    fn untyped(value: &Value, path: &str, found: &mut Vec<String>) {
        match value {
            // An empty entry under `security` is how a route says it needs no session.
            Value::Object(fields) if fields.is_empty() && !path.ends_with("/security") => {
                found.push(path.to_owned())
            }
            Value::Object(fields) => {
                for (name, field) in fields {
                    untyped(field, &format!("{path}/{name}"), found);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| untyped(item, path, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    untyped(&served, "", &mut found);
    assert_eq!(found, Vec::<String>::new());
    for (path, methods) in served["paths"].as_object().unwrap() {
        for (method, operation) in methods.as_object().unwrap() {
            for (status, response) in operation["responses"].as_object().unwrap() {
                let described = response["description"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty());
                assert!(described, "{method} {path} {status} has no description");
            }
        }
    }
}

#[tokio::test]
async fn one_dimension_or_cube_is_changed_without_sending_the_model() {
    let app = example();
    let cells = json!({ "cells": [write(["Rent", "US", "Jan", "Actual"], json!(25))] });
    call(&app, "POST", "/cells/write", Some(cells)).await;
    let updated = |version: u64| json!({ "version": version, "dropped_cells": 0 });

    // A cube is served as defined, and saved again with a view added.
    let (status, mut cube) = call(&app, "GET", "/cubes/general%20ledger", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cube["name"], "General Ledger");
    assert_eq!(cube["dimensions"].as_array().unwrap().len(), 4);
    cube["views"] =
        json!({ "US": { "mdx": "SELECT {[Entity].[US]} ON COLUMNS FROM [General Ledger]" } });
    let put = json!({ "based_on": 1, "cube": cube });
    let (status, body) = call(&app, "PUT", "/cubes/GeneralLedger", Some(put.clone())).await;
    assert_eq!((status, body), (StatusCode::OK, updated(2)));
    let (status, _) = call(&app, "GET", "/cubes/General%20Ledger/views/US", None).await;
    assert_eq!(status, StatusCode::OK);
    // It is checked against the model's version like any other change.
    let (status, body) = call(&app, "PUT", "/cubes/GeneralLedger", Some(put)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "stale_version");

    // A dimension is saved with a subset added; the cell stored before is still there.
    let (_, served) = call(&app, "GET", "/model", None).await;
    let dimensions = served["model"]["dimensions"].as_array().unwrap();
    let mut version = dimensions
        .iter()
        .find(|dimension| dimension["name"] == "Version")
        .unwrap()
        .clone();
    version["subsets"] = json!({ "Real": { "elements": ["Actual"] } });
    let put = json!({ "based_on": 2, "dimension": version });
    let (status, body) = call(&app, "PUT", "/dimensions/version", Some(put)).await;
    assert_eq!((status, body), (StatusCode::OK, updated(3)));
    let (_, served) = call(&app, "GET", "/dimensions/Version", None).await;
    assert_eq!(served["subsets"]["Real"], json!(["Actual"]));
    let reads = json!({ "cells": [cell(["Rent", "Total Entity", "Jan", "Actual"])] });
    let (_, body) = call(&app, "POST", "/cells/read", Some(reads)).await;
    assert_eq!(body, json!({ "cells": [{ "value": 25.0 }] }));

    // A name the model does not have yet adds the dimension or the cube.
    let scenario = json!({ "name": "Scenario", "type": "standard",
        "elements": [{ "name": "Base", "type": "numeric" }] });
    let put = json!({ "based_on": 3, "dimension": scenario });
    let (status, body) = call(&app, "PUT", "/dimensions/Scenario", Some(put)).await;
    assert_eq!((status, body), (StatusCode::OK, updated(4)));
    let plan = json!({ "name": "Plan", "dimensions": ["Scenario", "Period"] });
    let put = json!({ "based_on": 4, "cube": plan });
    let (status, body) = call(&app, "PUT", "/cubes/Plan", Some(put)).await;
    assert_eq!((status, body), (StatusCode::OK, updated(5)));
    // A change that would leave the model invalid is refused whole.
    let broken = json!({ "based_on": 5, "cube": { "name": "Plan", "dimensions": ["Nowhere"] } });
    let (status, body) = call(&app, "PUT", "/cubes/Plan", Some(broken)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "invalid_model");

    // A dimension goes only once no cube uses it.
    let (status, body) = call(&app, "DELETE", "/dimensions/Scenario?based_on=5", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "invalid_model");
    let (status, body) = call(&app, "DELETE", "/cubes/plan?based_on=5", None).await;
    assert_eq!((status, body), (StatusCode::OK, updated(6)));
    let (status, body) = call(&app, "DELETE", "/dimensions/Scenario?based_on=6", None).await;
    assert_eq!((status, body), (StatusCode::OK, updated(7)));
    for (uri, code) in [
        ("/cubes/Plan?based_on=7", "unknown_cube"),
        ("/dimensions/Scenario?based_on=7", "unknown_dimension"),
    ] {
        let (status, body) = call(&app, "DELETE", uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], code);
    }
    assert_eq!(
        call(&app, "GET", "/cubes/Plan", None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(call(&app, "GET", "/model", None).await.1["version"], 7);
}

#[tokio::test]
async fn every_response_says_which_model_version_answered() {
    let app = example();
    let version = |uri: &'static str| {
        let app = app.clone();
        async move {
            let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
            let response = app.oneshot(request).await.unwrap();
            let header = &response.headers()["epiphany-model-version"];
            header.to_str().unwrap().to_owned()
        }
    };
    assert_eq!(version("/dimensions/Period").await, "1");
    // A refusal carries it too.
    assert_eq!(version("/dimensions/Nowhere").await, "1");

    let extra = json!({ "based_on": 1, "dimension":
        { "name": "Extra", "elements": [{ "name": "X", "type": "numeric" }] } });
    let (status, _) = call(&app, "PUT", "/dimensions/Extra", Some(extra)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(version("/dimensions/Period").await, "2");
}

#[tokio::test]
async fn the_stored_cells_of_a_cube_are_listed_and_can_be_written_back() {
    let app = example();
    let cells = json!({ "cells": [
        write(["Rent", "UK", "Feb", "Actual"], json!(3)),
        write(["Salaries", "US", "Jan", "Budget"], json!(2.5)),
        write(["Product Revenue", "US", "Jan", "Actual"], json!(100)),
        write(["Rent", "US", "Jan", "Actual"], json!(0)),
    ] });
    call(&app, "POST", "/cells/write", Some(cells)).await;

    // Leaf cells that hold a value, in the order of their elements in each dimension:
    // accounts as the dimension lists them, then entities, periods and versions.
    let (status, listed) = call(&app, "GET", "/cubes/general%20ledger/cells", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["total"], 3);
    assert_eq!(
        listed["cells"],
        json!([
            write(["Product Revenue", "US", "Jan", "Actual"], json!(100.0)),
            write(["Salaries", "US", "Jan", "Budget"], json!(2.5)),
            write(["Rent", "UK", "Feb", "Actual"], json!(3.0)),
        ])
    );
    // A page at a time.
    let (_, page) = call(
        &app,
        "GET",
        "/cubes/General%20Ledger/cells?offset=1&limit=1",
        None,
    )
    .await;
    assert_eq!(page["total"], 3);
    assert_eq!(page["cells"], json!([listed["cells"][1]]));
    let (_, page) = call(&app, "GET", "/cubes/General%20Ledger/cells?offset=3", None).await;
    assert_eq!(page, json!({ "total": 3, "cells": [] }));

    // What comes out goes into another server as it is, and then the two hold the same.
    let other = example();
    let body = json!({ "cells": listed["cells"] });
    let (status, _) = call(&other, "POST", "/cells/write", Some(body)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, copied) = call(&other, "GET", "/cubes/General%20Ledger/cells", None).await;
    assert_eq!(copied, listed);

    let (status, body) = call(&app, "GET", "/cubes/Nowhere/cells", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "unknown_cube");
}

#[tokio::test]
async fn a_dimension_says_its_type_and_the_names_of_its_levels() {
    let app = example();
    let (_, plain) = call(&app, "GET", "/dimensions/Period", None).await;
    assert_eq!(plain["type"], "standard");
    assert_eq!(plain["levels"], json!([]));

    let (_, served) = call(&app, "GET", "/model", None).await;
    let dimensions = served["model"]["dimensions"].as_array().unwrap();
    let mut period = dimensions
        .iter()
        .find(|dimension| dimension["name"] == "Period")
        .unwrap()
        .clone();
    period["type"] = json!("time");
    period["levels"] = json!(["Month", "Quarter", "Half"]);
    let put = json!({ "based_on": 1, "dimension": period });
    let (status, _) = call(&app, "PUT", "/dimensions/Period", Some(put)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, timed) = call(&app, "GET", "/dimensions/Period", None).await;
    assert_eq!(timed["type"], "time");
    assert_eq!(timed["levels"], json!(["Month", "Quarter", "Half"]));
    // And a query may use them at once.
    let mdx = json!({ "mdx": "SELECT Qtd([Period].[May]) ON COLUMNS FROM [General Ledger]" });
    let (status, answer) = call(&app, "POST", "/mdx", Some(mdx)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["axes"][0]["tuples"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_large_write_is_taken_in_one_request() {
    let app = example();
    // Sixty thousand cells, some five megabytes of JSON.
    let cells: Vec<Value> = (0..60_000)
        .map(|index| write(["Rent", "US", "Jan", "Actual"], json!(index)))
        .collect();
    let body = json!({ "cells": cells });
    assert!(body.to_string().len() > 4 * 1024 * 1024);
    let (status, _) = call(&app, "POST", "/cells/write", Some(body)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let read = json!({ "cells": [cell(["Rent", "US", "Jan", "Actual"])] });
    let (_, body) = call(&app, "POST", "/cells/read", Some(read)).await;
    assert_eq!(body, json!({ "cells": [{ "value": 59_999.0 }] }));
}

#[tokio::test]
async fn a_request_that_only_asks_is_given_less_room_than_one_that_writes() {
    let app = example();
    // Five megabytes of cells to read: more than asking is given, less than writing is.
    let cells: Vec<Value> = (0..70_000)
        .map(|_| cell(["Rent", "US", "Jan", "Actual"]))
        .collect();
    let body = json!({ "cells": cells });
    assert!(body.to_string().len() > 4 * 1024 * 1024);
    let (status, refusal) = call(&app, "POST", "/cells/read", Some(body)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(refusal["code"], "invalid_request");
    // A tenth of them is an ordinary request.
    let cells: Vec<Value> = (0..7_000)
        .map(|_| cell(["Rent", "US", "Jan", "Actual"]))
        .collect();
    let (status, _) = call(&app, "POST", "/cells/read", Some(json!({ "cells": cells }))).await;
    assert_eq!(status, StatusCode::OK);
}

/// A journal full after every commit, whose snapshot says when it has begun and then waits
/// to be let finish.
#[derive(Debug)]
struct Held {
    begun: std::sync::mpsc::Sender<()>,
    finish: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl Journal for Held {
    fn commit(&mut self, _: &NamedWrites<'_>) -> std::io::Result<()> {
        Ok(())
    }

    fn model(&mut self, _: u64, _: &ModelDef) -> std::io::Result<()> {
        Ok(())
    }

    fn is_full(&self) -> bool {
        true
    }

    fn snapshot(&mut self, _: u64, _: &NamedCells<'_>) -> std::io::Result<()> {
        self.begun.send(()).unwrap();
        self.finish.lock().unwrap().recv().unwrap();
        Ok(())
    }
}

#[tokio::test]
async fn a_snapshot_is_written_while_reads_go_on_and_writes_wait() {
    let (begun, begins) = std::sync::mpsc::channel();
    let (finish, finishes) = std::sync::mpsc::channel();
    let model = Path::new(ROOT).join("../epiphany-core/tests/fixtures/model");
    let mut database = Database::new(load_model(&model).unwrap()).unwrap();
    let finish_held = std::sync::Mutex::new(finishes);
    database.set_journal(Box::new(Held {
        begun,
        finish: finish_held,
    }));
    let app = app(database);
    let rent = ["Rent", "US", "Jan", "Actual"];

    // The write that fills the log is answered without waiting for the snapshot.
    let filling = json!({ "cells": [write(rent, json!(25))] });
    let (status, _) = call(&app, "POST", "/cells/write", Some(filling)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let timeout = std::time::Duration::from_secs(10);
    begins.recv_timeout(timeout).expect("the snapshot begins");

    // While it is written, reads are answered, and a write waits.
    let reads = json!({ "cells": [cell(rent)] });
    let (status, body) = call(&app, "POST", "/cells/read", Some(reads.clone())).await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({ "cells": [{ "value": 25.0 }] }))
    );
    let next = json!({ "cells": [write(rent, json!(30))] });
    let waiting = tokio::spawn({
        let app = app.clone();
        async move { call(&app, "POST", "/cells/write", Some(next)).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!waiting.is_finished(), "a write waits for the snapshot");
    // Nor does a read wait behind that write.
    let read = call(&app, "POST", "/cells/read", Some(reads.clone()));
    let (status, body) = tokio::time::timeout(timeout, read)
        .await
        .expect("a read is not held up by a waiting write");
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({ "cells": [{ "value": 25.0 }] }))
    );

    // Once it is written, the write goes ahead, and fills the log again.
    finish.send(()).unwrap();
    assert_eq!(waiting.await.unwrap().0, StatusCode::NO_CONTENT);
    begins
        .recv_timeout(timeout)
        .expect("the next snapshot begins");
    finish.send(()).unwrap();
    let (_, body) = call(&app, "POST", "/cells/read", Some(reads)).await;
    assert_eq!(body, json!({ "cells": [{ "value": 30.0 }] }));
}

#[tokio::test]
async fn a_dashboard_of_saved_views_is_kept_with_the_model() {
    let app = example();
    let (_, mut cube) = call(&app, "GET", "/cubes/General%20Ledger", None).await;
    cube["views"] = json!({ "By entity": { "mdx": "SELECT {[Entity].Members} ON COLUMNS FROM [General Ledger]" } });
    call(
        &app,
        "PUT",
        "/cubes/General%20Ledger",
        Some(json!({ "based_on": 1, "cube": cube })),
    )
    .await;

    let dashboard = json!({
        "name": "Finance",
        "filters": ["Version", "Period"],
        "tiles": [
            { "cube": "General Ledger", "view": "By entity", "show": "figure" },
            { "title": "Entities", "cube": "general ledger", "view": "by entity", "width": 2 }
        ]
    });
    let put = json!({ "based_on": 2, "dashboard": dashboard });
    let (status, body) = call(&app, "PUT", "/dashboards/Finance", Some(put)).await;
    assert_eq!(
        (status, body["version"].clone()),
        (StatusCode::OK, json!(3))
    );
    let (_, served) = call(&app, "GET", "/model", None).await;
    assert_eq!(served["model"]["dashboards"][0]["tiles"][1]["width"], 2);
    assert_eq!(
        served["model"]["dashboards"][0]["tiles"][0]["show"],
        "figure"
    );
    assert_eq!(
        served["model"]["dashboards"][0]["filters"],
        json!(["Version", "Period"])
    );

    // A tile must show a view the cube has, one or two columns wide, and a filter must be
    // a dimension of the model.
    let tile = json!({ "cube": "General Ledger", "view": "By entity" });
    for broken in [
        json!({ "name": "Broken", "tiles": [{ "cube": "General Ledger", "view": "Nowhere" }] }),
        json!({ "name": "Broken", "tiles": [{ "cube": "Nowhere", "view": "By entity" }] }),
        json!({ "name": "Broken", "tiles": [{ "cube": "General Ledger", "view": "By entity", "width": 3 }] }),
        json!({ "name": "Broken", "filters": ["Nowhere"], "tiles": [tile] }),
    ] {
        let put = json!({ "based_on": 3, "dashboard": broken });
        let (status, body) = call(&app, "PUT", "/dashboards/Broken", Some(put)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }

    let (status, _) = call(&app, "DELETE", "/dashboards/finance?based_on=3", None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, served) = call(&app, "GET", "/model", None).await;
    assert!(served["model"].get("dashboards").is_none());
    let (status, _) = call(&app, "DELETE", "/dashboards/finance?based_on=4", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
