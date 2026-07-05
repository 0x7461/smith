use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::Receiver;
use ratatui::widgets::TableState;

use crate::build::{self, BuildHistory, BuildJob, BuildJobStatus, BuildMsg, BuildQueue};
use crate::dep_graph::DepGraph;
use crate::git::{self, GitMsg, GitOp, GitStatus};
use crate::package::{Package, PackageState, Status};
use crate::repo;
use crate::shlibs::{self, ShlibMap};
use crate::template;
use crate::version_check;

pub enum TemplateBumpMsg {
    Started(std::path::PathBuf),        // log_path — sent before bump begins
    Done(String, String, String),       // (pkgname, old_version, new_version)
    Failed(String, String),             // (pkgname, error_msg)
    AllDone,
}

#[derive(Debug, Clone, PartialEq)]
pub enum View {
    List,
    Tree,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PanelMode {
    None,
    Detail,
    BuildLog,
    BumpLog,
    GitMenu,
    Help,
}

pub struct App {
    pub packages: Vec<PackageState>,
    pub selected: usize,
    pub view: View,
    pub dep_graph: DepGraph,
    pub void_pkgs: PathBuf,
    pub panel: PanelMode,
    pub checking_versions: bool,
    pub status_msg: Option<String>,
    pub should_quit: bool,
    pub build_queue: BuildQueue,
    pub build_history: BuildHistory,
    pub version_check_rx: Option<Receiver<version_check::VersionMsg>>,
    pub git_status: Option<GitStatus>,
    pub git_op_rx: Option<Receiver<GitMsg>>,
    pub git_output: Vec<String>,
    pub git_op_active: bool,
    pub git_cancel_flag: Arc<AtomicBool>,
    pub git_current_child: Arc<Mutex<Option<Child>>>,
    pub git_current_op: Option<GitOp>,
    pub table_state: TableState,
    pub filter: String,
    pub filter_active: bool,
    pub shlib_map: ShlibMap,
    pub shlib_updates: Vec<(String, String, String, String)>, // (pkg, old_so, new_so, new_pkgver)
    pub build_log_scroll: usize, // lines offset from bottom; 0 = follow tail
    pub pkg_last_checked: Option<u64>, // unix timestamp of last pkg upstream check
    pub template_bump_rx: Option<Receiver<TemplateBumpMsg>>,
    pub template_bumping: bool,
    pub bump_cancel_flag: Arc<AtomicBool>,
    pub bump_had_failure: bool,
    pub bump_log_path: Option<std::path::PathBuf>,
    pub bump_log_scroll: usize,
    pub cancel_confirm: Option<String>, // op name being cancelled ("build"/"bump"/"git")
    pub quit_confirm: bool,
    pub build_preflight: Option<Vec<build::PreflightWarning>>, // pending build gated on warnings; jobs staged in build_queue
    pub preflight_pending: bool, // pre-flight check running in background (jobs staged, not yet started)
    pub preflight_rx: Option<Receiver<Vec<build::PreflightWarning>>>,
}

/// Discover and load all packages (committed + uncommitted).
/// Returns (packages, dep_graph, uncommitted_set).
fn discover_and_load(
    void_pkgs: &std::path::Path,
) -> anyhow::Result<(Vec<crate::package::Package>, DepGraph, std::collections::HashSet<String>)> {
    let committed = repo::discover_custom_packages(void_pkgs)?;
    let committed_set: std::collections::HashSet<String> = committed.iter().cloned().collect();
    let uncommitted = repo::discover_uncommitted_packages(void_pkgs, &committed_set);
    let mut names = committed;
    names.extend(uncommitted.iter().cloned());
    names.sort();
    let uncommitted_set: std::collections::HashSet<String> = uncommitted.into_iter().collect();
    let packages = repo::load_packages(void_pkgs, &names);
    let dep_graph = DepGraph::build(&packages);
    Ok((packages, dep_graph, uncommitted_set))
}

impl App {
    pub fn new(void_pkgs: PathBuf) -> anyhow::Result<Self> {
        let (packages, dep_graph, uncommitted_set) = discover_and_load(&void_pkgs)?;
        let mut states = repo::build_package_states(&void_pkgs, packages, &uncommitted_set);

        let git_status = git::get_git_status(&void_pkgs);
        let shlib_map = shlibs::parse_shlibs(&void_pkgs);

        // Populate shlibs and check mismatches
        for state in &mut states {
            if let Some(entries) = shlib_map.get(&state.package.name) {
                state.shlibs = entries.clone();
                state.soname_mismatches =
                    shlibs::check_soname_mismatches(&void_pkgs, entries, &state.package.name);
            }
        }

        // Prune old build logs at startup
        build::prune_build_logs(5);

        Ok(App {
            packages: states,
            selected: 0,
            view: View::List,
            dep_graph,
            void_pkgs,
            panel: PanelMode::None,
            checking_versions: false,
            status_msg: None,
            should_quit: false,
            build_queue: BuildQueue::new(),
            build_history: BuildHistory::load(),
            version_check_rx: None,
            git_status,
            git_op_rx: None,
            git_output: Vec::new(),
            git_op_active: false,
            git_cancel_flag: Arc::new(AtomicBool::new(false)),
            git_current_child: Arc::new(Mutex::new(None)),
            git_current_op: None,
            table_state: TableState::default(),
            filter: String::new(),
            filter_active: false,
            shlib_map,
            shlib_updates: Vec::new(),
            build_log_scroll: 0,
            pkg_last_checked: version_check::last_check_time(),
            template_bump_rx: None,
            template_bumping: false,
            bump_cancel_flag: Arc::new(AtomicBool::new(false)),
            bump_had_failure: false,
            bump_log_path: None,
            bump_log_scroll: 0,
            cancel_confirm: None,
            quit_confirm: false,
            build_preflight: None,
            preflight_pending: false,
            preflight_rx: None,
        })
    }

