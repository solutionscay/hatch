//! Settings "Sync" section: the job list, the job editor, preview, and runs.

use crate::sync::{self, Direction, Summary, SyncJob};
use crate::transfer::{Btn, Job, bar_btn_look, short_path};
use crate::*;
use gpui_kit::component::input::{Input, InputState};

/// The job being added or edited.
pub struct Editor {
    /// None for a new job.
    pub id: Option<String>,
    pub name: Entity<InputState>,
    pub path: Entity<InputState>,
    pub local: Entity<InputState>,
    pub root: String,
    pub direction: Direction,
    pub auto: bool,
    pub error: Option<String>,
}

/// Preview shown before the first run of a job.
pub struct PreviewBox {
    pub id: String,
    /// None while the phone and the folder are scanned.
    pub result: Option<Result<Summary, String>>,
}

#[derive(Default)]
pub struct SyncUi {
    pub jobs: Vec<SyncJob>,
    /// Job id -> "last run · result".
    pub last: std::collections::HashMap<String, String>,
    pub edit: Option<Editor>,
    pub preview: Option<PreviewBox>,
    /// Auto jobs waiting for their turn.
    pub queue: std::collections::VecDeque<String>,
    /// Job id after the first click on Delete.
    pub confirm: Option<String>,
}

impl SyncUi {
    pub fn load() -> SyncUi {
        let jobs = sync::load_jobs();
        let last = jobs.iter().filter_map(|j| sync::last_result(&j.id).map(|(t, r)| (j.id.clone(), format!("{t} · {r}")))).collect();
        SyncUi { jobs, last, ..Default::default() }
    }
}

fn arrow(d: Direction) -> &'static str {
    match d {
        Direction::ToComputer => "→",
        Direction::Both => "⇄",
    }
}

/// Small square choice button, as in the toolbar view switch.
fn chip(id: impl Into<ElementId>, label: String, on: bool, enabled: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(26.))
        .px_3()
        .flex()
        .flex_none()
        .items_center()
        .border_1()
        .border_color(if on { rgba(0x5fd4a057) } else { line() })
        .bg(if on { acc_s() } else { wash() })
        .text_size(px(12.))
        .text_color(if !enabled { faint() } else if on { acc() } else { dim() })
        .when(enabled, |d| d.cursor_pointer().hover(|d| d.bg(wash2())))
        .child(label)
}

fn field_label(s: &'static str) -> Div {
    div().w(px(112.)).flex_none().text_color(faint()).text_size(px(12.)).child(s)
}

fn field(input: &Entity<InputState>) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .h(px(30.))
        .px_2()
        .flex()
        .items_center()
        .bg(rgba(0x0000002e))
        .border_1()
        .border_color(line())
        .font_family(mono())
        .text_size(px(12.))
        .child(Input::new(input).appearance(false).small())
}

impl Hatch {
    fn root_label(&self, root: &str) -> String {
        if root == "media" {
            return "Media".into();
        }
        self.apps.iter().find(|a| a.bundle_id == root).map(|a| a.name.clone()).unwrap_or_else(|| root.to_string())
    }

    pub fn open_sync_editor(&mut self, job: Option<SyncJob>, window: &mut Window, cx: &mut Context<Self>) {
        let j = job.clone().unwrap_or_else(|| SyncJob {
            id: String::new(),
            name: String::new(),
            root: "media".into(),
            phone_path: "/DCIM".into(),
            local: "~/Pictures/iPhone".into(),
            direction: Direction::ToComputer,
            auto: false,
            udid: None,
        });
        let input = |v: String, ph: &'static str, window: &mut Window, cx: &mut Context<Self>| cx.new(|cx| InputState::new(window, cx).placeholder(ph).default_value(v));
        let name = input(j.name.clone(), "Job name", window, cx);
        let path = input(j.phone_path.clone(), "/DCIM", window, cx);
        let local = input(j.local.clone(), "~/Pictures/iPhone", window, cx);
        self.sync.edit = Some(Editor {
            id: job.map(|j| j.id),
            name,
            path,
            local,
            root: j.root,
            direction: j.direction,
            auto: j.auto,
            error: None,
        });
        self.sync.confirm = None;
        cx.notify();
    }

