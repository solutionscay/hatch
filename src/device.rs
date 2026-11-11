//! iPhone access over USB (usbmuxd + AFC). Every call opens a fresh connection.

use anyhow::{Result, bail};
use idevice::{
    IdeviceError, IdeviceService,
    afc::{AfcClient, opcode::AfcFopenMode},
    house_arrest::HouseArrestClient,
    installation_proxy::InstallationProxyClient,
    lockdown::LockdownClient,
    provider::{IdeviceProvider, UsbmuxdProvider},
    usbmuxd::{UsbmuxdAddr, UsbmuxdConnection},
};
use serde::Serialize;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Device {
    pub udid: String,
    pub name: String,
    pub model: String,
    pub ios: String,
    pub paired: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct App {
    pub bundle_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: String,
}

/// Where a path lives: the camera roll / media partition, or an app's Documents folder.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Root {
    Media,
    App(String),
}

impl Root {
    pub fn parse(s: &str) -> Root {
        match s {
            "" | "media" | "photos" => Root::Media,
            other => Root::App(other.to_string()),
        }
    }
}

fn info_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, Device>> {
    static C: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Device>>> =
        std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// Marks a failure to reach the usbmuxd socket, so the UI can show setup steps.
#[derive(Debug)]
pub struct MuxDown;

impl std::fmt::Display for MuxDown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&usbmuxd_help())
    }
}

impl std::error::Error for MuxDown {}

/// What to do when the usbmuxd socket is not there: install it, or start the service.
pub fn usbmuxd_help() -> String {
    let installed = ["/usr/sbin/usbmuxd", "/usr/bin/usbmuxd", "/sbin/usbmuxd", "/usr/local/sbin/usbmuxd"]
        .iter()
        .any(|p| std::path::Path::new(p).exists());
    if installed {
        "Hatch cannot reach the usbmuxd service. The service is stopped.\n\
         Connect the iPhone with a USB cable. The service usually starts by itself.\n\
         If it does not start, run this command:\n\
         sudo systemctl start usbmuxd"
            .into()
    } else {
        "Hatch needs the usbmuxd service. It is not installed.\n\
         Install it with this command:\n\
         sudo apt install usbmuxd\n\
         Then connect the iPhone again."
            .into()
    }
}

async fn mux() -> Result<UsbmuxdConnection> {
    UsbmuxdConnection::default().await.map_err(|_| anyhow::Error::new(MuxDown))
}

/// Turn an error into a short message that tells the user what to do.
pub fn explain(e: &anyhow::Error) -> String {
    for cause in e.chain() {
        if cause.is::<MuxDown>() {
            return cause.to_string();
        }
        if let Some(ie) = cause.downcast_ref::<IdeviceError>() {
            match ie {
                IdeviceError::PasswordProtected | IdeviceError::DeviceLocked => {
                    return "The iPhone is locked.\nUnlock it with the passcode. Then try again.".into();
                }
                IdeviceError::UserDeniedPairing => {
                    return "The iPhone does not trust this computer.\n\
                            Unplug the iPhone and connect it again.\n\
                            Click Trust this computer. Then tap Trust on the iPhone and enter the passcode."
                        .into();
                }
                IdeviceError::PairingDialogResponsePending => {
                    return "The iPhone shows the Trust dialog.\nTap Trust. Then enter the passcode.".into();
                }
                IdeviceError::InvalidHostID => {
                    return "This computer is not trusted.\nClick Trust this computer in the sidebar.".into();
                }
                IdeviceError::DeviceNotFound => {
                    return "The iPhone is not connected.\nCheck the USB cable. Then connect the iPhone again.".into();
                }
                _ => {}
            }
        }
    }
    format!("{e:#}")
}

pub(crate) async fn provider_for(udid: &str) -> Result<UsbmuxdProvider> {
    let addr = UsbmuxdAddr::default();
    let mut mux = mux().await?;
    let dev = mux.get_device(udid).await?;
    Ok(dev.to_provider(addr, "hatch"))
}

