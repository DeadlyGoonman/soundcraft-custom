//! The saved plugin scan (Setup › Plugin Scanner): the VST3 and CLAP plugins found on disk, kept
//! in `plugin-cache.json` in the settings folder so a launch reads the list instead of loading
//! every plugin binary again.
//!
//! A scan lists the plugin files in the search folders and *probes* only the ones that need it:
//! an incremental scan probes files that are new or whose binary changed (modified time and
//! size), drops the ones that are gone, and skips files that failed before; a full rescan probes
//! everything. Probing loads the binary and reads its factory, which runs plugin code, so the
//! desktop app probes each file in a child process (`soundcraft --probe-plugin <format> <path>`,
//! see [`child_probe`]): a plugin that crashes or hangs takes only that process down (it is
//! killed after [`PROBE_TIMEOUT`]) and is remembered as failed instead of retried every scan.
//! No module is loaded into the app itself, so the VST3 rule that a module's entry and exit run
//! on the same (main) thread is untouched; the app still loads a binary on first use, as before.
//! The scan itself runs on a background thread ([`ScanJob`]) and never blocks the UI.

use serde::{Deserialize, Serialize};
use soundcraft_clap_host::ClapDescriptor;
use soundcraft_vst3_host::Vst3Descriptor;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The saved scan, in the settings folder.
pub const CACHE_FILE: &str = "plugin-cache.json";
/// Scanner preferences (extra folders, quick scan at launch), in the settings folder.
pub const SETTINGS_FILE: &str = "plugin-scan.json";
/// Bumped when the cache layout changes: an older or newer file is ignored and rescanned.
pub const CACHE_VERSION: u32 = 1;
/// The command-line switch of the probe process: `--probe-plugin vst3|clap <path>`.
pub const PROBE_ARG: &str = "--probe-plugin";
/// How long one plugin file may take to load in the probe process before it is killed.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(45);
/// Prefix of the probe process's result line on standard output (plugins may print too).
const PROBE_MARK: &str = "SOUNDCRAFT-PROBE ";

/// A plugin format the scanner handles. Audio Units are listed by the system, without loading
/// anything, so they are not cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Vst3,
    Clap,
}

impl Format {
    pub const ALL: [Format; 2] = [Format::Vst3, Format::Clap];

    pub fn name(self) -> &'static str {
        match self {
            Format::Vst3 => "VST3",
            Format::Clap => "CLAP",
        }
    }

    /// The probe process's argument for this format.
    pub fn arg(self) -> &'static str {
        match self {
            Format::Vst3 => "vst3",
            Format::Clap => "clap",
        }
    }

    pub fn from_arg(s: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|f| f.arg() == s)
    }
}

/// What tells a plugin file changed: its binary's modified time and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Stamp {
    pub mtime_ns: u64,
    pub size: u64,
}

impl Stamp {
    /// The stamp of the binary inside a plugin bundle (or of the file itself).
    pub fn of(format: Format, path: &Path) -> Stamp {
        let binary = match format {
            Format::Vst3 => soundcraft_vst3_host::scan::bundle_binary(path),
            Format::Clap => soundcraft_clap_host::scan::bundle_executable(path),
        };
        let meta = binary.as_deref().and_then(|b| std::fs::metadata(b).ok()).or_else(|| std::fs::metadata(path).ok());
        meta.map_or_else(Stamp::default, |m| Stamp {
            mtime_ns: m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64),
            size: m.len(),
        })
    }
}

/// The plugins one file holds.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Probed {
    pub vst3: Vec<Vst3Descriptor>,
    pub clap: Vec<ClapDescriptor>,
}

impl Probed {
    fn ids(&self) -> impl Iterator<Item = &str> {
        self.vst3.iter().map(|d| d.id.as_str()).chain(self.clap.iter().map(|d| d.id.as_str()))
    }
}

