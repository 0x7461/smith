use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct BumpResult {
    pub old_version: String,
    pub new_version: String,
}

/// Bump a template to a new version: rewrite version, reset revision, update checksum.
/// Logs each step to the given log file path.
pub fn bump_template(void_pkgs: &Path, name: &str, new_version: &str, log_path: &Path, cancel: Arc<AtomicBool>) -> Result<BumpResult> {
    let mut log = fs::File::create(log_path)
        .with_context(|| format!("creating bump log: {}", log_path.display()))?;

    writeln!(log, "=> Bumping {} to v{}", name, new_version)?;

    let template_path = void_pkgs.join("srcpkgs").join(name).join("template");
    writeln!(log, "=> Reading template: {}", template_path.display())?;
    let content = fs::read_to_string(&template_path)
        .with_context(|| format!("reading template: {}", template_path.display()))?;

    // Parse variables from template to resolve distfiles URLs
    let vars = parse_template_vars(&content);
    let old_version = vars.get("version").cloned().unwrap_or_default();
    writeln!(log, "   Current version: {}", old_version)?;

    // One checksum line per arch branch (google-chrome). Update every one, so a
    // multi-arch template is not left with the other arch on the old hash.
    let targets = checksum_targets(&content);
    if targets.is_empty() {
        writeln!(log, "=> FAILED: no checksum/distfiles in template")?;
        bail!("no checksum/distfiles found in {} template", name);
    }

    let sources_dir = void_pkgs.join("hostdir").join("sources");
    let mut new_checksums: HashMap<usize, String> = HashMap::new();
    for (line_idx, raw_distfiles) in &targets {
        if raw_distfiles.is_empty() {
            writeln!(log, "=> FAILED: checksum at line {} has no distfiles", line_idx + 1)?;
            bail!("checksum at line {} of {} has no distfiles in scope", line_idx + 1, name);
        }
        let resolved = resolve_distfiles_url(raw_distfiles, &vars, new_version);
        if resolved.url.is_empty() {
            writeln!(log, "=> FAILED: could not resolve distfiles URL at line {}", line_idx + 1)?;
            bail!("Could not resolve distfiles URL for {} (line {})", name, line_idx + 1);
        }
        writeln!(log, "=> Downloading and computing SHA256:")?;
        writeln!(log, "   {}", resolved.url)?;
        let _ = log.flush();
        match download_and_checksum(&resolved.url, &resolved.cache_filename, &sources_dir, &cancel, true) {
            Ok(cs) => {
                writeln!(log, "   checksum={}", cs)?;
                new_checksums.insert(*line_idx, cs);
            }
            Err(e) => {
                writeln!(log, "=> FAILED: {:?}", e)?;
                return Err(e.context(format!("downloading {}", resolved.url)));
            }
        }
    }

    writeln!(log, "=> Rewriting template: version={}, revision=1", new_version)?;
    let new_content = rewrite_template(&content, new_version, &new_checksums);
    fs::write(&template_path, &new_content)
        .with_context(|| format!("writing template: {}", template_path.display()))?;

    writeln!(log, "=> Done. {} {} → {}", name, old_version, new_version)?;

    Ok(BumpResult {
        old_version,
        new_version: new_version.to_string(),
    })
}

/// The xbps-src target machine for this host: `XBPS_TARGET_MACHINE` when set,
/// else mapped from the build arch.
fn host_arch() -> String {
    if let Ok(a) = std::env::var("XBPS_TARGET_MACHINE") {
        if !a.is_empty() {
            return a;
        }
    }
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "x86" => "i686",
        "arm" => "armv7l",
        "powerpc64" => "ppc64le",
        other => other,
    }
    .to_string()
}

/// Tracks `case … esac` blocks so assignments are read from the branch that
/// matches this host. Without it the last branch won: google-chrome's `aarch64`
/// distfile and checksum overwrote the `x86_64` ones.
///
/// Handles the form void templates use: one pattern per line, `case <subject> in`,
/// `;;` between branches, `esac`. An inline `pattern) command ;;` is not
/// recognised.
pub(crate) struct CaseState {
    in_case: bool,
    matched: bool,
    active: bool,
}

impl CaseState {
    pub(crate) fn new() -> Self {
        Self { in_case: false, matched: false, active: false }
    }