pub async fn list_devices() -> Result<Vec<Device>> {
    let mut mux = mux().await?;
    let devices = mux.get_devices().await?;
    let mut out = Vec::new();
    for d in devices {
        let mut dev = Device {
            udid: d.udid.clone(),
            name: "iPhone".into(),
            model: String::new(),
            ios: String::new(),
            paired: false,
        };
        let cache = info_cache();
        let cached = cache.lock().unwrap().get(&d.udid).cloned();
        if let Some(c) = cached {
            dev = c;
        } else if let Ok(p) = provider_for(&d.udid).await {
            if let Ok(info) = describe(&p).await {
                dev.name = info.0;
                dev.model = info.1;
                dev.ios = info.2;
                dev.paired = true;
                cache.lock().unwrap().insert(d.udid.clone(), dev.clone());
            }
        }
        out.push(dev);
    }
    Ok(out)
}

pub(crate) async fn session(p: &UsbmuxdProvider) -> Result<LockdownClient> {
    let mut lockdown = LockdownClient::connect(p).await?;
    let pf = p.get_pairing_file().await?;
    lockdown.start_session(&pf).await?;
    Ok(lockdown)
}

async fn describe(p: &UsbmuxdProvider) -> Result<(String, String, String)> {
    let mut l = session(p).await?;
    let get = async |l: &mut LockdownClient, k: &str| -> String {
        l.get_value(Some(k), None)
            .await
            .ok()
            .and_then(|v| v.as_string().map(str::to_string))
            .unwrap_or_default()
    };
    Ok((
        get(&mut l, "DeviceName").await,
        get(&mut l, "ProductType").await,
        get(&mut l, "ProductVersion").await,
    ))
}

/// Pair with the phone. The phone shows a "Trust This Computer?" dialog.
pub async fn pair(udid: &str) -> Result<()> {
    let p = provider_for(udid).await?;
    let mut mux = mux().await?;
    let buid = mux.get_buid().await?;
    let mut lockdown = LockdownClient::connect(&p).await?;
    let host_id = uuid::Uuid::new_v4().to_string().to_uppercase();
    let record = lockdown.pair(host_id, buid, Some("Hatch")).await?;
    let bytes = record.serialize()?;
    mux.save_pair_record(udid, bytes).await?;
    info_cache().lock().unwrap().remove(udid);
    Ok(())
}

pub async fn list_apps(udid: &str) -> Result<Vec<App>> {
    let p = provider_for(udid).await?;
    let mut ip = InstallationProxyClient::connect(&p).await?;
    let apps = ip.get_apps(Some("User"), None).await?;
    let mut out: Vec<App> = apps
        .into_iter()
        .filter(|(_, v)| {
            v.as_dictionary()
                .and_then(|d| d.get("UIFileSharingEnabled"))
                .and_then(|x| x.as_boolean())
                .unwrap_or(false)
        })
        .map(|(id, v)| {
            let name = v
                .as_dictionary()
                .and_then(|d| d.get("CFBundleDisplayName").or(d.get("CFBundleName")))
                .and_then(|x| x.as_string())
                .unwrap_or(&id)
                .to_string();
            App { bundle_id: id, name }
        })
        .collect();
    out.sort_by_key(|a| a.name.to_lowercase());
    Ok(out)
}

type Key = (String, Root);
static POOL: std::sync::OnceLock<tokio::sync::Mutex<std::collections::HashMap<Key, AfcClient>>> =
    std::sync::OnceLock::new();

fn pool() -> &'static tokio::sync::Mutex<std::collections::HashMap<Key, AfcClient>> {
    POOL.get_or_init(Default::default)
}

/// Drop pooled connections (device unplugged or replugged).
pub async fn forget_connections() {
    pool().lock().await.clear();
}

async fn connect(udid: &str, root: &Root) -> Result<AfcClient> {
    let p = provider_for(udid).await?;
    Ok(match root {
        Root::Media => AfcClient::connect(&p).await?,
        Root::App(id) => HouseArrestClient::connect(&p)
            .await?
            .vend_documents(id.clone())
            .await?,
    })
}

