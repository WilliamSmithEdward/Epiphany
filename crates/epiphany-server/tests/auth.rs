mod common;

use std::fs;
use std::process::Command;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use common::Home;
use epiphany_server::{Auth, app, app_with, openapi, set_user};
use serde_json::{Value, json};
use tower::ServiceExt;

// Made up for these tests; they open nothing else.
const NAME: &str = "ada";
const PASSWORD: &str = "test-only-9f2c41";

/// A server directory with one user, and the application serving it.
fn secured(name: &str, idle: Option<Duration>) -> (Home, Router) {
    let home = Home::new(name);
    set_user(&home.0, NAME, PASSWORD).unwrap();
    let mut auth = Auth::load(&home.0)
        .unwrap()
        .expect("the directory has a user");
    if let Some(idle) = idle {
        auth.set_idle(idle);
    }
    let application = app_with(home.open(), Some(auth));
    (home, application)
}

/// Sends a request, with a session token if one is given, and returns the response's status,
/// its `WWW-Authenticate` header and its JSON body.
async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Option<String>, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    };
    let response = app.clone().oneshot(request.unwrap()).await.unwrap();
    let status = response.status();
    let challenge = response.headers().get(header::WWW_AUTHENTICATE);
    let challenge = challenge.map(|value| value.to_str().unwrap().to_owned());
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = match bytes.is_empty() {
        true => Value::Null,
        false => serde_json::from_slice(&bytes).unwrap(),
    };
    (status, challenge, body)
}

async fn sign_in(app: &Router, name: &str, password: &str) -> (StatusCode, Value) {
    let credentials = json!({ "name": name, "password": password });
    let (status, _, body) = send(app, "POST", "/sessions", None, Some(credentials)).await;
    (status, body)
}