    /// Returns (original_index, &PackageState) for packages matching the current filter.
    pub fn visible_packages(&self) -> Vec<(usize, &PackageState)> {
        if self.filter.is_empty() {
            self.packages.iter().enumerate().collect()
        } else {
            let filter_lower = self.filter.to_lowercase();
            self.packages
                .iter()
                .enumerate()
                .filter(|(_, p)| p.package.name.to_lowercase().contains(&filter_lower))
                .collect()
        }
    }

    pub fn selected_package(&self) -> Option<&PackageState> {
        let visible = self.visible_packages();
        visible.get(self.selected).map(|(_, p)| *p)
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        let visible_len = self.visible_packages().len();
        if self.selected + 1 < visible_len {
            self.selected += 1;
        }
    }

    pub fn start_filter(&mut self) {
        self.filter_active = true;
        self.filter.clear();
        self.selected = 0;
    }

    pub fn filter_input(&mut self, c: char) {
        self.filter.push(c);
        self.selected = 0;
    }

    pub fn filter_backspace(&mut self) {
        self.filter.pop();
        self.selected = 0;
    }

    pub fn stop_filter(&mut self, clear: bool) {
        self.filter_active = false;
        if clear {
            self.filter.clear();
            self.selected = 0;
        }
    }

    pub fn toggle_detail(&mut self) {
        self.panel = match self.panel {
            PanelMode::Detail => PanelMode::None,
            _ => PanelMode::Detail,
        };
    }

    pub fn toggle_tree(&mut self) {
        self.view = match self.view {
            View::List => View::Tree,
            View::Tree => View::List,
        };
    }

    pub fn refresh(&mut self) {
        if let Ok((packages, dep_graph, uncommitted_set)) = discover_and_load(&self.void_pkgs) {
            self.dep_graph = dep_graph;
            let mut states = repo::build_package_states(&self.void_pkgs, packages, &uncommitted_set);

            // Preserve latest versions and build-failed overrides from previous state
            let old_latest: std::collections::HashMap<String, String> = self
                .packages
                .iter()
                .filter_map(|p| {
                    p.latest
                        .as_ref()
                        .map(|v| (p.package.name.clone(), v.clone()))
                })
                .collect();

            let failed: std::collections::HashSet<String> = self
                .packages
                .iter()
                .filter(|p| p.status == Status::BuildFailed)
                .map(|p| p.package.name.clone())
                .collect();

            for state in &mut states {
                if let Some(latest) = old_latest.get(&state.package.name) {
                    state.latest = Some(latest.clone());
                    state.status = PackageState::compute_status(
                        &state.package,
                        &state.installed,
                        &state.built,
                    );
                }
                if failed.contains(&state.package.name) {
                    state.status = Status::BuildFailed;
                }

            }

            // Re-parse shlibs and check mismatches
            self.shlib_map = shlibs::parse_shlibs(&self.void_pkgs);
            for state in &mut states {
                if let Some(entries) = self.shlib_map.get(&state.package.name) {
                    state.shlibs = entries.clone();
                    state.soname_mismatches = shlibs::check_soname_mismatches(
                        &self.void_pkgs,
                        entries,
                        &state.package.name,
                    );
                }
            }

            self.packages = states;
            let visible_len = self.visible_packages().len();
            if self.selected >= visible_len {
                self.selected = visible_len.saturating_sub(1);
            }
            self.status_msg = Some("Refreshed".to_string());
        }
    }