/// A plugin file and what it held when it was probed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedFile {
    pub format: Format,
    pub path: PathBuf,
    pub stamp: Stamp,
    pub plugins: Probed,
}

/// A plugin file that failed to load (or crashed or hung the probe), skipped by incremental scans
/// until it changes or a full rescan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailedFile {
    pub format: Format,
    pub path: PathBuf,
    pub stamp: Stamp,
    pub reason: String,
}

/// The saved scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginCache {
    pub version: u32,
    /// When the last scan finished (Unix seconds; 0 = never).
    pub scanned_at: u64,
    pub files: Vec<CachedFile>,
    pub failed: Vec<FailedFile>,
}

impl Default for PluginCache {
    fn default() -> Self {
        Self { version: CACHE_VERSION, scanned_at: 0, files: Vec::new(), failed: Vec::new() }
    }
}

impl PluginCache {
    /// Reads a saved scan. A missing, unreadable, corrupt or other-version file gives `None`
    /// (the caller then scans again).
    pub fn load(path: &Path) -> Option<PluginCache> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                log::warn!("plugin cache {}: {e}; scanning again", path.display());
                return None;
            }
        };
        match serde_json::from_str::<PluginCache>(&text) {
            Ok(c) if c.version == CACHE_VERSION => Some(c),
            Ok(c) => {
                log::info!("plugin cache {} is version {}, not {CACHE_VERSION}; scanning again", path.display(), c.version);
                None
            }
            Err(e) => {
                log::warn!("plugin cache {} is damaged ({e}); scanning again", path.display());
                None
            }
        }
    }

    /// Writes the scan (through a temporary file, so a crash mid-write leaves the old one).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        write_json(path, self)
    }

    /// The VST3 plugins, as the host lists them: the first of an id wins, sorted by name.
    pub fn vst3(&self) -> Vec<Vst3Descriptor> {
        let mut seen = HashSet::new();
        let mut v: Vec<Vst3Descriptor> = self.files.iter().flat_map(|f| &f.plugins.vst3).filter(|d| seen.insert(d.id.clone())).cloned().collect();
        v.sort_by_cached_key(|d| d.name.to_lowercase());
        v
    }

    /// The CLAP plugins, as the host lists them.
    pub fn clap(&self) -> Vec<ClapDescriptor> {
        let mut seen = HashSet::new();
        let mut v: Vec<ClapDescriptor> = self.files.iter().flat_map(|f| &f.plugins.clap).filter(|d| seen.insert(d.id.clone())).cloned().collect();
        v.sort_by_cached_key(|d| d.name.to_lowercase());
        v
    }

    /// How many plugins of `format` the scan found.
    pub fn count(&self, format: Format) -> usize {
        match format {
            Format::Vst3 => self.vst3().len(),
            Format::Clap => self.clap().len(),
        }
    }

    /// Hands the lists to the plugin hosts, which then don't scan on first use.
    pub fn apply_to_hosts(&self) {
        soundcraft_vst3_host::set_scanned(self.vst3());
        soundcraft_clap_host::set_scanned(self.clap());
    }

    fn ids(&self) -> HashSet<String> {
        self.files.iter().flat_map(|f| f.plugins.ids()).map(str::to_string).collect()
    }
}

/// Scanner preferences.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanSettings {
    /// After loading the saved scan at launch, look for new plugin files in the background.
    pub quick_scan_at_launch: bool,
    /// Folders scanned (for both formats) besides the standard ones.
    pub extra_dirs: Vec<PathBuf>,
}

