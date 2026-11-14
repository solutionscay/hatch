//! Minimal MCP server (streamable HTTP, JSON responses) exposing the iPhone to an agent.

use crate::device::{self, Root};
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

pub const DEFAULT_PORT: u16 = 7433;

/// The MCP port. `HATCH_MCP_PORT` overrides it, so a second copy can run beside the first.
pub fn port() -> u16 {
    std::env::var("HATCH_MCP_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_PORT)
}

fn token_file() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join(".config/hatch/mcp-token")
}

/// 64 hex chars (two v4 UUIDs, 244 random bits from the OS RNG).
fn new_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

fn save_token(t: &str) -> anyhow::Result<()> {
    let p = token_file();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    std::io::Write::write_all(&mut o.open(&p)?, t.as_bytes())?;
    Ok(())
}

/// Read the token from the Hatch config folder. Create it on first launch.
pub fn load_or_create_token() -> String {
    if let Ok(t) = std::fs::read_to_string(token_file()) {
        let t = t.trim();
        if t.len() >= 32 {
            return t.to_string();
        }
    }
    let t = new_token();
    if let Err(e) = save_token(&t) {
        eprintln!("could not save mcp token: {e:#}");
    }
    t
}

/// Make a new token, save it, and return it. Old clients get 401 at once.
pub fn rotate_token(slot: &RwLock<String>) -> anyhow::Result<String> {
    let t = new_token();
    save_token(&t)?;
    *slot.write().unwrap() = t.clone();
    Ok(t)
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(got) = headers.get("authorization").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some((scheme, got)) = got.trim().split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    let (a, b) = (got.trim().as_bytes(), token.as_bytes());
    // Constant-time compare.
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Clone)]
pub struct LogEntry {
    pub ts: String,
    pub tool: String,
    pub path: String,
    pub ok: bool,
    pub denied: bool,
    pub write: bool,
    pub ms: u128,
}

#[derive(Clone)]
pub struct McpState {
    pub allow_writes: Arc<AtomicBool>,
    pub calls: Arc<AtomicUsize>,
    pub log: Arc<std::sync::Mutex<Vec<LogEntry>>>,
    pub token: Arc<RwLock<String>>,
}

pub async fn serve(state: McpState) -> anyhow::Result<()> {
    let app = Router::new().route("/mcp", post(handle)).with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port())).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle(State(st): State<McpState>, headers: HeaderMap, body: String) -> Response {
    // Block browser-origin requests (DNS rebinding / drive-by pages).
    if let Some(o) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        if !(o.starts_with("http://127.0.0.1") || o.starts_with("http://localhost")) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    if !authorized(&headers, &st.token.read().unwrap()) {
        return (StatusCode::UNAUTHORIZED, [("www-authenticate", "Bearer")], "missing or wrong bearer token\n").into_response();
    }
    let Ok(req) = serde_json::from_str::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(id) = req.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response(); // notification
    };
    let method = req["method"].as_str().unwrap_or("");
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2025-03-26"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "hatch", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Browse and read files on a USB-connected iPhone. Root media is the phone media partition (photos under /DCIM, books, downloads). Any other root is an app bundle id (use list_apps) and maps to that app's Documents folder."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            st.calls.fetch_add(1, Ordering::Relaxed);
            let name = req["params"]["name"].as_str().unwrap_or("").to_string();
            let args = req["params"]["arguments"].clone();
            let started = std::time::Instant::now();
            let res = call(&st, &name, &args).await;
            let denied = res.as_ref().err().is_some_and(|e| e.to_string().starts_with("writes are disabled"));
            let mut log = st.log.lock().unwrap();
            log.push(LogEntry {
                ts: chrono::Local::now().format("%H:%M:%S").to_string(),
                write: matches!(name.as_str(), "write_file" | "make_dir" | "delete") || (name == "run_sync_job" && !args["preview"].as_bool().unwrap_or(false)),
                path: args["path"].as_str().or(args["dest"].as_str()).or(args["job"].as_str()).unwrap_or("-").to_string(),
                tool: name.clone(),
                ok: res.is_ok(),
                denied,
                ms: started.elapsed().as_millis(),
            });
            if log.len() > 500 {
                log.remove(0);
            }
            drop(log);
            Ok(match res {
                Ok(content) => json!({ "content": content, "isError": false }),
                Err(e) => json!({ "content": [{"type":"text","text": crate::device::explain(&e)}], "isError": true }),
            })
        }
        _ => Err(json!({ "code": -32601, "message": "method not found" })),
    };
    let body = match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
    };
    ([("content-type", "application/json")], body.to_string()).into_response()
}

