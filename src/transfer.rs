//! Multi-select, download to this computer, and drag-and-drop upload to the iPhone.
//! The user always may upload. The "Agent writes" switch gates only the MCP agent.

use crate::device::{self, Entry, Transfer};
use crate::*;
use gpui_kit::component::tooltip::Tooltip;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

pub struct Job {
    pub upload: bool,
    pub t: Arc<Transfer>,
    pub count: usize,
    /// Where the files go: a local folder or a phone folder.
    pub place: String,
    /// None while the copy runs. Then the message for the user (Ok) or the error (Err).
    pub outcome: Option<Result<String, String>>,
    /// Name of the sync job, when this is a sync run.
    pub sync: Option<String>,
}

impl Job {
    fn running(&self) -> bool {
        self.outcome.is_none()
    }
}

/// The folder with the home folder shown as ~.
pub(crate) fn short_path(p: &std::path::Path) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match p.strip_prefix(&home) {
        Ok(rest) if !home.is_empty() => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}

/// Style of a bottom-bar button.
pub(crate) enum Btn {
    /// Text only. Fills on hover.
    Quiet,
    /// The one accented action at the right of a bar, with a Lucide icon.
    Primary(&'static str),
}

/// Frame of the bars under the file view (selection, transfer). Same edges and background as the side panels.
fn bottom_bar() -> Div {
    v_flex().relative().flex_none().border_t_1().border_color(line()).bg(panel())
}

/// One row in a bottom bar. px_4 lines up with the file rows above it.
fn bar_row() -> Div {
    h_flex().h(px(44.)).px_4().gap_2().items_center()
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "file" } else { "files" }
}

impl Hatch {
    pub(crate) fn job_running(&self) -> bool {
        self.job.as_ref().is_some_and(Job::running)
    }

    /// Click on a row or tile. Plain click picks one file and previews it, or opens a folder.
    /// Ctrl-click adds or removes a file. Shift-click picks the files between the anchor and here.
    pub fn pick_entry(&mut self, i: usize, e: Entry, m: Modifiers, cx: &mut Context<Self>) {
        self.came_from = None;
        let toggle = m.control || m.platform;
        if e.is_dir && !toggle && !m.shift {
            self.click_entry(e, cx);
            return;
        }
        if m.shift {
            let a = self.anchor.unwrap_or(i);
            let (lo, hi) = (a.min(i), a.max(i));
            if !toggle {
                self.picked.clear();
            }
            for x in self.entries[lo..=hi.min(self.entries.len().saturating_sub(1))].iter().filter(|x| !x.is_dir) {
                self.picked.insert(x.name.clone());
            }
            cx.notify();
            return;
        }
        if e.is_dir {
            return; // Folders are not picked. Ctrl-click on a folder does nothing.
        }
        self.anchor = Some(i);
        if toggle {
            if !self.picked.remove(&e.name) {
                self.picked.insert(e.name.clone());
            }
            cx.notify();
            return;
        }
        self.picked = BTreeSet::from([e.name.clone()]);
        self.click_entry(e, cx);
    }

    fn pick_all(&mut self, cx: &mut Context<Self>) {
        self.picked = self.entries.iter().filter(|e| !e.is_dir).map(|e| e.name.clone()).collect();
        cx.notify();
    }

