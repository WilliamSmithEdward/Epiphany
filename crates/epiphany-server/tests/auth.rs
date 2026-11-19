mod common;

use std::fs;
use std::process::Command;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use common::Home;
use epiphany_server::{Auth, Refusal, Role, app, app_with, openapi, set_user};
use serde_json::{Value, json};
use tower::ServiceExt;

// Made up for these tests; they open nothing else.
const NAME: &str = "ada";
const PASSWORD: &str = "test-only-9f2c41";

/// A server directory with one user, and the application serving it.
fn secured(name: &str, idle: Option<Duration>) -> (Home, Router) {
    let home = Home::new(name);
    set_user(&home.0, NAME, PASSWORD, Role::Admin).unwrap();
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
async fn a_user_may_do_only_what_the_role_allows() {
    let home = Home::new("roles");
    for (name, role) in [
        ("reader", Role::Read),
        ("writer", Role::Write),
        ("admin", Role::Admin),
    ] {
        set_user(&home.0, name, PASSWORD, role).unwrap();
    }
    let app = app_with(home.open(), Auth::load(&home.0).unwrap());
    let mut tokens = Vec::new();
    for (name, role) in [("reader", "read"), ("writer", "write"), ("admin", "admin")] {
        let (status, body) = sign_in(&app, name, PASSWORD).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["role"], role);
        tokens.push(body["token"].as_str().unwrap().to_owned());
    }
    let [reader, writer, admin] = tokens.try_into().unwrap();

    let at = json!(["Rent", "UK", "Jan", "Actual"]);
    let read = json!({ "cells": [{ "cube": "General Ledger", "at": at }] });
    let write = json!({ "cells": [{ "cube": "General Ledger", "at": at, "value": 1 }] });
    let query = json!({ "mdx": "SELECT {[Entity].[US]} ON COLUMNS FROM [General Ledger]" });
    let (_, _, model) = send(&app, "GET", "/model", Some(&reader), None).await;
    let update = json!({ "based_on": model["version"], "model": model["model"] });

    // Reading is open to all three.
    for token in [&reader, &writer, &admin] {
        for (method, uri, body) in [
            ("GET", "/model", None),
            ("GET", "/dimensions/Period", None),
            ("POST", "/cells/read", Some(read.clone())),
            ("POST", "/cells/explain", Some(read.clone())),
            ("POST", "/mdx", Some(query.clone())),
        ] {
            let (status, _, _) = send(&app, method, uri, Some(token), body).await;
            assert_eq!(status, StatusCode::OK, "{method} {uri}");
        }
    }
    // Writing and transactions need the write role.
    for (method, uri, body) in [
        ("POST", "/cells/write", Some(write.clone())),
        ("POST", "/transactions", None),
    ] {
        let (status, _, body_) = send(&app, method, uri, Some(&reader), body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(body_["code"], "forbidden");
        assert_eq!(body_["message"], "this needs a user with the role 'write'");
        let (status, _, _) = send(&app, method, uri, Some(&writer), body).await;
        assert!(status.is_success(), "{method} {uri}");
    }
    // What was refused was not done.
    let (_, _, cells) = send(&app, "POST", "/cells/read", Some(&reader), Some(read)).await;
    assert_eq!(cells["cells"][0]["value"], 1.0);
    // Changing the model needs the admin role.
    for token in [&reader, &writer] {
        let (status, _, _) = send(&app, "PUT", "/model", Some(token), Some(update.clone())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    let (status, _, _) = send(&app, "PUT", "/model", Some(&admin), Some(update)).await;
    assert!(status.is_success());
    // Anyone may sign out.
    let (status, _, _) = send(&app, "DELETE", "/sessions", Some(&reader), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn too_many_wrong_passwords_pause_a_name() {
    let home = Home::new("pause");
    set_user(&home.0, NAME, PASSWORD, Role::Admin).unwrap();
    let mut auth = Auth::load(&home.0).unwrap().unwrap();
    auth.set_pause(Duration::from_secs(3));
    let app = app_with(home.open(), Some(auth));

    // A right password among the wrong ones starts the count again.
    for password in ["1", "2", "3", "4", PASSWORD, "1", "2", "3", "4", "5"] {
        let (status, _) = sign_in(&app, NAME, password).await;
        let expected = match password == PASSWORD {
            true => StatusCode::CREATED,
            false => StatusCode::UNAUTHORIZED,
        };
        assert_eq!(status, expected);
    }
    // After five running, even the right password is refused, and told how long to wait.
    let credentials = json!({ "name": "ADA", "password": PASSWORD });
    let request = Request::builder()
        .method("POST")
        .uri("/sessions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(credentials.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let wait: u64 = response.headers()[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=3).contains(&wait), "{wait}");
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], "too_many_attempts");
    // Other names are not held up.
    let (status, _) = sign_in(&app, "nobody", PASSWORD).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    tokio::time::sleep(Duration::from_millis(3100)).await;
    let (status, _) = sign_in(&app, NAME, PASSWORD).await;
    assert_eq!(status, StatusCode::CREATED);
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
    // The times leave room for the other tests, which keep the processor busy with passwords.
    let (_home, app) = secured("idle", Some(Duration::from_secs(2)));
    let (idle, busy) = (token(&app).await, token(&app).await);
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(400)).await;
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
    set_user(&home.0, NAME, PASSWORD, Role::Admin).unwrap();
    set_user(&home.0, "Grace", PASSWORD, Role::Read).unwrap();
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
    set_user(&home.0, "ADA", "another-test-only-77", Role::Write).unwrap();
    let auth = Auth::load(&home.0).unwrap().unwrap();
    assert_eq!(auth.sign_in(NAME, PASSWORD), Err(Refusal::Wrong));
    let role = |name: &str, password: &str| auth.sign_in(name, password).map(|(_, role)| role);
    assert_eq!(role(NAME, "another-test-only-77"), Ok(Role::Write));
    assert_eq!(role("grace", PASSWORD), Ok(Role::Read));
    assert!(set_user(&home.0, " ", PASSWORD, Role::Admin).is_err());
    assert!(set_user(&home.0, NAME, "", Role::Admin).is_err());

    // A user recorded before there were roles may do everything, as then.
    let old = json!([{ "name": "Old", "password": hashes[0] }]);
    fs::write(home.path("users.json"), old.to_string()).unwrap();
    let auth = Auth::load(&home.0).unwrap().unwrap();
    assert_eq!(
        auth.sign_in("old", PASSWORD).map(|(_, role)| role),
        Ok(Role::Admin)
    );
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
    assert!(auth.sign_in(NAME, PASSWORD).is_ok());
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
