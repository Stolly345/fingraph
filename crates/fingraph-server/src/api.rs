//! The HTTP API (`/api/v1`) and the small status page.
use crate::config::Config;
use crate::ledger::Ledger;
use crate::state::{Device, State};
use crate::{backups, inbox, views};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query, State as Ext};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

pub const API: u32 = 1;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct App {
    pub cfg: Config,
    pub ledger: Ledger,
    pub state: Mutex<State>,
    pub fingerprint: String,
    last_pair: Mutex<Option<Instant>>,
}

pub type Shared = Arc<App>;

impl App {
    pub fn new(cfg: Config, ledger: Ledger, state: State, fingerprint: String) -> Shared {
        Arc::new(App { cfg, ledger, state: Mutex::new(state), fingerprint, last_pair: Mutex::new(None) })
    }
    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

pub fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}
pub fn internal(msg: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, msg.to_string())
}

/// The paired device making this request.
pub fn device(app: &App, headers: &HeaderMap) -> ApiResult<Device> {
    let token = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::trim).unwrap_or("");
    if token.is_empty() {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "This device isn't paired with the server.".into()));
    }
    app.state().device_for_token(token).ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "This device is no longer paired with the server. Pair it again.".into()))
}

pub fn router(app: Shared) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/api/v1/info", get(info))
        .route("/api/v1/pair", post(pair))
        .route("/api/v1/me", get(me))
        .route("/api/v1/devices", get(devices))
        .route("/api/v1/devices/{id}", delete(revoke))
        .route("/api/v1/lease", post(take_lease).delete(release_lease))
        .route("/api/v1/ledger", get(download_ledger).put(upload_ledger).layer(DefaultBodyLimit::max(4 << 30)))
        .route("/api/v1/statements", post(upload_statement).layer(DefaultBodyLimit::max(200 << 20)))
        .route("/api/v1/statements/{sha}", get(download_statement))
        .route("/api/v1/view/{name}", get(view))
        .with_state(app)
}