    /// Ask for a local folder, then copy the picked files into it.
    fn choose_download_folder(&mut self, cx: &mut Context<Self>) {
        if self.picked.is_empty() || self.job_running() {
            return;
        }
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Download here".into()),
        });
        cx.spawn(async move |this, cx| {
            let dir = match rx.await {
                Ok(Ok(Some(mut p))) if !p.is_empty() => p.remove(0),
                Ok(Ok(_)) => return, // The user closed the picker.
                // No file picker on this desktop: use ~/Downloads.
                _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join("Downloads"),
            };
            this.update(cx, |this, cx| this.start_download(dir, cx)).ok();
        })
        .detach();
    }

    fn start_download(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let Some(udid) = self.udid.clone() else { return };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.job = Some(Job { upload: false, t: Default::default(), count: 0, place: dir.display().to_string(), outcome: Some(Err(format!("Cannot use {}: {e}", dir.display()))), sync: None });
            cx.notify();
            return;
        }
        // Never replace a file on this computer: pick "name (2).ext" when the name is taken.
        let mut taken: BTreeSet<String> = BTreeSet::new();
        let files: Vec<(String, PathBuf)> = self
            .entries
            .iter()
            .filter(|e| !e.is_dir && self.picked.contains(&e.name))
            .map(|e| {
                let n = device::free_name(&e.name, |n| taken.contains(n) || dir.join(n).exists() || dir.join(format!("{n}.part")).exists());
                taken.insert(n.clone());
                (self.full(&e.name), dir.join(n))
            })
            .collect();
        if files.is_empty() {
            return;
        }
        let t = Arc::new(Transfer::default());
        let count = files.len();
        let place = short_path(&dir);
        self.job = Some(Job { upload: false, t: t.clone(), count, place: place.clone(), outcome: None, sync: None });
        self.watch_job(cx);
        let root = self.root.clone();
        let t2 = t.clone();
        run(cx, async move { device::download(&udid, &root, &files, &t2).await }, move |this, r, cx| {
            let n = t.files_done.load(Ordering::Relaxed);
            let msg = match r {
                Ok(bytes) => Ok(format!("Downloaded {n} {} ({}) to {place}", plural(n), human(bytes))),
                Err(e) if e.is::<device::Cancelled>() => Ok(format!("Stopped. {n} of {count} {} are in {place}.", plural(count))),
                Err(e) => Err(format!("Download stopped after {n} of {count} {}: {}", plural(count), device::explain(&e))),
            };
            if let Some(j) = &mut this.job {
                j.outcome = Some(msg);
            }
            this.next_sync(cx);
            cx.notify();
        });
    }

    /// Copy dropped local files into the phone folder `dir` (in the current root).
    fn start_upload(&mut self, dir: String, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(udid) = self.udid.clone() else { return };
        if self.job_running() {
            return;
        }
        let (files, skipped): (Vec<PathBuf>, Vec<PathBuf>) = paths.into_iter().partition(|p| p.is_file());
        let note = if skipped.is_empty() { String::new() } else { format!(" Skipped {} folder(s): upload the files inside them.", skipped.len()) };
        let root = self.root.clone();
        let place = format!("{}{}", if root == Root::Media { "Media" } else { "Documents" }, if dir == "/" { String::new() } else { dir.clone() });
        let t = Arc::new(Transfer::default());
        let count = files.len();
        if count == 0 {
            self.job = Some(Job { upload: true, t, count, place, outcome: Some(Err(format!("Nothing to upload.{note}"))), sync: None });
            cx.notify();
            return;
        }
        self.job = Some(Job { upload: true, t: t.clone(), count, place: place.clone(), outcome: None, sync: None });
        self.watch_job(cx);
        let (t2, root2, dir2) = (t.clone(), root.clone(), dir.clone());
        run(cx, async move { device::upload(&udid, &root2, &dir2, &files, &t2).await }, move |this, r, cx| {
            let n = t.files_done.load(Ordering::Relaxed);
            let msg = match r {
                Ok(_) => Ok(format!("Uploaded {n} {} ({}) to {place}.{note}", plural(n), human(t.done.load(Ordering::Relaxed)))),
                Err(e) if e.is::<device::Cancelled>() => Ok(format!("Stopped. {n} of {count} {} reached {place}.", plural(count))),
                Err(e) => Err(format!("Upload stopped after {n} of {count} {}: {}", plural(count), device::explain(&e))),
            };
            if let Some(j) = &mut this.job {
                j.outcome = Some(msg);
            }
            this.next_sync(cx);
            // Show the new files if the user still looks at that folder.
            if this.root == root && this.path == dir {
                this.load_dir(cx);
            }
            cx.notify();
        });
    }

    /// Redraw a few times a second while a copy runs, for the progress bar.
    pub(crate) fn watch_job(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let go = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.job_running()
                    })
                    .unwrap_or(false);
                if !go {
                    break;
                }
            }
        })
        .detach();
    }

    /// Make `el` accept files dragged from the desktop. They upload into `dir`.
    pub fn drop_target<E: InteractiveElement>(&self, el: E, dir: String, cx: &mut Context<Self>) -> E {
        let me = cx.entity();
        el.drag_over::<ExternalPaths>(|s, _, _, _| s.bg(acc_s()).border_color(acc()))
            .on_drop(move |paths: &ExternalPaths, _, cx| {
                let p = paths.paths().to_vec();
                let d = dir.clone();
                me.update(cx, |this, cx| this.start_upload(d, p, cx));
            })
    }

    /// Button for the bars under the file view. All have the same height.
    pub(crate) fn bar_btn(
        &self,
        id: impl Into<ElementId>,
        label: &str,
        kind: Btn,
        enabled: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Hatch, &mut Context<Hatch>) + 'static,
    ) -> Stateful<Div> {
        let me = cx.entity();
        bar_btn_look(id, label, kind, enabled).on_click(move |_, _, cx| {
            if enabled {
                me.update(cx, |this, cx| f(this, cx))
            }
        })
    }

    /// Bar under the file list while files are picked.
    pub fn selection_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Count only picked files that are still listed. The list empties when the phone goes away.
        let (n, bytes) = self.entries.iter().filter(|e| self.picked.contains(&e.name)).fold((0, 0u64), |(n, b), e| (n + 1, b + e.size));
        if n == 0 {
            return None;
        }
        let files = self.entries.iter().filter(|e| !e.is_dir).count();
        let busy = self.job_running();
        let label = div()
            .id("sel-count")
            .flex_1()
            .min_w_0()
            .truncate()
            .child(format!("{n} selected · {}", human(bytes)))
            .tooltip(|w, cx| Tooltip::new("Ctrl-click adds or removes a file. Shift-click picks a range.").build(w, cx));
        Some(
            bottom_bar()
                .child(
                    bar_row()
                        .child(label)
                        .child(self.bar_btn("sel-all", "Select all", Btn::Quiet, n < files, cx, |t, cx| t.pick_all(cx)))
                        .child(self.bar_btn("sel-clear", "Clear", Btn::Quiet, true, cx, |t, cx| {
                            t.picked.clear();
                            t.anchor = None;
                            cx.notify();
                        }))
                        .child(self.bar_btn("sel-download", "Download…", Btn::Primary("icons/download.svg"), !busy, cx, |t, cx| t.choose_download_folder(cx))),
                )
                .into_any_element(),
        )
    }

    /// Bar at the bottom of the file view: progress and Cancel, then the result.
    pub fn transfer_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let j = self.job.as_ref()?;
        let (done, total) = (j.t.done.load(Ordering::Relaxed), j.t.total.load(Ordering::Relaxed));
        let frac = if total == 0 { 0. } else { (done as f32 / total as f32).clamp(0., 1.) };
        let mut bar = bottom_bar();
        let row = bar_row();
        let row = match &j.outcome {
            None => {
                let count = j.count.max(j.t.files_total.load(Ordering::Relaxed));
                let k = (j.t.files_done.load(Ordering::Relaxed) + 1).min(count);
                let cur = j.t.current.lock().unwrap().clone();
                let verb = if j.upload { "Uploading" } else { "Downloading" };
                let cancelling = j.t.cancel.load(Ordering::Relaxed);
                let text = match &j.sync {
                    Some(name) if count == 0 => format!("Syncing {name} · comparing files…"),
                    Some(name) => format!("Syncing {name} · {k} of {count} · {cur}"),
                    None => format!("{verb} {k} of {count} · {cur} · to {}", j.place),
                };
                let icon = match (&j.sync, j.upload) {
                    (Some(_), _) => Icon::empty().path("icons/refresh-cw.svg"),
                    (None, true) => Icon::new(IconName::ArrowUp),
                    (None, false) => Icon::new(IconName::ArrowDown),
                };
                row.child(icon.small().text_color(acc()))
                    .child(div().flex_1().min_w_0().truncate().text_color(ink()).child(text))
                    .child(div().flex_none().text_color(dim()).child(format!("{} of {}", human(done), human(total))))
                    .child(self.bar_btn("xfer-cancel", if cancelling { "Stopping…" } else { "Cancel" }, Btn::Quiet, !cancelling, cx, |t, cx| {
                        if let Some(j) = &t.job {
                            j.t.cancel.store(true, Ordering::Relaxed);
                        }
                        cx.notify();
                    }))
            }
            Some(r) => {
                let (ok, text) = match r {
                    Ok(t) => (true, t.clone()),
                    Err(t) => (false, t.clone()),
                };
                row.child(Icon::new(if ok { IconName::Check } else { IconName::Info }).small().text_color(if ok { acc() } else { cor() }))
                    .child(div().flex_1().min_w_0().truncate().text_color(if ok { ink() } else { cor() }).child(text))
                    .child(self.bar_btn("xfer-close", "Close", Btn::Quiet, true, cx, |t, cx| {
                        t.job = None;
                        cx.notify();
                    }))
            }
        };
        bar = bar.child(row);
        if j.running() {
            // Over the top edge, so this bar has the same height as the selection bar.
            bar = bar.child(div().absolute().top_0().left_0().right_0().h(px(2.)).bg(wash()).child(div().h_full().w(relative(frac)).bg(acc())));
        }
        Some(bar.into_any_element())
    }
}

/// Look of a bar button, with no click handler.
pub(crate) fn bar_btn_look(id: impl Into<ElementId>, label: &str, kind: Btn, enabled: bool) -> Stateful<Div> {
    let (primary, icon) = match kind {
        Btn::Quiet => (false, None),
        Btn::Primary(icon) => (true, Some(icon)),
    };
    div()
        .id(id)
        .h(px(28.))
        .px_3()
        .gap_1p5()
        .flex()
        .flex_none()
        .items_center()
        .text_size(px(12.))
        .text_color(if !enabled { faint() } else if primary { acc() } else { dim() })
        .when(primary, |d| {
            d.border_1()
                .border_color(if enabled { rgba(0x5fd4a057) } else { line() })
                .bg(if enabled { acc_s() } else { wash() })
                .font_weight(FontWeight::MEDIUM)
        })
        .when(enabled, |d| d.cursor_pointer().hover(move |d| if primary { d.bg(rgba(0x5fd4a033)) } else { d.bg(wash()).text_color(ink()) }))
        .when_some(icon, |d, p| d.child(Icon::empty().path(p).small()))
        .child(label.to_string())
}
