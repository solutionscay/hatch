//! Sync jobs: pair a phone folder with a folder on this computer.
//!
//! Change detection compares each side with the size and modified time saved at the last
//! run. Rules:
//! - A job never deletes a file. A file missing on one side is copied from the other side.
//! - Changed on one side only: the change is copied over the other side.
//! - Changed on both sides (a conflict): both copies stay.
//!   - Both ways: the phone copy keeps the name on both sides. The computer copy is renamed to
//!     "name (conflict YYYY-MM-DD).ext" and goes to the phone too.
//!   - Phone to computer: the incoming phone copy lands as "name (conflict YYYY-MM-DD).ext".
//!     The computer copy keeps the name and is never replaced while it differs.
//! - The media partition (DCIM) is phone to computer only. Hatch never writes there.
//!
//! The job list is ~/.config/hatch/sync-jobs.json. Each job keeps its per-file state in
//! ~/.config/hatch/sync-state/<job-id>.json. Both are written to a temp file, then renamed.

use crate::device::{self, Root, Stat, Transfer};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    ToComputer,
    Both,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SyncJob {
    pub id: String,
    pub name: String,
    /// "media" or an app bundle id.
    pub root: String,
    /// Folder inside the root, for example "/DCIM" or "/".
    pub phone_path: String,
    /// Folder on this computer. "~" means the home folder.
    pub local: String,
    pub direction: Direction,
    /// Run when the phone is plugged in.
    #[serde(default)]
    pub auto: bool,
    /// The phone the job was made for. Auto runs only on this phone.
    #[serde(default)]
    pub udid: Option<String>,
}

impl SyncJob {
    pub fn root(&self) -> Root {
        Root::parse(&self.root)
    }

    /// True when a run can write to the phone.
    pub fn writes_phone(&self) -> bool {
        self.direction == Direction::Both && self.root() != Root::Media
    }

    pub fn local_dir(&self) -> PathBuf {
        expand(&self.local)
    }

    /// Check a job before it is saved or run.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("Give the job a name.");
        }
        if !self.phone_path.starts_with('/') {
            bail!("The phone folder must start with /.");
        }
        if self.phone_path.split('/').any(|p| p == "..") {
            bail!("The phone folder cannot contain \"..\".");
        }
        if self.local.trim().is_empty() {
            bail!("Choose a folder on this computer.");
        }
        if !self.local_dir().is_absolute() {
            bail!("The computer folder must be a full path, for example ~/Pictures/iPhone.");
        }
        if self.root() == Root::Media && self.direction != Direction::ToComputer {
            bail!("Media folders sync from the phone to the computer only.");
        }
        Ok(())
    }
}

pub fn expand(p: &str) -> PathBuf {
    let p = p.trim();
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    if p == "~" {
        PathBuf::from(home)
    } else if let Some(rest) = p.strip_prefix("~/") {
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(p)
    }
}

fn config_dir() -> PathBuf {
    expand("~/.config/hatch")
}

fn jobs_file() -> PathBuf {
    config_dir().join("sync-jobs.json")
}

fn state_file(id: &str) -> PathBuf {
    config_dir().join("sync-state").join(format!("{id}.json"))
}

/// Write `text` to a temp file next to `path`, then rename it over `path`.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load_jobs() -> Vec<SyncJob> {
    std::fs::read_to_string(jobs_file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save_jobs(jobs: &[SyncJob]) -> Result<()> {
    write_atomic(&jobs_file(), &serde_json::to_string_pretty(jobs)?)
}

/// Remove a job's state. Its files stay where they are.
pub fn forget_state(id: &str) {
    std::fs::remove_file(state_file(id)).ok();
}

/// What both sides held at the last run, for one file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Seen {
    pub phone: Option<Stat>,
    pub local: Option<Stat>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Local time of the last run that finished or stopped.
    pub last_run: Option<String>,
    /// One-line result of that run.
    pub last_result: Option<String>,
    pub files: BTreeMap<String, Seen>,
}

pub fn load_state(id: &str) -> Option<State> {
    std::fs::read_to_string(state_file(id)).ok().and_then(|t| serde_json::from_str(&t).ok())
}