    /// Consume a line as case-control syntax. Returns true when it was one, so
    /// the caller skips it as an assignment.
    pub(crate) fn observe(&mut self, trimmed: &str) -> bool {
        if !self.in_case {
            if trimmed.starts_with("case ") && trimmed.ends_with(" in") {
                self.in_case = true;
                self.matched = false;
                self.active = false;
                return true;
            }
            return false;
        }
        if trimmed == "esac" {
            self.in_case = false;
            self.matched = false;
            self.active = false;
            return true;
        }
        if trimmed == ";;" {
            self.active = false;
            return true;
        }
        if let Some(pat) = branch_pattern(trimmed) {
            self.active = !self.matched && pattern_matches(pat);
            self.matched |= self.active;
            return true;
        }
        false
    }

    /// Whether an assignment on the current line applies to this host.
    pub(crate) fn allow(&self) -> bool {
        !self.in_case || self.active
    }
}

fn branch_pattern(line: &str) -> Option<&str> {
    let inner = line.strip_suffix(')')?.trim();
    if inner.is_empty()
        || inner.contains('(')
        || inner.contains('=')
        || inner.chars().any(|c| c.is_whitespace())
    {
        return None;
    }
    Some(inner)
}

fn pattern_matches(pattern: &str) -> bool {
    let arch = host_arch();
    pattern
        .split('|')
        .map(str::trim)
        .any(|p| p == arch || p == "*" || p == "all")
}

/// For each `checksum=` assignment: its 0-based line index and the raw
/// `distfiles` value in scope (top-level, or the enclosing case branch). The
/// bump uses this to download and update every arch's checksum, not just the
/// host branch's.
fn checksum_targets(content: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut case_state = CaseState::new();
    let mut in_function = false;
    let mut distfiles = String::new();
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.ends_with("() {") {
            in_function = true;
            continue;
        }
        if trimmed == "}" {
            in_function = false;
            continue;
        }
        if in_function {
            continue;
        }
        if case_state.observe(trimmed) {
            continue;
        }
        if let Some(eq) = trimmed.find('=') {
            let lhs = trimmed[..eq].trim();
            let (name, append) = match lhs.strip_suffix('+') {
                Some(n) => (n.trim(), true),
                None => (lhs, false),
            };
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                let val = trimmed[eq + 1..].trim().trim_matches('"').to_string();
                if name == "distfiles" {
                    if append {
                        if !distfiles.is_empty() {
                            distfiles.push(' ');
                        }
                        distfiles.push_str(&val);
                    } else {
                        distfiles = val;
                    }
                } else if name == "checksum" {
                    out.push((idx, distfiles.clone()));
                }
            }
        }
    }
    out
}