/// Take a pooled connection (true) or open a new one (false).
async fn take(udid: &str, root: &Root, fresh: bool) -> Result<(AfcClient, bool)> {
    if !fresh {
        if let Some(c) = pool().lock().await.remove(&(udid.to_string(), root.clone())) {
            return Ok((c, true));
        }
    }
    Ok((connect(udid, root).await?, false))
}

/// Run `body` with an AFC client. Reuses a pooled connection and retries once on a fresh
/// one if the pooled connection went stale.
macro_rules! with_afc {
    ($udid:expr, $root:expr, |$c:ident| $body:expr) => {{
        let mut fresh = false;
        loop {
            let (mut $c, pooled) = take($udid, $root, fresh).await?;
            let r: Result<_> = async { $body }.await;
            match r {
                Ok(v) => {
                    pool().lock().await.insert(($udid.to_string(), $root.clone()), $c);
                    break v;
                }
                Err(_) if pooled => fresh = true,
                Err(e) => return Err(e),
            }
        }
    }};
}

/// App roots vend the Documents folder, which iOS exposes at /Documents.
fn real(root: &Root, path: &str) -> String {
    match root {
        Root::Media => path.to_string(),
        Root::App(_) => {
            let p = path.trim_end_matches('/');
            if p.is_empty() { "/Documents".into() } else { format!("/Documents{p}") }
        }
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir == "/" || dir.is_empty() {
        format!("/{name}")
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), name)
    }
}

pub async fn list_dir(udid: &str, root: &Root, path: &str) -> Result<Vec<Entry>> {
    let path = real(root, if path.is_empty() { "/" } else { path });
    let mut out = with_afc!(udid, root, |c| {
        let mut names = c.list_dir(path.as_str()).await?;
        names.retain(|n| n != "." && n != "..");
        let mut out = Vec::new();
        for n in names {
            out.push(match c.get_file_info(join(&path, &n)).await {
                Ok(i) => Entry {
                    is_dir: i.st_ifmt == "S_IFDIR",
                    size: i.size as u64,
                    modified: i.modified.format("%Y-%m-%d %H:%M").to_string(),
                    name: n,
                },
                Err(_) => Entry { name: n, is_dir: false, size: 0, modified: String::new() },
            });
        }
        Ok::<_, anyhow::Error>(out)
    });
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

/// What a folder holds, for "hide empty folders".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    Empty,
    /// Only names that start with a dot.
    DotOnly,
    Full,
}

type FillKey = (String, Root, String);

fn fill_cache() -> &'static std::sync::Mutex<std::collections::HashMap<FillKey, Fill>> {
    static C: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<FillKey, Fill>>> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

fn norm(path: &str) -> String {
    let p = path.trim_end_matches('/');
    if p.is_empty() { "/".into() } else { p.to_string() }
}

fn parent_of(path: &str) -> String {
    let n = norm(path);
    match n.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => n[..i].to_string(),
    }
}

/// The cached fill of a folder (root-relative path), if it was checked.
pub fn cached_fill(udid: &str, root: &Root, path: &str) -> Option<Fill> {
    fill_cache().lock().unwrap().get(&(udid.to_string(), root.clone(), norm(path))).copied()
}

/// Record the fill of a folder from a listing the UI already has.
pub fn note_fill(udid: &str, root: &Root, path: &str, names: &[String]) {
    let f = if names.is_empty() {
        Fill::Empty
    } else if names.iter().all(|n| n.starts_with('.')) {
        Fill::DotOnly
    } else {
        Fill::Full
    };
    fill_cache().lock().unwrap().insert((udid.to_string(), root.clone(), norm(path)), f);
}

/// Drop the cached fill of every direct child of `dir` (the user reloaded `dir`).
pub fn forget_children(udid: &str, root: &Root, dir: &str) {
    let dir = norm(dir);
    fill_cache().lock().unwrap().retain(|(u, r, p), _| !(u == udid && r == root && p != "/" && parent_of(p) == dir));
}