fn save_state(id: &str, s: &State) -> Result<()> {
    write_atomic(&state_file(id), &serde_json::to_string(s)?)
}

/// The last run time and result of a job, without its file list. None if it never ran.
pub fn last_result(id: &str) -> Option<(String, String)> {
    let s = load_state(id)?;
    Some((s.last_run.unwrap_or_default(), s.last_result.unwrap_or_default()))
}

/// Partial copies, and hidden files such as .DS_Store or iOS .MISC folders, are not synced.
fn skip(name: &str) -> bool {
    name.starts_with('.') || name.ends_with(".part")
}

fn local_walk(dir: &Path) -> Result<Vec<(String, Stat)>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    let mut todo = vec![(dir.to_path_buf(), String::new())];
    while let Some((d, rel)) = todo.pop() {
        for e in std::fs::read_dir(&d).with_context(|| format!("cannot read {}", d.display()))? {
            let e = e?;
            let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
            if skip(&name) {
                continue;
            }
            let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let ft = e.file_type()?;
            if ft.is_dir() {
                todo.push((e.path(), r));
            } else if ft.is_file() {
                out.push((r, local_stat(&e.path())?));
            }
        }
    }
    Ok(out)
}

fn local_stat(p: &Path) -> Result<Stat> {
    let m = std::fs::metadata(p)?;
    let mtime = m.modified()?.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0);
    Ok(Stat { size: m.len(), mtime })
}

/// Same file on one side as at the last run. Whole seconds: some file systems round.
fn same(a: Option<Stat>, b: Option<Stat>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.size == b.size && a.mtime.div_euclid(1_000_000_000) == b.mtime.div_euclid(1_000_000_000),
        (None, None) => true,
        _ => false,
    }
}

/// The two sides hold the same file: same size, times within 2 s.
fn matches(p: Stat, l: Stat) -> bool {
    p.size == l.size && (p.mtime - l.mtime).abs() < 2_000_000_000
}

#[derive(Clone, Debug, PartialEq)]
enum Act {
    /// Phone file to the computer under the same path.
    Down { rel: String, new: bool },
    /// Computer file to the phone under the same path.
    Up { rel: String, new: bool },
    /// Both ways, changed on both sides: rename the computer copy, bring the phone copy down,
    /// send the renamed copy up.
    ConflictBoth { rel: String },
    /// Phone to computer, changed on both sides: bring the phone copy down beside it.
    ConflictDown { rel: String },
    /// Both sides already match. Record them.
    Adopt { rel: String },
    /// Gone from both sides. Drop it from the state.
    Forget { rel: String },
}