    /// Check upstream version for the selected package only.
    pub fn check_version_selected(&mut self) {
        if self.checking_versions {
            self.status_msg = Some("Version check already in progress".to_string());
            return;
        }
        let pkg = match self.selected_package() {
            Some(p) => p.package.clone(),
            None => return,
        };
        self.checking_versions = true;
        self.status_msg = Some(format!("Checking upstream for {}...", pkg.name));
        let void_pkgs = self.void_pkgs.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.version_check_rx = Some(rx);
        std::thread::spawn(move || {
            version_check::check_all_versions_streaming(&void_pkgs, &[pkg], true, tx);
        });
    }

    /// Check upstream versions for all packages (respects cache TTL).
    pub fn check_versions(&mut self) {
        if self.checking_versions {
            return;
        }
        self.checking_versions = true;
        self.status_msg = Some("Checking upstream versions...".to_string());

        let pkgs: Vec<Package> = self.packages.iter().map(|s| s.package.clone()).collect();
        let void_pkgs = self.void_pkgs.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.version_check_rx = Some(rx);

        std::thread::spawn(move || {
            version_check::check_all_versions_streaming(&void_pkgs, &pkgs, false, tx);
        });
    }

    /// Poll for version check results. Call each tick.
    pub fn poll_version_check(&mut self) {
        let msgs: Vec<version_check::VersionMsg> = if let Some(ref rx) = self.version_check_rx {
            rx.try_iter().collect()
        } else {
            return;
        };

        for msg in msgs {
            match msg {
                version_check::VersionMsg::Found(name, ver, cache_age) => {
                    for state in &mut self.packages {
                        if state.package.name == name {
                            state.latest = Some(ver.clone());
                            state.status = PackageState::compute_status(
                                &state.package,
                                &state.installed,
                                &state.built,
                            );
                        }
                    }
                    self.status_msg = Some(match cache_age {
                        Some(age) => format!("Checked {} (cached {}m ago)", name, age / 60),
                        None => format!("Checked {}", name),
                    });
                }
                version_check::VersionMsg::Done(count, rate_limited) => {
                    self.checking_versions = false;
                    self.version_check_rx = None;
                    // Use current time — cache TTL hits don't update disk timestamps
                    // but the user did just run a check, so "last checked" = now.
                    self.pkg_last_checked = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    );
                    self.status_msg = Some(if rate_limited {
                        format!("Checked {}/{} packages — GitHub rate limited (wait ~1hr or set GITHUB_TOKEN)", count, self.packages.len())
                    } else {
                        format!("Checked {} packages", count)
                    });
                    return;
                }
            }
        }
    }

    /// Poll for template bump results. Call each tick.
    pub fn poll_template_bump(&mut self) {
        let msgs: Vec<TemplateBumpMsg> = if let Some(ref rx) = self.template_bump_rx {
            rx.try_iter().collect()
        } else {
            return;
        };

        for msg in msgs {
            match msg {
                TemplateBumpMsg::Started(path) => {
                    self.bump_log_path = Some(path);
                    self.bump_log_scroll = 0;
                }
                TemplateBumpMsg::Done(name, old, new) => {
                    self.status_msg = Some(format!("Bumped {} {} → {}", name, old, new));
                }
                TemplateBumpMsg::Failed(name, err) => {
                    self.status_msg = Some(format!("Bump failed for {}: {}", name, err));
                    self.bump_had_failure = true;
                }
                TemplateBumpMsg::AllDone => {
                    self.template_bumping = false;
                    self.template_bump_rx = None;
                    let preserve_msg = self.bump_had_failure;
                    self.bump_had_failure = false;
                    let saved = self.status_msg.clone();
                    self.refresh();
                    if preserve_msg {
                        self.status_msg = saved;
                    }
                    return;
                }
            }
        }
    }

