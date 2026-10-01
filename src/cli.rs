//! Non-interactive CLI surface. Drives the same primitives as the TUI:
//! `version_check::check_all_versions_streaming` and `template::bump_template`.
//! Designed to be composed from runit/cron wrappers (see [[caretaker]]).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::{
    build, config,
    package::{version_newer_pub as version_newer, Package},
    repo, template, version_check,
};

pub fn run(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("check-updates") => check_updates(&args[1..]),
        Some("bump") => bump(&args[1..]),
        Some(other) => bail!("unknown subcommand: {}", other),
        None => unreachable!("dispatch guards against this"),
    }
}

#[derive(Serialize)]
struct UpdateRow {
    name: String,
    current: String,
    latest: String,
}

fn discover_and_load(cfg: &config::Config) -> Result<Vec<Package>> {
    let names = repo::discover_custom_packages(&cfg.void_packages)
        .context("discovering custom packages")?;
    Ok(repo::load_packages(&cfg.void_packages, &names))
}

/// Run a full version sweep, returning `name -> latest` for everything we resolved.
fn sweep_latest(void_pkgs: &Path, pkgs: &[Package], force: bool) -> (HashMap<String, String>, bool) {
    let (tx, rx) = std::sync::mpsc::channel();
    // Synchronous: the streaming function blocks until done, sender is dropped on return.
    version_check::check_all_versions_streaming(void_pkgs, pkgs, force, tx);

    let mut latest: HashMap<String, String> = HashMap::new();
    let mut rate_limited = false;
    for msg in rx {
        match msg {
            version_check::VersionMsg::Found(name, ver, _age) => {
                latest.insert(name, ver);
            }
            version_check::VersionMsg::Done(_, rl) => rate_limited = rl,
        }
    }
    (latest, rate_limited)
}

fn check_updates(args: &[String]) -> Result<i32> {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            other => bail!("unknown flag: {}", other),
        }
    }

    let cfg = config::load();
    let packages = discover_and_load(&cfg)?;
    let (latest, rate_limited) = sweep_latest(&cfg.void_packages, &packages, true);

    let mut rows: Vec<UpdateRow> = packages
        .iter()
        .filter_map(|p| {
            latest.get(&p.name).and_then(|l| {
                version_newer(l, &p.version).then(|| UpdateRow {
                    name: p.name.clone(),
                    current: p.version.clone(),
                    latest: l.clone(),
                })
            })
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        for r in &rows {
            println!("{} {} -> {}", r.name, r.current, r.latest);
        }
    }

    if rate_limited {
        eprintln!("warning: GitHub rate-limited mid-sweep; results may be incomplete");
        return Ok(2);
    }
    Ok(if rows.is_empty() { 0 } else { 1 })
}

fn bump(args: &[String]) -> Result<i32> {
    let cfg = config::load();
    let packages = discover_and_load(&cfg)?;

    let mut all = false;
    let mut target: Option<String> = None;
    for arg in args {
        match arg.as_str() {
            "--all" => all = true,
            other if other.starts_with("--") => bail!("unknown flag: {}", other),
            other => {
                if target.is_some() {
                    bail!("bump takes a single package name (or --all)");
                }
                target = Some(other.to_string());
            }
        }
    }
    if all && target.is_some() {
        bail!("--all and <pkg> are mutually exclusive");
    }
    if !all && target.is_none() {
        bail!("bump requires <pkg> or --all");
    }

    // force=false: reuse the 1h cache if `check-updates` just primed it.
    let (latest, rate_limited) = sweep_latest(&cfg.void_packages, &packages, false);
    if rate_limited {
        eprintln!("warning: GitHub rate-limited mid-sweep; some packages may be skipped");
    }

    let targets: Vec<(String, String)> = if all {
        packages
            .iter()
            .filter_map(|p| {
                latest
                    .get(&p.name)
                    .filter(|l| version_newer(l, &p.version))
                    .map(|l| (p.name.clone(), l.clone()))
            })
            .collect()
    } else {
        let name = target.unwrap();
        let pkg = packages
            .iter()
            .find(|p| p.name == name)
            .with_context(|| format!("unknown package: {}", name))?;
        let l = latest
            .get(&name)
            .with_context(|| format!("no upstream version resolved for {}", name))?;
        if !version_newer(l, &pkg.version) {
            eprintln!("{} already at {} (latest {})", name, pkg.version, l);
            return Ok(0);
        }
        vec![(name, l.clone())]
    };

    if targets.is_empty() {
        eprintln!("nothing to bump");
        return Ok(0);
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let mut failed = 0;
    for (name, new_ver) in targets {
        let log_path = build::bump_log_path(&name);
        eprintln!("bumping {} -> {} (log: {})", name, new_ver, log_path.display());
        match template::bump_template(&cfg.void_packages, &name, &new_ver, &log_path, cancel.clone())
        {
            Ok(r) => println!("{} {} -> {}", name, r.old_version, r.new_version),
            Err(e) => {
                eprintln!("bump failed for {}: {}", name, e);
                failed += 1;
            }
        }
    }
    Ok(if failed == 0 { 0 } else { 1 })
}