/// What a run will do. The preview and the run use the same plan.
pub struct Plan {
    acts: Vec<Act>,
    phone: BTreeMap<String, Stat>,
    local: BTreeMap<String, Stat>,
    state: State,
    first_run: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Summary {
    pub new: usize,
    pub changed: usize,
    pub conflicts: usize,
    /// Bytes to copy, both directions.
    pub bytes: u64,
    pub to_computer: usize,
    pub to_phone: usize,
    pub first_run: bool,
}

impl Summary {
    pub fn line(&self) -> String {
        if self.new + self.changed + self.conflicts == 0 {
            return "Up to date. Nothing to copy.".into();
        }
        let mut parts = vec![format!("{} new", self.new), format!("{} changed", self.changed)];
        if self.conflicts > 0 {
            parts.push(format!("{} conflict{}", self.conflicts, if self.conflicts == 1 { "" } else { "s" }));
        }
        format!("{} · {}", parts.join(" · "), crate::human(self.bytes))
    }
}

impl Plan {
    pub fn summary(&self) -> Summary {
        let mut s = Summary { first_run: self.first_run, ..Default::default() };
        let psize = |r: &str| self.phone.get(r).map_or(0, |x| x.size);
        let lsize = |r: &str| self.local.get(r).map_or(0, |x| x.size);
        for a in &self.acts {
            match a {
                Act::Down { rel, new } => {
                    if *new { s.new += 1 } else { s.changed += 1 }
                    s.bytes += psize(rel);
                    s.to_computer += 1;
                }
                Act::Up { rel, new } => {
                    if *new { s.new += 1 } else { s.changed += 1 }
                    s.bytes += lsize(rel);
                    s.to_phone += 1;
                }
                Act::ConflictBoth { rel } => {
                    s.conflicts += 1;
                    s.bytes += psize(rel) + lsize(rel);
                    s.to_computer += 1;
                    s.to_phone += 1;
                }
                Act::ConflictDown { rel } => {
                    s.conflicts += 1;
                    s.bytes += psize(rel);
                    s.to_computer += 1;
                }
                Act::Adopt { .. } | Act::Forget { .. } => {}
            }
        }
        s
    }
}

/// Scan both sides and decide what to copy. Copies nothing.
pub async fn plan(udid: &str, job: &SyncJob) -> Result<Plan> {
    job.validate()?;
    let phone: BTreeMap<String, Stat> = device::walk(udid, &job.root(), &job.phone_path, skip).await?.into_iter().collect();
    let dir = job.local_dir();
    let local: BTreeMap<String, Stat> = tokio::task::spawn_blocking(move || local_walk(&dir)).await??.into_iter().collect();
    let saved = load_state(&job.id);
    let first_run = saved.is_none();
    let state = saved.unwrap_or_default();
    let acts = decide(&phone, &local, &state, job.writes_phone());
    Ok(Plan { acts, phone, local, state, first_run })
}

/// The plan rules, without any I/O.
fn decide(phone: &BTreeMap<String, Stat>, local: &BTreeMap<String, Stat>, state: &State, both: bool) -> Vec<Act> {
    let names: BTreeSet<&String> = phone.keys().chain(local.keys()).chain(state.files.keys()).collect();
    let mut acts = Vec::new();
    for rel in names {
        let (p, l) = (phone.get(rel).copied(), local.get(rel).copied());
        let seen = state.files.get(rel);
        let rel = rel.clone();
        match (p, l, seen) {
            (None, None, _) => acts.push(Act::Forget { rel }),
            // Missing on one side: new there, or deleted there. Deletes never propagate, so
            // the file comes back from the other side.
            (Some(_), None, _) => acts.push(Act::Down { new: true, rel }),
            (None, Some(_), _) => {
                if both {
                    acts.push(Act::Up { new: true, rel });
                }
            }
            (Some(p), Some(l), None) => {
                if matches(p, l) {
                    acts.push(Act::Adopt { rel });
                } else if both {
                    acts.push(Act::ConflictBoth { rel });
                } else {
                    acts.push(Act::ConflictDown { rel });
                }
            }
            (Some(p), Some(l), Some(s)) => {
                let pc = !same(Some(p), s.phone);
                let lc = !same(Some(l), s.local);
                match (pc, lc) {
                    (false, false) => {}
                    // Changed back to the same file on both sides, or a copy that finished
                    // before the state was saved.
                    _ if matches(p, l) => acts.push(Act::Adopt { rel }),
                    (true, false) => acts.push(Act::Down { rel, new: false }),
                    (false, true) => {
                        if both {
                            acts.push(Act::Up { rel, new: false });
                        }
                    }
                    (true, true) => acts.push(if both { Act::ConflictBoth { rel } } else { Act::ConflictDown { rel } }),
                }
            }
        }
    }
    acts
}

/// Result of one run.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub to_computer: usize,
    pub to_phone: usize,
    pub bytes: u64,
    /// Relative paths of files changed on both sides, with the name of the extra copy.
    pub conflicts: Vec<String>,
    /// Set when the run stopped early: cancelled, or an error.
    pub stopped: Option<String>,
}