    fn pick_sync_root(&mut self, root: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(e) = &mut self.sync.edit else { return };
        let was_media = e.root == "media";
        e.root = root;
        let media = e.root == "media";
        if media {
            e.direction = Direction::ToComputer;
        }
        // Swap the default phone folder when it is still the default of the other kind.
        let cur = e.path.read(cx).value().to_string();
        if media != was_media && (cur == "/DCIM" || cur == "/" || cur.is_empty()) {
            let v = if media { "/DCIM" } else { "/" };
            e.path.update(cx, |s, cx| s.set_value(v, window, cx));
        }
        cx.notify();
    }

    fn choose_sync_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Sync with this folder".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut p))) = rx.await else { return };
            if p.is_empty() {
                return;
            }
            let dir = p.remove(0);
            let home = std::env::var("HOME").unwrap_or_default();
            let text = match dir.strip_prefix(&home) {
                Ok(rest) if !home.is_empty() => format!("~/{}", rest.display()),
                _ => dir.display().to_string(),
            };
            this.update_in(cx, |this, window, cx| {
                if let Some(e) = &this.sync.edit {
                    e.local.update(cx, |s, cx| s.set_value(text, window, cx));
                }
            })
            .ok();
        })
        .detach();
    }

    fn save_sync_editor(&mut self, cx: &mut Context<Self>) {
        let Some(e) = &self.sync.edit else { return };
        let old = e.id.as_ref().and_then(|id| self.sync.jobs.iter().find(|j| &j.id == id)).cloned();
        let job = SyncJob {
            id: e.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()[..12].to_string()),
            name: e.name.read(cx).value().trim().to_string(),
            root: e.root.clone(),
            phone_path: e.path.read(cx).value().trim().to_string(),
            local: e.local.read(cx).value().trim().to_string(),
            direction: e.direction,
            auto: e.auto,
            udid: old.as_ref().and_then(|o| o.udid.clone()).or_else(|| self.udid.clone()),
        };
        if let Err(err) = job.validate() {
            if let Some(e) = &mut self.sync.edit {
                e.error = Some(err.to_string());
            }
            cx.notify();
            return;
        }
        // A job that now points somewhere else starts over: its old state does not apply.
        if let Some(o) = &old
            && (o.root != job.root || o.phone_path != job.phone_path || sync::expand(&o.local) != job.local_dir() || o.direction != job.direction)
        {
            sync::forget_state(&job.id);
            self.sync.last.remove(&job.id);
        }
        let mut jobs = self.sync.jobs.clone();
        match jobs.iter_mut().find(|j| j.id == job.id) {
            Some(j) => *j = job,
            None => jobs.push(job),
        }
        if let Err(err) = sync::save_jobs(&jobs) {
            if let Some(e) = &mut self.sync.edit {
                e.error = Some(format!("Could not save the job list: {err:#}"));
            }
            cx.notify();
            return;
        }
        self.sync.jobs = jobs;
        self.sync.edit = None;
        cx.notify();
    }

    fn delete_sync_job(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.sync.confirm.as_deref() != Some(id) {
            self.sync.confirm = Some(id.to_string());
            cx.notify();
            return;
        }
        let mut jobs = self.sync.jobs.clone();
        jobs.retain(|j| j.id != id);
        if let Err(e) = sync::save_jobs(&jobs) {
            self.error = Some(format!("Could not save the job list: {e:#}"));
        } else {
            sync::forget_state(id);
            self.sync.jobs = jobs;
            self.sync.last.remove(id);
        }
        self.sync.confirm = None;
        if self.sync.preview.as_ref().is_some_and(|p| p.id == id) {
            self.sync.preview = None;
        }
        cx.notify();
    }

    /// The phone to run a job on: its own phone when connected, else the selected phone.
    fn sync_udid(&self, job: &SyncJob) -> Option<String> {
        let paired = |u: &str| self.devices.iter().any(|d| d.udid == u && d.paired);
        match &job.udid {
            Some(u) if paired(u) => Some(u.clone()),
            _ => self.udid.clone().filter(|u| paired(u)),
        }
    }

    /// "Sync now". The first run of a job shows a preview first.
    fn sync_now(&mut self, id: &str, cx: &mut Context<Self>) {
        if sync::load_state(id).is_none() {
            self.preview_sync(id, cx);
        } else {
            self.start_sync(id, cx);
        }
    }

    fn preview_sync(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(job) = self.sync.jobs.iter().find(|j| j.id == id).cloned() else { return };
        let Some(udid) = self.sync_udid(&job) else { return };
        self.sync.preview = Some(PreviewBox { id: id.to_string(), result: None });
        cx.notify();
        let id = id.to_string();
        run(cx, async move { sync::preview(&udid, &job).await.map_err(|e| device::explain(&e)) }, move |this, r, cx| {
            if let Some(p) = &mut this.sync.preview
                && p.id == id
            {
                p.result = Some(r);
            }
            cx.notify();
        });
    }

    pub fn start_sync(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(job) = self.sync.jobs.iter().find(|j| j.id == id).cloned() else { return };
        if self.job_running() || sync::is_running() {
            if !self.sync.queue.contains(&job.id) {
                self.sync.queue.push_back(job.id);
            }
            return;
        }
        let Some(udid) = self.sync_udid(&job) else { return };
        if self.sync.preview.as_ref().is_some_and(|p| p.id == id) {
            self.sync.preview = None;
        }
        let t = Arc::new(device::Transfer::default());
        let place = short_path(&job.local_dir());
        self.job = Some(Job { upload: false, t: t.clone(), count: 0, place, outcome: None, sync: Some(job.name.clone()) });
        self.watch_job(cx);
        let t2 = t.clone();
        let job2 = job.clone();
        run(cx, async move { sync::run(&udid, &job2, &t2).await }, move |this, r, cx| {
            let msg = match r {
                Ok(rep) => {
                    let mut text = format!("{}: {}", job.name, rep.line());
                    if let Some(first) = rep.conflicts.first() {
                        text.push_str(&format!(" · {first}"));
                        if rep.conflicts.len() > 1 {
                            text.push_str(&format!(" and {} more", rep.conflicts.len() - 1));
                        }
                    }
                    if let Some((t, r)) = sync::last_result(&job.id) {
                        this.sync.last.insert(job.id.clone(), format!("{t} · {r}"));
                    }
                    if rep.stopped.as_deref().is_some_and(|s| s != "cancelled") { Err(text) } else { Ok(text) }
                }
                Err(e) => Err(format!("{}: {}", job.name, device::explain(&e))),
            };
            if let Some(j) = &mut this.job {
                j.outcome = Some(msg);
            }
            this.next_sync(cx);
            cx.notify();
        });
    }

    /// Start the next queued job when nothing else runs.
    pub fn next_sync(&mut self, cx: &mut Context<Self>) {
        if self.job_running() || sync::is_running() {
            return;
        }
        while let Some(id) = self.sync.queue.pop_front() {
            if self.sync.jobs.iter().any(|j| j.id == id) {
                self.start_sync(&id, cx);
                return;
            }
        }
    }

    /// The device poll saw a trusted phone appear: queue its auto jobs. A job that never ran
    /// waits for its first run by hand, with the preview.
    pub fn queue_auto_sync(&mut self, udid: &str, cx: &mut Context<Self>) {
        for j in &self.sync.jobs {
            let mine = j.udid.as_deref().is_none_or(|u| u == udid);
            if j.auto && mine && sync::load_state(&j.id).is_some() && !self.sync.queue.contains(&j.id) {
                self.sync.queue.push_back(j.id.clone());
            }
        }
        self.next_sync(cx);
    }

    pub fn sync_section(&self, cx: &mut Context<Self>) -> Div {
        let me = cx.entity();
        let mut page = v_flex();
        page = page.child(
            div()
                .px_6()
                .pb_3()
                .text_size(px(12.))
                .text_color(dim())
                .child("Copy files between a folder on the iPhone and a folder on this computer. A job never deletes files. When a file changed on both sides, both copies stay."),
        );
        let connected = self.current().is_some_and(|d| d.paired);
        let busy = self.job_running();
        for j in self.sync.jobs.clone() {
            let id = j.id.clone();
            let editing = self.sync.edit.as_ref().is_some_and(|e| e.id.as_deref() == Some(&id));
            if editing {
                page = page.child(self.sync_editor(cx));
                continue;
            }
            let can_run = connected && self.sync_udid(&j).is_some();
            let last = self.sync.last.get(&id).cloned().unwrap_or_else(|| "Never run".into());
            let confirm = self.sync.confirm.as_deref() == Some(&id);
            let (i1, i2, i3) = (id.clone(), id.clone(), id.clone());
            let j2 = j.clone();
            let me2 = me.clone();
            let mut row = v_flex().px_6().py_3().gap_1().border_b_1().border_color(line()).child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(div().size(px(28.)).flex_none().flex().items_center().justify_center().bg(wash()).child(Icon::empty().path("icons/folder-sync.svg").small().text_color(if j.auto { acc() } else { faint() })))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().truncate().text_color(ink()).child(j.name.clone()))
                                    .when(j.auto, |d| d.child(div().px_1p5().text_size(px(10.)).text_color(acc()).bg(acc_s()).child("AUTO"))),
                            )
                            .child(div().truncate().font_family(mono()).text_size(px(11.)).text_color(faint()).child(format!("{}:{} {} {}", self.root_label(&j.root), j.phone_path, arrow(j.direction), j.local))),
                    )
                    .child(
                        bar_btn_look(SharedString::from(format!("sync-edit-{id}")), "Edit", Btn::Quiet, !busy).on_click(move |_, window, cx| {
                            let j = j2.clone();
                            if !busy {
                                me2.update(cx, |this, cx| this.open_sync_editor(Some(j), window, cx));
                            }
                        }),
                    )
                    .child(self.bar_btn(SharedString::from(format!("sync-del-{id}")), if confirm { "Confirm delete" } else { "Delete" }, Btn::Quiet, true, cx, move |t, cx| t.delete_sync_job(&i1, cx)))
                    .child(self.bar_btn(SharedString::from(format!("sync-run-{id}")), "Sync now", Btn::Primary("icons/refresh-cw.svg"), can_run && !busy, cx, move |t, cx| t.sync_now(&i2, cx))),
            );
            row = row.child(div().pl(px(40.)).truncate().text_size(px(11.)).text_color(faint()).child(last));
            if let Some(p) = self.sync.preview.as_ref().filter(|p| p.id == id) {
                row = row.child(self.preview_box(p, i3, cx));
            }
            page = page.child(row);
        }
        if self.sync.jobs.is_empty() && self.sync.edit.is_none() {
            page = page.child(div().px_6().py_3().text_size(px(12.)).text_color(faint()).child("No sync jobs yet."));
        }
        if self.sync.edit.as_ref().is_some_and(|e| e.id.is_none()) {
            page = page.child(self.sync_editor(cx));
        } else if self.sync.edit.is_none() {
            let me3 = me.clone();
            page = page.child(
                h_flex().px_6().py_3().child(bar_btn_look("sync-add", "Add sync job", Btn::Quiet, true).child(Icon::empty().path("icons/plus.svg").small()).on_click(move |_, window, cx| {
                    me3.update(cx, |this, cx| this.open_sync_editor(None, window, cx));
                })),
            );
        }
        if !connected {
            page = page.child(div().px_6().pb_3().text_size(px(11.)).text_color(faint()).child("Connect and trust an iPhone to run a job."));
        }
        page
    }

    fn preview_box(&self, p: &PreviewBox, id: String, cx: &mut Context<Self>) -> Div {
        let body = match &p.result {
            None => div().text_color(dim()).child("Comparing the phone and the folder…").into_any_element(),
            Some(Err(e)) => div().text_color(cor()).child(e.clone()).into_any_element(),
            Some(Ok(s)) => {
                let mut v = v_flex().gap_1().child(div().text_color(ink()).child(format!("First run: {}", s.line())));
                if s.to_phone > 0 {
                    v = v.child(div().text_size(px(11.)).text_color(amb()).child(format!("{} file(s) go to the phone.", s.to_phone)));
                }
                if s.conflicts > 0 {
                    v = v.child(div().text_size(px(11.)).text_color(amb()).child("Files that differ on both sides are kept twice. The extra copy gets \"(conflict date)\" in its name."));
                }
                v.into_any_element()
            }
        };
        let ready = matches!(p.result, Some(Ok(_)));
        let busy = self.job_running();
        h_flex()
            .ml(px(40.))
            .mt_2()
            .p_3()
            .gap_3()
            .items_center()
            .bg(rgba(0x00000033))
            .border_1()
            .border_color(line())
            .text_size(px(12.))
            .child(div().flex_1().min_w_0().child(body))
            .child(self.bar_btn("sync-preview-cancel", "Cancel", Btn::Quiet, true, cx, |t, cx| {
                t.sync.preview = None;
                cx.notify();
            }))
            .child(self.bar_btn("sync-preview-run", "Run", Btn::Primary("icons/refresh-cw.svg"), ready && !busy, cx, move |t, cx| t.start_sync(&id, cx)))
    }

    fn sync_editor(&self, cx: &mut Context<Self>) -> Div {
        let Some(e) = &self.sync.edit else { return div() };
        let me = cx.entity();
        let mut roots: Vec<(String, String)> = vec![("media".into(), "Media".into())];
        roots.extend(self.apps.iter().map(|a| (a.bundle_id.clone(), a.name.clone())));
        if !roots.iter().any(|(k, _)| k == &e.root) {
            roots.push((e.root.clone(), e.root.clone()));
        }
        let mut root_row = h_flex().flex_1().min_w_0().flex_wrap().gap_1();
        for (i, (key, label)) in roots.into_iter().enumerate() {
            let me = me.clone();
            let on = e.root == key;
            root_row = root_row.child(chip(("sync-root", i), label, on, true).on_click(move |_, window, cx| {
                let k = key.clone();
                me.update(cx, |this, cx| this.pick_sync_root(k, window, cx));
            }));
        }
        let media = e.root == "media";
        let dir_chip = |id: &'static str, label: &str, d: Direction, enabled: bool| {
            let me = me.clone();
            chip(id, label.to_string(), e.direction == d, enabled).on_click(move |_, _, cx| {
                if enabled {
                    me.update(cx, |this, cx| {
                        if let Some(e) = &mut this.sync.edit {
                            e.direction = d;
                        }
                        cx.notify();
                    })
                }
            })
        };
        let me_f = me.clone();
        let me_a = me.clone();
        let auto = e.auto;
        let row = |label: &'static str| h_flex().gap_3().items_center().child(field_label(label));
        v_flex()
            .mx_6()
            .my_3()
            .p_4()
            .gap_3()
            .bg(rgba(0x00000033))
            .border_1()
            .border_color(line())
            .text_size(px(12.))
            .child(div().text_color(ink()).font_weight(FontWeight::MEDIUM).child(if e.id.is_some() { "Edit sync job" } else { "New sync job" }))
            .child(row("Name").child(field(&e.name)))
            .child(row("Phone location").child(root_row))
            .child(row("Phone folder").child(field(&e.path)))
            .child(
                row("Computer folder").child(field(&e.local)).child(bar_btn_look("sync-choose", "Choose…", Btn::Quiet, true).on_click(move |_, window, cx| {
                    me_f.update(cx, |this, cx| this.choose_sync_folder(window, cx));
                })),
            )
            .child(
                row("Direction")
                    .child(dir_chip("sync-dir-down", "Phone → computer", Direction::ToComputer, true))
                    .child(dir_chip("sync-dir-both", "Both ways", Direction::Both, !media))
                    .when(media, |d| d.child(div().text_size(px(11.)).text_color(faint()).child("Media is one way. Hatch never writes to DCIM."))),
            )
            .child(
                row("Auto on plug-in").child(
                    h_flex()
                        .id("sync-auto")
                        .gap_2()
                        .items_center()
                        .cursor_pointer()
                        .child(switch(auto, acc(), acc_s(), rgba(0x5fd4a057)))
                        .child(div().text_size(px(11.)).text_color(faint()).child("Runs when this iPhone is plugged in, after the first run by hand."))
                        .on_click(move |_, _, cx| {
                            me_a.update(cx, |this, cx| {
                                if let Some(e) = &mut this.sync.edit {
                                    e.auto = !e.auto;
                                }
                                cx.notify();
                            })
                        }),
                ),
            )
            .when_some(e.error.clone(), |d, err| d.child(div().text_color(cor()).child(err)))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(self.bar_btn("sync-edit-cancel", "Cancel", Btn::Quiet, true, cx, |t, cx| {
                        t.sync.edit = None;
                        cx.notify();
                    }))
                    .child(self.bar_btn("sync-edit-save", "Save", Btn::Primary("icons/check.svg"), true, cx, |t, cx| t.save_sync_editor(cx))),
            )
    }
}
