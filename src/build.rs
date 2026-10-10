use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq)]
pub enum BuildJobStatus {
    Pending,
    Building,
    Success,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct BuildJob {
    pub name: String,
    pub status: BuildJobStatus,
}

pub enum BuildMsg {
    Started(String),
    Output(String, String),                    // (name, line)
    Finished(String, PathBuf),                 // (name, log_path)
    Failed(String, Vec<String>, PathBuf),      // (name, last N error lines, log_path)
    Cancelled(String, PathBuf),                // (name, log_path) — our own kill, not a failure
    QueueComplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildHistoryEntry {
    pub name: String,
    pub success: bool,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuildHistory {
    pub entries: Vec<BuildHistoryEntry>,
}

impl BuildHistory {
    pub fn load() -> Self {
        let path = Self::path();
        if let Ok(data) = std::fs::read_to_string(&path) {
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, data);
        }
    }

    pub fn record(&mut self, name: &str, success: bool) {
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.entries.push(BuildHistoryEntry {
            name: name.to_string(),
            success,
            timestamp,
        });
        self.save();
    }

    fn path() -> PathBuf {
        let cache = std::env::var("XDG_CACHE_HOME")
            .or_else(|_| std::env::var("HOME").map(|h| format!("{}/.cache", h)))
            .unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(cache).join("smith/build_history.json")
    }
}

// --- Build Log Persistence ---

struct BuildLog {
    file: std::fs::File,
    path: PathBuf,
}

impl BuildLog {
    fn new(pkg_name: &str) -> Option<Self> {
        let dir = log_dir();
        let _ = std::fs::create_dir_all(&dir);

        let now = chrono_timestamp();
        let filename = format!("{}-{}.log", pkg_name, now);
        let path = dir.join(filename);

        match std::fs::File::create(&path) {
            Ok(file) => Some(BuildLog { file, path }),
            Err(_) => None,
        }
    }

    fn write_line(&mut self, line: &str) {
        let _ = writeln!(self.file, "{}", line);
    }

    fn finish(self) -> PathBuf {
        self.path
    }
}

fn log_dir() -> PathBuf {
    let cache = std::env::var("XDG_CACHE_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{}/.cache", h)))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(cache).join("smith/logs")
}

fn chrono_timestamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Return the path for a new bump log file, creating the directory if needed.
pub fn bump_log_path(name: &str) -> PathBuf {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{}-bump-{}.log", name, chrono_timestamp()))
}

/// Prune old build logs, keeping at most `max_per_pkg` per package.
pub fn prune_build_logs(max_per_pkg: usize) {
    let dir = log_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut by_pkg: HashMap<String, Vec<PathBuf>> = HashMap::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "log").unwrap_or(false) {
            if let Some(filename) = path.file_stem().and_then(|s| s.to_str()) {
                // Format: pkgname-YYYYMMDD-HHMMSS
                // Find the timestamp part (last 15 chars: YYYYMMDD-HHMMSS)
                if filename.len() > 16 {
                    let pkg = &filename[..filename.len() - 16]; // strip "-YYYYMMDD-HHMMSS"
                    by_pkg.entry(pkg.to_string()).or_default().push(path);
                }
            }
        }
    }

    for (_pkg, mut logs) in by_pkg {
        if logs.len() <= max_per_pkg {
            continue;
        }
        logs.sort();
        // Oldest first (filenames sort chronologically), remove excess
        let to_remove = logs.len() - max_per_pkg;
        for path in &logs[..to_remove] {
            let _ = std::fs::remove_file(path);
        }
    }
}

// --- Pre-flight checks ---

/// A condition worth surfacing to the user before a build starts.
/// `cleanable` = a `./xbps-src clean` would resolve it (offers the "clean & build" action).
pub struct PreflightWarning {
    pub message: String,
    pub cleanable: bool,
}