/// Parse shell-style variable assignments from a template, keeping values unexpanded.
fn parse_template_vars(content: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    let mut in_multiline: Option<String> = None;
    let mut multiline_buf = String::new();
    let mut case_state = CaseState::new();

    for line in content.lines() {
        if let Some(ref varname) = in_multiline {
            if line.contains('"') {
                let before_quote = line.split('"').next().unwrap_or("");
                multiline_buf.push(' ');
                multiline_buf.push_str(before_quote.trim());
                let varname = varname.clone();
                vars.insert(varname, multiline_buf.trim().to_string());
                multiline_buf.clear();
                in_multiline = None;
            } else {
                multiline_buf.push(' ');
                multiline_buf.push_str(line.trim());
            }
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if case_state.observe(trimmed) {
            continue;
        }
        if !case_state.allow() {
            continue;
        }

        if let Some(idx) = trimmed.find('=') {
            let varname = trimmed[..idx].trim();
            if !varname.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let rest = trimmed[idx + 1..].trim();

            // Multiline
            if rest.starts_with('"') && rest.len() > 1 && !rest[1..].contains('"') {
                in_multiline = Some(varname.to_string());
                multiline_buf = rest[1..].trim().to_string();
                continue;
            }
            if rest == "\"" {
                in_multiline = Some(varname.to_string());
                multiline_buf.clear();
                continue;
            }

            let val = rest.trim_matches('"').to_string();
            vars.insert(varname.to_string(), val);
        }
    }

    vars
}

/// A distfile resolved against a specific version: the download URL plus the
/// filename xbps-src will use to cache it under `hostdir/sources/`.
struct ResolvedDistfile {
    url: String,
    cache_filename: String,
}

/// Resolve distfiles URL by substituting template variables with the new version.
/// Honors xbps-src's `URL>rename` syntax — when present, `rename` is used as the
/// cache filename (matching xbps-src), since the upstream URL filename is often
/// version-agnostic (e.g. `zed-linux-x86_64.tar.gz`) and would collide across
/// releases.
fn resolve_distfiles_url(raw: &str, vars: &HashMap<String, String>, new_version: &str) -> ResolvedDistfile {
    let mut expanded = raw.to_string();

    // Substitute ${version} with new version
    expanded = expanded.replace("${version}", new_version);

    // Substitute other known variables. Sort longest key first so that a key
    // which is a prefix of another (e.g. $foo vs $foobar) doesn't corrupt the
    // longer match before it gets a chance to be replaced.
    let mut sorted_keys: Vec<&String> = vars.keys().collect();
    sorted_keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
    for key in sorted_keys {
        if key == "version" {
            continue; // already handled
        }
        let resolved_val = vars[key].replace("${version}", new_version);
        expanded = expanded.replace(&format!("${{{}}}", key), &resolved_val);
        expanded = expanded.replace(&format!("${}", key), &resolved_val);
    }

    // Split off any `>rename` suffix
    let (url_part, rename_part) = match expanded.rfind('>') {
        Some(idx) => (expanded[..idx].to_string(), Some(expanded[idx + 1..].trim().to_string())),
        None => (expanded, None),
    };
    let url = url_part.trim().to_string();

    let cache_filename = rename_part.unwrap_or_else(|| {
        let raw_filename = url.split('/').next_back().unwrap_or("download");
        raw_filename.split('?').next().unwrap_or(raw_filename).to_string()
    });

    ResolvedDistfile { url, cache_filename }
}

/// Lowercase-hex-encode a digest. `digest` 0.11's `Output` array dropped its `LowerHex` impl
/// (generic-array -> hybrid-array), so the bytes are formatted directly.
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Download a URL, stream to sources_dir for xbps-src caching, and return its SHA256 hex digest.
/// `cache_filename` is the name used under `sources_dir/` and must match xbps-src's filename
/// (which is the `>rename` part of a distfiles entry when present, not the URL tail).
///
/// When `force` is true, any existing cache file at the target path is removed before
/// downloading. Required on the bump path: xbps-src's content-addressable hardlinks
/// (`hostdir/sources/by_sha256/`) can populate `<pkg>-<new_version>/<file>` with content
/// that was looked up using the *old* checksum, so trusting an existing file there would
/// silently checksum the wrong artifact.
fn download_and_checksum(url: &str, cache_filename: &str, sources_dir: &Path, cancel: &Arc<AtomicBool>, force: bool) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("smith/0.8")
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()?;

    fs::create_dir_all(sources_dir)
        .with_context(|| format!("creating sources dir: {}", sources_dir.display()))?;
    let final_path = sources_dir.join(cache_filename);
    let tmp_path = sources_dir.join(format!("{}.tmp", cache_filename));

    if force && final_path.exists() {
        fs::remove_file(&final_path)
            .with_context(|| format!("removing stale cache file: {}", final_path.display()))?;
    } else if final_path.exists() {
        // Reuse cached file if already present (avoids re-downloading after xbps-src or manual dl)
        let mut f = fs::File::open(&final_path)
            .with_context(|| format!("opening cached file: {}", final_path.display()))?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = std::io::Read::read(&mut f, &mut buf).context("hashing cached file")?;
            if n == 0 { break; }
            hasher.update(&buf[..n]);
        }
        return Ok(to_hex(&hasher.finalize()));
    }

    let mut response = client.get(url).send()?.error_for_status()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    {
        let mut tmp_file = fs::File::create(&tmp_path)
            .with_context(|| format!("creating temp file: {}", tmp_path.display()))?;
        loop {
            if cancel.load(Ordering::SeqCst) {
                drop(tmp_file);
                let _ = fs::remove_file(&tmp_path);
                bail!("cancelled");
            }
            let n = std::io::Read::read(&mut response, &mut buf)
                .context("reading response body")?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            tmp_file.write_all(&buf[..n])
                .context("writing to temp file")?;
        }
    }
    fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("moving to source cache: {}", final_path.display()))?;

    let hash = hasher.finalize();
    Ok(to_hex(&hash))
}