/// A write at `path` changes what `path` and its parent hold.
fn touched(udid: &str, root: &Root, path: &str) {
    let mut c = fill_cache().lock().unwrap();
    for p in [norm(path), parent_of(path)] {
        c.remove(&(udid.to_string(), root.clone(), p));
    }
}

/// List each folder (root-relative paths) on the pooled connection and cache its fill.
/// A folder that cannot be listed is left out, so it stays visible.
pub async fn check_fills(udid: &str, root: &Root, paths: &[String]) -> Result<Vec<(String, Fill)>> {
    let out = with_afc!(udid, root, |c| {
        let mut out = Vec::new();
        for p in paths {
            let Ok(mut names) = c.list_dir(real(root, p).as_str()).await else { continue };
            names.retain(|n| n != "." && n != "..");
            out.push((p.clone(), names));
        }
        Ok::<_, anyhow::Error>(out)
    });
    Ok(out
        .into_iter()
        .map(|(p, names)| {
            note_fill(udid, root, &p, &names);
            let f = cached_fill(udid, root, &p).unwrap_or(Fill::Full);
            (p, f)
        })
        .collect())
}

/// Read at most `max` bytes from the start of the file. Returns the bytes and the file size.
async fn read_head(c: &mut AfcClient, path: &str, max: usize) -> Result<(Vec<u8>, u64)> {
    let info = c.get_file_info(path).await?;
    if info.st_ifmt == "S_IFDIR" {
        bail!("{path} is a directory");
    }
    let total = info.size as u64;
    // Never ask for more than the file holds: read_n allocates `n` bytes up front.
    let want = max.min(info.size);
    if want > 256 * 1024 * 1024 {
        bail!("{path} is {total} bytes. Too large to read in one piece.");
    }
    let mut f = c.open(path, AfcFopenMode::RdOnly).await?;
    let read = f.read_n(want).await;
    f.close().await.ok();
    Ok((read?, total))
}

/// Read a file. `max` caps the number of bytes read from the phone and returned.
pub async fn read_file(udid: &str, root: &Root, path: &str, max: usize) -> Result<(Vec<u8>, u64)> {
    let path = real(root, path);
    Ok(with_afc!(udid, root, |c| read_head(&mut c, &path, max).await))
}

/// A JPEG preview for a photo or video. Uses the phone's own thumbnail cache for DCIM
/// files (works for HEIC and video), else reads small image files directly.
pub async fn preview_image(udid: &str, root: &Root, path: &str) -> Result<Vec<u8>> {
    if *root == Root::Media {
        if let Some(rest) = path.strip_prefix("/DCIM/") {
            let dir = format!("/PhotoData/Thumbnails/V2/DCIM/{rest}");
            let thumb = with_afc!(udid, root, |c| {
                let mut found = None;
                if let Ok(names) = c.list_dir(dir.as_str()).await {
                    let mut best: Option<(usize, String)> = None;
                    for n in names.into_iter().filter(|n| n.to_lowercase().ends_with(".jpg")) {
                        let full = format!("{dir}/{n}");
                        if let Ok(i) = c.get_file_info(full.as_str()).await {
                            if best.as_ref().is_none_or(|b| i.size > b.0) {
                                best = Some((i.size, full));
                            }
                        }
                    }
                    if let Some((_, full)) = best {
                        let mut f = c.open(full.as_str(), AfcFopenMode::RdOnly).await?;
                        let read = f.read_entire().await;
                        f.close().await.ok();
                        found = Some(read?);
                    }
                }
                Ok::<_, anyhow::Error>(found)
            });
            if let Some(t) = thumb {
                return Ok(t);
            }
        }
    }
    let (data, total) = read_file(udid, root, path, 20 * 1024 * 1024).await?;
    if total > 20 * 1024 * 1024 {
        bail!("image is {total} bytes, too large to preview");
    }
    Ok(data)
}