    /// Bump template for the selected package (only when upstream is ahead). Does not build.
    pub fn bump_template_selected(&mut self) {
        if self.template_bumping {
            self.status_msg = Some("Template bump already in progress".to_string());
            return;
        }
        let (name, latest) = match self.selected_package() {
            Some(p) if p.upstream_newer() => match p.latest.clone() {
                Some(v) => (p.package.name.clone(), v),
                None => {
                    self.status_msg = Some("No upstream version known — run u first".to_string());
                    return;
                }
            },
            Some(p) => {
                self.status_msg = Some(format!("{} — not upstream ahead", p.package.name));
                return;
            }
            None => return,
        };
        self.template_bumping = true;
        self.bump_cancel_flag.store(false, Ordering::SeqCst);
        self.status_msg = Some(format!("Bumping {} to v{}...", name, latest));
        let void_pkgs = self.void_pkgs.clone();
        let cancel = self.bump_cancel_flag.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.template_bump_rx = Some(rx);
        self.panel = PanelMode::BumpLog;
        std::thread::spawn(move || {
            let log_path = build::bump_log_path(&name);
            let _ = tx.send(TemplateBumpMsg::Started(log_path.clone()));
            match template::bump_template(&void_pkgs, &name, &latest, &log_path, cancel) {
                Ok(result) => {
                    let _ = tx.send(TemplateBumpMsg::Done(name, result.old_version, result.new_version));
                }
                Err(e) => {
                    let _ = tx.send(TemplateBumpMsg::Failed(name, e.to_string()));
                }
            }
            let _ = tx.send(TemplateBumpMsg::AllDone);
        });
    }