/// Rewrite template content: update version, reset revision to 1, and replace
/// each checksum line with the digest computed for its own distfiles (keyed by
/// 0-based line index). Checksum lines absent from the map are left untouched.
fn rewrite_template(content: &str, new_version: &str, new_checksums: &HashMap<usize, String>) -> String {
    let mut lines: Vec<String> = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        let indent = &line[..line.len() - trimmed.len()];

        if trimmed.starts_with("version=") {
            lines.push(format!("{}version={}", indent, new_version));
        } else if trimmed.starts_with("revision=") {
            lines.push(format!("{}revision=1", indent));
        } else if let Some(new_checksum) = trimmed
            .starts_with("checksum=")
            .then(|| new_checksums.get(&idx))
            .flatten()
        {
            lines.push(format!("{}checksum={}", indent, new_checksum));
        } else {
            lines.push(line.to_string());
        }
    }

    let mut result = lines.join("\n");
    if content.ends_with('\n') {
        result.push('\n');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn rename_suffix_becomes_cache_filename() {
        // zed's distfiles uses >rename because the upstream filename is version-agnostic
        let raw = "https://github.com/zed-industries/zed/releases/download/v${version}/zed-linux-x86_64.tar.gz>zed-${version}.tar.gz";
        let r = resolve_distfiles_url(raw, &vars(&[]), "1.2.6");
        assert_eq!(r.url, "https://github.com/zed-industries/zed/releases/download/v1.2.6/zed-linux-x86_64.tar.gz");
        assert_eq!(r.cache_filename, "zed-1.2.6.tar.gz");
    }

    #[test]
    fn no_rename_uses_url_tail() {
        let raw = "https://example.com/foo/bar-${version}.tar.gz";
        let r = resolve_distfiles_url(raw, &vars(&[]), "2.0.0");
        assert_eq!(r.url, "https://example.com/foo/bar-2.0.0.tar.gz");
        assert_eq!(r.cache_filename, "bar-2.0.0.tar.gz");
    }

    #[test]
    fn channel_var_substitution() {
        // chrome-style: distfiles uses ${_channel} alongside ${version}
        let raw = "https://dl.google.com/foo/google-chrome-${_channel}_${version}-1_amd64.deb";
        let r = resolve_distfiles_url(raw, &vars(&[("_channel", "stable")]), "148.0.7778.167");
        assert_eq!(r.url, "https://dl.google.com/foo/google-chrome-stable_148.0.7778.167-1_amd64.deb");
        assert_eq!(r.cache_filename, "google-chrome-stable_148.0.7778.167-1_amd64.deb");
    }

    #[test]
    fn to_hex_matches_lowercase_sha256() {
        // sha256("abc") — pins the encoding to the format the old `{:x}` produced
        let mut hasher = Sha256::new();
        hasher.update(b"abc");
        assert_eq!(
            to_hex(&hasher.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn query_string_stripped_from_cache_filename() {
        let raw = "https://example.com/dl?file=foo-${version}.tar.gz";
        let r = resolve_distfiles_url(raw, &vars(&[]), "1.0");
        assert_eq!(r.cache_filename, "dl");
    }

    #[test]
    fn case_block_reads_host_branch() {
        let content = "\
pkgname=chrome\nversion=155\ncase \"$XBPS_TARGET_MACHINE\" in\nx86_64)\n\tdistfiles=\"https://x/amd64-${version}.deb\"\n\tchecksum=aaa\n\t;;\naarch64)\n\tdistfiles=\"https://x/arm64-${version}.deb\"\n\tchecksum=bbb\n\t;;\n*)\n\tbroken=\"no distfiles\"\n\t;;\nesac\n";
        let v = parse_template_vars(content);
        assert!(v["distfiles"].contains("amd64"), "got {}", v["distfiles"]);
        assert!(!v.contains_key("broken"), "fallback branch leaked in");
        // Every branch's checksum is captured (so all arches can be updated).
        let targets = checksum_targets(content);
        assert_eq!(targets.len(), 2);
        assert!(targets[0].1.contains("amd64"));
        assert!(targets[1].1.contains("arm64"));
    }

    #[test]
    fn rewrite_updates_each_checksum_by_line() {
        let content = "\
version=1\nrevision=3\ncase \"$X\" in\nx86_64)\n\tchecksum=old1\n\t;;\naarch64)\n\tchecksum=old2\n\t;;\nesac\n";
        let mut m = HashMap::new();
        m.insert(4usize, "new1".to_string());
        m.insert(7usize, "new2".to_string());
        let out = rewrite_template(content, "2", &m);
        assert!(out.contains("version=2"));
        assert!(out.contains("revision=1"));
        assert!(out.contains("checksum=new1"));
        assert!(out.contains("checksum=new2"));
        assert!(!out.contains("old1") && !out.contains("old2"));
    }
}