fn tools() -> Value {
    let dev = json!({"type":"string","description":"Device UDID. Defaults to the first connected device."});
    let root = json!({"type":"string","description":"'media' (camera roll) or an app bundle id from list_apps. Default 'media'."});
    let path = json!({"type":"string","description":"Absolute path inside the root, e.g. /DCIM"});
    json!([
        {"name":"list_devices","description":"List USB-connected iPhones and whether they trust this computer.",
         "inputSchema":{"type":"object","properties":{}}},
        {"name":"device_info","description":"Read-only device info: name, model, iOS version and build, serial, UDID, battery level and charging state, total and free storage in bytes.",
         "inputSchema":{"type":"object","properties":{"udid":dev}}},
        {"name":"list_apps","description":"List apps that expose a Documents folder (iTunes file sharing).",
         "inputSchema":{"type":"object","properties":{"udid":dev}}},
        {"name":"list_dir","description":"List a directory on the iPhone.",
         "inputSchema":{"type":"object","properties":{"udid":dev,"root":root,"path":path}}},
        {"name":"read_file","description":"Read a file. Text is returned as text. Binary files are returned as base64. Output is capped by max_bytes (default 262144).",
         "inputSchema":{"type":"object","properties":{"udid":dev,"root":root,"path":path,"max_bytes":{"type":"integer"}},"required":["path"]}},
        {"name":"download_file","description":"Copy a file from the iPhone to a path on this computer. Streams the file, so any size works. Replaces dest if it exists.",
         "inputSchema":{"type":"object","properties":{"udid":dev,"root":root,"path":path,"dest":{"type":"string","description":"Local destination file path"}},"required":["path","dest"]}},
        {"name":"write_file","description":"Write text to a file on the iPhone. Disabled unless the user enables writes in the Hatch window.",
         "inputSchema":{"type":"object","properties":{"udid":dev,"root":root,"path":path,"text":{"type":"string"}},"required":["path","text"]}},
        {"name":"make_dir","description":"Create a directory on the iPhone. Disabled unless the user enables writes in the Hatch window.",
         "inputSchema":{"type":"object","properties":{"udid":dev,"root":root,"path":path},"required":["path"]}},
        {"name":"delete","description":"Delete a file or directory tree on the iPhone. Disabled unless the user enables writes in the Hatch window.",
         "inputSchema":{"type":"object","properties":{"udid":dev,"root":root,"path":path},"required":["path"]}}
    ])
    .as_array()
    .cloned()
    .unwrap_or_default()
    .into_iter()
    .chain(crate::photos::tools())
    .chain(sync_tools())
    .collect()
}

fn sync_tools() -> Vec<Value> {
    vec![
        json!({"name":"list_sync_jobs",
            "description":"List the sync jobs the user saved in Hatch. Each job pairs a folder on the iPhone with a folder on this computer. direction is to_computer or both. writes_phone is true when a run can write to the iPhone.",
            "inputSchema":{"type":"object","properties":{}}}),
        json!({"name":"run_sync_job",
            "description":"Run a saved sync job by id or name. A job never deletes files. Files changed on both sides are kept twice, and the extra copy gets ' (conflict YYYY-MM-DD)' in its name. A job with writes_phone needs 'Agent writes' on in Hatch. Set preview to true to count what a run would copy, without copying. A job that never ran must run once in Hatch first. Agents cannot create or edit jobs.",
            "inputSchema":{"type":"object","properties":{
                "job":{"type":"string","description":"Job id or name from list_sync_jobs"},
                "preview":{"type":"boolean","description":"Only count new, changed and conflicting files. Default false."}
            },"required":["job"]}}),
    ]
}

fn text(s: impl Into<String>) -> Vec<Value> {
    vec![json!({"type":"text","text": s.into()})]
}

async fn udid_of(args: &Value) -> anyhow::Result<String> {
    if let Some(u) = args["udid"].as_str() {
        return Ok(u.to_string());
    }
    device::list_devices()
        .await?
        .into_iter()
        .next()
        .map(|d| d.udid)
        .ok_or_else(|| anyhow::anyhow!("no iPhone connected over USB"))
}

