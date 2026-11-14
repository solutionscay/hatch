//! Demo mode for screenshots. `HATCH_DEMO=<folder>` serves a local folder as a fake iPhone:
//! `<folder>/media` is the media root, `<folder>/apps/<bundle id>` are app Documents folders,
//! and `<folder>/apps.txt` holds `bundle id<TAB>name` lines. Nothing talks to usbmuxd, and
//! every write is refused.

use crate::device::{App, Device, Entry, Root};
use crate::devinfo::DeviceInfo;
use anyhow::{Result, bail};
use std::path::PathBuf;

pub const UDID: &str = "00008110-DEMO0000DEMO0000";

pub fn dir() -> Option<PathBuf> {
    std::env::var_os("HATCH_DEMO").filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn on() -> bool {
    dir().is_some()
}

fn local(root: &Root, path: &str) -> PathBuf {
    let base = dir().unwrap_or_default();
    let base = match root {
        Root::Media => base.join("media"),
        Root::App(id) => base.join("apps").join(id),
    };
    let rel = path.trim_start_matches('/');
    if rel.split('/').any(|p| p == "..") { base } else { base.join(rel) }
}

pub fn devices() -> Vec<Device> {
    vec![Device { udid: UDID.into(), name: "Demo iPhone".into(), model: "iPhone15,2".into(), ios: "18.1".into(), paired: true }]
}

pub fn info() -> DeviceInfo {
    DeviceInfo {
        udid: UDID.into(),
        name: "Demo iPhone".into(),
        model: "iPhone15,2".into(),
        model_number: "MQ0X3LL/A".into(),
        ios: "18.1".into(),
        build: "22B83".into(),
        serial: "DEMO00000000".into(),
        battery_level: Some(76),
        charging: Some(true),
        external_power: Some(true),
        fully_charged: Some(false),
        storage_total: Some(128_000_000_000),
        storage_data_total: Some(119_000_000_000),
        storage_free: Some(41_300_000_000),
    }
}

pub fn apps() -> Vec<App> {
    let txt = std::fs::read_to_string(dir().unwrap_or_default().join("apps.txt")).unwrap_or_default();
    txt.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(id, name)| App { bundle_id: id.into(), name: name.into() })
        .collect()
}

pub fn list_dir(root: &Root, path: &str) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(local(root, path))? {
        let e = e?;
        let m = e.metadata()?;
        let modified = m
            .modified()
            .ok()
            .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        out.push(Entry { name: e.file_name().to_string_lossy().into(), is_dir: m.is_dir(), size: m.len(), modified });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

pub fn names(root: &Root, path: &str) -> Option<Vec<String>> {
    let rd = std::fs::read_dir(local(root, path)).ok()?;
    Some(rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into()).collect())
}

pub fn read_file(root: &Root, path: &str, max: usize) -> Result<(Vec<u8>, u64)> {
    let p = local(root, path);
    let total = std::fs::metadata(&p)?.len();
    let mut data = std::fs::read(&p)?;
    data.truncate(max);
    Ok((data, total))
}

pub fn refuse() -> Result<()> {
    bail!("Demo mode is read-only.")
}