impl ScanSettings {
    /// Reads the preferences; defaults when the file is missing or unreadable.
    pub fn load(path: &Path) -> ScanSettings {
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        write_json(path, self)
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The folders a scan looks in: each format's standard folders (and `VST3_PATH` / `CLAP_PATH`),
/// then `extra` for both formats.
pub fn search_dirs(extra: &[PathBuf]) -> Vec<(Format, PathBuf)> {
    let mut v: Vec<(Format, PathBuf)> = soundcraft_vst3_host::scan::default_search_paths().into_iter().map(|d| (Format::Vst3, d)).collect();
    v.extend(soundcraft_clap_host::scan::default_search_paths().into_iter().map(|d| (Format::Clap, d)));
    for d in extra {
        for f in Format::ALL {
            if !v.contains(&(f, d.clone())) {
                v.push((f, d.clone()));
            }
        }
    }
    v
}

/// A plugin file found in the search folders.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub format: Format,
    pub path: PathBuf,
    pub stamp: Stamp,
}

/// The plugin files in `dirs`, with their stamps.
pub fn list_candidates(dirs: &[(Format, PathBuf)]) -> Vec<Candidate> {
    let mut out = Vec::new();
    for format in Format::ALL {
        let mine: Vec<PathBuf> = dirs.iter().filter(|(f, _)| *f == format).map(|(_, d)| d.clone()).collect();
        let found = match format {
            Format::Vst3 => soundcraft_vst3_host::scan::find_bundles(&mine),
            Format::Clap => soundcraft_clap_host::scan::find_bundles(&mine),
        };
        let mut seen = HashSet::new();
        for path in found.into_iter().filter(|p| seen.insert(p.clone())) {
            out.push(Candidate { format, stamp: Stamp::of(format, &path), path });
        }
    }
    out
}

/// What a scan does with each file.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Unchanged files, kept as they are.
    pub keep: Vec<CachedFile>,
    /// Unchanged files that failed before, skipped.
    pub skip: Vec<FailedFile>,
    /// Files to probe.
    pub probe: Vec<Candidate>,
    /// Old entries of the files being probed again, restored if the scan is cancelled first.
    pub stale: Vec<CachedFile>,
    pub stale_failed: Vec<FailedFile>,
}

/// Sorts the files found now against the saved scan. Files no longer found are dropped; a full
/// scan probes everything.
pub fn plan(old: &PluginCache, now: &[Candidate], full: bool) -> Plan {
    let mut p = Plan::default();
    for c in now {
        let cached = old.files.iter().find(|f| f.format == c.format && f.path == c.path);
        let failed = old.failed.iter().find(|f| f.format == c.format && f.path == c.path);
        match (cached, failed) {
            (Some(f), _) if !full && f.stamp == c.stamp => p.keep.push(f.clone()),
            (_, Some(f)) if !full && f.stamp == c.stamp => p.skip.push(f.clone()),
            _ => {
                p.stale.extend(cached.cloned());
                p.stale_failed.extend(failed.cloned());
                p.probe.push(c.clone());
            }
        }
    }
    p
}

/// Probes one plugin file: what it holds, or why it can't be loaded.
pub type ProbeFn = Arc<dyn Fn(Format, &Path) -> Result<Probed, String> + Send + Sync>;
/// Lists the plugin files in the search folders ([`list_candidates`] outside tests).
pub type ListFn = Arc<dyn Fn(&[(Format, PathBuf)]) -> Vec<Candidate> + Send + Sync>;

/// What a scan changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Plugins (ids) that weren't in the saved scan.
    pub added: usize,
    /// Plugins of the saved scan that are gone.
    pub removed: usize,
    /// Files probed, and how many of them failed.
    pub probed: usize,
    pub failed: usize,
    pub cancelled: bool,
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.cancelled {
            write!(f, "Cancelled. ")?;
        }
        if self.added == 0 && self.removed == 0 && self.failed == 0 {
            return write!(f, "No changes ({} files checked)", self.probed);
        }
        write!(f, "{} new, {} removed", self.added, self.removed)?;
        if self.failed > 0 {
            write!(f, ", {} failed to load", self.failed)?;
        }
        Ok(())
    }
}

