//! The Epiphany GUI: serves the browser application, and passes its API calls on to an
//! Epiphany server with the session token the browser's scripts never see.

mod assets;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::Serialize;
use serde_json::{Value, json};

/// The cookie that holds a signed-in browser's session token.
const COOKIE: &str = "epiphany_session";

/// The header the application's own scripts send with every request that changes anything.
/// A page on another site cannot send it without the browser asking this one first, which
/// it is never allowed to.
pub const CLIENT_HEADER: &str = "x-requested-by";
pub const CLIENT: &str = "epiphany-gui";

/// The most a sign-in may carry.
const MOST_SIGN_IN: usize = 16 * 1024;

/// What the browser may load and from where: this server only, no inline scripts or
/// styles, no framing.
const POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
    img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; \
    base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// The application and where its API calls go.
#[derive(Clone)]
pub struct Gui(Arc<Inner>);

struct Inner {
    /// The Epiphany server, as `http://host:port`.
    server: String,
    client: Client<HttpConnector, Body>,
    /// Who each session token belongs to, for the sessions started here.
    sessions: Mutex<HashMap<String, Value>>,
    /// For each view as another user started here, the token of the administrator viewing,
    /// to go back to when the view ends. The page never holds either.
    viewers: Mutex<HashMap<String, String>>,
    /// Where to read the application's files from instead of the copy built in.
    web: Option<PathBuf>,
}

impl Gui {
    /// The application passing its API calls to the Epiphany server at `server`, as
    /// `http://host:port`. With `web`, its files are read from that folder on every request,
    /// so that they can be edited while it runs.
    pub fn new(server: &str, web: Option<PathBuf>) -> Self {
        let client = Client::builder(TokioExecutor::new()).build_http();
        Self(Arc::new(Inner {
            server: server.trim_end_matches('/').to_owned(),
            client,
            sessions: Mutex::default(),
            viewers: Mutex::default(),
            web,
        }))
    }

    /// The routes, to be served with the address of each connection
    /// (`into_make_service_with_connect_info::<SocketAddr>`).
    pub fn router(self) -> Router {
        Router::new()
            .route("/gui/session", get(session).post(sign_in).delete(sign_out))
            .route("/gui/view-as", post(view_as).delete(stop_viewing))
            .route("/api/{*path}", any(pass))
            .fallback(get(assets::serve))
            .layer(from_fn(secure))
            .with_state(self)
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Value>> {
        self.0
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn viewers(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.0
            .viewers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends a request to the Epiphany server.
    async fn send(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Body,
        kind: Option<&HeaderValue>,
    ) -> Result<Response, Box<Response>> {
        let uri: Uri = format!("{}{path}", self.0.server).parse().map_err(|_| {
            Box::new(refusal(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "that path cannot be passed on",
            ))
        })?;
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(kind) = kind {
            request = request.header(header::CONTENT_TYPE, kind);
        }
        let request = request
            .body(body)
            .expect("a request from parts already checked");
        match self.0.client.request(request).await {
            Ok(response) => Ok(response.map(Body::new)),
            Err(_) => Err(Box::new(refusal(
                StatusCode::BAD_GATEWAY,
                "unavailable",
                "the Epiphany server cannot be reached",
            ))),
        }
    }
}

/// An error in the shape the Epiphany server uses for its own.
fn refusal(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "code": code, "message": message }))).into_response()
}

/// The session token a request's cookie carries.
fn token(headers: &HeaderMap) -> Option<&str> {
    let cookies = headers.get_all(header::COOKIE).into_iter();
    let pairs = cookies
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'));
    pairs
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, token)| token)
        .filter(|token| !token.is_empty())
}

/// Whether a request that changes something came from the application's own scripts.
fn from_client(headers: &HeaderMap) -> bool {
    headers
        .get(CLIENT_HEADER)
        .is_some_and(|value| value == CLIENT)
}

fn changes(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// The cookie that keeps a session, or ends one when `token` is empty.
fn cookie(token: &str, headers: &HeaderMap) -> HeaderValue {
    // Behind a proxy that ends TLS the cookie is kept to secure connections.
    let secure = headers
        .get("x-forwarded-proto")
        .is_some_and(|value| value == "https");
    let ending = match token.is_empty() {
        true => "; Max-Age=0",
        false => "",
    };
    let secure = if secure { "; Secure" } else { "" };
    let value = format!("{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/{secure}{ending}");
    HeaderValue::from_str(&value).expect("a token is plain text")
}

/// Who is signed in, or whether the server needs anyone to be.
async fn session(State(gui): State<Gui>, headers: HeaderMap) -> Response {
    if let Some(profile) = token(&headers).and_then(|token| gui.sessions().get(token).cloned()) {
        return Json(json!({ "signed_in": true, "user": profile })).into_response();
    }
    // A route that needs a session answers 401 on a server with users, and only then.
    let probe = gui
        .send(Method::GET, "/cubes/%20", None, Body::empty(), None)
        .await;
    match probe {
        Ok(answer) => {
            let open = answer.status() != StatusCode::UNAUTHORIZED;
            Json(json!({ "signed_in": false, "open": open })).into_response()
        }
        Err(refused) => *refused,
    }
}

#[derive(Serialize)]
struct Profile<'a> {
    name: &'a str,
    #[serde(flatten)]
    session: &'a serde_json::Map<String, Value>,
}

