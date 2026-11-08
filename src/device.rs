//! iPhone access over USB (usbmuxd + AFC). Every call opens a fresh connection.

use anyhow::{Context, Result, anyhow, bail};
use idevice::{
    IdeviceService,
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

async fn provider_for(udid: &str) -> Result<UsbmuxdProvider> {
    let addr = UsbmuxdAddr::default();
    let mut mux = UsbmuxdConnection::default()
        .await
        .context("cannot reach usbmuxd (is the usbmuxd service running?)")?;
    let dev = mux
        .get_device(udid)
        .await
        .map_err(|_| anyhow!("device {udid} is not connected"))?;
    Ok(dev.to_provider(addr, "hatch"))
}

pub async fn list_devices() -> Result<Vec<Device>> {
    let mut mux = UsbmuxdConnection::default()
        .await
        .context("cannot reach usbmuxd (is the usbmuxd service running?)")?;
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

async fn session(p: &UsbmuxdProvider) -> Result<LockdownClient> {
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
    let mut mux = UsbmuxdConnection::default().await?;
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
    let path = real(root, path);
    with_afc!(udid, root, |c| {
        let mut f = c.open(path.as_str(), AfcFopenMode::WrOnly).await?;
        let w = f.write_entire(data).await;
        f.close().await.ok();
        w.map_err(anyhow::Error::from)
    });
    Ok(())
}

pub async fn make_dir(udid: &str, root: &Root, path: &str) -> Result<()> {
    let path = real(root, path);
    with_afc!(udid, root, |c| c.mk_dir(path.as_str()).await.map_err(anyhow::Error::from));
    Ok(())
}

pub async fn delete(udid: &str, root: &Root, path: &str) -> Result<()> {
    let path = real(root, path);
    with_afc!(udid, root, |c| c.remove_all(path.as_str()).await.map_err(anyhow::Error::from));
    Ok(())
}