impl Report {
    pub fn line(&self) -> String {
        let mut s = if self.to_computer + self.to_phone == 0 && self.conflicts.is_empty() {
            "Up to date. Nothing to copy.".to_string()
        } else {
            let mut parts = vec![format!("{} to computer", self.to_computer)];
            if self.to_phone > 0 {
                parts.push(format!("{} to phone", self.to_phone));
            }
            if !self.conflicts.is_empty() {
                parts.push(format!("{} conflict{} kept", self.conflicts.len(), if self.conflicts.len() == 1 { "" } else { "s" }));
            }
            format!("{} · {}", parts.join(" · "), crate::human(self.bytes))
        };
        if let Some(why) = &self.stopped {
            s = format!("Stopped: {why}. Done so far: {s}");
        }
        s
    }
}

/// "a/IMG 1.HEIC" -> "a/IMG 1 (conflict 2026-11-10).HEIC", with " (2)" and up if taken.
fn conflict_name(rel: &str, taken: impl Fn(&str) -> bool) -> String {
    let (dir, name) = match rel.rfind('/') {
        Some(i) => (&rel[..=i], &rel[i + 1..]),
        None => ("", rel),
    };
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let day = chrono::Local::now().format("%Y-%m-%d");
    let base = format!("{stem} (conflict {day}){ext}");
    let leaf = device::free_name(&base, |n| taken(&format!("{dir}{n}")));
    format!("{dir}{leaf}")
}

fn phone_path(job: &SyncJob, rel: &str) -> String {
    let base = job.phone_path.trim_end_matches('/');
    format!("{base}/{rel}")
}

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Held while a job runs. Only one job runs at a time, from the window or from an agent.
struct Running;