/// Leftover entries under any `masterdir-*/builddir/`. A clean masterdir has an empty
/// builddir; anything here is residue from an interrupted/failed build and can cause
/// `cannot access wrksrc` failures when versions no longer match.
fn dirty_masterdir_entries(void_pkgs: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(void_pkgs) else {
        return out;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("masterdir") {
            continue;
        }
        if let Ok(builddir) = std::fs::read_dir(entry.path().join("builddir")) {
            for be in builddir.flatten() {
                out.push(be.file_name().to_string_lossy().to_string());
            }
        }
    }
    out.sort();
    out
}

/// Up to `n` names joined, with a "+N more" suffix when truncated.
fn preview_names(names: &[String], n: usize) -> String {
    let shown: Vec<&str> = names.iter().take(n).map(|s| s.as_str()).collect();
    let more = names.len().saturating_sub(shown.len());
    if more > 0 {
        format!("{}, +{} more", shown.join(", "), more)
    } else {
        shown.join(", ")
    }
}

/// True if a binary with this exact pkgver already exists under `hostdir/binpkgs` (built
/// earlier). Filename layout: `<pkgver>.<arch>.xbps`.
fn local_binpkg_exists(void_pkgs: &Path, pkgver: &str) -> bool {
    let prefix = format!("{}.", pkgver); // trailing '.' disambiguates _1 from _11 etc.
    let Ok(repos) = std::fs::read_dir(void_pkgs.join("hostdir/binpkgs")) else {
        return false;
    };
    for repo in repos.flatten() {
        if let Ok(files) = std::fs::read_dir(repo.path()) {
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().to_string();
                if name.starts_with(&prefix) && name.ends_with(".xbps") {
                    return true;
                }
            }
        }
    }
    false
}

/// True if a binary matching this *exact* pkgver is installable — from the remote repos or
/// already built into `hostdir/binpkgs`. xbps-src builds a dep from source unless the exact
/// version pinned in the local tree is available; the remote shipping a *different* version
/// (newer or older) does not satisfy it. This is direction-agnostic — both a tree ahead of
/// the repo and a tree behind it trigger source builds.
fn binary_available_exact(void_pkgs: &Path, pkgver: &str) -> bool {
    if let Ok(o) = Command::new("xbps-query")
        .args(["-R", "--property=pkgver", pkgver])
        .output()
    {
        if o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty() {
            return true;
        }
    }
    local_binpkg_exists(void_pkgs, pkgver)
}

/// Build-deps of `job_names` that xbps-src will compile from source rather than install as a
/// binary: either no binary exists, or the local template is ahead of the available binary
/// (repo lag). This is the surprise when a binary-repack package drags in a full deps build.
fn source_build_deps(void_pkgs: &Path, job_names: &[String]) -> Vec<String> {
    let mut deps: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for name in job_names {
        let out = Command::new("./xbps-src")
            .args(["show-build-deps", name])
            .current_dir(void_pkgs)
            .output();
        if let Ok(o) = out {
            if o.status.success() {
                for line in String::from_utf8_lossy(&o.stdout).lines() {
                    let d = line.trim();
                    if !d.is_empty() {
                        deps.insert(d.to_string());
                    }
                }
            }
        }
    }
    // The packages being built are not their own "from source" deps.
    for name in job_names {
        deps.remove(name);
    }

    deps.into_iter()
        .filter(|dep| {
            let tmpl = void_pkgs.join("srcpkgs").join(dep).join("template");
            // Skip virtuals / non-tree deps we can't resolve — under-warn rather than over-warn.
            let Ok(pkg) = crate::package::parse_template(&tmpl) else {
                return false;
            };
            // xbps-src needs the exact version pinned in the local tree; if no binary matches
            // it (in either direction), the dep compiles from source.
            let pkgver = format!("{}-{}_{}", dep, pkg.version, pkg.revision);
            !binary_available_exact(void_pkgs, &pkgver)
        })
        .collect()
}