/// Runs a scan on the calling thread: lists the files in `dirs`, probes what [`plan`] says and
/// returns the new cache. `cancel` is checked before each probe; a cancelled scan keeps what it
/// probed and the old entries of the files it didn't get to. `progress` gets (done, total, file).
pub fn run_scan(
    old: &PluginCache,
    dirs: &[(Format, PathBuf)],
    full: bool,
    list: &ListFn,
    probe: &ProbeFn,
    cancel: &AtomicBool,
    mut progress: impl FnMut(usize, usize, &Path),
) -> (PluginCache, Summary) {
    let now = list(dirs);
    let Plan { keep: mut files, skip: mut failed, probe: todo, stale, stale_failed } = plan(old, &now, full);
    let mut summary = Summary::default();
    let total = todo.len();
    for (i, c) in todo.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            files.extend(stale.iter().filter(|f| f.format == c.format && f.path == c.path).cloned());
            failed.extend(stale_failed.iter().filter(|f| f.format == c.format && f.path == c.path).cloned());
            continue;
        }
        progress(i, total, &c.path);
        summary.probed += 1;
        match probe(c.format, &c.path) {
            Ok(plugins) => files.push(CachedFile { format: c.format, path: c.path, stamp: c.stamp, plugins }),
            Err(reason) => {
                log::warn!("plugin scan: {}: {reason}", c.path.display());
                summary.failed += 1;
                failed.push(FailedFile { format: c.format, path: c.path, stamp: c.stamp, reason });
            }
        }
    }
    progress(total, total, Path::new(""));
    files.sort_by(|a, b| (a.format.arg(), &a.path).cmp(&(b.format.arg(), &b.path)));
    failed.sort_by(|a, b| (a.format.arg(), &a.path).cmp(&(b.format.arg(), &b.path)));
    let cache = PluginCache { version: CACHE_VERSION, scanned_at: now_secs(), files, failed };
    let (before, after) = (old.ids(), cache.ids());
    summary.added = after.difference(&before).count();
    summary.removed = before.difference(&after).count();
    (cache, summary)
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// How far a running scan has got.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    /// The file being probed (empty while listing or when done).
    pub current: String,
}

#[derive(Default)]
struct Shared {
    progress: Mutex<Progress>,
    cancel: AtomicBool,
    result: Mutex<Option<(PluginCache, Summary)>>,
}