pub async fn write_file(udid: &str, root: &Root, path: &str, data: &[u8]) -> Result<()> {
    let orig = path.to_string();
    touched(udid, root, path);
    let path = real(root, path);
    with_afc!(udid, root, |c| {
        let mut f = c.open(path.as_str(), AfcFopenMode::WrOnly).await?;
        let w = f.write_entire(data).await;
        f.close().await.ok();
        w.map_err(anyhow::Error::from)
    });
    touched(udid, root, &orig);
    Ok(())
}

pub async fn make_dir(udid: &str, root: &Root, path: &str) -> Result<()> {
    let orig = path.to_string();
    touched(udid, root, path);
    let path = real(root, path);
    with_afc!(udid, root, |c| c.mk_dir(path.as_str()).await.map_err(anyhow::Error::from));
    touched(udid, root, &orig);
    Ok(())
}

pub async fn delete(udid: &str, root: &Root, path: &str) -> Result<()> {
    let orig = path.to_string();
    touched(udid, root, path);
    let path = real(root, path);
    with_afc!(udid, root, |c| c.remove_all(path.as_str()).await.map_err(anyhow::Error::from));
    touched(udid, root, &orig);
    Ok(())
}

/// Progress and cancel state for one batch of copies. The UI polls it; the copy loop
/// checks `cancel` between chunks.
#[derive(Default)]
pub struct Transfer {
    pub cancel: std::sync::atomic::AtomicBool,
    /// Bytes copied in the whole batch.
    pub done: std::sync::atomic::AtomicU64,
    /// Bytes in the whole batch.
    pub total: std::sync::atomic::AtomicU64,
    /// Files finished.
    pub files_done: std::sync::atomic::AtomicUsize,
    /// Files in the whole batch, when the caller does not know it up front (sync jobs).
    pub files_total: std::sync::atomic::AtomicUsize,
    /// Name of the file in progress.
    pub current: std::sync::Mutex<String>,
}

/// Marks a transfer the user stopped.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

impl Transfer {
    fn check(&self) -> Result<()> {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(anyhow::Error::new(Cancelled));
        }
        Ok(())
    }

    fn add(&self, n: u64) {
        self.done.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }

    fn start(&self, name: &str) {
        *self.current.lock().unwrap() = name.to_string();
    }
}

/// AFC moves at most 1 MiB per packet.
const CHUNK: usize = 1024 * 1024;

fn leaf(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// Copy one phone file to `dest` in 1 MiB pieces. Writes `dest.part`, then renames it,
/// so a stopped or failed copy never leaves a short file under the real name.
async fn copy_out(c: &mut AfcClient, path: &str, dest: &std::path::Path, t: &Transfer) -> Result<u64> {
    use tokio::io::AsyncWriteExt;
    let info = c.get_file_info(path).await?;
    if info.st_ifmt == "S_IFDIR" {
        bail!("{path} is a directory");
    }
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let mut out = tokio::fs::File::create(&part).await?;
    let mut f = c.open(path, AfcFopenMode::RdOnly).await?;
    let mut copied = 0u64;
    let res: Result<()> = async {
        loop {
            t.check()?;
            let buf = f.read_n(CHUNK).await?;
            if buf.is_empty() {
                break;
            }
            out.write_all(&buf).await?;
            copied += buf.len() as u64;
            t.add(buf.len() as u64);
        }
        out.flush().await?;
        Ok(())
    }
    .await;
    f.close().await.ok();
    drop(out);
    match res {
        Ok(()) => {
            tokio::fs::rename(&part, dest).await?;
            // Keep the phone's modified time, so a later sync sees both copies as equal.
            if let Some(ns) = info.modified.and_utc().timestamp_nanos_opt() {
                let when = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ns.max(0) as u64);
                if let Ok(f) = std::fs::File::options().write(true).open(dest) {
                    f.set_modified(when).ok();
                }
            }
            Ok(copied)
        }
        Err(e) => {
            tokio::fs::remove_file(&part).await.ok();
            Err(e)
        }
    }
}