/// Conditions to warn about before launching `jobs`. Empty = nothing to flag, build directly.
pub fn preflight(void_pkgs: &Path, jobs: &[BuildJob]) -> Vec<PreflightWarning> {
    let mut warnings = Vec::new();

    // #1 — leftover masterdir build state (cleanable).
    let leftover = dirty_masterdir_entries(void_pkgs);
    if !leftover.is_empty() {
        warnings.push(PreflightWarning {
            message: format!(
                "masterdir has {} leftover build dir(s) from an interrupted build ({}); \
                 these can cause 'cannot access wrksrc' failures",
                leftover.len(),
                preview_names(&leftover, 3)
            ),
            cleanable: true,
        });
    }

    // #2 — dependencies that will compile from source (repo lag / no binary).
    let job_names: Vec<String> = jobs.iter().map(|j| j.name.clone()).collect();
    let from_source = source_build_deps(void_pkgs, &job_names);
    if !from_source.is_empty() {
        warnings.push(PreflightWarning {
            message: format!(
                "{} dependenc{} will build from source (no binary matches the pinned version): {} — expect extra compile time",
                from_source.len(),
                if from_source.len() == 1 { "y" } else { "ies" },
                preview_names(&from_source, 4)
            ),
            cleanable: false,
        });
    }

    warnings
}

/// Run `./xbps-src clean` + `remove-autodeps` to reset masterdir build state. Fast (no compile).
/// Returns a status line for the UI.
pub fn clean_masterdir(void_pkgs: &Path) -> String {
    let clean = Command::new("./xbps-src")
        .arg("clean")
        .current_dir(void_pkgs)
        .output();
    let _ = Command::new("./xbps-src")
        .arg("remove-autodeps")
        .current_dir(void_pkgs)
        .output();
    match clean {
        Ok(o) if o.status.success() => "Cleaned masterdir build state".to_string(),
        Ok(o) => format!(
            "Clean failed: {}",
            String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("non-zero exit")
        ),
        Err(e) => format!("Clean failed: {}", e),
    }
}

// --- Build Queue ---

pub struct BuildQueue {
    pub jobs: Vec<BuildJob>,
    pub current_output: Vec<String>,
    pub receiver: Option<Receiver<BuildMsg>>,
    pub active: bool,
    pub cancel_flag: Arc<AtomicBool>,
    pub current_child: Arc<Mutex<Option<Child>>>,
}

impl BuildQueue {
    pub fn new() -> Self {
        BuildQueue {
            jobs: Vec::new(),
            current_output: Vec::new(),
            receiver: None,
            active: false,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            current_child: Arc::new(Mutex::new(None)),
        }
    }

    pub fn kill_current(&self) {
        if let Ok(mut guard) = self.current_child.lock() {
            if let Some(ref mut child) = *guard {
                let _ = child.kill();
            }
        }
    }

    /// Start building the queued jobs in a background thread.
    pub fn start(&mut self, void_pkgs: PathBuf) {
        if self.jobs.is_empty() {
            return;
        }

        self.active = true;
        self.current_output.clear();
        self.cancel_flag.store(false, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);

        let names: Vec<String> = self.jobs.iter().map(|j| j.name.clone()).collect();
        let cancel = self.cancel_flag.clone();
        let current_child = self.current_child.clone();

        std::thread::spawn(move || {
            run_build_queue(void_pkgs, names, tx, cancel, current_child);
        });
    }
}

