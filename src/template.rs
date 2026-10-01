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

    // Parse variables from template to resolve distfiles URL
    let vars = parse_template_vars(&content);
    let old_version = vars.get("version").cloned().unwrap_or_default();
    writeln!(log, "   Current version: {}", old_version)?;

    // Get the raw distfiles line (with ${version} unexpanded)
    let raw_distfiles = vars.get("distfiles").cloned().unwrap_or_default();

    // Resolve the download URL with the new version
    let resolved = resolve_distfiles_url(&raw_distfiles, &vars, new_version);
    writeln!(log, "=> Resolved distfiles URL:")?;
    writeln!(log, "   {}", resolved.url)?;
    writeln!(log, "   cache filename: {}", resolved.cache_filename)?;

    if resolved.url.is_empty() {
        writeln!(log, "=> FAILED: could not resolve distfiles URL")?;
        bail!("Could not resolve distfiles URL for {}", name);
    }

    // Download tarball and compute SHA256 (also caches to hostdir/sources/ for xbps-src)
    let sources_dir = void_pkgs.join("hostdir").join("sources");
    writeln!(log, "=> Downloading and computing SHA256...")?;
    let _ = log.flush();
    match download_and_checksum(&resolved.url, &resolved.cache_filename, &sources_dir, &cancel, true) {
        Ok(new_checksum) => {
            writeln!(log, "   checksum={}", new_checksum)?;

            // Rewrite template file line by line
            writeln!(log, "=> Rewriting template: version={}, revision=1", new_version)?;
            let new_content = rewrite_template(&content, new_version, &new_checksum);
            fs::write(&template_path, &new_content)
                .with_context(|| format!("writing template: {}", template_path.display()))?;

            writeln!(log, "=> Done. {} {} → {}", name, old_version, new_version)?;

            Ok(BumpResult {
                old_version,
                new_version: new_version.to_string(),
            })
        }
        Err(e) => {
            writeln!(log, "=> FAILED: {:?}", e)?;
            Err(e.context(format!("downloading {}", resolved.url)))
        }
    }
}

/// Parse shell-style variable assignments from a template, keeping values unexpanded.
fn parse_template_vars(content: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    let mut in_multiline: Option<String> = None;
    let mut multiline_buf = String::new();

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
        return Ok(format!("{:x}", hasher.finalize()));
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
    Ok(format!("{:x}", hash))
}

/// Rewrite template content: update version, reset revision to 1, update checksum.
fn rewrite_template(content: &str, new_version: &str, new_checksum: &str) -> String {
    let mut lines: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("version=") {
            // Preserve indentation
            let indent = &line[..line.len() - trimmed.len()];
            lines.push(format!("{}version={}", indent, new_version));
        } else if trimmed.starts_with("revision=") {
            let indent = &line[..line.len() - trimmed.len()];
            lines.push(format!("{}revision=1", indent));
        } else if trimmed.starts_with("checksum=") {
            let indent = &line[..line.len() - trimmed.len()];
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
    fn query_string_stripped_from_cache_filename() {
        let raw = "https://example.com/dl?file=foo-${version}.tar.gz";
        let r = resolve_distfiles_url(raw, &vars(&[]), "1.0");
        assert_eq!(r.cache_filename, "dl");
    }
}