    /// Bump templates for all packages with an upstream update. Does not build.
    pub fn bump_template_all(&mut self) {
        if self.template_bumping {
            self.status_msg = Some("Template bump already in progress".to_string());
            return;
        }
        let targets: Vec<(String, String)> = self
            .packages
            .iter()
            .filter(|p| p.upstream_newer())
            .filter_map(|p| p.latest.as_ref().map(|v| (p.package.name.clone(), v.clone())))
            .collect();
        if targets.is_empty() {
            self.status_msg = Some("No packages with upstream updates".to_string());
            return;
        }
        self.template_bumping = true;
        self.bump_cancel_flag.store(false, Ordering::SeqCst);
        self.status_msg = Some(format!("Bumping {} packages...", targets.len()));
        let void_pkgs = self.void_pkgs.clone();
        let cancel = self.bump_cancel_flag.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.template_bump_rx = Some(rx);
        self.panel = PanelMode::BumpLog;
        std::thread::spawn(move || {
            for (name, latest) in targets {
                if cancel.load(Ordering::SeqCst) {
                    break;
                }
                let log_path = build::bump_log_path(&name);
                let _ = tx.send(TemplateBumpMsg::Started(log_path.clone()));
                match template::bump_template(&void_pkgs, &name, &latest, &log_path, cancel.clone()) {
                    Ok(result) => {
                        let _ = tx.send(TemplateBumpMsg::Done(
                            name,
                            result.old_version,
                            result.new_version,
                        ));
                    }
                    Err(e) => {
                        let _ = tx.send(TemplateBumpMsg::Failed(name, e.to_string()));
                        if cancel.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(TemplateBumpMsg::AllDone);
        });
    }

    /// Clean old built packages, keeping only the latest version per package.
    pub fn clean_old_packages(&mut self) {
        let names: Vec<String> = self.packages.iter().map(|p| p.package.name.clone()).collect();
        let (deleted, freed) = repo::clean_old_packages(&self.void_pkgs, &names);
        self.refresh();
        if deleted > 0 {
            self.status_msg = Some(format!(
                "Cleaned {} old package(s), freed {}", deleted, format_bytes(freed)
            ));
        } else {
            self.status_msg = Some("Nothing to clean — already at one version per package".to_string());
        }
    }

    /// Clean ALL built packages for managed packages.
    pub fn clean_all_packages(&mut self) {
        let names: Vec<String> = self.packages.iter().map(|p| p.package.name.clone()).collect();
        let (deleted, freed) = repo::clean_all_packages(&self.void_pkgs, &names);
        self.refresh();
        if deleted > 0 {
            self.status_msg = Some(format!(
                "Removed {} package(s), freed {}", deleted, format_bytes(freed)
            ));
        } else {
            self.status_msg = Some("No built packages to remove".to_string());
        }
    }

    /// Build the poll queue for messages from the background thread.
    pub fn poll_build(&mut self) {
        if !self.build_queue.active {
            return;
        }

        let msgs: Vec<BuildMsg> = if let Some(ref rx) = self.build_queue.receiver {
            rx.try_iter().collect()
        } else {
            return;
        };

        for msg in msgs {
            match msg {
                BuildMsg::Started(name) => {
                    for job in &mut self.build_queue.jobs {
                        if job.name == name {
                            job.status = BuildJobStatus::Building;
                        }
                    }
                    self.build_queue.current_output.clear();
                    self.build_log_scroll = 0;
                    self.status_msg = Some(format!("Building {}...", name));
                }
                BuildMsg::Output(_name, line) => {
                    self.build_queue.current_output.push(line);
                    // Trim deferred to after the loop — O(1) amortized vs O(n²) drain per msg
                }
                BuildMsg::Finished(name, log_path) => {
                    for job in &mut self.build_queue.jobs {
                        if job.name == name {
                            job.status = BuildJobStatus::Success;
                        }
                    }
                    // Store log path and check for shlib updates
                    let log_str = log_path.to_string_lossy().to_string();
                    // Drop any stale entries for this package before adding fresh ones
                    self.shlib_updates.retain(|(pkg, _, _, _)| pkg != &name);
                    for state in &mut self.packages {
                        if state.package.name == name {
                            state.build_log = Some(log_str.clone());
                            // Re-check against the just-built .xbps (startup mismatch
                            // data reflects the previously installed package)
                            if let Some(entries) = self.shlib_map.get(&name) {
                                state.soname_mismatches = shlibs::check_soname_mismatches(
                                    &self.void_pkgs,
                                    entries,
                                    &name,
                                );
                            }
                            for mm in &state.soname_mismatches {
                                let pkg_ver = format!(
                                    "{}-{}_{}",
                                    state.package.name, state.package.version, state.package.revision
                                );
                                self.shlib_updates.push((
                                    state.package.name.clone(),
                                    mm.registered.clone(),
                                    mm.installed.clone(),
                                    pkg_ver,
                                ));
                            }
                        }
                    }
                    self.build_history.record(&name, true);
                }
                BuildMsg::Failed(name, error_lines, log_path) => {
                    for job in &mut self.build_queue.jobs {
                        if job.name == name {
                            job.status = BuildJobStatus::Failed;
                        }
                    }
                    // Set BuildFailed status and store log path
                    let log_str = log_path.to_string_lossy().to_string();
                    for state in &mut self.packages {
                        if state.package.name == name {
                            state.status = Status::BuildFailed;
                            state.build_log = Some(log_str.clone());
                        }
                    }
                    // Append error lines to output
                    for line in &error_lines {
                        self.build_queue.current_output.push(format!("ERR: {}", line));
                    }
                    // pkglint SONAME-bump failure carries its own fix — stage it
                    let bumps =
                        shlibs::parse_soname_bump_errors(&self.build_queue.current_output);
                    if !bumps.is_empty() {
                        self.shlib_updates.retain(|(pkg, _, _, _)| pkg != &name);
                        for (old, new, pkgver) in bumps {
                            self.shlib_updates.push((name.clone(), old, new, pkgver));
                        }
                        self.status_msg = Some(format!(
                            "{}: SONAME bump failed pkglint — S updates common/shlibs, then rebuild",
                            name
                        ));
                    }
                    self.build_history.record(&name, false);
                }
                BuildMsg::QueueComplete => {
                    self.build_queue.active = false;
                    self.build_queue.receiver = None;

                    // Refresh to pick up new .xbps files
                    self.refresh();

                    // Build the xi command for successful packages
                    let succeeded: Vec<String> = self
                        .build_queue
                        .jobs
                        .iter()
                        .filter(|j| j.status == BuildJobStatus::Success)
                        .map(|j| j.name.clone())
                        .collect();

                    if !succeeded.is_empty() {
                        self.status_msg =
                            Some(format!("Run: xi {}", succeeded.join(" ")));
                    } else {
                        self.status_msg = Some("Build queue finished (no successful builds)".to_string());
                    }
                }
            }
        }

        // Trim output buffer once after processing all messages (avoids O(n²) drain per message).
        let out = &mut self.build_queue.current_output;
        if out.len() > 200 {
            out.drain(..out.len() - 200);
        }
    }

    /// Build the currently selected package (best-effort: skip if not buildable).
    pub fn build_selected(&mut self) {
        if self.build_queue.active || self.preflight_pending {
            self.status_msg = Some("Build already in progress".to_string());
            return;
        }

        let (name, status) = match self.selected_package() {
            Some(p) => (p.package.name.clone(), p.status.clone()),
            None => return,
        };

        match status {
            Status::BuildOutdated | Status::BuildFailed => {}
            Status::ReadyToInstall => {
                self.status_msg = Some(format!("{} — already built, run: xi {}", name, name));
                return;
            }
            Status::UpToDate => {
                self.status_msg = Some(format!("{} — nothing to build", name));
                return;
            }
        }

        self.build_queue.jobs = vec![BuildJob {
            name,
            status: BuildJobStatus::Pending,
        }];
        self.gate_build();
    }

/// Build all packages with BuildOutdated or BuildFailed status in topo order.
    pub fn build_all_buildable(&mut self) {
        if self.build_queue.active || self.preflight_pending {
            self.status_msg = Some("Build already in progress".to_string());
            return;
        }

        let buildable: std::collections::HashSet<String> = self
            .packages
            .iter()
            .filter(|p| matches!(p.status, Status::BuildOutdated | Status::BuildFailed))
            .map(|p| p.package.name.clone())
            .collect();

        if buildable.is_empty() {
            self.status_msg = Some("No packages to build (try 't' to bump templates)".to_string());
            return;
        }

        let topo = self.dep_graph.topological_sort();
        let ordered: Vec<String> = topo.into_iter().filter(|n| buildable.contains(n)).collect();

        self.status_msg = Some(format!("Building {} packages...", ordered.len()));

        self.build_queue.jobs = ordered
            .into_iter()
            .map(|n| BuildJob { name: n, status: BuildJobStatus::Pending })
            .collect();
        self.gate_build();
    }

    /// Run pre-flight checks against the staged build jobs in a background thread (they shell
    /// out to xbps-src/xbps and would otherwise freeze the UI). `poll_preflight` handles the
    /// result: build immediately if clean, else show the pre-flight modal.
    fn gate_build(&mut self) {
        let void_pkgs = self.void_pkgs.clone();
        let jobs = self.build_queue.jobs.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.preflight_rx = Some(rx);
        self.preflight_pending = true;
        self.status_msg = Some("Checking build pre-flight…".to_string());
        std::thread::spawn(move || {
            let _ = tx.send(build::preflight(&void_pkgs, &jobs));
        });
    }

    /// Poll for pre-flight results. Call each tick.
    pub fn poll_preflight(&mut self) {
        if !self.preflight_pending {
            return;
        }
        let warnings = match self.preflight_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            Some(w) => w,
            None => return,
        };
        self.preflight_pending = false;
        self.preflight_rx = None;
        if warnings.is_empty() {
            self.start_build_now();
        } else {
            self.build_preflight = Some(warnings);
        }
    }

    fn start_build_now(&mut self) {
        self.build_queue.start(self.void_pkgs.clone());
        self.panel = PanelMode::BuildLog;
    }

    /// True when the pending pre-flight has a condition `./xbps-src clean` would fix.
    pub fn preflight_cleanable(&self) -> bool {
        self.build_preflight
            .as_ref()
            .is_some_and(|ws| ws.iter().any(|w| w.cleanable))
    }

    /// Pre-flight: build anyway, ignoring the warnings.
    pub fn preflight_proceed(&mut self) {
        self.build_preflight = None;
        self.start_build_now();
    }

    /// Pre-flight: clean masterdir build state first, then build.
    pub fn preflight_clean_and_build(&mut self) {
        self.build_preflight = None;
        self.status_msg = Some(build::clean_masterdir(&self.void_pkgs));
        self.start_build_now();
    }

    /// Pre-flight: dismiss and do nothing (drop the staged jobs).
    pub fn preflight_dismiss(&mut self) {
        self.build_preflight = None;
        self.build_queue.jobs.clear();
        self.status_msg = Some("Build cancelled".to_string());
    }

/// Apply pending shlib updates to common/shlibs.
    pub fn apply_shlib_updates(&mut self) {
        if self.shlib_updates.is_empty() {
            return;
        }

        let updates: Vec<(String, String, String)> = self
            .shlib_updates
            .iter()
            .map(|(_, old, new, pkgver)| (old.clone(), new.clone(), pkgver.clone()))
            .collect();

        match shlibs::update_shlibs_file(&self.void_pkgs, &updates) {
            Ok(()) => {
                let count = self.shlib_updates.len();
                let touched: Vec<String> =
                    self.shlib_updates.iter().map(|(p, _, _, _)| p.clone()).collect();
                self.shlib_updates.clear();
                self.refresh();
                // Point at the rebuild when the update came from a failed build
                let needs_rebuild: Vec<&str> = self
                    .packages
                    .iter()
                    .filter(|s| {
                        s.status == Status::BuildFailed && touched.contains(&s.package.name)
                    })
                    .map(|s| s.package.name.as_str())
                    .collect();
                self.status_msg = Some(if needs_rebuild.is_empty() {
                    format!("Updated {} shlib entries", count)
                } else {
                    format!(
                        "Updated {} shlib entries — rebuild with b: {}",
                        count,
                        needs_rebuild.join(", ")
                    )
                });
            }
            Err(e) => {
                self.status_msg = Some(format!("Failed to write common/shlibs: {}", e));
            }
        }
    }

    pub fn any_op_active(&self) -> bool {
        self.build_queue.active || self.template_bumping || self.git_op_active
    }

    /// Show cancel confirmation for the active operation.
    pub fn request_cancel(&mut self) {
        let op = if self.build_queue.active {
            "build"
        } else if self.template_bumping {
            "bump"
        } else if self.git_op_active {
            "git"
        } else {
            return;
        };
        self.cancel_confirm = Some(op.to_string());
    }

    /// Actually cancel the confirmed operation.
    pub fn confirm_cancel(&mut self) {
        match self.cancel_confirm.as_deref() {
            Some("build") => {
                self.build_queue.cancel_flag.store(true, Ordering::SeqCst);
                self.build_queue.kill_current();
                self.status_msg = Some("Cancelling build...".to_string());
            }
            Some("bump") => {
                self.bump_cancel_flag.store(true, Ordering::SeqCst);
                self.status_msg = Some("Cancelling bump...".to_string());
            }
            Some("git") => {
                self.git_cancel_flag.store(true, Ordering::SeqCst);
                if let Ok(mut guard) = self.git_current_child.lock() {
                    if let Some(ref mut c) = *guard {
                        let _ = c.kill();
                    }
                }
                self.status_msg = Some("Cancelling git operation...".to_string());
            }
            _ => {}
        }
        self.cancel_confirm = None;
    }

    pub fn deny_cancel(&mut self) {
        self.cancel_confirm = None;
    }

    /// Show quit confirmation if any op is running, otherwise quit immediately.
    pub fn request_quit(&mut self) {
        if self.any_op_active() {
            self.quit_confirm = true;
        } else {
            self.should_quit = true;
        }
    }

    /// Kill all ops and quit.
    pub fn confirm_quit(&mut self) {
        self.kill_all();
        self.should_quit = true;
    }

    pub fn deny_quit(&mut self) {
        self.quit_confirm = false;
    }

    /// Kill all active operations immediately.
    pub fn kill_all(&mut self) {
        if self.build_queue.active {
            self.build_queue.cancel_flag.store(true, Ordering::SeqCst);
            self.build_queue.kill_current();
        }
        if self.template_bumping {
            self.bump_cancel_flag.store(true, Ordering::SeqCst);
        }
        if self.git_op_active {
            self.git_cancel_flag.store(true, Ordering::SeqCst);
            if let Ok(mut guard) = self.git_current_child.lock() {
                if let Some(ref mut c) = *guard {
                    let _ = c.kill();
                }
            }
            // Run rebase --abort if that was the active git op
            if self.git_current_op == Some(GitOp::RebaseCustom) {
                let _ = std::process::Command::new("git")
                    .args(["rebase", "--abort"])
                    .current_dir(&self.void_pkgs)
                    .output();
            }
        }
    }

    pub fn refresh_git_status(&mut self) {
        self.git_status = git::get_git_status(&self.void_pkgs);
    }

    pub fn open_git_menu(&mut self) {
        self.panel = match self.panel {
            PanelMode::GitMenu => PanelMode::None,
            _ => {
                self.refresh_git_status();
                PanelMode::GitMenu
            }
        };
    }

    fn start_git_op(&mut self, op: GitOp) {
        if self.git_op_active {
            self.status_msg = Some("Git operation already in progress".to_string());
            return;
        }
        self.git_op_active = true;
        self.git_current_op = Some(op);
        self.git_cancel_flag.store(false, Ordering::SeqCst);
        self.git_output.clear();
        let void_pkgs = self.void_pkgs.clone();
        let cancel = self.git_cancel_flag.clone();
        let current_child = self.git_current_child.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.git_op_rx = Some(rx);
        std::thread::spawn(move || {
            git::run_git_op(void_pkgs, op, tx, cancel, current_child);
        });
    }

    pub fn git_sync_master(&mut self) {
        self.start_git_op(GitOp::SyncMaster);
    }

    pub fn git_rebase_custom(&mut self) {
        self.start_git_op(GitOp::RebaseCustom);
    }

    pub fn git_push_custom(&mut self) {
        self.start_git_op(GitOp::PushCustom);
    }

    pub fn poll_git(&mut self) {
        let msgs: Vec<GitMsg> = if let Some(ref rx) = self.git_op_rx {
            rx.try_iter().collect()
        } else {
            return;
        };

        for msg in msgs {
            match msg {
                GitMsg::Output(line) => {
                    self.git_output.push(line);
                }
                GitMsg::Success(line) => {
                    self.git_output.push(line.clone());
                    self.status_msg = Some(line);
                }
                GitMsg::Failed(line) => {
                    self.git_output.push(format!("ERR: {}", line));
                    self.status_msg = Some(line);
                }
                GitMsg::Done => {
                    self.git_op_active = false;
                    self.git_current_op = None;
                    self.git_op_rx = None;
                    self.refresh_git_status();
                    return;
                }
            }
        }
    }

    pub fn scroll_log_up(&mut self) {
        let max = self.build_queue.current_output.len().saturating_sub(1);
        self.build_log_scroll = (self.build_log_scroll + 1).min(max);
    }

    pub fn scroll_log_down(&mut self) {
        self.build_log_scroll = self.build_log_scroll.saturating_sub(1);
    }

    /// Packages with no known upstream version yet.
    pub fn unchecked_count(&self) -> usize {
        self.packages.iter().filter(|p| p.latest.is_none()).count()
    }

    /// Get summary counts for status bar.
    pub fn status_counts(&self) -> StatusCounts {
        let mut counts = StatusCounts::default();
        for p in &self.packages {
            // upstream-ahead is orthogonal to the build lifecycle: count it separately
            // so a package can be both (e.g.) BUILD OUTDATED and have an upstream update.
            if p.upstream_newer() {
                counts.upstream_ahead += 1;
            }
            match p.status {
                Status::UpToDate => counts.up_to_date += 1,
                Status::BuildOutdated => counts.build_outdated += 1,
                Status::ReadyToInstall => counts.ready_to_install += 1,
                Status::BuildFailed => counts.build_failed += 1,
            }
        }
        counts
    }
}

#[derive(Default)]
pub struct StatusCounts {
    pub up_to_date: usize,
    pub upstream_ahead: usize,
    pub build_outdated: usize,
    pub ready_to_install: usize,
    pub build_failed: usize,
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}
