use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct ShlibEntry {
    pub soname: String,
}

#[derive(Debug, Clone)]
pub struct SonameMismatch {
    pub registered: String,
    pub installed: String,
}

pub type ShlibMap = HashMap<String, Vec<ShlibEntry>>;

/// Parse common/shlibs from void-packages, returning a map of package name -> shlib entries.
pub fn parse_shlibs(void_pkgs: &Path) -> ShlibMap {
    let shlibs_path = void_pkgs.join("common/shlibs");
    let content = match std::fs::read_to_string(&shlibs_path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let mut map: ShlibMap = HashMap::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Format: "libfoo.so.1 foo-1.2.3_1"
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }

        let soname = parts[0].to_string();

        // Extract package name from "pkgname-ver_rev": everything before the last '-'
        let pkg_name = match parts[1].rfind('-') {
            Some(idx) => parts[1][..idx].to_string(),
            None => continue,
        };

        map.entry(pkg_name)
            .or_default()
            .push(ShlibEntry { soname });
    }

    map
}

/// SONAMEs a package provides, from xbps `shlib-provides` metadata.
/// Empty vec if the package isn't found or provides no shlibs.
fn query_shlib_provides(extra_args: &[&str], pkg_name: &str) -> Vec<String> {
    let mut cmd = Command::new("xbps-query");
    cmd.args(extra_args).args(["-p", "shlib-provides", pkg_name]);
    match cmd.output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .map(|s| s.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// SONAMEs of the installed package (pkgdb metadata — no readelf needed).
pub fn get_installed_sonames(pkg_name: &str) -> Vec<String> {
    query_shlib_provides(&[], pkg_name)
}

/// SONAMEs of the freshly built .xbps in the local repos, if any.
/// `-i` ignores configured repos so only the local build output is consulted.
pub fn get_built_sonames(void_pkgs: &Path, pkg_name: &str) -> Vec<String> {
    for repo in ["hostdir/binpkgs/custom", "hostdir/binpkgs"] {
        let repo_path = void_pkgs.join(repo);
        if !repo_path.exists() {
            continue;
        }
        let repo_arg = format!("--repository={}", repo_path.display());
        let sonames = query_shlib_provides(&["-i", &repo_arg], pkg_name);
        if !sonames.is_empty() {
            return sonames;
        }
    }
    Vec::new()
}

/// Compare registered SONAMEs against the package's actual sonames.
/// The freshly built .xbps (if any) wins over the installed package, so a
/// soname bump surfaces as `!so` right after the build, before install.
pub fn check_soname_mismatches(
    void_pkgs: &Path,
    shlibs: &[ShlibEntry],
    pkg_name: &str,
) -> Vec<SonameMismatch> {
    if shlibs.is_empty() {
        return Vec::new();
    }

    let built = get_built_sonames(void_pkgs, pkg_name);
    let installed = if built.is_empty() {
        get_installed_sonames(pkg_name)
    } else {
        built
    };
    let mut mismatches = Vec::new();

    for entry in shlibs {
        let registered = &entry.soname;

        if installed.contains(registered) {
            continue;
        }

        // Find the closest match by base name (e.g. libfoo.so.*)
        let base = soname_base(registered);
        let found = installed
            .iter()
            .find(|s| soname_base(s) == base);

        let installed_str = match found {
            Some(s) => s.clone(),
            None if installed.is_empty() => "not found".to_string(),
            None => "not found".to_string(),
        };

        mismatches.push(SonameMismatch {
            registered: registered.clone(),
            installed: installed_str,
        });
    }

    mismatches
}

/// Update common/shlibs file with new SONAME entries.
/// Each tuple is (old_soname, new_soname, new_pkg_ver).
pub fn update_shlibs_file(void_pkgs: &Path, updates: &[(String, String, String)]) -> Result<(), std::io::Error> {
    let shlibs_path = void_pkgs.join("common/shlibs");
    let content = std::fs::read_to_string(&shlibs_path)?;

    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    for (old_soname, new_soname, new_pkg_ver) in updates {
        // Skip "not found" entries — can't update what doesn't exist
        if new_soname == "not found" {
            continue;
        }
        for line in &mut lines {
            let trimmed = line.trim();
            if trimmed.starts_with(old_soname) {
                let after = &trimmed[old_soname.len()..];
                if after.starts_with(' ') || after.starts_with('\t') {
                    *line = format!("{} {}", new_soname, new_pkg_ver);
                }
            }
        }
    }

    let new_content = lines.join("\n");
    // Preserve trailing newline if original had one
    let final_content = if content.ends_with('\n') {
        format!("{}\n", new_content)
    } else {
        new_content
    };

    std::fs::write(&shlibs_path, final_content)
}

/// Extract base library name: "libfoo.so.4" -> "libfoo.so"
fn soname_base(soname: &str) -> &str {
    match soname.find(".so.") {
        Some(idx) => &soname[..idx + 3], // include ".so"
        None => soname,
    }
}

/// Parse xbps-src pkglint SONAME-bump failures out of build output.
/// Matches the message pair (any line prefix tolerated):
///   `=> ERROR: pkg-1.0_1: SONAME bump detected: libfoo.so.0 -> libfoo.so.4`
///   `=> ERROR: pkg-1.0_1: please update common/shlibs with this line: "libfoo.so.4 pkg-1.0_1"`
/// Returns (old_soname, new_soname, new_pkgver) triples, deduped.
pub fn parse_soname_bump_errors(lines: &[String]) -> Vec<(String, String, String)> {
    const BUMP: &str = "SONAME bump detected: ";
    const UPDATE: &str = "please update common/shlibs with this line: \"";

    // Collect suggested shlibs lines: new_soname -> new_pkgver
    let mut suggested: HashMap<String, String> = HashMap::new();
    for line in lines {
        if let Some(pos) = line.find(UPDATE) {
            let quoted = &line[pos + UPDATE.len()..];
            if let Some(end) = quoted.find('"') {
                let mut parts = quoted[..end].split_whitespace();
                if let (Some(so), Some(ver)) = (parts.next(), parts.next()) {
                    suggested.insert(so.to_string(), ver.to_string());
                }
            }
        }
    }

    let mut out: Vec<(String, String, String)> = Vec::new();
    for line in lines {
        if let Some(pos) = line.find(BUMP) {
            let tail = &line[pos + BUMP.len()..];
            if let Some((old, new)) = tail.split_once(" -> ") {
                let old = old.trim().to_string();
                let new = new.trim().to_string();
                if let Some(ver) = suggested.get(&new) {
                    if !out.iter().any(|(o, n, _)| o == &old && n == &new) {
                        out.push((old, new, ver.clone()));
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pkglint_soname_bump_pair() {
        let lines: Vec<String> = [
            "=> hyprgraphics-0.5.1_1: running pre-pkg hook: 99-pkglint ...",
            "ERR: => ERROR: hyprgraphics-0.5.1_1: SONAME bump detected: libhyprgraphics.so.0 -> libhyprgraphics.so.4",
            "ERR: => ERROR: hyprgraphics-0.5.1_1: please update common/shlibs with this line: \"libhyprgraphics.so.4 hyprgraphics-0.5.1_1\"",
            "ERR: => ERROR: hyprgraphics-0.5.1_1: cannot continue with installation!",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let got = parse_soname_bump_errors(&lines);
        assert_eq!(
            got,
            vec![(
                "libhyprgraphics.so.0".to_string(),
                "libhyprgraphics.so.4".to_string(),
                "hyprgraphics-0.5.1_1".to_string()
            )]
        );
    }

    #[test]
    fn bump_without_update_line_is_ignored() {
        let lines = vec!["SONAME bump detected: libfoo.so.1 -> libfoo.so.2".to_string()];
        assert!(parse_soname_bump_errors(&lines).is_empty());
    }

    #[test]
    fn duplicate_bump_lines_dedupe() {
        let lines: Vec<String> = [
            "SONAME bump detected: libfoo.so.1 -> libfoo.so.2",
            "SONAME bump detected: libfoo.so.1 -> libfoo.so.2",
            "please update common/shlibs with this line: \"libfoo.so.2 foo-2.0_1\"",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(parse_soname_bump_errors(&lines).len(), 1);
    }
}