async fn token(app: &Router) -> String {
    let (status, body) = sign_in(app, NAME, PASSWORD).await;
    assert_eq!(status, StatusCode::CREATED);
    body["token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn requests_without_a_session_are_refused() {
    let (_home, app) = secured("refused", None);
    let reads = json!({ "cells": [] });
    for (method, uri, body) in [
        ("GET", "/model", None),
        ("POST", "/cells/read", Some(reads.clone())),
        ("POST", "/cells/write", Some(reads)),
        ("POST", "/transactions", None),
        ("GET", "/dimensions/Period", None),
    ] {
        let (status, challenge, body) = send(&app, method, uri, None, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert_eq!(challenge.as_deref(), Some("Bearer"));
        assert_eq!(body["code"], "not_signed_in");
    }
    let (status, _, _) = send(&app, "GET", "/model", Some("not-a-token"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The contract says how to sign in, so it can be read without doing so.
    let (status, _, _) = send(&app, "GET", "/openapi.json", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn signing_in_needs_the_right_name_and_password() {
    let (_home, app) = secured("sign-in", None);
    for (name, password) in [(NAME, "wrong"), ("nobody", PASSWORD), (NAME, "")] {
        let (status, body) = sign_in(&app, name, password).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            body,
            json!({ "code": "wrong_credentials", "message": "wrong name or password" })
        );
    }

    // Names are matched whatever their case.
    let (status, body) = sign_in(&app, "ADA", PASSWORD).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["idle_seconds"], 1800);
    let token = body["token"].as_str().unwrap();
    assert_eq!(token.len(), 64);
    let (status, _, body) = send(&app, "GET", "/model", Some(token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], 1);
    // Every sign-in gets a session of its own.
    assert_ne!(self::token(&app).await, token);
}

#[tokio::test]
async fn signing_out_ends_the_session() {
    let (_home, app) = secured("sign-out", None);
    let (first, second) = (token(&app).await, token(&app).await);
    let (status, _, _) = send(&app, "DELETE", "/sessions", Some(&first), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(&app, "GET", "/model", Some(&first), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Another session of the same user carries on.
    let (status, _, _) = send(&app, "GET", "/model", Some(&second), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_session_left_unused_ends_and_a_used_one_lives_on() {
    let (_home, app) = secured("idle", Some(Duration::from_millis(400)));
    let (idle, busy) = (token(&app).await, token(&app).await);
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (status, _, _) = send(&app, "GET", "/model", Some(&busy), None).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, _, body) = send(&app, "GET", "/model", Some(&idle), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "not_signed_in");
}

#[tokio::test]
async fn a_server_without_users_asks_for_no_sign_in() {
    let home = Home::new("open");
    assert!(Auth::load(&home.0).unwrap().is_none());
    let app = app(home.open());
    let (status, _, _) = send(&app, "GET", "/model", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = sign_in(&app, NAME, PASSWORD).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");
}

#[test]
fn passwords_are_stored_only_as_salted_hashes() {
    let home = Home::new("users");
    set_user(&home.0, NAME, PASSWORD).unwrap();
    set_user(&home.0, "Grace", PASSWORD).unwrap();
    let stored = fs::read_to_string(home.path("users.json")).unwrap();
    assert!(!stored.contains(PASSWORD));
    let users: Value = serde_json::from_str(&stored).unwrap();
    let hashes: Vec<&str> = users
        .as_array()
        .unwrap()
        .iter()
        .map(|user| user["password"].as_str().unwrap())
        .collect();
    assert!(hashes.iter().all(|hash| hash.starts_with("$argon2id$")));
    // The same password gives two different hashes, because each has its own salt.
    assert_ne!(hashes[0], hashes[1]);

    // Setting a user again changes the password instead of adding a second user.
    set_user(&home.0, "ADA", "another-test-only-77").unwrap();
    let auth = Auth::load(&home.0).unwrap().unwrap();
    assert!(auth.sign_in(NAME, PASSWORD).is_none());
    assert!(auth.sign_in(NAME, "another-test-only-77").is_some());
    assert!(auth.sign_in("grace", PASSWORD).is_some());
    assert!(set_user(&home.0, " ", PASSWORD).is_err());
    assert!(set_user(&home.0, NAME, "").is_err());
}

#[tokio::test]
async fn transaction_ids_cannot_be_guessed_from_one_another() {
    let (_home, app) = secured("ids", None);
    let token = token(&app).await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        let (status, _, body) = send(&app, "POST", "/transactions", Some(&token), None).await;
        assert_eq!(status, StatusCode::CREATED);
        ids.push(body["transaction"].as_u64().unwrap());
    }
    // Random ids of fifty-three bits: small enough for JavaScript, and not one after another.
    assert!(
        ids.iter().all(|&id| id > 1_000_000 && id < 1 << 53),
        "{ids:?}"
    );
    assert!(
        ids[0].abs_diff(ids[1]) > 1 && ids[1].abs_diff(ids[2]) > 1,
        "{ids:?}"
    );
}

#[test]
fn the_contract_says_which_routes_need_a_session() {
    let contract = serde_json::to_value(openapi()).unwrap();
    assert_eq!(
        contract["components"]["securitySchemes"]["session"],
        json!({ "type": "http", "scheme": "bearer" })
    );
    assert_eq!(contract["security"], json!([{ "session": [] }]));
    for (path, methods) in contract["paths"].as_object().unwrap() {
        for (method, operation) in methods.as_object().unwrap() {
            let open = path == "/sessions" && method == "post";
            if open {
                assert_eq!(operation["security"], json!([{}]));
                assert_eq!(
                    operation["responses"]["401"]["description"],
                    "Wrong name or password"
                );
            } else {
                assert_eq!(
                    operation["responses"]["401"]["description"],
                    "Not signed in, or the session has ended",
                    "{method} {path}"
                );
            }
        }
    }
}

#[test]
fn the_command_line_reads_a_password_from_standard_input() {
    use std::io::Write;
    use std::process::Stdio;

    let home = Home::new("cli");
    let server = env!("CARGO_BIN_EXE_epiphany-server");
    let mut command = Command::new(server);
    let command = command.arg("user").arg(&home.0).arg(NAME);
    let mut child = command
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // PowerShell pipes text with a byte-order mark before it and a line ending after it;
    // neither is part of the password.
    let piped = format!("\u{feff}{PASSWORD}\r\n");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(piped.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());

    let auth = Auth::load(&home.0).unwrap().unwrap();
    assert!(auth.sign_in(NAME, PASSWORD).is_some());
}

#[test]
fn a_server_without_users_refuses_to_listen_beyond_this_machine() {
    let home = Home::new("bind");
    let server = env!("CARGO_BIN_EXE_epiphany-server");
    let output = Command::new(server)
        .arg("serve")
        .arg(&home.0)
        .arg("0.0.0.0:0")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("this directory has no users"), "{message}");
}