async fn page(Ext(app): Ext<Shared>) -> Html<String> {
    let (devices, lease) = {
        let s = app.state();
        (s.devices().map(|d| d.len()).unwrap_or(0), s.lease())
    };
    let waiting = inbox::ready_files(&app.cfg.inbox(), Duration::ZERO).len();
    let attention = attention(&app.cfg).len();
    let backup = backups::latest(&app.cfg.backups()).map(|(_, d)| d.to_string()).unwrap_or_else(|| "none yet".into());
    let editing = lease.map(|l| format!("<li>Being edited on <b>{}</b></li>", html(&l.device))).unwrap_or_default();
    Html(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>FinGraph Server</title><style>
:root{{color-scheme:light dark;--fg:#1b1f23;--bg:#f6f7f8;--card:#fff;--muted:#5d6670;--accent:#1c6e8c}}
@media (prefers-color-scheme:dark){{:root{{--fg:#e7eaee;--bg:#15181b;--card:#1f2327;--muted:#9aa3ad;--accent:#5fb3d1}}}}
body{{margin:0;font:16px/1.5 system-ui,sans-serif;background:var(--bg);color:var(--fg)}}main{{max-width:560px;margin:0 auto;padding:32px 16px}}
h1{{font-size:24px;margin:0 0 4px}}p{{color:var(--muted);margin:0 0 24px}}section{{background:var(--card);border-radius:12px;padding:16px 20px;margin-bottom:16px}}
ul{{margin:0;padding-left:20px}}code{{font:14px ui-monospace,monospace;background:rgba(127,127,127,.15);padding:2px 6px;border-radius:6px;word-break:break-all}}b{{color:var(--accent)}}
</style></head><body><main><h1>FinGraph Server</h1><p>Version {VERSION} · running</p>
<section><ul><li>{devices} paired device{ds}</li>{editing}<li>{waiting} statement{ws} in the inbox · {attention} needing attention</li><li>Last backup: {backup}</li></ul></section>
<section>To pair the desktop app or the phone, make a one-time code on the server:<br><br><code>docker exec fingraph fingraph-server pair</code><br><br>
Then in FinGraph choose <b>Preferences → Server</b> (desktop) or <b>Connect to a server</b> (phone).</section>
<section>Server fingerprint: <code>{fp}</code></section></main></body></html>"#,
        ds = if devices == 1 { "" } else { "s" },
        ws = if waiting == 1 { "" } else { "s" },
        fp = crate::tls::short_fingerprint(&app.fingerprint),
    ))
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

async fn info() -> Json<Value> {
    Json(json!({ "product": "fingraph-server", "version": VERSION, "api": API }))
}

#[derive(Deserialize)]
struct PairBody {
    proof: String,
    name: String,
    #[serde(default)]
    platform: String,
}

async fn pair(Ext(app): Ext<Shared>, Json(b): Json<PairBody>) -> ApiResult<Json<Value>> {
    {
        // one try a second at most, from everyone together
        let mut last = app.last_pair.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return Err(ApiError(StatusCode::TOO_MANY_REQUESTS, "Too many tries; wait a moment.".into()));
        }
        *last = Some(Instant::now());
    }
    let r = app.state().pair(&b.proof, &app.fingerprint, &b.name, &b.platform);
    match r {
        Ok((device, token)) => {
            log(&format!("paired {} ({})", device.name, device.platform));
            Ok(Json(json!({ "token": token, "device": device, "fingerprint": app.fingerprint })))
        }
        Err(e) => Err(ApiError(StatusCode::FORBIDDEN, e)),
    }
}

/// Inbox files that went to `needs-attention/`, with their reasons.
pub fn attention(cfg: &Config) -> Vec<Value> {
    let dir = cfg.attention();
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut v: Vec<Value> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("pdf")))
        .map(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let reason = std::fs::read_to_string(dir.join(format!("{name}.txt"))).ok().and_then(|t| t.lines().next().map(str::to_string)).unwrap_or_default();
            json!({ "file": name, "reason": reason })
        })
        .collect();
    v.sort_by(|a, b| a["file"].as_str().cmp(&b["file"].as_str()));
    v
}

async fn me(Ext(app): Ext<Shared>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let d = device(&app, &headers)?;
    let (lease, version, changed) = {
        let s = app.state();
        (s.lease(), s.ledger_version(), s.meta("ledger_changed"))
    };
    let workspaces = app.ledger.call(|db| db.entities().map(|e| e.len()).unwrap_or(0)).await.map_err(internal)?;
    Ok(Json(json!({
        "device": d,
        "server": {
            "version": VERSION, "api": API,
            "ledger_version": version, "ledger_changed": changed, "workspaces": workspaces,
            "lease": lease,
            "inbox": inbox::ready_files(&app.cfg.inbox(), Duration::ZERO).len(),
            "needs_attention": attention(&app.cfg),
            "last_backup": backups::latest(&app.cfg.backups()).map(|(_, d)| d.to_string()),
        }
    })))
}

async fn devices(Ext(app): Ext<Shared>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let me = device(&app, &headers)?;
    let list = app.state().devices().map_err(internal)?;
    Ok(Json(json!({ "this_device": me.id, "devices": list })))
}

async fn revoke(Ext(app): Ext<Shared>, headers: HeaderMap, UrlPath(id): UrlPath<i64>) -> ApiResult<StatusCode> {
    device(&app, &headers)?;
    if app.state().revoke(id).map_err(internal)? {
        log(&format!("unpaired device {id}"));
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(StatusCode::NOT_FOUND, "No such device.".into()))
    }
}

// ---------------------------------------------------------------------- the edit lease

async fn take_lease(Ext(app): Ext<Shared>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let d = device(&app, &headers)?;
    let s = app.state();
    match s.take_lease(&d) {
        Ok(l) => Ok(Json(json!({ "lease": l, "ledger_version": s.ledger_version(), "minutes": crate::state::LEASE_MINUTES }))),
        Err(other) => Err(ApiError(StatusCode::CONFLICT, format!("The ledger is being edited on {}.", other.device))),
    }
}

async fn release_lease(Ext(app): Ext<Shared>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let d = device(&app, &headers)?;
    app.state().release_lease(&d);
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------- the whole ledger

fn tmp_dir(cfg: &Config) -> std::path::PathBuf {
    let d = cfg.data.join("tmp");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// A complete backup of the ledger as it is now, for a device to work on.
async fn download_ledger(Ext(app): Ext<Shared>, headers: HeaderMap) -> ApiResult<Response> {
    device(&app, &headers)?;
    let version = app.state().ledger_version();
    let dir = tmp_dir(&app.cfg);
    let path = dir.join(format!("download-{}.tar", crate::state::sha256_hex(&crate::state::random_bytes::<8>())));
    let p2 = path.clone();
    app.ledger.call(move |db| db.backup_archive(&p2)).await.map_err(internal)?.map_err(internal)?;
    let file = tokio::fs::File::open(&path).await.map_err(internal)?;
    let len = file.metadata().await.map_err(internal)?.len();
    // the open file keeps its contents; the name can go now
    let _ = std::fs::remove_file(&path);
    let mut r = Body::from_stream(tokio_util::io::ReaderStream::new(file)).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-tar"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert("x-ledger-version", HeaderValue::from(version));
    Ok(r)
}

#[derive(Deserialize)]
struct UploadQuery {
    /// The ledger version the device started from; if the server's has moved on since, the upload is
    /// refused rather than losing those changes.
    base: Option<i64>,
}

async fn body_to_file(body: Body, path: &std::path::Path) -> ApiResult<u64> {
    let mut f = tokio::fs::File::create(path).await.map_err(internal)?;
    let mut stream = body.into_data_stream();
    let mut n = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| bad(format!("the upload was cut off: {e}")))?;
        n += chunk.len() as u64;
        f.write_all(&chunk).await.map_err(internal)?;
    }
    f.flush().await.map_err(internal)?;
    Ok(n)
}

/// Replaces the server's ledger with the device's (a complete backup), from the device holding the lease.
async fn upload_ledger(Ext(app): Ext<Shared>, headers: HeaderMap, Query(q): Query<UploadQuery>, body: Body) -> ApiResult<Json<Value>> {
    let d = device(&app, &headers)?;
    {
        let s = app.state();
        match s.lease() {
            Some(l) if l.device_id == d.id => {}
            Some(l) => return Err(ApiError(StatusCode::CONFLICT, format!("The ledger is being edited on {}.", l.device))),
            None => return Err(ApiError(StatusCode::CONFLICT, "Take the edit lease before sending the ledger.".into())),
        }
        if let Some(base) = q.base
            && base != s.ledger_version()
        {
            return Err(ApiError(StatusCode::CONFLICT, "The ledger on the server changed since this device took its copy.".into()));
        }
    }
    let path = tmp_dir(&app.cfg).join(format!("upload-{}.tar", crate::state::sha256_hex(&crate::state::random_bytes::<8>())));
    let size = body_to_file(body, &path).await;
    if let Err(e) = size {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    let p2 = path.clone();
    let app2 = app.clone();
    let restored = app
        .ledger
        .call(move |db| -> Result<usize, String> {
            // what the server had stays recoverable
            let _ = db.auto_backup("before-upload");
            db.restore_from(&p2).map_err(|e| e.to_string())?;
            let s = app2.state();
            Ok(inbox::file_missing(db, &s, &app2.cfg.statements))
        })
        .await
        .map_err(internal)?;
    let _ = std::fs::remove_file(&path);
    let filed = restored.map_err(bad)?;
    let version = app.state().bump_ledger_version().map_err(internal)?;
    log(&format!("ledger received from {} (version {version}, {filed} statement{} filed)", d.name, if filed == 1 { "" } else { "s" }));
    Ok(Json(json!({ "ledger_version": version, "filed": filed })))
}

// ---------------------------------------------------------------------- single statements

#[derive(Deserialize)]
struct StatementQuery {
    name: String,
}

/// A statement PDF from a device: it goes through the inbox like any other (so a phone can share one).
async fn upload_statement(Ext(app): Ext<Shared>, headers: HeaderMap, Query(q): Query<StatementQuery>, body: Body) -> ApiResult<Json<Value>> {
    let d = device(&app, &headers)?;
    let inbox = app.cfg.inbox();
    std::fs::create_dir_all(&inbox).map_err(internal)?;
    let dest = crate::filing::free_name(&inbox, &q.name);
    let tmp = inbox.join(format!(".{}.partial", crate::state::sha256_hex(&crate::state::random_bytes::<8>())));
    if let Err(e) = body_to_file(body, &tmp).await {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &dest).map_err(internal)?;
    log(&format!("{} sent {}", d.name, dest.file_name().unwrap_or_default().to_string_lossy()));
    Ok(Json(json!({ "queued": dest.file_name().unwrap_or_default().to_string_lossy() })))
}

/// A statement's PDF by its SHA-256.
async fn download_statement(Ext(app): Ext<Shared>, headers: HeaderMap, UrlPath(sha): UrlPath<String>) -> ApiResult<Response> {
    device(&app, &headers)?;
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad("Not a statement identifier."));
    }
    let filed = app.state().filed(&sha).map(|rel| app.cfg.statements.join(rel)).filter(|p| p.exists());
    let path = match filed {
        Some(p) => Some(p),
        None => {
            let s2 = sha.clone();
            app.ledger.call(move |db| db.stored_pdf(&s2, "")).await.map_err(internal)?
        }
    };
    let path = path.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "The server doesn't have that statement's file.".into()))?;
    let bytes = tokio::fs::read(&path).await.map_err(internal)?;
    Ok(([(header::CONTENT_TYPE, "application/pdf")], bytes).into_response())
}

