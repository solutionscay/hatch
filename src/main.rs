mod device;
mod mcp;
mod transfer;

use device::{App, Device, Entry, Root};
use gpui_kit::component::{Icon, IconName, Sizable, Theme, ThemeMode, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("tokio runtime"))
}

/// Run a future on the tokio runtime and hand the result back to the UI thread.
fn run<T, F>(cx: &mut Context<Hatch>, fut: F, done: impl FnOnce(&mut Hatch, T, &mut Context<Hatch>) + 'static)
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let handle = rt().spawn(fut);
    cx.spawn(async move |this, cx| {
        if let Ok(v) = handle.await {
            this.update(cx, |this, cx| done(this, v, cx)).ok();
        }
    })
    .detach();
}

enum Preview {
    Text(String),
    Image(Arc<gpui::Image>),
}

#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq)]
struct Fav {
    root: String,
    path: String,
    label: String,
}

fn fav_file() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join(".config/hatch/favorites.json")
}

fn load_favs() -> Vec<Fav> {
    std::fs::read_to_string(fav_file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_favs(f: &[Fav]) {
    let p = fav_file();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).ok();
    }
    if let Ok(t) = serde_json::to_string_pretty(f) {
        std::fs::write(p, t).ok();
    }
}

fn is_visual(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    ["jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "mov", "mp4", "m4v", "dng", "tiff", "bmp"].contains(&ext.as_str())
}

struct Hatch {
    grid: bool,
    favs: Vec<Fav>,
    epoch: u64,
    devices: Vec<Device>,
    udid: Option<String>,
    apps: Vec<App>,
    root: Root,
    path: String,
    entries: Vec<Entry>,
    selected: Option<String>,
    /// Files picked for a batch action (download). Names in the current folder.
    picked: std::collections::BTreeSet<String>,
    /// Index of the last plain or ctrl click, for shift-click ranges.
    anchor: Option<usize>,
    /// The running or last finished transfer.
    job: Option<transfer::Job>,
    show_log: bool,
    settings: Settings,
    page_settings: bool,
    copied: Option<String>,
    log: Arc<std::sync::Mutex<Vec<mcp::LogEntry>>>,
    preview: Option<Preview>,
    thumbs: std::collections::HashMap<String, Arc<gpui::Image>>,
    status: String,
    error: Option<String>,
    /// Set when the usbmuxd socket cannot be reached. Holds the setup steps.
    mux_down: Option<String>,
    /// True while the pair request waits for the user on the iPhone.
    pairing: bool,
    busy: bool,
    allow_writes: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    token: Arc<std::sync::RwLock<String>>,
}

impl Hatch {
    fn new(cx: &mut Context<Self>) -> Self {
        let allow_writes = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let log: Arc<std::sync::Mutex<Vec<mcp::LogEntry>>> = Default::default();
        let token = Arc::new(std::sync::RwLock::new(mcp::load_or_create_token()));
        let state = mcp::McpState { allow_writes: allow_writes.clone(), calls: calls.clone(), log: log.clone(), token: token.clone() };
        let mut error = None;
        // Bind synchronously so a port clash shows up in the UI.
        match std::net::TcpListener::bind(("127.0.0.1", mcp::PORT)) {
            Ok(l) => {
                drop(l);
                rt().spawn(async move {
                    if let Err(e) = mcp::serve(state).await {
                        eprintln!("mcp server stopped: {e:#}");
                    }
                });
            }
            Err(e) => error = Some(format!("MCP port {} unavailable: {e}", mcp::PORT)),
        }
        let this = Self {
            grid: load_settings().start_in_grid,
            settings: load_settings(),
            page_settings: false,
            copied: None,
            favs: load_favs(),
            epoch: 0,
            devices: vec![],
            udid: None,
            apps: vec![],
            root: Root::Media,
            path: "/".into(),
            entries: vec![],
            selected: None,
            picked: Default::default(),
            anchor: None,
            job: None,
            show_log: false,
            log,
            preview: None,
            thumbs: Default::default(),
            status: "Looking for an iPhone…".into(),
            error,
            mux_down: None,
            pairing: false,
            busy: false,
            allow_writes,
            calls,
            token,
        };
        // Poll for devices and keep the call counter fresh.
        cx.spawn(async move |this, cx| {
            loop {
                let devices = rt().spawn(device::list_devices()).await;
                let ok = this
                    .update(cx, |this, cx| {
                        // Re-render only on change: a full frame is costly on a software renderer.
                        let before = (this.devices.clone(), this.status.clone());
                        match devices {
                            Ok(Ok(d)) => {
                                this.mux_down = None;
                                this.set_devices(d, cx);
                            }
                            Ok(Err(e)) if e.is::<device::MuxDown>() => {
                                this.mux_down = Some(device::explain(&e));
                                this.set_devices(vec![], cx);
                            }
                            Ok(Err(e)) => this.status = device::explain(&e),
                            Err(_) => {}
                        }
                        if before != (this.devices.clone(), this.status.clone()) {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !ok {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(3)).await;
            }
        })
        .detach();
        let calls2 = this.calls.clone();
        cx.spawn(async move |this, cx| {
            let mut seen = 0;
            loop {
                cx.background_executor().timer(Duration::from_millis(400)).await;
                let n = calls2.load(Ordering::Relaxed);
                if n != seen {
                    seen = n;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }
        })
        .detach();
        this
    }

    fn set_devices(&mut self, devices: Vec<Device>, cx: &mut Context<Self>) {
        let first_seen = self.devices.is_empty() && !devices.is_empty();
        if first_seen || devices.is_empty() {
            rt().spawn(device::forget_connections());
        }
        self.devices = devices;
        let still_there = self.udid.as_ref().is_some_and(|u| self.devices.iter().any(|d| &d.udid == u));
        if !still_there {
            self.udid = self.devices.first().map(|d| d.udid.clone());
            self.entries.clear();
            self.apps.clear();
            self.preview = None;
        }
        if self.devices.is_empty() {
            self.status = "Connect an iPhone with a USB cable".into();
        } else if first_seen || (!still_there && self.udid.is_some()) {
            if self.current().is_some_and(|d| d.paired) {
                self.open_root(Root::Media, cx);
                self.load_apps(cx);
            }
        }
    }

    fn root_key(&self) -> String {
        match &self.root { Root::Media => "media:".into(), Root::App(id) => format!("{id}:") }
    }

    fn current(&self) -> Option<&Device> {
        self.devices.iter().find(|d| Some(&d.udid) == self.udid.as_ref())
    }

    fn pair(&mut self, cx: &mut Context<Self>) {
        let Some(udid) = self.udid.clone() else { return };
        self.status = "Unlock the iPhone and tap Trust…".into();
        self.error = None;
        self.pairing = true;
        cx.notify();
        run(cx, async move { device::pair(&udid).await.map_err(|e| device::explain(&e)) }, |this, r, cx| {
            this.pairing = false;
            match r {
                Ok(()) => {
                    this.status = "Paired".into();
                    this.devices.clear(); // forces a refresh and initial load
                }
                Err(e) => this.error = Some(e),
            }
            cx.notify();
        });
    }

    fn load_apps(&mut self, cx: &mut Context<Self>) {
        let Some(udid) = self.udid.clone() else { return };
        run(cx, async move { device::list_apps(&udid).await.map_err(|e| device::explain(&e)) }, |this, r, cx| {
            match r {
                Ok(a) => this.apps = a,
                Err(e) => this.error = Some(format!("Apps: {e}")),
            }
            cx.notify();
        });
    }

    fn full(&self, name: &str) -> String {
        if self.path == "/" { format!("/{name}") } else { format!("{}/{}", self.path, name) }
    }

    fn root_str(&self) -> String {
        match &self.root { Root::Media => "media".into(), Root::App(id) => id.clone() }
    }

    fn is_fav(&self, root: &str, path: &str) -> bool {
        self.favs.iter().any(|f| f.root == root && f.path == path)
    }

    fn toggle_fav(&mut self, root: String, path: String, cx: &mut Context<Self>) {
        if let Some(i) = self.favs.iter().position(|f| f.root == root && f.path == path) {
            self.favs.remove(i);
        } else {
            let leaf = path.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
            let label = if leaf.is_empty() {
                match self.apps.iter().find(|a| a.bundle_id == root) { Some(a) => a.name.clone(), None => "Media".into() }
            } else { leaf };
            self.favs.push(Fav { root, path, label });
        }
        save_favs(&self.favs);
        cx.notify();
    }

    fn open_fav(&mut self, f: Fav, cx: &mut Context<Self>) {
        self.page_settings = false;
        self.root = Root::parse(&f.root);
        self.path = f.path;
        self.load_dir(cx);
    }

    /// Fetch thumbnails for the current folder in order, one at a time. A newer folder load cancels it.
    fn load_thumbs(&mut self, cx: &mut Context<Self>) {
        let Some(udid) = self.udid.clone() else { return };
        self.epoch += 1;
        let epoch = self.epoch;
        let root = self.root.clone();
        let rk = self.root_key();
        let todo: Vec<String> = self
            .entries
            .iter()
            .filter(|e| !e.is_dir && is_visual(&e.name))
            .map(|e| self.full(&e.name))
            .filter(|p| !self.thumbs.contains_key(&format!("{rk}{p}")))
            .collect();
        cx.spawn(async move |this, cx| {
            for p in todo {
                let (u, r, pp) = (udid.clone(), root.clone(), p.clone());
                let res = rt().spawn(async move { device::preview_image(&u, &r, &pp).await }).await;
                let key = format!("{rk}{p}");
                let alive = this
                    .update(cx, |this, cx| {
                        if this.epoch != epoch {
                            return false;
                        }
                        if let Ok(Ok(bytes)) = res {
                            this.thumbs.insert(key, Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Jpeg, bytes)));
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn location_visible(&self, key: &str) -> bool {
        !self.settings.hidden.iter().any(|h| h == key)
    }

    fn set_location_visible(&mut self, key: &str, visible: bool, cx: &mut Context<Self>) {
        self.settings.hidden.retain(|h| h != key);
        if !visible {
            self.settings.hidden.push(key.to_string());
        }
        save_settings(&self.settings);
        cx.notify();
    }

    fn rotate_token(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = mcp::rotate_token(&self.token) {
            self.error = Some(format!("Could not rotate the MCP token: {e:#}"));
        }
        self.copied = Some("rotated".into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1600)).await;
            this.update(cx, |this, cx| {
                this.copied = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn copy_text(&mut self, id: &str, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied = Some(id.to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1600)).await;
            this.update(cx, |this, cx| {
                this.copied = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_root(&mut self, root: Root, cx: &mut Context<Self>) {
        self.page_settings = false;
        self.root = root;
        self.path = "/".into();
        self.load_dir(cx);
    }

    fn load_dir(&mut self, cx: &mut Context<Self>) {
        let Some(udid) = self.udid.clone() else { return };
        let (root, path) = (self.root.clone(), self.path.clone());
        self.busy = true;
        self.epoch += 1;
        self.selected = None;
        self.picked.clear();
        self.anchor = None;
        self.preview = None;
        self.error = None;
        cx.notify();
        run(
            cx,
            async move { device::list_dir(&udid, &root, &path).await.map_err(|e| device::explain(&e)) },
            |this, r, cx| {
                this.busy = false;
                match r {
                    Ok(e) => {
                        this.status = format!("{} items", e.len());
                        this.entries = e;
                        if this.grid {
                            this.load_thumbs(cx);
                        }
                    }
                    Err(e) => {
                        this.entries.clear();
                        this.error = Some(e);
                    }
                }
                cx.notify();
            },
        );
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if self.path == "/" {
            return;
        }
        let up = match self.path.trim_end_matches('/').rfind('/') {
            Some(0) | None => "/".to_string(),
            Some(i) => self.path[..i].to_string(),
        };
        self.path = up;
        self.load_dir(cx);
    }

    fn click_entry(&mut self, e: Entry, cx: &mut Context<Self>) {
        let full = if self.path == "/" { format!("/{}", e.name) } else { format!("{}/{}", self.path, e.name) };
        if e.is_dir {
            self.path = full;
            self.load_dir(cx);
            return;
        }
        self.selected = Some(e.name.clone());
        self.preview = Some(Preview::Text("Loading…".into()));
        cx.notify();
        let Some(udid) = self.udid.clone() else { return };
        let root = self.root.clone();
        let name = e.name.clone();
        let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
        let visual = ["jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "mov", "mp4", "m4v", "dng", "tiff", "bmp"].contains(&ext.as_str());
        if visual {
            let key = format!("{}{}", self.root_key(), full);
            if let Some(img) = self.thumbs.get(&key) {
                self.preview = Some(Preview::Image(img.clone()));
                return;
            }
            run(
                cx,
                async move { device::preview_image(&udid, &root, &full).await.map_err(|e| device::explain(&e)) },
                move |this, r, cx| {
                    if this.selected.as_deref() == Some(&name) {
                        this.preview = Some(match r {
                            Ok(bytes) => {
                                let img = Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Jpeg, bytes));
                                this.thumbs.insert(key, img.clone());
                                Preview::Image(img)
                            }
                            Err(e) => Preview::Text(format!("No preview: {e}")),
                        });
                    }
                    cx.notify();
                },
            );
            return;
        }
        if e.size > 1024 * 1024 {
            self.preview = Some(Preview::Text(format!("{} · too large for text preview", human(e.size))));
            return;
        }
        run(
            cx,
            async move { device::read_file(&udid, &root, &full, 64 * 1024).await.map_err(|e| device::explain(&e)) },
            move |this, r, cx| {
                if this.selected.as_deref() == Some(&name) {
                    this.preview = Some(Preview::Text(match r {
                        Ok((data, total)) => match String::from_utf8(data) {
                            Ok(s) if total as usize > s.len() => format!("{s}\n\n… ({total} bytes total)"),
                            Ok(s) => s,
                            Err(_) => format!("Binary file, {total} bytes. No text preview."),
                        },
                        Err(e) => format!("Cannot read: {e}"),
                    }));
                }
                cx.notify();
            },
        );
    }
}

// Glass palette: near-black translucent ground, warm greys, one soft emerald accent.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    /// Location keys hidden from the sidebar: "media" or an app bundle id.
    hidden: Vec<String>,
    start_in_grid: bool,
}

fn settings_file() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join(".config/hatch/settings.json")
}

fn load_settings() -> Settings {
    std::fs::read_to_string(settings_file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_settings(s: &Settings) {
    let p = settings_file();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).ok();
    }
    if let Ok(t) = serde_json::to_string_pretty(s) {
        std::fs::write(p, t).ok();
    }
}

fn ink() -> Rgba { rgba(0xe9e7e2ff) }
fn dim() -> Rgba { rgba(0xa9a69eff) }
fn faint() -> Rgba { rgba(0x8a877fff) }
fn acc() -> Rgba { rgba(0x5fd4a0ff) }
fn acc_s() -> Rgba { rgba(0x5fd4a01f) }
fn amb() -> Rgba { rgba(0xe0b062ff) }
fn amb_s() -> Rgba { rgba(0xe0b0621f) }
fn cor() -> Rgba { rgba(0xe48b81ff) }
fn cor_s() -> Rgba { rgba(0xe48b811f) }
fn line() -> Rgba { rgba(0xffffff14) }
fn wash() -> Rgba { rgba(0xffffff0a) }
fn wash2() -> Rgba { rgba(0xffffff14) }
fn ground() -> Rgba { rgba(0x101315cc) }
fn panel() -> Rgba { rgba(0x181b1d66) }
fn none() -> Rgba { rgba(0x00000000) }

fn mono() -> &'static str {
    static F: OnceLock<String> = OnceLock::new();
    F.get_or_init(|| pick_font(&["Geist Mono", "JetBrains Mono", "DejaVu Sans Mono", "Noto Sans Mono"], "monospace"))
}

fn sans() -> &'static str {
    static F: OnceLock<String> = OnceLock::new();
    F.get_or_init(|| pick_font(&["Geist", "Inter", "Instrument Sans", "Noto Sans", "DejaVu Sans"], "sans-serif"))
}

fn pick_font(cands: &[&str], fallback: &str) -> String {
    let list = std::process::Command::new("fc-list").arg(":").arg("family").output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    for f in cands {
        if list.lines().any(|l| l.split(',').any(|n| n.trim() == *f)) {
            return f.to_string();
        }
    }
    fallback.to_string()
}

fn human(n: u64) -> String {
    let mut v = n as f64;
    for u in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 {
            return if u == "B" { format!("{n} B") } else { format!("{v:.1} {u}") };
        }
        v /= 1024.0;
    }
    format!("{v:.1} TB")
}

fn ext_of(name: &str) -> String {
    name.rsplit('.').next().unwrap_or("").to_lowercase()
}

fn kind_of(e: &Entry) -> String {
    if e.is_dir {
        return "Folder".into();
    }
    match ext_of(&e.name).as_str() {
        "heic" | "heif" => "HEIC image".into(),
        "jpg" | "jpeg" => "JPEG image".into(),
        "png" => "PNG image".into(),
        "mov" | "mp4" | "m4v" => "Video".into(),
        x if x == e.name.to_lowercase() => "File".into(),
        x => format!("{} file", x.to_uppercase()),
    }
}

fn is_video(name: &str) -> bool {
    matches!(ext_of(name).as_str(), "mov" | "mp4" | "m4v")
}

/// Tiny uppercase section label.
fn section(label: &str) -> Div {
    div().px_4().pt_4().pb_2().text_size(px(10.)).text_color(faint()).font_weight(FontWeight::MEDIUM).child(label.to_uppercase())
}

fn kv(label: &'static str, value: String, mono_val: bool) -> Div {
    h_flex()
        .h(px(24.))
        .gap_2()
        .child(div().w(px(72.)).flex_none().text_color(faint()).child(label))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(ink())
                .when(mono_val, |d| d.font_family(mono()).text_size(px(12.)))
                .child(value),
        )
}

fn switch(on: bool, color: Rgba, tint: Rgba, edge: Rgba) -> Div {
    div()
        .w(px(34.))
        .h(px(18.))
        .flex_none()
        .flex()
        .items_center()
        .px(px(2.))
        .border_1()
        .border_color(if on { edge } else { line() })
        .bg(if on { tint } else { wash() })
        .when(on, |d| d.justify_end())
        .child(div().size(px(12.)).bg(if on { color } else { faint() }))
}

impl Hatch {
    fn settings_row(
        &self,
        id: String,
        icon: IconName,
        title: String,
        sub: Option<String>,
        on: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Hatch, bool, &mut Context<Hatch>) + 'static,
    ) -> impl IntoElement {
        let me = cx.entity();
        h_flex()
            .id(SharedString::from(id))
            .h(px(52.))
            .flex_none()
            .px_6()
            .gap_3()
            .items_center()
            .cursor_pointer()
            .border_b_1()
            .border_color(line())
            .hover(|d| d.bg(wash()))
            .child(div().size(px(28.)).flex_none().flex().items_center().justify_center().bg(wash()).child(Icon::new(icon).small().text_color(if on { acc() } else { faint() })))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().text_color(if on { ink() } else { dim() }).child(title))
                    .when_some(sub, |d, s| d.child(div().truncate().font_family(mono()).text_size(px(11.)).text_color(faint()).child(s))),
            )
            .child(switch(on, acc(), acc_s(), rgba(0x5fd4a057)))
            .on_click(move |_, _, cx| me.update(cx, |this, cx| f(this, !on, cx)))
    }

    fn settings_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // pb_4 matches the pt_4 above each section header, so the last section does not touch the panel edge.
        let mut page = v_flex().w_full().pb_4();
        page = page.child(section("Locations"));
        page = page.child(div().px_6().pb_3().text_size(px(12.)).text_color(dim()).child("Choose which locations appear in the sidebar. Hidden locations still work for the agent."));
        page = page.child(self.settings_row(
            "set-media".into(),
            IconName::Folder,
            "Media (all)".into(),
            Some("/var/mobile/Media".into()),
            self.location_visible("media"),
            cx,
            |this, v, cx| this.set_location_visible("media", v, cx),
        ));
        for a in self.apps.clone() {
            let key = a.bundle_id.clone();
            page = page.child(self.settings_row(
                format!("set-{key}"),
                IconName::Folder,
                a.name.clone(),
                Some(a.bundle_id.clone()),
                self.location_visible(&a.bundle_id),
                cx,
                move |this, v, cx| this.set_location_visible(&key, v, cx),
            ));
        }
        if self.apps.is_empty() {
            page = page.child(div().px_6().py_3().text_size(px(12.)).text_color(faint()).child("App folders appear here once an iPhone is connected and trusted."));
        }
        page = page.child(section("Browsing"));
        page = page.child(self.settings_row(
            "set-grid".into(),
            IconName::LayoutDashboard,
            "Start in grid view".into(),
            Some("Photos and videos show thumbnails".into()),
            self.settings.start_in_grid,
            cx,
            |this, v, cx| {
                this.settings.start_in_grid = v;
                save_settings(&this.settings);
                cx.notify();
            },
        ));
        page = page.child(section("Connect an agent"));
        page = page.child(div().px_6().pb_3().text_size(px(12.)).text_color(dim()).child("Hatch must be running. Run one of these once, then ask your agent to list the iPhone's files. Each command holds the access token. After you rotate the token, run the command again."));
        let url = format!("http://127.0.0.1:{}/mcp", mcp::PORT);
        let token = self.token.read().unwrap().clone();
        let auth = format!("Authorization: Bearer {token}");
        let codex_cfg = "\"${CODEX_HOME:-$HOME/.codex}/config.toml\"";
        let snippets: Vec<(&str, &str, String)> = vec![
            ("cc", "Claude Code", format!("claude mcp remove hatch >/dev/null 2>&1; claude mcp add --transport http hatch {url} --header \"{auth}\"")),
            (
                "codex",
                "Codex",
                format!("codex mcp remove hatch >/dev/null 2>&1; printf '\\n[mcp_servers.hatch]\\nurl = \"{url}\"\\nhttp_headers = {{ Authorization = \"Bearer {token}\" }}\\n' >> {codex_cfg}"),
            ),
            ("gemini", "Gemini CLI", format!("gemini mcp add --scope user --transport http hatch {url} --header \"{auth}\"")),
            ("grok", "Grok", format!("grok mcp add --transport http hatch {url} --header \"{auth}\"")),
            (
                "json",
                "Other clients (JSON)",
                format!("{{\"mcpServers\":{{\"hatch\":{{\"type\":\"http\",\"url\":\"{url}\",\"headers\":{{\"Authorization\":\"Bearer {token}\"}}}}}}}}"),
            ),
            ("url", "Endpoint URL", url.clone()),
            ("token", "Access token", token.clone()),
        ];
        for (id, label, text) in snippets {
            let me = cx.entity();
            let done = self.copied.as_deref() == Some(id);
            let t2 = text.clone();
            page = page.child(
                v_flex()
                    .px_6()
                    .py_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(line())
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(div().text_color(ink()).child(label))
                            .child(h_flex().gap_2().when(id == "token", |d| {
                                let me = cx.entity();
                                let rotated = self.copied.as_deref() == Some("rotated");
                                d.child(
                                    div()
                                        .id("rotate-token")
                                        .h(px(26.))
                                        .px_3()
                                        .flex()
                                        .items_center()
                                        .cursor_pointer()
                                        .border_1()
                                        .border_color(if rotated { rgba(0x5fd4a057) } else { line() })
                                        .bg(if rotated { acc_s() } else { wash() })
                                        .text_size(px(12.))
                                        .text_color(if rotated { acc() } else { dim() })
                                        .hover(|d| d.bg(wash2()))
                                        .child(if rotated { "Rotated" } else { "Rotate" })
                                        .on_click(move |_, _, cx| me.update(cx, |this, cx| this.rotate_token(cx))),
                                )
                            }).child(
                                div()
                                    .id(SharedString::from(format!("copy-{id}")))
                                    .h(px(26.))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .cursor_pointer()
                                    .border_1()
                                    .border_color(if done { rgba(0x5fd4a057) } else { line() })
                                    .bg(if done { acc_s() } else { wash() })
                                    .text_size(px(12.))
                                    .text_color(if done { acc() } else { dim() })
                                    .hover(|d| d.bg(wash2()))
                                    .child(if done { "Copied" } else { "Copy" })
                                    .on_click(move |_, _, cx| {
                                        let t = t2.clone();
                                        me.update(cx, |this, cx| this.copy_text(id, t, cx))
                                    }),
                            )),
                    )
                    .child(
                        div()
                            .p_2()
                            .bg(rgba(0x00000033))
                            .border_1()
                            .border_color(line())
                            .font_family(mono())
                            .text_size(px(11.))
                            .text_color(dim())
                            .child(text),
                    ),
            );
        }
        page = page.child(section("Agent"));
        let writes = self.allow_writes.load(Ordering::Relaxed);
        page = page.child(
            h_flex()
                .h(px(52.))
                .px_6()
                .gap_3()
                .items_center()
                .border_b_1()
                .border_color(line())
                .child(div().size(px(28.)).flex_none().flex().items_center().justify_center().bg(wash()).child(Icon::new(IconName::Bot).small().text_color(faint())))
                .child(
                    v_flex()
                        .flex_1()
                        .child(div().text_color(ink()).child("MCP endpoint"))
                        .child(div().font_family(mono()).text_size(px(11.)).text_color(faint()).child(format!("http://127.0.0.1:{}/mcp", mcp::PORT))),
                ),
        );
        page = page.child(
            h_flex()
                .h(px(52.))
                .px_6()
                .gap_3()
                .items_center()
                .border_b_1()
                .border_color(line())
                .child(div().size(px(28.)).flex_none().flex().items_center().justify_center().bg(wash()).child(Icon::empty().path(if writes { "icons/pen.svg" } else { "icons/lock.svg" }).small().text_color(if writes { amb() } else { faint() })))
                .child(
                    v_flex()
                        .flex_1()
                        .child(div().text_color(ink()).child("Agent writes"))
                        .child(div().text_size(px(11.)).text_color(if writes { amb() } else { faint() }).child(if writes { "Allowed this session. Always off at launch." } else { "Read only. Always off at launch." })),
                ),
        );
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                h_flex()
                    .h(px(56.))
                    .flex_none()
                    .px_6()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(line())
                    .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child("Settings"))
                    .child(self.icon_btn("settings-close", IconName::Close, false, true, cx, |t, cx| {
                        t.page_settings = false;
                        cx.notify();
                    })),
            )
            .child(div().id("settings-scroll").flex_1().min_h_0().overflow_y_scroll().child(page))
    }

    fn nav_item(
        &self,
        id: String,
        label: String,
        icon: IconName,
        active: bool,
        starred: bool,
        hide_key: Option<String>,
        on: impl Fn(&mut Hatch, &mut Context<Hatch>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let me = cx.entity();
        let group = SharedString::from(format!("g-{id}"));
        let me_h = me.clone();
        h_flex()
            .group(group.clone())
            .id(SharedString::from(id.clone()))
            .h(px(32.))
            .px_4()
            .gap_2()
            .items_center()
            .cursor_pointer()
            .border_l_2()
            .border_color(if active { acc() } else { none() })
            .text_color(if active { ink() } else { dim() })
            .when(active, |d| d.bg(acc_s()))
            .hover(|d| d.bg(wash()).text_color(ink()))
            .child(Icon::new(icon).small().text_color(if active || starred { acc() } else { faint() }))
            .child(div().flex_1().truncate().child(label))
            .when_some(hide_key, |d, key| {
                d.child(
                    div()
                        .id(SharedString::from(format!("hide-{id}")))
                        .invisible()
                        .group_hover(group.clone(), |s| s.visible())
                        .size(px(22.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|d| d.bg(wash2()))
                        .child(Icon::new(IconName::EyeOff).xsmall().text_color(faint()))
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            me_h.update(cx, |this, cx| this.set_location_visible(&key, false, cx));
                        }),
                )
            })
            .on_click(move |_, _, cx| me.update(cx, |this, cx| on(this, cx)))
    }

    fn icon_btn(&self, id: &'static str, icon: IconName, on: bool, enabled: bool, cx: &mut Context<Self>, f: impl Fn(&mut Hatch, &mut Context<Hatch>) + 'static) -> impl IntoElement {
        let me = cx.entity();
        div()
            .id(id)
            .size(px(32.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .when(on, |d| d.bg(wash2()))
            .when(enabled, |d| d.cursor_pointer().hover(|d| d.bg(wash())))
            .child(Icon::new(icon).small().text_color(if !enabled { faint() } else if on { acc() } else { dim() }))
            .on_click(move |_, _, cx| {
                if enabled {
                    me.update(cx, |this, cx| f(this, cx))
                }
            })
    }
}

impl Render for Hatch {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let me = cx.entity();
        let dev = self.current().cloned();
        let paired = dev.as_ref().is_some_and(|d| d.paired);
        let writes = self.allow_writes.load(Ordering::Relaxed);
        let calls = self.calls.load(Ordering::Relaxed);
        let root_name = match &self.root {
            Root::Media => "Media".to_string(),
            Root::App(id) => self.apps.iter().find(|a| &a.bundle_id == id).map(|a| a.name.clone()).unwrap_or_else(|| id.clone()),
        };
        let crumb = format!("{}{}", root_name, if self.path == "/" { String::new() } else { self.path.clone() });

        // ---- title bar
        let title = TitleBar::new()
            .bg(none())
            .on_close_window(|_, window, _| window.remove_window())
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_3()
                    .child(Icon::new(IconName::Folder).small().text_color(acc()))
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(ink()).child("Hatch"))
                    .child(div().truncate().text_color(dim()).child(match &dev {
                        Some(d) => format!("{} · {}", d.name, crumb),
                        None => "No iPhone".into(),
                    })),
            );

        // ---- sidebar (flush column)
        let mut side = v_flex().w(px(248.)).h_full().flex_none().bg(panel()).border_r_1().border_color(line());
        side = side.child(match &dev {
            None => h_flex()
                .h(px(56.))
                .flex_none()
                .px_4()
                .items_center()
                .gap_3()
                .border_b_1()
                .border_color(line())
                .child(div().size(px(36.)).flex_none().bg(wash()).flex().items_center().justify_center().child(Icon::empty().path("icons/smartphone.svg").text_color(faint())))
                .child(v_flex().child(div().font_weight(FontWeight::SEMIBOLD).child("No iPhone")).child(div().text_size(px(11.)).text_color(if self.mux_down.is_some() { amb() } else { dim() }).child(if self.mux_down.is_some() { "usbmuxd is not running" } else { "Connect a USB cable" }))),
            Some(d) => h_flex()
                .h(px(56.))
                .flex_none()
                .px_4()
                .gap_3()
                .items_center()
                .border_b_1()
                .border_color(line())
                .child(div().size(px(36.)).flex_none().bg(wash()).flex().items_center().justify_center().child(Icon::empty().path("icons/smartphone.svg").text_color(dim())))
                .child(
                    v_flex()
                        .min_w_0()
                        .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(d.name.clone()))
                        .child(div().text_size(px(11.)).text_color(if d.paired { dim() } else { amb() }).truncate().child(if d.paired {
                            format!("{} · iOS {} · Trusted", d.model, d.ios)
                        } else {
                            "Not trusted".to_string()
                        })),
                ),
        });
        if let Some(d) = &dev {
            if !d.paired {
                side = side.child(
                    div().p_4().child(
                        div()
                            .id("pair")
                            .h(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .bg(acc())
                            .text_color(rgba(0x0b1a13ff))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Trust this computer")
                            .on_click(cx.listener(|this, _, _, cx| this.pair(cx))),
                    ),
                );
            }
        }
        if paired {
            let mut nav = v_flex();
            if !self.favs.is_empty() {
                nav = nav.child(section("Favorites"));
                for (i, f) in self.favs.iter().cloned().enumerate() {
                    let active = !self.page_settings && self.root_str() == f.root && self.path == f.path;
                    let f2 = f.clone();
                    nav = nav.child(self.nav_item(format!("fav-{i}"), f.label.clone(), IconName::Star, active, true, None, move |this, cx| this.open_fav(f2.clone(), cx), cx));
                }
            }
            nav = nav.child(section("Locations"));
            if self.location_visible("media") {
                nav = nav.child(self.nav_item("loc-media".into(), "Media (all)".into(), IconName::Folder, !self.page_settings && self.root == Root::Media && self.path == "/", false, Some("media".into()), |this, cx| this.open_root(Root::Media, cx), cx));
            }
            for a in self.apps.clone() {
                if !self.location_visible(&a.bundle_id) {
                    continue;
                }
                let r = Root::App(a.bundle_id.clone());
                let active = !self.page_settings && self.root == r && self.path == "/";
                nav = nav.child(self.nav_item(format!("loc-{}", a.bundle_id), a.name.clone(), IconName::Folder, active, false, Some(a.bundle_id.clone()), move |this, cx| this.open_root(r.clone(), cx), cx));
            }
            let hidden_n = self.settings.hidden.len();
            if hidden_n > 0 {
                nav = nav.child(
                    div()
                        .id("hidden-note")
                        .px_4()
                        .py_2()
                        .text_size(px(11.))
                        .text_color(faint())
                        .cursor_pointer()
                        .hover(|d| d.text_color(dim()))
                        .child(format!("{hidden_n} hidden · manage in Settings"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.page_settings = true;
                            cx.notify();
                        })),
                );
            }
            side = side.child(div().id("nav").flex_1().min_h_0().overflow_y_scroll().child(nav));
        } else {
            side = side.child(div().flex_1());
        }
        let settings_on = self.page_settings;
        side = side.child(
            div().border_t_1().border_color(line()).child(self.nav_item(
                "settings".into(),
                "Settings".into(),
                IconName::Settings2,
                settings_on,
                false,
                None,
                |this, cx| {
                    this.page_settings = !this.page_settings;
                    cx.notify();
                },
                cx,
            )),
        );
        // writes gate + mcp status, pinned to the bottom
        let me_w = me.clone();
        side = side
            .child(
                h_flex()
                    .id("writes")
                    .h(px(44.))
                    .px_4()
                    .gap_3()
                    .items_center()
                    .cursor_pointer()
                    .border_t_1()
                    .border_color(line())
                    .child(Icon::empty().path(if writes { "icons/pen.svg" } else { "icons/lock.svg" }).small().text_color(if writes { amb() } else { faint() }))
                    .child(div().flex_1().truncate().font_weight(FontWeight::MEDIUM).child("Agent writes"))
                    .child(div().flex_none().text_size(px(11.)).text_color(if writes { amb() } else { faint() }).child(if writes { "On" } else { "Off" }))
                    .child(
                        div()
                            .w(px(34.))
                            .h(px(18.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px(px(2.))
                            .border_1()
                            .border_color(if writes { rgba(0xe0b06257) } else { line() })
                            .bg(if writes { amb_s() } else { wash() })
                            .when(writes, |d| d.justify_end())
                            .child(div().size(px(12.)).bg(if writes { amb() } else { faint() })),
                    )
                    .on_click(move |_, _, cx| {
                        me_w.update(cx, |this, cx| {
                            let v = !this.allow_writes.load(Ordering::Relaxed);
                            this.allow_writes.store(v, Ordering::Relaxed);
                            cx.notify();
                        })
                    }),
            )
            .child(
                h_flex()
                    .px_4()
                    .pb_3()
                    .gap_2()
                    .items_center()
                    .text_size(px(11.))
                    .text_color(faint())
                    .child(div().size(px(6.)).bg(acc()))
                    .child(div().font_family(mono()).child(format!("127.0.0.1:{} · {} calls", mcp::PORT, calls))),
            );

        // ---- toolbar
        let (fav_root, fav_path) = (self.root_str(), self.path.clone());
        let is_fav_here = self.is_fav(&fav_root, &fav_path);
        let toolbar = h_flex()
            .h(px(56.))
            .flex_none()
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(line())
            .child(self.icon_btn("up", IconName::ArrowLeft, false, paired && self.path != "/", cx, |t, cx| t.go_up(cx)))
            .child(self.icon_btn("reload", IconName::Redo, false, paired, cx, |t, cx| t.load_dir(cx)))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .h(px(32.))
                    .px_3()
                    .items_center()
                    .bg(rgba(0x0000002e))
                    .border_1()
                    .border_color(line())
                    .font_family(mono())
                    .text_size(px(12.))
                    .text_color(ink())
                    .child(div().truncate().child(crumb.clone())),
            )
            .child(div().flex_none().text_size(px(12.)).text_color(faint()).child(if self.busy { "Loading…".to_string() } else { format!("{} items", self.entries.len()) }))
            .child({
                let (r, p) = (fav_root.clone(), fav_path.clone());
                self.icon_btn("fav-here", IconName::Star, is_fav_here, paired && self.path != "/", cx, move |t, cx| t.toggle_fav(r.clone(), p.clone(), cx))
            })
            .child(
                h_flex()
                    .border_1()
                    .border_color(line())
                    .child(self.icon_btn("v-list", IconName::Menu, !self.grid, true, cx, |t, cx| {
                        t.grid = false;
                        t.epoch += 1;
                        cx.notify();
                    }))
                    .child(self.icon_btn("v-grid", IconName::LayoutDashboard, self.grid, true, cx, |t, cx| {
                        t.grid = true;
                        t.load_thumbs(cx);
                        cx.notify();
                    })),
            )
            .child({
                let on = self.show_log;
                h_flex()
                    .id("agent-btn")
                    .h(px(32.))
                    .px_3()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .border_1()
                    .border_color(if on { rgba(0x5fd4a057) } else { line() })
                    .bg(if on { acc_s() } else { wash() })
                    .text_size(px(12.))
                    .text_color(if on { acc() } else { dim() })
                    .child(Icon::new(IconName::Bot).small())
                    .child(format!("Agent · {calls}"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_log = !this.show_log;
                        cx.notify();
                    }))
            });

        // ---- content
        let message = |icon: IconName, tint: Rgba, title: &str, text: String| {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(div().size(px(56.)).flex().items_center().justify_center().bg(wash()).mb_3().child(Icon::new(icon).text_color(tint)))
                .child(div().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).child(title.to_string()))
                .child(div().max_w(px(340.)).text_center().text_color(dim()).child(text))
        };
        let content: AnyElement = if let Some(err) = &self.error {
            message(IconName::File, cor(), "Something went wrong", err.clone()).into_any_element()
        } else if let (None, Some(help)) = (&dev, &self.mux_down) {
            message(IconName::Info, amb(), "usbmuxd is not running", help.clone()).into_any_element()
        } else if dev.is_none() {
            message(IconName::Folder, faint(), "No iPhone connected", "Connect the iPhone with a USB cable.\nUnlock the iPhone.\nHatch finds it automatically.".into()).into_any_element()
        } else if self.pairing {
            message(IconName::Info, amb(), "Look at the iPhone", "1. Unlock the iPhone.\n2. Tap Trust.\n3. Enter the iPhone passcode.\nHatch continues when you finish.".into()).into_any_element()
        } else if !paired {
            message(IconName::Info, amb(), "Trust this computer", "The iPhone must trust this computer first.\n1. Unlock the iPhone.\n2. Click Trust this computer in the sidebar.\n3. On the iPhone, tap Trust.\n4. Enter the iPhone passcode.".into()).into_any_element()
        } else if self.entries.is_empty() && !self.busy {
            message(IconName::Folder, faint(), "This folder is empty", format!("{crumb} has no items.")).into_any_element()
        } else if self.grid {
            let mut g = div().id("files").size_full().overflow_y_scroll().flex().flex_wrap().content_start();
            for (i, e) in self.entries.iter().cloned().enumerate() {
                let on = self.picked.contains(&e.name) && !e.is_dir;
                let drop_dir = e.is_dir.then(|| self.full(&e.name));
                let thumb = self.thumbs.get(&format!("{}{}", self.root_key(), self.full(&e.name))).cloned();
                let me = me.clone();
                let e2 = e.clone();
                let video = is_video(&e.name);
                g = g.child(
                    div()
                        .id(("tile", i))
                        .relative()
                        .w(px(176.))
                        .h(px(132.))
                        .flex_none()
                        .overflow_hidden()
                        .cursor_pointer()
                        .bg(wash())
                        .border_2()
                        .border_color(if on { acc() } else { rgba(0x101315ff) })
                        .child(match thumb {
                            Some(t) => img(t).size_full().object_fit(ObjectFit::Cover).into_any_element(),
                            None => div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(Icon::new(if e.is_dir { IconName::Folder } else { IconName::File }).text_color(if e.is_dir { acc() } else { faint() }))
                                .into_any_element(),
                        })
                        .when(video, |d| {
                            d.child(div().absolute().top_1().left_1().px_1p5().h(px(18.)).flex().items_center().bg(rgba(0x0a0b0c8c)).text_size(px(10.)).text_color(ink()).child("VIDEO"))
                        })
                        .child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .px_2()
                                .h(px(22.))
                                .flex()
                                .items_center()
                                .bg(rgba(0x0a0b0cb3))
                                .text_size(px(11.))
                                .text_color(if on { ink() } else { dim() })
                                .child(div().truncate().child(if e.is_dir { format!("{}/", e.name) } else { e.name.clone() })),
                        )
                        .hover(|d| d.bg(wash2()))
                        .when_some(drop_dir, |d, dir| self.drop_target(d, dir, cx))
                        .on_click(move |ev, _, cx| {
                            let (e, m) = (e2.clone(), ev.modifiers());
                            me.update(cx, |this, cx| this.pick_entry(i, e, m, cx))
                        }),
                );
            }
            g.into_any_element()
        } else {
            let mut list = v_flex();
            for (i, e) in self.entries.iter().cloned().enumerate() {
                let on = self.picked.contains(&e.name) && !e.is_dir;
                let drop_dir = e.is_dir.then(|| self.full(&e.name));
                let me = me.clone();
                let e2 = e.clone();
                list = list.child(
                    h_flex()
                        .id(("entry", i))
                        .h(px(40.))
                        .flex_none()
                        .px_4()
                        .gap_3()
                        .items_center()
                        .cursor_pointer()
                        .border_l_2()
                        .border_color(if on { acc() } else { none() })
                        .when(on, |d| d.bg(acc_s()))
                        .hover(|d| d.bg(wash()))
                        .child(
                            div()
                                .size(px(26.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(wash())
                                .child(Icon::new(if e.is_dir { IconName::Folder } else { IconName::File }).small().text_color(if e.is_dir || on { acc() } else { faint() })),
                        )
                        .child(div().flex_1().min_w_0().truncate().text_color(ink()).child(e.name.clone()))
                        .child(div().w(px(80.)).flex_none().text_right().font_family(mono()).text_size(px(12.)).text_color(dim()).child(if e.is_dir { String::new() } else { human(e.size) }))
                        .child(div().w(px(140.)).flex_none().text_right().text_size(px(12.)).text_color(faint()).child(e.modified.clone()))
                        .when_some(drop_dir, |d, dir| self.drop_target(d, dir, cx))
                        .on_click(move |ev, _, cx| {
                            let (e, m) = (e2.clone(), ev.modifiers());
                            me.update(cx, |this, cx| this.pick_entry(i, e, m, cx))
                        }),
                );
            }
            div().id("files").size_full().overflow_y_scroll().child(list).into_any_element()
        };

        let main = if self.page_settings {
            self.settings_page(cx).into_any_element()
        } else {
            let here = self.path.clone();
            let area = div().id("drop-area").flex_1().min_h_0().child(content);
            let area = if paired && self.error.is_none() { self.drop_target(area, here, cx) } else { area };
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(toolbar)
                .child(area)
                // Below the list, so the rows do not move when the first file is picked.
                .children(self.selection_bar(cx))
                .children(self.transfer_bar(cx))
                .into_any_element()
        };

        // ---- preview column (flush, full-bleed image)
        let mut cols = h_flex().flex_1().min_h_0().child(side).child(main);
        if paired && self.error.is_none() && !self.page_settings {
            if let (Some(name), Some(p)) = (self.selected.clone(), &self.preview) {
                if let Some(e) = self.entries.iter().find(|e| e.name == name) {
                    let mut col = v_flex().w_full();
                    match p {
                        Preview::Image(i) => {
                            col = col.child(div().w_full().h(px(228.)).bg(wash()).child(img(i.clone()).size_full().object_fit(ObjectFit::Cover)));
                        }
                        Preview::Text(_) => {}
                    }
                    let mut info = v_flex().p_4().gap_1();
                    info = info
                        .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(e.name.clone()))
                        .child(div().text_size(px(11.)).text_color(dim()).mb_3().child(kind_of(e)))
                        .child(kv("Size", human(e.size), true))
                        .child(kv("Modified", e.modified.clone(), false))
                        .child(kv("Path", self.full(&e.name), true));
                    if let Preview::Text(t) = p {
                        info = info.child(
                            div()
                                .mt_3()
                                .p_3()
                                .bg(rgba(0x00000033))
                                .border_1()
                                .border_color(line())
                                .font_family(mono())
                                .text_size(px(12.))
                                .text_color(dim())
                                .child(t.clone()),
                        );
                    }
                    cols = cols.child(
                        v_flex()
                            .id("preview")
                            .w(px(304.))
                            .h_full()
                            .flex_none()
                            .overflow_y_scroll()
                            .bg(panel())
                            .border_l_1()
                            .border_color(line())
                            .child(
                                h_flex()
                                    .h(px(56.))
                                    .flex_none()
                                    .pl_4()
                                    .items_center()
                                    .justify_between()
                                    .border_b_1()
                                    .border_color(line())
                                    .child(div().text_size(px(10.)).text_color(faint()).font_weight(FontWeight::MEDIUM).child("PREVIEW"))
                                    .child(self.icon_btn("preview-close", IconName::Close, false, true, cx, |t, cx| {
                                        t.selected = None;
                                        t.preview = None;
                                        cx.notify();
                                    })),
                            )
                            .child(col.child(info)),
                    );
                }
            }
        }

        // ---- agent activity (overlay drawer, does not resize the layout)
        let mut overlay: Vec<AnyElement> = Vec::new();
        if self.show_log {
            let entries: Vec<mcp::LogEntry> = self.log.lock().unwrap().iter().rev().take(40).cloned().collect();
            let mut lg = v_flex().id("agent-log").flex_1().min_h_0().overflow_y_scroll();
            if entries.is_empty() {
                lg = lg.child(div().p_4().text_color(faint()).child("No calls yet. Point your agent at the endpoint above."));
            }
            for e in entries {
                let (c, bgc, bar) = if e.denied || !e.ok { (cor(), cor_s(), cor()) } else if e.write { (amb(), amb_s(), amb()) } else { (dim(), none(), none()) };
                lg = lg.child(
                    h_flex()
                        .px_4()
                        .py_2()
                        .gap_3()
                        .items_start()
                        .border_l_2()
                        .border_color(bar)
                        .bg(bgc)
                        .child(div().w(px(58.)).flex_none().font_family(mono()).text_size(px(11.)).text_color(faint()).child(e.ts.clone()))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().font_family(mono()).text_size(px(12.)).text_color(if e.write || !e.ok { c } else { ink() }).child(e.tool.clone()))
                                .child(div().font_family(mono()).text_size(px(11.)).text_color(c).truncate().child(e.path.clone())),
                        )
                        .child(
                            v_flex()
                                .items_end()
                                .child(div().text_size(px(11.)).text_color(c).child(if e.denied { "refused" } else if e.ok { "ok" } else { "error" }))
                                .child(div().font_family(mono()).text_size(px(11.)).text_color(faint()).child(format!("{}ms", e.ms))),
                        ),
                );
            }
            overlay.push(
                div()
                    .id("scrim")
                    .absolute()
                    .top(px(35.))
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .bg(rgba(0x06070866))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_log = false;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            overlay.push(
                v_flex()
                    .id("drawer")
                    .occlude()
                    .absolute()
                    .top(px(35.))
                    .right_0()
                    .bottom_0()
                    .w(px(380.))
                    .bg(rgba(0x16191bf2))
                    .border_l_1()
                    .border_color(line())
                    .shadow_lg()
                    .child(
                        h_flex()
                            .h(px(56.))
                            .flex_none()
                            .pl_4()
                            .items_center()
                            .justify_between()
                            .border_b_1()
                            .border_color(line())
                            .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child("Agent activity"))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_3()
                                    .child(div().text_size(px(11.)).text_color(if writes { amb() } else { dim() }).child(if writes { "writes on" } else { "read only" }))
                                    .child(self.icon_btn("drawer-close", IconName::Close, false, true, cx, |t, cx| {
                                        t.show_log = false;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(lg)
                    .into_any_element(),
            );
        }

        v_flex()
            .relative()
            .size_full()
            .bg(ground())
            .text_color(ink())
            .font_family(sans())
            .text_size(px(13.))
            .child(title)
            .child(div().h(px(1.)).bg(line()))
            .child(cols)
            .children(overlay)
    }
}

fn main() {
    let _ = rt();
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(|cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        let opts = WindowOptions {
            window_background: WindowBackgroundAppearance::Blurred,
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1180.), px(740.)), cx))),
            ..TitleBar::window_options()
        };
        gpui_kit::open_window(opts, cx, |_, cx| cx.new(Hatch::new)).expect("failed to open window");
        cx.activate(true);
    });
}