/// Signs in on the Epiphany server, keeping the session's token in a cookie the page's
/// scripts cannot read.
async fn sign_in(State(gui): State<Gui>, headers: HeaderMap, request: Request) -> Response {
    if !from_client(&headers) {
        return refusal(
            StatusCode::FORBIDDEN,
            "forbidden",
            "sign in from the Epiphany application",
        );
    }
    let Ok(body) = axum::body::to_bytes(request.into_body(), MOST_SIGN_IN).await else {
        return refusal(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request",
            "the sign-in is too long",
        );
    };
    let name = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|sign_in| {
            sign_in
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let json = HeaderValue::from_static("application/json");
    let answer = match gui
        .send(
            Method::POST,
            "/sessions",
            None,
            Body::from(body),
            Some(&json),
        )
        .await
    {
        Ok(answer) => answer,
        Err(refused) => return *refused,
    };
    // A refusal goes back as it came; a session is kept here, its token out of the answer.
    if !answer.status().is_success() {
        return answer;
    }
    match keep(&gui, answer, &name).await {
        Ok((token, profile)) => signed_in(&token, &profile, &headers),
        Err(refused) => *refused,
    }
}

/// Keeps the session the Epiphany server answered with, under the name it is of: its token
/// stays here, out of the page's reach. Gives the token and the profile the page is told.
async fn keep(gui: &Gui, answer: Response, name: &str) -> Result<(String, Value), Box<Response>> {
    let unavailable = |message| Box::new(refusal(StatusCode::BAD_GATEWAY, "unavailable", message));
    let Ok(bytes) = answer
        .into_body()
        .collect()
        .await
        .map(|body| body.to_bytes())
    else {
        return Err(unavailable("the session was cut off"));
    };
    let Ok(Value::Object(mut session)) = serde_json::from_slice::<Value>(&bytes) else {
        return Err(unavailable("the session was not understood"));
    };
    let Some(Value::String(token)) = session.remove("token") else {
        return Err(unavailable("the server gave no session"));
    };
    let profile = serde_json::to_value(Profile {
        name,
        session: &session,
    })
    .expect("plain data");
    gui.sessions().insert(token.clone(), profile.clone());
    Ok((token, profile))
}

/// The answer that the page is signed in as a profile, the cookie holding its session.
fn signed_in(token: &str, profile: &Value, headers: &HeaderMap) -> Response {
    let mut response = Json(json!({ "signed_in": true, "user": profile })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie(token, headers));
    response
}

/// Starts a view of the server as another user has it, for the administrator signed in
/// here: the cookie then holds the view's session, which only reads, and the
/// administrator's is kept here to go back to.
async fn view_as(State(gui): State<Gui>, headers: HeaderMap, request: Request) -> Response {
    if !from_client(&headers) {
        return refusal(
            StatusCode::FORBIDDEN,
            "forbidden",
            "view as another user from the Epiphany application",
        );
    }
    let Some(own) = token(&headers).map(str::to_owned) else {
        return refusal(StatusCode::UNAUTHORIZED, "unauthorized", "sign in first");
    };
    let Ok(body) = axum::body::to_bytes(request.into_body(), MOST_SIGN_IN).await else {
        return refusal(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request",
            "the request is too long",
        );
    };
    let user = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|asked| asked.get("user").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default();
    let json = HeaderValue::from_static("application/json");
    let answer = match gui
        .send(
            Method::POST,
            "/admin/view-as",
            Some(&own),
            Body::from(body),
            Some(&json),
        )
        .await
    {
        Ok(answer) => answer,
        Err(refused) => return *refused,
    };
    if !answer.status().is_success() {
        return answer;
    }
    match keep(&gui, answer, &user).await {
        Ok((view, profile)) => {
            gui.viewers().insert(view.clone(), own);
            signed_in(&view, &profile, &headers)
        }
        Err(refused) => *refused,
    }
}

/// Ends a view as another user, and goes back to the administrator's own session, where the
/// server still has it.
async fn stop_viewing(State(gui): State<Gui>, headers: HeaderMap) -> Response {
    if !from_client(&headers) {
        return refusal(
            StatusCode::FORBIDDEN,
            "forbidden",
            "stop viewing from the Epiphany application",
        );
    }
    let Some(view) = token(&headers).map(str::to_owned) else {
        return refusal(StatusCode::UNAUTHORIZED, "unauthorized", "sign in first");
    };
    let Some(own) = gui.viewers().remove(&view) else {
        return refusal(
            StatusCode::CONFLICT,
            "not_viewing",
            "this session views the server as nobody else",
        );
    };
    gui.sessions().remove(&view);
    let _ = gui
        .send(
            Method::DELETE,
            "/sessions",
            Some(&view),
            Body::empty(),
            None,
        )
        .await;
    let profile = gui.sessions().get(&own).cloned();
    match profile {
        Some(profile) => signed_in(&own, &profile, &headers),
        None => {
            let mut response = Json(json!({ "signed_in": false })).into_response();
            response
                .headers_mut()
                .insert(header::SET_COOKIE, cookie("", &headers));
            response
        }
    }
}

/// Signs out on the Epiphany server and forgets the cookie.
async fn sign_out(State(gui): State<Gui>, headers: HeaderMap) -> Response {
    if !from_client(&headers) {
        return refusal(
            StatusCode::FORBIDDEN,
            "forbidden",
            "sign out from the Epiphany application",
        );
    }
    if let Some(token) = token(&headers) {
        // Signing out of a view signs the administrator viewing out as well.
        let own = gui.viewers().remove(token);
        for token in std::iter::once(token).chain(own.as_deref()) {
            gui.sessions().remove(token);
            // Signed out here whatever the server says: the cookie goes either way.
            let _ = gui
                .send(
                    Method::DELETE,
                    "/sessions",
                    Some(token),
                    Body::empty(),
                    None,
                )
                .await;
        }
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie("", &headers));
    response
}

/// Passes an API call on to the Epiphany server, with the session's token.
async fn pass(
    State(gui): State<Gui>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let token = token(&parts.headers).map(str::to_owned);
    let known = token
        .as_ref()
        .is_some_and(|token| gui.sessions().contains_key(token));
    // Only this machine may reach a server without signing in here first: a server with no
    // users must not be opened to the network through this one.
    if !peer.ip().is_loopback() && !known {
        return refusal(StatusCode::UNAUTHORIZED, "unauthorized", "sign in first");
    }
    if changes(&parts.method) && !from_client(&parts.headers) {
        return refusal(
            StatusCode::FORBIDDEN,
            "forbidden",
            "changes are made from the Epiphany application",
        );
    }
    let path = parts.uri.path().strip_prefix("/api").unwrap_or_default();
    let query = parts
        .uri
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let kind = parts.headers.get(header::CONTENT_TYPE);
    let answer = gui
        .send(
            parts.method.clone(),
            &format!("{path}{query}"),
            token.as_deref(),
            body,
            kind,
        )
        .await;
    let mut answer = match answer {
        Ok(answer) => answer,
        Err(refused) => return *refused,
    };
    // Only what the page needs comes back: the body, its type, and the model's version.
    let kept: [HeaderName; 3] = [
        header::CONTENT_TYPE,
        header::RETRY_AFTER,
        HeaderName::from_static("epiphany-model-version"),
    ];
    let headers = std::mem::take(answer.headers_mut());
    for name in kept {
        if let Some(value) = headers.get(&name) {
            answer.headers_mut().insert(name, value.clone());
        }
    }
    // A session the server no longer knows is over here as well.
    if answer.status() == StatusCode::UNAUTHORIZED
        && let Some(token) = &token
    {
        gui.sessions().remove(token);
        gui.viewers().remove(token);
        answer
            .headers_mut()
            .insert(header::SET_COOKIE, cookie("", &parts.headers));
    }
    answer
}

/// Adds to every response the headers that keep a browser from doing anything with it but
/// what the application means to.
async fn secure(request: Request, next: Next) -> Response {
    let api =
        request.uri().path().starts_with("/api/") || request.uri().path().starts_with("/gui/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let set = [
        (header::CONTENT_SECURITY_POLICY, POLICY),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
        (
            HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
        (
            HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
        (
            HeaderName::from_static("permissions-policy"),
            "camera=(), microphone=(), geolocation=()",
        ),
    ];
    for (name, value) in set {
        headers.insert(name, HeaderValue::from_static(value));
    }
    if api {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}
