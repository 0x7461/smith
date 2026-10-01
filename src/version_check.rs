use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::package::Package;

const CACHE_TTL_SECS: u64 = 3600; // 1 hour

#[derive(Serialize, Deserialize, Default)]
struct VersionCache {
    entries: HashMap<String, CacheEntry>,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    version: String,
    timestamp: u64,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
}

fn cache_path() -> PathBuf {
    let dir = dirs_cache().join("smith");
    fs::create_dir_all(&dir).ok();
    dir.join("versions.json")
}

fn dirs_cache() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(format!("{}/.cache", home))
    } else {
        PathBuf::from("/tmp")
    }
}

fn load_cache() -> VersionCache {
    let path = cache_path();
    if let Ok(data) = fs::read_to_string(&path) {
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        VersionCache::default()
    }
}

fn save_cache(cache: &VersionCache) {
    let path = cache_path();
    if let Ok(data) = serde_json::to_string_pretty(cache) {
        fs::write(path, data).ok();
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Extract GitHub owner/repo from a distfiles or homepage URL.
fn extract_github_repo(pkg: &Package) -> Option<(String, String)> {
    // Try distfiles first, then homepage, then changelog
    for url in [&pkg.distfiles, &pkg.homepage, &pkg.changelog] {
        if let Some((owner, repo)) = parse_github_url(url) {
            return Some((owner, repo));
        }
    }
    None
}

fn parse_github_url(url: &str) -> Option<(String, String)> {
    // Match github.com/owner/repo patterns
    let url = url.split('>').next().unwrap_or(url); // Strip >rename suffix
    if !url.contains("github.com") {
        return None;
    }
    let parts: Vec<&str> = url.split("github.com/").collect();
    if parts.len() < 2 {
        return None;
    }
    let path_parts: Vec<&str> = parts[1].split('/').collect();
    if path_parts.len() >= 2 {
        let owner = path_parts[0].to_string();
        let repo = path_parts[1]
            .trim_end_matches(".git")
            .to_string();
        if !owner.is_empty() && !repo.is_empty() {
            return Some((owner, repo));
        }
    }
    None
}

/// Check upstream version for a single package via GitHub API.
fn check_github(owner: &str, repo: &str) -> Result<Option<String>> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases/latest",
        owner, repo
    );

    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(&url)
        .header("User-Agent", "smith/0.8")
        .header("Accept", "application/vnd.github+json")
        .send()?;

    if !resp.status().is_success() {
        // Detect rate limit before trying fallback — no point hitting tags endpoint either
        if resp.status() == reqwest::StatusCode::FORBIDDEN
            || resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            return Err(anyhow::anyhow!("rate_limited"));
        }

        // Try tags endpoint as fallback (some repos don't use releases)
        let url = format!(
            "https://api.github.com/repos/{}/{}/tags?per_page=1",
            owner, repo
        );
        let resp = client
            .get(&url)
            .header("User-Agent", "smith/0.8")
            .header("Accept", "application/vnd.github+json")
            .send()?;

        if resp.status().is_success() {
            let tags: Vec<GitHubRelease> = resp.json()?;
            if let Some(tag) = tags.first() {
                return Ok(Some(clean_version_tag(&tag.tag_name)));
            }
        }
        return Ok(None);
    }

    let release: GitHubRelease = resp.json()?;
    Ok(Some(clean_version_tag(&release.tag_name)))
}

/// Clean version tag: strip leading 'v', 'release-', etc.
fn clean_version_tag(tag: &str) -> String {
    let tag = tag.strip_prefix('v').unwrap_or(tag);
    let tag = tag.strip_prefix("release-").unwrap_or(tag);
    tag.to_string()
}

/// Check upstream version via xbps-src update-check (fallback).
fn check_xbps_src(void_pkgs: &Path, name: &str) -> Result<Option<String>> {
    let output = Command::new("./xbps-src")
        .args(["update-check", name])
        .current_dir(void_pkgs)
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_update_check(name, &stdout))
}

/// Parse `xbps-src update-check` output.
/// Format: "pkgname-oldver -> pkgname-newver" (newver may carry "_revision").
fn parse_update_check(name: &str, stdout: &str) -> Option<String> {
    let prefix = format!("{name}-");
    for line in stdout.lines() {
        let Some((_, rhs)) = line.split_once("->") else { continue };
        let rhs = rhs.trim();
        let ver = rhs.strip_prefix(&prefix).unwrap_or(rhs);
        let ver = ver.split('_').next().unwrap_or(ver);
        if !ver.is_empty() {
            return Some(ver.to_string());
        }
    }
    None
}

/// Returns the oldest timestamp across all cache entries (seconds since epoch).
/// Represents the least-recently-checked package — the true staleness floor.
pub fn last_check_time() -> Option<u64> {
    let cache = load_cache();
    cache.entries.values().map(|e| e.timestamp).min()
}

pub enum VersionMsg {
    Found(String, String, Option<u64>), // (name, version, cache_age_secs if from cache)
    Done(usize, bool),                  // (packages attempted, rate_limited)
}

/// Check all packages for upstream versions, sending results incrementally.
pub fn check_all_versions_streaming(
    void_pkgs: &Path,
    packages: &[Package],
    force: bool,
    tx: std::sync::mpsc::Sender<VersionMsg>,
) {
    let mut cache = load_cache();
    let now = now_secs();
    let mut count = 0;

    for pkg in packages {
        count += 1;

        // Check cache first
        if !force {
            if let Some(entry) = cache.entries.get(&pkg.name) {
                if now - entry.timestamp < CACHE_TTL_SECS {
                    let age = now - entry.timestamp;
                    let _ = tx.send(VersionMsg::Found(pkg.name.clone(), entry.version.clone(), Some(age)));
                    continue;
                }
            }
        }

        // Try GitHub first
        let version = if let Some((owner, repo)) = extract_github_repo(pkg) {
            match check_github(&owner, &repo) {
                Ok(Some(v)) => Some(v),
                Err(e) if e.to_string() == "rate_limited" => {
                    save_cache(&cache);
                    let _ = tx.send(VersionMsg::Done(count, true));
                    return;
                }
                _ => check_xbps_src(void_pkgs, &pkg.name).ok().flatten(),
            }
        } else {
            check_xbps_src(void_pkgs, &pkg.name).ok().flatten()
        };

        if let Some(ver) = version {
            cache.entries.insert(
                pkg.name.clone(),
                CacheEntry {
                    version: ver.clone(),
                    timestamp: now,
                },
            );
            let _ = tx.send(VersionMsg::Found(pkg.name.clone(), ver, None));
        }
    }

    save_cache(&cache);
    let _ = tx.send(VersionMsg::Done(count, false));
}


#[cfg(test)]
mod tests {
    use super::parse_update_check;

    #[test]
    fn parses_arrow_format() {
        let out = "google-chrome-145.0.7632.67 -> google-chrome-148.0.7778.167\n";
        assert_eq!(parse_update_check("google-chrome", out).as_deref(), Some("148.0.7778.167"));
    }

    #[test]
    fn parses_arrow_with_revision_suffix() {
        let out = "foo-1.0 -> foo-1.2_3\n";
        assert_eq!(parse_update_check("foo", out).as_deref(), Some("1.2"));
    }

    #[test]
    fn returns_none_when_no_update() {
        assert_eq!(parse_update_check("foo", ""), None);
        assert_eq!(parse_update_check("foo", "some unrelated line\n"), None);
    }
}