/// Copy phone files (`remote` path, local `dest` file) to this computer, one after another,
/// on a connection of its own. Stops at the first error or on cancel.
pub async fn download(udid: &str, root: &Root, files: &[(String, std::path::PathBuf)], t: &Transfer) -> Result<u64> {
    let mut c = connect(udid, root).await?;
    let mut total = 0u64;
    for (remote, _) in files {
        if let Ok(i) = c.get_file_info(real(root, remote)).await {
            total += i.size as u64;
        }
    }
    t.total.fetch_add(total, std::sync::atomic::Ordering::Relaxed);
    let mut copied = 0;
    for (remote, dest) in files {
        t.check()?;
        t.start(leaf(remote));
        copied += copy_out(&mut c, &real(root, remote), dest, t).await?;
        t.files_done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(copied)
}

/// `name`, or `name (2).ext`, `name (3).ext`… whichever `taken` says is free.
pub fn free_name(name: &str, mut taken: impl FnMut(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|c| !taken(c))
        .expect("an unused name")
}

/// Copy one local file to the phone path `remote` in 1 MiB pieces. Writes `remote.part`,
/// then renames it (AFC rename replaces a file under the final name), so a stopped copy or a
/// pulled cable never leaves a short file under the real name. Sets the phone file's modified
/// time to the local one. Returns the bytes copied.
async fn copy_in(c: &mut AfcClient, local: &std::path::Path, remote: &str, t: &Transfer) -> Result<u64> {
    use tokio::io::AsyncReadExt;
    let mut src = tokio::fs::File::open(local).await?;
    let mtime = src.metadata().await.ok().and_then(|m| m.modified().ok()).and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok());
    let part = format!("{remote}.part");
    let mut f = c.open(part.as_str(), AfcFopenMode::WrOnly).await?;
    let mut buf = vec![0u8; CHUNK];
    let mut copied = 0u64;
    let res: Result<()> = async {
        loop {
            t.check()?;
            let n = src.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            f.write_entire(&buf[..n]).await?;
            copied += n as u64;
            t.add(n as u64);
        }
        Ok(())
    }
    .await;
    f.close().await.ok();
    let res = match res {
        Ok(()) => match c.rename(part.as_str(), remote).await {
            Ok(()) => Ok(()),
            // If AFC will not rename over a file, remove the old file first. The complete
            // copy is already in place under .part, so this never leaves a short file.
            Err(_) => {
                c.remove(remote).await.ok();
                c.rename(part.as_str(), remote).await.map_err(anyhow::Error::from)
            }
        },
        Err(e) => Err(e),
    };
    if let Err(e) = res {
        c.remove(part.as_str()).await.ok();
        return Err(e);
    }
    if let Some(d) = mtime {
        // Older iOS versions may not support this. The sync state records what the phone reports.
        c.set_file_time(remote, d.as_nanos() as u64).await.ok();
    }
    Ok(copied)
}

/// Copy local files into the phone folder `dir`. A name that is already there gets a
/// " (2)" suffix; nothing on the phone is replaced. Returns the phone paths written.
/// The user always may upload. The MCP "Agent writes" gate does not apply here.
pub async fn upload(udid: &str, root: &Root, dir: &str, files: &[std::path::PathBuf], t: &Transfer) -> Result<Vec<String>> {
    touched(udid, root, dir);
    let mut c = connect(udid, root).await?;
    let total: u64 = files.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
    t.total.fetch_add(total, std::sync::atomic::Ordering::Relaxed);
    let rdir = real(root, dir);
    let mut names: std::collections::HashSet<String> = c.list_dir(rdir.as_str()).await?.into_iter().collect();
    let mut written = Vec::new();
    for local in files {
        t.check()?;
        let Some(name) = local.file_name().and_then(|n| n.to_str()) else {
            bail!("{} has a name the iPhone cannot store", local.display());
        };
        let name = free_name(name, |n| names.contains(n));
        t.start(&name);
        copy_in(&mut c, local, &join(&rdir, &name), t).await?;
        names.insert(name.clone());
        written.push(join(dir, &name));
        t.files_done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    touched(udid, root, dir);
    Ok(written)
}

/// Size and modified time (ns since 1970) of one file, as the sync engine compares them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stat {
    pub size: u64,
    pub mtime: i64,
}