/// A scan running on a background thread.
pub struct ScanJob {
    shared: Arc<Shared>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ScanJob {
    pub fn start(old: PluginCache, dirs: Vec<(Format, PathBuf)>, full: bool, list: ListFn, probe: ProbeFn) -> ScanJob {
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        let handle = std::thread::Builder::new()
            .name("plugin-scan".into())
            .spawn(move || {
                let out = run_scan(&old, &dirs, full, &list, &probe, &s.cancel, |done, total, file| {
                    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    *s.progress.lock().unwrap_or_else(PoisonError::into_inner) = Progress { done, total, current: name };
                });
                *s.result.lock().unwrap_or_else(PoisonError::into_inner) = Some(out);
            })
            .map_err(|e| log::warn!("plugin scan: cannot start the scan thread: {e}"))
            .ok();
        ScanJob { shared, handle }
    }

    pub fn progress(&self) -> Progress {
        self.shared.progress.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Stops after the file being probed (its result is kept).
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    pub fn cancelling(&self) -> bool {
        self.shared.cancel.load(Ordering::Relaxed)
    }

    /// The result once the scan has finished (taken: `None` afterwards). A scan whose thread
    /// could not start finishes at once with no result.
    pub fn finished(&mut self) -> Option<(PluginCache, Summary)> {
        let out = self.shared.result.lock().unwrap_or_else(PoisonError::into_inner).take();
        if out.is_some()
            && let Some(h) = self.handle.take()
        {
            let _ = h.join();
        }
        out
    }

    /// Whether the job has ended (result ready or the thread never ran).
    pub fn is_done(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    /// Blocks until the scan ends (tests).
    pub fn wait(mut self) -> Option<(PluginCache, Summary)> {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.shared.result.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

/// Loads one plugin file in this process and reads its plugins.
pub fn probe_in_process(format: Format, path: &Path) -> Result<Probed, String> {
    match format {
        Format::Vst3 => soundcraft_vst3_host::load_bundle(path).map(|vst3| Probed { vst3, ..Probed::default() }).map_err(|e| e.to_string()),
        Format::Clap => soundcraft_clap_host::load_bundle(path).map(|clap| Probed { clap, ..Probed::default() }).map_err(|e| e.to_string()),
    }
}

/// The probe process: when `args` (without the program name) are `--probe-plugin <format>
/// <path>`, probes that file, prints the result line and returns the exit code; otherwise `None`.
pub fn probe_command_line(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some(PROBE_ARG) {
        return None;
    }
    let result = match (args.get(1).and_then(|f| Format::from_arg(f)), args.get(2)) {
        (Some(format), Some(path)) => probe_in_process(format, Path::new(path)),
        _ => Err(format!("usage: {PROBE_ARG} vst3|clap <path>")),
    };
    let code = i32::from(result.is_err());
    println!("{PROBE_MARK}{}", serde_json::to_string(&result).unwrap_or_default());
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    Some(code)
}

/// The result line in a probe process's output.
fn parse_probe_output(text: &str) -> Option<Result<Probed, String>> {
    text.lines().rev().find_map(|l| l.trim_end().strip_prefix(PROBE_MARK)).and_then(|j| serde_json::from_str(j).ok())
}

/// Probes each file by running `exe --probe-plugin <format> <path>` (the app itself), killing it
/// after `timeout`. A crash or hang is reported as the failure reason.
pub fn child_probe(exe: PathBuf, timeout: Duration) -> ProbeFn {
    soundcraft_vst3_host::quiet_crash_dialogs();
    Arc::new(move |format, path| probe_child(&exe, format, path, timeout))
}

fn probe_child(exe: &Path, format: Format, path: &Path, timeout: Duration) -> Result<Probed, String> {
    let args = [std::ffi::OsStr::new(PROBE_ARG), std::ffi::OsStr::new(format.arg()), path.as_os_str()];
    let (text, status) = run_child(exe, &args, None, timeout)?;
    match (parse_probe_output(&text), status) {
        (Some(r), _) => r,
        (None, None) => Err(format!("timed out after {} s", timeout.as_secs())),
        (None, Some(s)) => Err(format!("crashed while loading ({s})")),
    }
}

/// Runs `exe args` with no window, `input` (if any) on its standard input, killing it after
/// `timeout`: its standard output, and its exit status (`None` when it was killed).
pub(crate) fn run_child(
    exe: &Path,
    args: &[&std::ffi::OsStr],
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<(String, Option<std::process::ExitStatus>), String> {
    use std::io::{Read as _, Write as _};
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(exe);
    let stdin = if input.is_some() { Stdio::piped() } else { Stdio::null() };
    cmd.args(args).stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
/// An exit status as text: a Windows crash code as hex (`0xC0000005`), else the code or signal.
pub(crate) fn exit_text(s: &std::process::ExitStatus) -> String {
    match s.code() {
        #[allow(clippy::cast_sign_loss)]
        Some(c) if !(0..=0xFFFF).contains(&c) => format!("exit code 0x{:08X}", c as u32),
        Some(c) => format!("exit code {c}"),
        None => s.to_string(),
    }
}

    let mut child = cmd.spawn().map_err(|e| format!("cannot start the scanner process: {e}"))?;
    // Written from a thread of its own, so a child that doesn't read it can't block this one.
    if let (Some(data), Some(mut pipe)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }
    let mut stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = stdout.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        let _ = tx.send(s);
    });
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(e) => return Err(format!("scanner process: {e}")),
        }
    };
    // A plugin that left a process of its own holding the pipe must not hang the scan.
    Ok((rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default(), status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_dsp::Category;

    fn vst3(id: &str, name: &str, path: &str) -> Vst3Descriptor {
        Vst3Descriptor {
            id: format!("vst3:{id}"),
            class_id: id.into(),
            name: name.into(),
            vendor: "Acme".into(),
            version: "1".into(),
            sdk_version: String::new(),
            sub_categories: vec!["Fx".into()],
            category: Category::Other,
    if let Some(s) = status.filter(|s| !s.success()) {
        // 1 is the probes' own "refused" code; anything else (0xC0000005, a signal...) is a crash.
        if s.code() != Some(1) {
            let what: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
            log::warn!("child process died abnormally ({}): {} {}", exit_text(&s), exe.display(), what.join(" "));
        }
    }
            is_instrument: false,
            path: path.into(),
        }
    }

    fn cand(path: &str, mtime: u64) -> Candidate {
        Candidate { format: Format::Vst3, path: path.into(), stamp: Stamp { mtime_ns: mtime, size: 10 } }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sc-plugin-scan-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Probes by file name: `bad*` fails, anything else holds one plugin named after the file.
    fn fake(calls: Arc<Mutex<Vec<PathBuf>>>) -> ProbeFn {
        Arc::new(move |_, p: &Path| {
            calls.lock().unwrap().push(p.to_path_buf());
            let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
            if stem.starts_with("bad") {
                Err("not a VST3 plugin".into())
            } else {
                Ok(Probed { vst3: vec![vst3(&stem, &stem, &p.to_string_lossy())], ..Probed::default() })
            }
        })
    }

    fn lister(now: Vec<Candidate>) -> ListFn {
        Arc::new(move |_| now.clone())
    }

    fn scan(old: &PluginCache, now: Vec<Candidate>, full: bool, calls: &Arc<Mutex<Vec<PathBuf>>>) -> (PluginCache, Summary) {
        calls.lock().unwrap().clear();
        run_scan(old, &[], full, &lister(now), &fake(calls.clone()), &AtomicBool::new(false), |_, _, _| {})
    }

    #[test]
    fn cache_round_trips_through_its_file() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (cache, _) = scan(&PluginCache::default(), vec![cand("a.vst3", 1), cand("bad.vst3", 1)], false, &calls);
        let path = tmp("round").join(CACHE_FILE);
        cache.save(&path).unwrap();
        assert_eq!(PluginCache::load(&path), Some(cache.clone()));
        assert_eq!(cache.count(Format::Vst3), 1);
        assert_eq!(cache.failed.len(), 1);
    }

    #[test]
    fn a_corrupt_or_foreign_cache_reads_as_none() {
        let dir = tmp("corrupt");
        let path = dir.join(CACHE_FILE);
        assert_eq!(PluginCache::load(&path), None, "missing");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(PluginCache::load(&path), None, "corrupt");
        std::fs::write(&path, r#"{"version": 999, "files": []}"#).unwrap();
        assert_eq!(PluginCache::load(&path), None, "other version");
        std::fs::write(&path, r#"{"version": 1, "files": [{"format": "vst3"}]}"#).unwrap();
        assert_eq!(PluginCache::load(&path), None, "bad entry");
    }

    #[test]
    fn incremental_scan_probes_only_new_and_changed_files_and_drops_missing_ones() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (first, s) = scan(&PluginCache::default(), vec![cand("a.vst3", 1), cand("b.vst3", 1), cand("c.vst3", 1)], false, &calls);
        assert_eq!((s.added, s.removed, s.probed), (3, 0, 3));
        // b changed, c is gone, d is new.
        let (second, s) = scan(&first, vec![cand("a.vst3", 1), cand("b.vst3", 2), cand("d.vst3", 1)], false, &calls);
        assert_eq!(*calls.lock().unwrap(), vec![PathBuf::from("b.vst3"), PathBuf::from("d.vst3")]);
        assert_eq!((s.added, s.removed, s.probed, s.failed), (1, 1, 2, 0));
        assert_eq!(s.to_string(), "1 new, 1 removed");
        let names: Vec<String> = second.vst3().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["a", "b", "d"]);
        // Nothing changed: nothing probed.
        let (_, s) = scan(&second, vec![cand("a.vst3", 1), cand("b.vst3", 2), cand("d.vst3", 1)], false, &calls);
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(s.to_string(), "No changes (0 files checked)");
    }

    #[test]
    fn failed_files_are_skipped_until_they_change_or_a_full_rescan() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (first, s) = scan(&PluginCache::default(), vec![cand("bad.vst3", 1), cand("a.vst3", 1)], false, &calls);
        assert_eq!((s.failed, first.failed.len()), (1, 1));
        assert_eq!(first.failed[0].reason, "not a VST3 plugin");
        let (second, _) = scan(&first, vec![cand("bad.vst3", 1), cand("a.vst3", 1)], false, &calls);
        assert!(calls.lock().unwrap().is_empty(), "retried a failed file");
        assert_eq!(second.failed.len(), 1, "forgot the failure");
        scan(&second, vec![cand("bad.vst3", 2), cand("a.vst3", 1)], false, &calls);
        assert_eq!(*calls.lock().unwrap(), vec![PathBuf::from("bad.vst3")], "a changed file is retried");
        scan(&second, vec![cand("bad.vst3", 1), cand("a.vst3", 1)], true, &calls);
        assert_eq!(calls.lock().unwrap().len(), 2, "a full rescan probes everything");
    }

    #[test]
    fn a_cancelled_scan_keeps_the_old_entries_it_did_not_reach() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (first, _) = scan(&PluginCache::default(), vec![cand("a.vst3", 1), cand("b.vst3", 1)], false, &calls);
        let cancel = AtomicBool::new(true);
        let (after, s) =
            run_scan(&first, &[], true, &lister(vec![cand("a.vst3", 1), cand("b.vst3", 1)]), &fake(calls.clone()), &cancel, |_, _, _| {});
        assert!(s.cancelled);
        assert_eq!(after.count(Format::Vst3), 2);
        assert!(s.to_string().starts_with("Cancelled."));
    }

    #[test]
    fn a_background_job_reports_its_result() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let job = ScanJob::start(PluginCache::default(), Vec::new(), false, lister(vec![cand("a.vst3", 1)]), fake(calls));
        let (cache, s) = job.wait().unwrap();
        assert_eq!((cache.count(Format::Vst3), s.added), (1, 1));
        assert!(cache.scanned_at > 0);
    }

    #[test]
    fn settings_round_trip_and_default_when_missing() {
        let path = tmp("settings").join(SETTINGS_FILE);
        assert_eq!(ScanSettings::load(&path), ScanSettings::default());
        let s = ScanSettings { quick_scan_at_launch: true, extra_dirs: vec!["D:/Plugins".into()] };
        s.save(&path).unwrap();
        assert_eq!(ScanSettings::load(&path), s);
    }

    #[test]
    fn probe_output_is_found_among_plugin_chatter() {
        let ok: Result<Probed, String> = Ok(Probed { vst3: vec![vst3("A", "A", "a.vst3")], ..Probed::default() });
        let text = format!("plugin says hi\n{PROBE_MARK}{}\r\nbye\n", serde_json::to_string(&ok).unwrap());
        assert_eq!(parse_probe_output(&text), Some(ok));
        assert_eq!(parse_probe_output("crash"), None);
        assert_eq!(probe_command_line(&["--other".into()]), None);
    }

    #[test]
    fn extra_folders_are_searched_for_both_formats() {
        let dirs = search_dirs(&[PathBuf::from("X:/extra")]);
        assert!(dirs.contains(&(Format::Vst3, "X:/extra".into())) && dirs.contains(&(Format::Clap, "X:/extra".into())));
    }
}