impl Running {
    fn take() -> Result<Running> {
        if RUNNING.swap(true, Ordering::SeqCst) {
            bail!("Another sync job is running. Try again when it finishes.");
        }
        Ok(Running)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// Preview a job: scan both sides and count what a run would do.
pub async fn preview(udid: &str, job: &SyncJob) -> Result<Summary> {
    Ok(plan(udid, job).await?.summary())
}

/// Run a job. Progress goes to `t`; `t.cancel` stops it after the current chunk. Files that
/// finished stay, and their state is saved. A stop or an error is in `Report::stopped`.
pub async fn run(udid: &str, job: &SyncJob, t: &Transfer) -> Result<Report> {
    let _guard = Running::take()?;
    let root = job.root();
    let dir = job.local_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot make {}", dir.display()))?;
    let Plan { acts, phone, local, mut state, .. } = plan(udid, job).await?;
    let mut report = Report::default();

    // Taken names on each side, so a conflict copy never replaces a file.
    let mut taken: BTreeSet<String> = phone.keys().chain(local.keys()).cloned().collect();
    // (phone path, local file, relative path, Some(old local state) for a one-way conflict)
    let mut downs: Vec<(String, PathBuf, String, Option<Option<Stat>>)> = Vec::new();
    let mut ups: Vec<(PathBuf, String, String)> = Vec::new();
    let mut prep_err = None;
    for a in &acts {
        match a {
            Act::Down { rel, .. } => downs.push((phone_path(job, rel), dir.join(rel), rel.clone(), None)),
            Act::Up { rel, .. } => ups.push((dir.join(rel), phone_path(job, rel), rel.clone())),
            Act::ConflictBoth { rel } => {
                let alt = conflict_name(rel, |n| taken.contains(n));
                taken.insert(alt.clone());
                // Rename the computer copy first. Nothing is lost if a later step fails.
                if let Err(e) = std::fs::rename(dir.join(rel), dir.join(&alt)) {
                    prep_err = Some(format!("cannot rename {rel}: {e}"));
                    break;
                }
                state.files.remove(rel);
                report.conflicts.push(format!("{rel} -> {alt}"));
                downs.push((phone_path(job, rel), dir.join(rel), rel.clone(), None));
                ups.push((dir.join(&alt), phone_path(job, &alt), alt));
            }
            Act::ConflictDown { rel } => {
                let alt = conflict_name(rel, |n| taken.contains(n));
                taken.insert(alt.clone());
                report.conflicts.push(format!("{rel} -> {alt}"));
                let old = state.files.get(rel).and_then(|s| s.local);
                downs.push((phone_path(job, rel), dir.join(&alt), rel.clone(), Some(old)));
            }
            Act::Adopt { rel } => {
                state.files.insert(rel.clone(), Seen { phone: phone.get(rel).copied(), local: local.get(rel).copied() });
            }
            Act::Forget { rel } => {
                state.files.remove(rel);
            }
        }
    }
    t.files_total.store(downs.len() + ups.len(), Ordering::Relaxed);

    let mut stopped = prep_err;
    // Phone to computer.
    if stopped.is_none() && !downs.is_empty() {
        for (_, dest, _, _) in &downs {
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p).ok();
            }
        }
        let list: Vec<(String, PathBuf)> = downs.iter().map(|(r, d, _, _)| (r.clone(), d.clone())).collect();
        let res = device::download(udid, &root, &list, t).await;
        // download copies in order; files_done counts the ones that finished.
        let done = t.files_done.load(Ordering::Relaxed).min(downs.len());
        for (_, dest, rel, conflict) in &downs[..done] {
            let Ok(l) = local_stat(dest) else { continue };
            report.to_computer += 1;
            report.bytes += l.size;
            let local = match conflict {
                // One-way conflict: keep the old computer state, so the computer copy still
                // counts as changed and is never replaced.
                Some(old) => *old,
                None => Some(l),
            };
            state.files.insert(rel.clone(), Seen { phone: phone.get(rel).copied(), local });
        }
        if let Err(e) = res {
            stopped = Some(stop_reason(&e));
        }
    }
    // Computer to phone.
    if stopped.is_none() && !ups.is_empty() {
        if !job.writes_phone() {
            bail!("this job does not write to the phone");
        }
        let before = t.files_done.load(Ordering::Relaxed);
        let list: Vec<(PathBuf, String)> = ups.iter().map(|(l, r, _)| (l.clone(), r.clone())).collect();
        let res = device::upload_to(udid, &root, &list, t).await;
        let stats = match &res {
            Ok(s) => s.clone(),
            Err(_) => Vec::new(),
        };
        let done = (t.files_done.load(Ordering::Relaxed) - before).min(ups.len());
        for (i, (src, _, rel)) in ups[..done].iter().enumerate() {
            let Ok(l) = local_stat(src) else { continue };
            report.to_phone += 1;
            report.bytes += l.size;
            // Without the read-back (the run stopped), the next run matches the two sides.
            let p = stats.get(i).copied();
            if p.is_some() {
                state.files.insert(rel.clone(), Seen { phone: p, local: Some(l) });
            }
        }
        if let Err(e) = res {
            stopped = Some(stop_reason(&e));
        }
    }
    report.stopped = stopped;
    state.last_run = Some(chrono::Local::now().format("%Y-%m-%d %H:%M").to_string());
    state.last_result = Some(report.line());
    save_state(&job.id, &state)?;
    Ok(report)
}

fn stop_reason(e: &anyhow::Error) -> String {
    if e.is::<device::Cancelled>() { "cancelled".into() } else { device::explain(e).replace('\n', " ") }
}

/// Jobs as an agent sees them.
pub fn list_view() -> serde_json::Value {
    let jobs: Vec<serde_json::Value> = load_jobs()
        .into_iter()
        .map(|j| {
            let last = last_result(&j.id);
            serde_json::json!({
                "id": j.id,
                "name": j.name,
                "root": j.root,
                "phone_path": j.phone_path,
                "local": j.local,
                "direction": j.direction,
                "auto_on_plug_in": j.auto,
                "writes_phone": j.writes_phone(),
                "last_run": last.as_ref().map(|l| l.0.clone()),
                "last_result": last.map(|l| l.1),
            })
        })
        .collect();
    serde_json::Value::Array(jobs)
}