fn run_build_queue(void_pkgs: PathBuf, names: Vec<String>, tx: Sender<BuildMsg>, cancel: Arc<AtomicBool>, current_child: Arc<Mutex<Option<Child>>>) {
    for name in &names {
        if cancel.load(Ordering::SeqCst) {
            break;
        }

        let _ = tx.send(BuildMsg::Started(name.clone()));
        let mut build_log = BuildLog::new(name);

        let result = Command::new("./xbps-src")
            .args(["pkg", name])
            .current_dir(&void_pkgs)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        match result {
            Ok(mut child) => {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                // Store child so main thread can kill it if needed
                *current_child.lock().unwrap() = Some(child);

                // Read stdout in a background thread concurrently with stderr to prevent
                // buffer deadlock when xbps-src writes >64KB to either stream.
                let tx2 = tx.clone();
                let name2 = name.clone();
                let cancel2 = cancel.clone();
                let mut log_for_thread = build_log.take();
                let stdout_handle = std::thread::spawn(move || {
                    if let Some(out) = stdout {
                        let reader = std::io::BufReader::new(out);
                        for line in reader.lines().map_while(Result::ok) {
                            if cancel2.load(Ordering::SeqCst) {
                                break;
                            }
                            if let Some(ref mut log) = log_for_thread {
                                log.write_line(&line);
                            }
                            let _ = tx2.send(BuildMsg::Output(name2.clone(), line));
                        }
                    }
                    log_for_thread
                });

                // Read stderr on this thread concurrently, buffering for error reporting.
                let stderr_lines: Vec<String> = if let Some(err) = stderr {
                    std::io::BufReader::new(err)
                        .lines()
                        .map_while(Result::ok)
                        .collect()
                } else {
                    Vec::new()
                };

                let build_log = stdout_handle.join().ok().flatten();

                if cancel.load(Ordering::SeqCst) {
                    if let Ok(mut guard) = current_child.lock() {
                        if let Some(ref mut c) = *guard {
                            let _ = c.kill();
                        }
                    }
                }

                let log_path = build_log.map(|l| l.finish()).unwrap_or_default();

                let wait_result = {
                    let mut guard = current_child.lock().unwrap();
                    guard.as_mut().map(|c| c.wait())
                };
                *current_child.lock().unwrap() = None;

                match wait_result {
                    Some(Ok(status)) if status.success() => {
                        let _ = tx.send(BuildMsg::Finished(name.clone(), log_path));
                    }
                    // A cancelled build is our own kill, not a failure: don't
                    // mark the package BUILD FAILED or write a history failure.
                    _ if cancel.load(Ordering::SeqCst) => {
                        let _ = tx.send(BuildMsg::Cancelled(name.clone(), log_path));
                    }
                    Some(Ok(_)) | None => {
                        if log_path != PathBuf::new() {
                            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&log_path) {
                                for line in &stderr_lines {
                                    let _ = writeln!(f, "ERR: {}", line);
                                }
                            }
                        }
                        let start = stderr_lines.len().saturating_sub(50);
                        let error_lines = if stderr_lines.is_empty() {
                            vec!["Build failed with non-zero exit code".to_string()]
                        } else {
                            stderr_lines[start..].to_vec()
                        };
                        let _ = tx.send(BuildMsg::Failed(name.clone(), error_lines, log_path));
                    }
                    Some(Err(e)) => {
                        let _ = tx.send(BuildMsg::Failed(name.clone(), vec![e.to_string()], log_path));
                    }
                }
            }
            Err(e) => {
                let log_path = build_log.map(|l| l.finish()).unwrap_or_default();
                let _ = tx.send(BuildMsg::Failed(name.clone(), vec![e.to_string()], log_path));
            }
        }
    }

    let _ = tx.send(BuildMsg::QueueComplete);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_void_pkgs(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("smith-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn empty_builddir_is_not_dirty() {
        let root = temp_void_pkgs("clean");
        std::fs::create_dir_all(root.join("masterdir-x86_64/builddir")).unwrap();
        assert!(dirty_masterdir_entries(&root).is_empty());
        assert!(preflight(&root, &[]).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn leftover_builddir_entries_flagged() {
        let root = temp_void_pkgs("dirty");
        let bd = root.join("masterdir-x86_64/builddir");
        std::fs::create_dir_all(bd.join("curl-8.19.0")).unwrap();
        std::fs::create_dir_all(bd.join(".xbps-zed")).unwrap();

        let entries = dirty_masterdir_entries(&root);
        assert_eq!(entries.len(), 2);
        assert!(entries.contains(&"curl-8.19.0".to_string()));

        let warnings = preflight(&root, &[]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].cleanable);
        assert!(warnings[0].message.contains("curl-8.19.0"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn local_binpkg_exact_match() {
        let root = temp_void_pkgs("binpkg");
        let repo = root.join("hostdir/binpkgs/custom");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("curl-8.20.0_1.x86_64.xbps"), b"").unwrap();

        assert!(local_binpkg_exists(&root, "curl-8.20.0_1"));
        // exact match only — different revision / version must not match
        assert!(!local_binpkg_exists(&root, "curl-8.20.0_2"));
        assert!(!local_binpkg_exists(&root, "curl-8.21.0_1"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
