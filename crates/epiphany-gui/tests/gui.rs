use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use epiphany_gui::{CLIENT, CLIENT_HEADER, Gui};
use epiphany_server::{Auth, Grants, Role, app_with, open, set_user};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A server directory holding the example model, removed when dropped.
struct Home(PathBuf);

impl Home {
    fn new(name: &str, user: bool) -> Self {
        let home = std::env::temp_dir().join(format!("epiphany-gui-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../epiphany-core/tests/fixtures/model");
        copy(&fixture, &home.join("model"));
        if user {
            set_user(
                &home,
                "ada",
                "a long password",
                &Grants::new(Role::Admin, BTreeMap::new()),
            )
            .unwrap();
        }
        Self(home)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        match entry.file_type().unwrap().is_dir() {
            true => copy(&entry.path(), &target),
            false => drop(fs::copy(entry.path(), target).unwrap()),
        }
    }
}

/// An Epiphany server for the directory, on a port of its own; its address as a URL.
async fn server(home: &Home) -> String {
    let auth = Auth::load(&home.0).unwrap();
    let application = app_with(open(&home.0).unwrap(), auth);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, application).await.unwrap() });
    format!("http://{address}")
}

/// What came back from the GUI: status, headers, and the body as JSON or text.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

/// Sends a request to the GUI as a browser at `from` would.
async fn call(
    gui: &Gui,
    from: &str,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Answer {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let mut request = request.body(body).unwrap();
    let peer: SocketAddr = from.parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    let response = gui.clone().router().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

const HERE: &str = "127.0.0.1:50000";
const AWAY: &str = "192.0.2.7:50000";
const FROM_CLIENT: (&str, &str) = (CLIENT_HEADER, CLIENT);

/// The session cookie a response set, as a browser would send it back.
fn cookie(answer: &Answer) -> String {
    let set = answer.headers[header::SET_COOKIE].to_str().unwrap();
    set.split(';').next().unwrap().to_owned()
}

#[tokio::test]
async fn the_application_is_served_with_headers_that_keep_it_to_itself() {
    let gui = Gui::new("http://127.0.0.1:1", None);
    let page = call(&gui, HERE, Method::GET, "/", &[], None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("<title>Epiphany</title>"));
    assert!(
        page.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let policy = page.headers[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(policy.contains("script-src 'self'") && policy.contains("frame-ancestors 'none'"));
    assert_eq!(page.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(page.headers[header::X_FRAME_OPTIONS], "DENY");

    // An address the application routes itself gets its page; a missing file does not.
    let routed = call(&gui, HERE, Method::GET, "/explore/sales", &[], None).await;
    assert_eq!(
        (routed.status, routed.body == page.body),
        (StatusCode::OK, true)
    );
    assert_eq!(
        call(&gui, HERE, Method::GET, "/missing.js", &[], None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // A file not changed since it was last fetched is not sent again.
    let tag = page.headers[header::ETAG].to_str().unwrap().to_owned();
    let again = call(
        &gui,
        HERE,
        Method::GET,
        "/",
        &[("if-none-match", &tag)],
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn every_file_of_the_application_is_built_into_the_program() {
    // One left out of the program would be missed only once it shipped: a folder served
    // while the application is worked on has every file.
    let gui = Gui::new("http://127.0.0.1:1", None);
    let web = Path::new(env!("CARGO_MANIFEST_DIR")).join("web");
    for entry in fs::read_dir(web).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let answer = call(&gui, HERE, Method::GET, &format!("/{name}"), &[], None).await;
        assert_eq!(answer.status, StatusCode::OK, "{name} is not built in");
    }
}

#[tokio::test]
async fn signing_in_keeps_the_token_in_a_cookie_the_page_cannot_read() {
    let home = Home::new("sign-in", true);
    let gui = Gui::new(&server(&home).await, None);

    let state = call(&gui, HERE, Method::GET, "/gui/session", &[], None).await;
    assert_eq!(state.json(), json!({ "signed_in": false, "open": false }));
    assert_eq!(
        call(&gui, HERE, Method::GET, "/api/model", &[], None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    let sign_in = json!({ "name": "ada", "password": "a long password" });
    let forged = call(
        &gui,
        HERE,
        Method::POST,
        "/gui/session",
        &[],
        Some(sign_in.clone()),
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    let wrong = json!({ "name": "ada", "password": "not it" });
    let refused = call(
        &gui,
        HERE,
        Method::POST,
        "/gui/session",
        &[FROM_CLIENT],
        Some(wrong),
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);

    let signed = call(
        &gui,
        HERE,
        Method::POST,
        "/gui/session",
        &[FROM_CLIENT],
        Some(sign_in),
    )
    .await;
    assert_eq!(signed.status, StatusCode::OK);
    let set = signed.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(
        set.contains("HttpOnly") && set.contains("SameSite=Strict"),
        "{set}"
    );
    assert!(
        !signed.body.contains(set.split(['=', ';']).nth(1).unwrap()),
        "the token stays out of the page"
    );
    assert_eq!(signed.json()["user"]["name"], "ada");
    assert_eq!(signed.json()["user"]["role"], "admin");
    let session = cookie(&signed);

    let model = call(
        &gui,
        HERE,
        Method::GET,
        "/api/model",
        &[("cookie", &session)],
        None,
    )
    .await;
    assert_eq!(model.status, StatusCode::OK);
    assert!(model.headers.contains_key("epiphany-model-version"));
    assert_eq!(model.headers[header::CACHE_CONTROL], "no-store");
    let state = call(
        &gui,
        HERE,
        Method::GET,
        "/gui/session",
        &[("cookie", &session)],
        None,
    )
    .await;
    assert_eq!(state.json()["user"]["name"], "ada");

    // A change needs the application's own header as well as the cookie.
    let write = json!({ "cells": [{ "cube": "General Ledger", "at": ["Rent", "US", "Jan", "Actual"], "value": 5 }] });
    let forged = call(
        &gui,
        HERE,
        Method::POST,
        "/api/cells/write",
        &[("cookie", &session)],
        Some(write.clone()),
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    let written = call(
        &gui,
        HERE,
        Method::POST,
        "/api/cells/write",
        &[("cookie", &session), FROM_CLIENT],
        Some(write),
    )
    .await;
    assert_eq!(written.status, StatusCode::NO_CONTENT);

    let out = call(
        &gui,
        HERE,
        Method::DELETE,
        "/gui/session",
        &[("cookie", &session), FROM_CLIENT],
        None,
    )
    .await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert!(
        out.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    let after = call(
        &gui,
        HERE,
        Method::GET,
        "/api/model",
        &[("cookie", &session)],
        None,
    )
    .await;
    assert_eq!(
        after.status,
        StatusCode::UNAUTHORIZED,
        "the server ended the session too"
    );
}

#[tokio::test]
async fn a_server_that_asks_no_one_to_sign_in_is_reached_only_from_this_machine() {
    let home = Home::new("open", false);
    let gui = Gui::new(&server(&home).await, None);
    let state = call(&gui, HERE, Method::GET, "/gui/session", &[], None).await;
    assert_eq!(state.json(), json!({ "signed_in": false, "open": true }));
    assert_eq!(
        call(&gui, HERE, Method::GET, "/api/model", &[], None)
            .await
            .status,
        StatusCode::OK
    );
    let away = call(&gui, AWAY, Method::GET, "/api/model", &[], None).await;
    assert_eq!(
        (away.status, away.json()["code"].clone()),
        (StatusCode::UNAUTHORIZED, json!("unauthorized"))
    );
}

#[tokio::test]
async fn a_server_that_cannot_be_reached_is_reported_in_the_usual_shape() {
    let gui = Gui::new("http://127.0.0.1:1", None);
    let answer = call(&gui, HERE, Method::GET, "/api/model", &[], None).await;
    assert_eq!(answer.status, StatusCode::BAD_GATEWAY);
    assert_eq!(answer.json()["code"], "unavailable");
}