/// Find a saved job by id or by name (case-insensitive).
pub fn find(key: &str) -> Result<SyncJob> {
    let jobs = load_jobs();
    jobs.iter()
        .find(|j| j.id == key)
        .or_else(|| jobs.iter().find(|j| j.name.eq_ignore_ascii_case(key)))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no sync job named {key:?}. Use list_sync_jobs."))
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    fn st(size: u64, secs: i64) -> Stat {
        Stat { size, mtime: secs * S }
    }

    fn map(v: &[(&str, Stat)]) -> BTreeMap<String, Stat> {
        v.iter().map(|(k, s)| (k.to_string(), *s)).collect()
    }

    fn seen(v: &[(&str, Option<Stat>, Option<Stat>)]) -> State {
        State { files: v.iter().map(|(k, p, l)| (k.to_string(), Seen { phone: *p, local: *l })).collect(), ..Default::default() }
    }

    #[test]
    fn rules() {
        let a = st(10, 100);
        let a2 = st(11, 200);
        let b = st(12, 300);
        // First run: new on each side, identical on both, different on both.
        let phone = map(&[("p", a), ("same", a), ("diff", a)]);
        let local = map(&[("l", b), ("same", st(10, 101)), ("diff", b)]);
        let acts = decide(&phone, &local, &State::default(), true);
        assert!(acts.contains(&Act::Down { rel: "p".into(), new: true }));
        assert!(acts.contains(&Act::Up { rel: "l".into(), new: true }));
        assert!(acts.contains(&Act::Adopt { rel: "same".into() }));
        assert!(acts.contains(&Act::ConflictBoth { rel: "diff".into() }));
        // One way: computer-only files stay put, a difference is a one-way conflict.
        let acts = decide(&phone, &local, &State::default(), false);
        assert!(!acts.iter().any(|x| matches!(x, Act::Up { .. })));
        assert!(acts.contains(&Act::ConflictDown { rel: "diff".into() }));

        // Later runs.
        let state = seen(&[
            ("none", Some(a), Some(a)),
            ("pchg", Some(a), Some(a)),
            ("lchg", Some(a), Some(a)),
            ("both", Some(a), Some(a)),
            ("ldel", Some(a), Some(a)),
            ("pdel", Some(a), Some(a)),
            ("gone", Some(a), Some(a)),
        ]);
        let phone = map(&[("none", a), ("pchg", a2), ("lchg", a), ("both", a2), ("ldel", a), ("lchg2", a)]);
        let local = map(&[("none", st(10, 100)), ("pchg", a), ("lchg", b), ("both", b), ("pdel", a)]);
        let acts = decide(&phone, &local, &state, true);
        let has = |x: Act| acts.contains(&x);
        assert!(!acts.iter().any(|x| matches!(x, Act::Down { rel, .. } | Act::Up { rel, .. } if rel == "none")));
        assert!(has(Act::Down { rel: "pchg".into(), new: false }));
        assert!(has(Act::Up { rel: "lchg".into(), new: false }));
        assert!(has(Act::ConflictBoth { rel: "both".into() }));
        // Deletes never propagate: the file comes back from the other side.
        assert!(has(Act::Down { rel: "ldel".into(), new: true }));
        assert!(has(Act::Up { rel: "pdel".into(), new: true }));
        assert!(has(Act::Forget { rel: "gone".into() }));
        // One way: a computer edit is left alone.
        let acts = decide(&phone, &local, &state, false);
        assert!(!acts.iter().any(|x| matches!(x, Act::Up { .. })));
        assert!(!acts.iter().any(|x| matches!(x, Act::Down { rel, .. } if rel == "lchg")));
        // One way after a conflict: the old computer state is kept, so a later phone change
        // is a conflict again and the computer copy is never replaced.
        let state = seen(&[("f", Some(a), None)]);
        let acts = decide(&map(&[("f", a2)]), &map(&[("f", b)]), &state, false);
        assert_eq!(acts, vec![Act::ConflictDown { rel: "f".into() }]);
        let acts = decide(&map(&[("f", a)]), &map(&[("f", b)]), &state, false);
        assert!(acts.is_empty());
    }

    #[test]
    fn conflict_names() {
        let day = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(conflict_name("a/IMG 1.HEIC", |_| false), format!("a/IMG 1 (conflict {day}).HEIC"));
        let taken = format!("notes (conflict {day})");
        assert_eq!(conflict_name("notes", |n| n == taken), format!("notes (conflict {day}) (2)"));
    }
}