/// Raw st_size and st_mtime (ns) from AFC. None for folders and other non-files.
async fn stat_of(c: &mut AfcClient, path: &str) -> Result<Option<Stat>> {
    let kv = c.get_file_info_raw(path).await?;
    if kv.get("st_ifmt").map(String::as_str) != Some("S_IFREG") {
        return Ok(None);
    }
    let size = kv.get("st_size").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mtime = kv.get("st_mtime").and_then(|v| v.parse().ok()).unwrap_or(0);
    Ok(Some(Stat { size, mtime }))
}

/// Every file under `dir`, with paths relative to `dir` ("100APPLE/IMG_0001.HEIC").
/// `skip` gets each file or folder name and leaves it out when it returns true.
/// A missing `dir` gives an empty list.
pub async fn walk(udid: &str, root: &Root, dir: &str, skip: fn(&str) -> bool) -> Result<Vec<(String, Stat)>> {
    let base = real(root, dir);
    Ok(with_afc!(udid, root, |c| {
        let mut out = Vec::new();
        let mut todo = vec![String::new()];
        let mut first = true;
        while let Some(rel) = todo.pop() {
            let full = if rel.is_empty() { base.clone() } else { join(&base, &rel) };
            let names = match c.list_dir(full.as_str()).await {
                Ok(n) => n,
                Err(IdeviceError::Afc(idevice::afc::errors::AfcError::ObjectNotFound)) if first => break,
                Err(e) => return Err(e.into()),
            };
            first = false;
            for n in names {
                if n == "." || n == ".." || skip(&n) {
                    continue;
                }
                let r = if rel.is_empty() { n.clone() } else { format!("{rel}/{n}") };
                let kv = c.get_file_info_raw(join(&full, &n)).await?;
                match kv.get("st_ifmt").map(String::as_str) {
                    Some("S_IFDIR") => todo.push(r),
                    Some("S_IFREG") => {
                        let size = kv.get("st_size").and_then(|v| v.parse().ok()).unwrap_or(0);
                        let mtime = kv.get("st_mtime").and_then(|v| v.parse().ok()).unwrap_or(0);
                        out.push((r, Stat { size, mtime }));
                    }
                    _ => {} // links and other kinds: left out
                }
            }
        }
        Ok::<_, anyhow::Error>(out)
    }))
}

/// Copy local files to exact phone paths (`local`, phone path in the root), one after another,
/// on a connection of its own. Replaces a file that is already there, through `.part` and a
/// rename. Makes missing parent folders. Returns the phone size and time of each file written.
/// Refuses the media partition: sync jobs never write to DCIM or the camera roll.
pub async fn upload_to(udid: &str, root: &Root, files: &[(std::path::PathBuf, String)], t: &Transfer) -> Result<Vec<Stat>> {
    if *root == Root::Media {
        bail!("Hatch does not sync files into the media partition.");
    }
    let mut c = connect(udid, root).await?;
    let total: u64 = files.iter().filter_map(|(p, _)| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
    t.total.fetch_add(total, std::sync::atomic::Ordering::Relaxed);
    let mut made: std::collections::HashSet<String> = Default::default();
    let mut out = Vec::new();
    for (local, remote) in files {
        t.check()?;
        // Each new file can fill its folder and the folders above it.
        let mut p = remote.clone();
        while p != "/" && !p.is_empty() {
            touched(udid, root, &p);
            p = parent_of(&p);
        }
        let full = real(root, remote);
        // Make each missing parent folder, top down.
        let mut parent = String::new();
        let dirs: Vec<&str> = full.trim_start_matches('/').split('/').collect();
        for d in &dirs[..dirs.len().saturating_sub(1)] {
            parent = format!("{parent}/{d}");
            if made.insert(parent.clone()) {
                c.mk_dir(parent.as_str()).await.ok();
            }
        }
        t.start(leaf(remote));
        copy_in(&mut c, local, &full, t).await?;
        out.push(stat_of(&mut c, &full).await?.unwrap_or(Stat { size: 0, mtime: 0 }));
        t.files_done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(out)
}