async fn call(st: &McpState, name: &str, args: &Value) -> anyhow::Result<Vec<Value>> {
    let root = Root::parse(args["root"].as_str().unwrap_or("media"));
    let path = args["path"].as_str().unwrap_or("/");
    let write_gate = || -> anyhow::Result<()> {
        if st.allow_writes.load(Ordering::Relaxed) {
            Ok(())
        } else {
            anyhow::bail!("writes are disabled. Ask the user to turn on 'Agent writes' in Hatch.")
        }
    };
    match name {
        "list_devices" => Ok(text(serde_json::to_string_pretty(&device::list_devices().await?)?)),
        "device_info" => {
            let u = udid_of(args).await?;
            Ok(text(serde_json::to_string_pretty(&crate::devinfo::device_info(&u).await?)?))
        }
        "list_apps" => {
            let u = udid_of(args).await?;
            Ok(text(serde_json::to_string_pretty(&device::list_apps(&u).await?)?))
        }
        "list_dir" => {
            let u = udid_of(args).await?;
            Ok(text(serde_json::to_string_pretty(&device::list_dir(&u, &root, path).await?)?))
        }
        "read_file" => {
            let u = udid_of(args).await?;
            let max = args["max_bytes"].as_u64().unwrap_or(262_144) as usize;
            let (data, total) = device::read_file(&u, &root, path, max).await?;
            let note = if (data.len() as u64) < total {
                format!("[truncated: {} of {} bytes]\n", data.len(), total)
            } else {
                String::new()
            };
            Ok(match String::from_utf8(data) {
                Ok(s) => text(format!("{note}{s}")),
                Err(e) => text(format!("{note}[binary, base64]\n{}", STANDARD.encode(e.into_bytes()))),
            })
        }
        "download_file" => {
            let u = udid_of(args).await?;
            let dest = args["dest"].as_str().ok_or_else(|| anyhow::anyhow!("dest is required"))?;
            // Streams in 1 MiB pieces, so large videos never sit in memory.
            let t = device::Transfer::default();
            let n = device::download(&u, &root, &[(path.to_string(), dest.into())], &t).await?;
            Ok(text(format!("saved {n} bytes to {dest}")))
        }
        "write_file" => {
            write_gate()?;
            let u = udid_of(args).await?;
            let t = args["text"].as_str().unwrap_or("");
            device::write_file(&u, &root, path, t.as_bytes()).await?;
            Ok(text(format!("wrote {} bytes to {path}", t.len())))
        }
        "make_dir" => {
            write_gate()?;
            device::make_dir(&udid_of(args).await?, &root, path).await?;
            Ok(text(format!("created {path}")))
        }
        "delete" => {
            write_gate()?;
            device::delete(&udid_of(args).await?, &root, path).await?;
            Ok(text(format!("deleted {path}")))
        }
        "list_sync_jobs" => Ok(text(serde_json::to_string_pretty(&crate::sync::list_view())?)),
        "run_sync_job" => {
            let key = args["job"].as_str().ok_or_else(|| anyhow::anyhow!("job is required"))?;
            let job = crate::sync::find(key)?;
            let preview = args["preview"].as_bool().unwrap_or(false);
            if job.writes_phone() && !preview {
                write_gate()?;
            }
            // The first run copies everything and needs the user's preview, as with auto runs.
            if !preview && crate::sync::load_state(&job.id).is_none() {
                anyhow::bail!("{} has never run. Ask the user to run it once in Hatch (Settings, Sync), after the preview. Use preview: true to count the files.", job.name);
            }
            // The job's own phone when it is connected, else the first phone.
            let devices = device::list_devices().await?;
            let u = match &job.udid {
                Some(u) if devices.iter().any(|d| &d.udid == u) => u.clone(),
                _ => udid_of(args).await?,
            };
            if preview {
                let s = crate::sync::preview(&u, &job).await?;
                return Ok(text(format!("{}\n{}", s.line(), serde_json::to_string_pretty(&s)?)));
            }
            let t = device::Transfer::default();
            let r = crate::sync::run(&u, &job, &t).await?;
            Ok(text(format!("{}\n{}", r.line(), serde_json::to_string_pretty(&r)?)))
        }
        n if crate::photos::handles(n) => {
            let r = crate::photos::call(&udid_of(args).await?, n, args).await?;
            Ok(text(serde_json::to_string_pretty(&r)?))
        }
        _ => anyhow::bail!("unknown tool {name}"),
    }
}