// ---------------------------------------------------------------------- what the screens show

async fn view(Ext(app): Ext<Shared>, headers: HeaderMap, UrlPath(name): UrlPath<String>, Query(q): Query<views::Params>) -> ApiResult<Json<Value>> {
    device(&app, &headers)?;
    let version = app.state().ledger_version();
    let out = app.ledger.call(move |db| views::render(db, &name, &q)).await.map_err(internal)?;
    match out {
        Ok(mut v) => {
            if let Some(o) = v.as_object_mut() {
                o.insert("ledger_version".into(), json!(version));
            }
            Ok(Json(v))
        }
        Err(views::ViewError::Unknown) => Err(ApiError(StatusCode::NOT_FOUND, "No such view.".into())),
        Err(views::ViewError::Failed(e)) => Err(internal(e)),
    }
}

pub fn log(msg: &str) {
    println!("{} {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    struct T {
        _dir: tempfile::TempDir,
        app: Shared,
    }

    fn server() -> T {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            data: dir.path().join("data"),
            statements: dir.path().join("statements"),
            listen: "127.0.0.1:0".into(),
            address: None,
            inbox_every: Duration::from_secs(1),
            settle: Duration::ZERO,
            backup_hour: 3,
        };
        let ledger = Ledger::open(&cfg.ledger()).unwrap();
        let state = State::open(&cfg.server_db()).unwrap();
        T { app: App::new(cfg, ledger, state, "ab".repeat(32)), _dir: dir }
    }

    async fn call(t: &T, method: &str, uri: &str, token: Option<&str>, body: Body) -> (StatusCode, Vec<u8>) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(tok) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {tok}"));
        }
        if method == "POST" {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        let res = router(t.app.clone()).oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        (status, res.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    fn js(b: &[u8]) -> Value {
        serde_json::from_slice(b).unwrap_or(Value::Null)
    }

    async fn pair_device(t: &T, name: &str) -> String {
        let code = t.app.state().new_pairing_code().unwrap();
        *t.app.last_pair.lock().unwrap() = None;
        let proof = crate::state::pairing_proof(&code, &t.app.fingerprint);
        let (s, b) = call(t, "POST", "/api/v1/pair", None, Body::from(json!({ "proof": proof, "name": name, "platform": "test" }).to_string())).await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        js(&b)["token"].as_str().unwrap().to_string()
    }

    fn demo_backup(dir: &std::path::Path) -> Vec<u8> {
        let db = fingraph_core::db::Db::open(&dir.join("demo/ledger.db")).unwrap();
        db.load_demo().unwrap();
        let tar = dir.join("demo.tar");
        db.backup_archive(&tar).unwrap();
        std::fs::read(tar).unwrap()
    }

    #[tokio::test]
    async fn pairing_the_lease_and_moving_a_ledger_in_and_out() {
        let t = server();
        let (s, b) = call(&t, "GET", "/api/v1/info", None, Body::empty()).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(js(&b)["api"], API);
        assert_eq!(call(&t, "GET", "/api/v1/me", None, Body::empty()).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call(&t, "GET", "/api/v1/view/summary", Some("guess"), Body::empty()).await.0, StatusCode::UNAUTHORIZED);

        // a wrong code is refused
        t.app.state().new_pairing_code().unwrap();
        let bad_proof = crate::state::pairing_proof("AAAABBBB", &t.app.fingerprint);
        let (s, _) = call(&t, "POST", "/api/v1/pair", None, Body::from(json!({ "proof": bad_proof, "name": "x" }).to_string())).await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        let desk = pair_device(&t, "Desktop").await;
        let phone = pair_device(&t, "Phone").await;
        let (s, b) = call(&t, "GET", "/api/v1/me", Some(&desk), Body::empty()).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(js(&b)["device"]["name"], "Desktop");
        assert_eq!(js(&b)["server"]["workspaces"], 0);

        // sending the ledger needs the lease
        let tar = demo_backup(t._dir.path());
        assert_eq!(call(&t, "PUT", "/api/v1/ledger", Some(&desk), Body::from(tar.clone())).await.0, StatusCode::CONFLICT);
        assert_eq!(call(&t, "POST", "/api/v1/lease", Some(&desk), Body::empty()).await.0, StatusCode::OK);
        let (s, b) = call(&t, "POST", "/api/v1/lease", Some(&phone), Body::empty()).await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(js(&b)["error"].as_str().unwrap().contains("Desktop"));
        assert_eq!(call(&t, "PUT", "/api/v1/ledger", Some(&phone), Body::from(tar.clone())).await.0, StatusCode::CONFLICT);

        let (s, b) = call(&t, "PUT", "/api/v1/ledger?base=0", Some(&desk), Body::from(tar.clone())).await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert_eq!(js(&b)["ledger_version"], 1);
        // a device working from an older copy can't overwrite newer changes
        assert_eq!(call(&t, "PUT", "/api/v1/ledger?base=0", Some(&desk), Body::from(tar)).await.0, StatusCode::CONFLICT);

        // the phone reads what the desktop sent
        let (s, b) = call(&t, "GET", "/api/v1/view/summary?month=2026-09", Some(&phone), Body::empty()).await;
        assert_eq!(s, StatusCode::OK);
        let v = js(&b);
        assert_eq!(v["workspaces"].as_array().unwrap().len(), 2);
        assert_eq!(v["ledger_version"], 1);
        assert_eq!(call(&t, "GET", "/api/v1/view/nope", Some(&phone), Body::empty()).await.0, StatusCode::NOT_FOUND);

        // and the whole ledger comes back out intact
        let (s, b) = call(&t, "GET", "/api/v1/ledger", Some(&desk), Body::empty()).await;
        assert_eq!(s, StatusCode::OK);
        let out = t._dir.path().join("back.tar");
        std::fs::write(&out, &b).unwrap();
        let copy = fingraph_core::db::Db::open(&t._dir.path().join("copy/ledger.db")).unwrap();
        copy.restore_from(&out).unwrap();
        assert_eq!(copy.entities().unwrap().len(), 2);

        // giving the lease back lets the phone have it
        assert_eq!(call(&t, "DELETE", "/api/v1/lease", Some(&desk), Body::empty()).await.0, StatusCode::NO_CONTENT);
        assert_eq!(call(&t, "POST", "/api/v1/lease", Some(&phone), Body::empty()).await.0, StatusCode::OK);

        // unpairing ends a device's access at once
        let (_, b) = call(&t, "GET", "/api/v1/devices", Some(&desk), Body::empty()).await;
        let phone_id = js(&b)["devices"].as_array().unwrap().iter().find(|d| d["name"] == "Phone").unwrap()["id"].as_i64().unwrap();
        assert_eq!(call(&t, "DELETE", &format!("/api/v1/devices/{phone_id}"), Some(&desk), Body::empty()).await.0, StatusCode::NO_CONTENT);
        assert_eq!(call(&t, "GET", "/api/v1/me", Some(&phone), Body::empty()).await.0, StatusCode::UNAUTHORIZED);
        assert!(t.app.state().lease().is_none(), "its lease went with it");
    }

    #[tokio::test]
    async fn a_statement_sent_by_a_device_lands_in_the_inbox() {
        let t = server();
        let tok = pair_device(&t, "Phone").await;
        let (s, b) = call(&t, "POST", "/api/v1/statements?name=Oct%20statement.pdf", Some(&tok), Body::from(b"%PDF-1.4 x".to_vec())).await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(t.app.cfg.inbox().join("Oct statement.pdf").exists());
        let (s, _) = call(&t, "POST", "/api/v1/statements?name=..%2F..%2Fescape.pdf", Some(&tok), Body::from(b"x".to_vec())).await;
        assert_eq!(s, StatusCode::OK);
        assert!(t.app.cfg.inbox().join("escape.pdf").exists(), "a path in the name can't leave the inbox");
    }

    #[tokio::test]
    async fn the_status_page_shows_no_secrets() {
        let t = server();
        let code = t.app.state().new_pairing_code().unwrap();
        let (s, b) = call(&t, "GET", "/", None, Body::empty()).await;
        assert_eq!(s, StatusCode::OK);
        let page = String::from_utf8(b).unwrap();
        assert!(page.contains("fingraph-server pair"));
        assert!(!page.contains(&code) && !page.contains(&State::display_code(&code)));
    }
}
