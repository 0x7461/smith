use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct ShlibEntry {
    pub soname: String,
    /// The `pkgname-version_revision` field as registered in common/shlibs.
    /// Kept (not discarded at parse time) so an entry can be validated against
    /// the srcpkgs template: a soname comparison alone cannot see a stale
    /// *version* on an otherwise-correct soname.
    pub pkgver: String,
}

/// Why a registered entry disagrees with reality. The kinds need different
/// actions, and collapsing them is what made the badge a dead end: a bump is
/// rewritable in place, the other two are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MismatchKind {
    /// Soname moved (libfoo.so.1 -> libfoo.so.2). Rewritable.
    Bump,
    /// Registered soname has no counterpart at all — the package no longer
    /// provides it. The fix is deleting the line, not rewriting it.
    Orphaned,
    /// Soname is right but the registered `pkgname-version_revision` is behind
    /// the template. Invisible to a soname comparison; the case that let
    /// libhyprlang.so.2 sit registered twice with different versions.
    StaleVersion,
}

#[derive(Debug, Clone)]
pub struct SonameMismatch {
    pub registered: String,
    pub installed: String,
    pub kind: MismatchKind,
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
        let pkgver = parts[1].to_string();

        // Extract package name from "pkgname-ver_rev": everything before the last '-'
        let pkg_name = match parts[1].rfind('-') {
            Some(idx) => parts[1][..idx].to_string(),
            None => continue,
        };

        map.entry(pkg_name)
            .or_default()
            .push(ShlibEntry { soname, pkgver });
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
    template_pkgver: &str,
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
            // Soname is right; the registered version can still be behind. A
            // soname comparison structurally cannot see this — it is why
            // libhyprlang.so.2 stayed registered twice at different versions.
            if !template_pkgver.is_empty() && entry.pkgver != template_pkgver {
                mismatches.push(SonameMismatch {
                    registered: registered.clone(),
                    installed: template_pkgver.to_string(),
                    kind: MismatchKind::StaleVersion,
                });
            }
            continue;
        }

        // Find the closest match by base name (e.g. libfoo.so.*)
        let base = soname_base(registered);
        let found = installed.iter().find(|s| soname_base(s) == base);

        // No counterpart at all is a different problem from a bump: nothing to
        // rewrite the line to, so the entry is stale and wants deleting.
        let (installed_str, kind) = match found {
            Some(s) => (s.clone(), MismatchKind::Bump),
            None => ("not found".to_string(), MismatchKind::Orphaned),
        };

        mismatches.push(SonameMismatch {
            registered: registered.clone(),
            installed: installed_str,
            kind,
        });
    }

    mismatches
}

/// One requested edit to common/shlibs.
#[derive(Debug, Clone)]
pub struct ShlibUpdate {
    /// Package that owns the line. Required: `common/shlibs` registers the same
    /// soname from several packages upstream (`libc.so`, `libjava.so`,
    /// `ld.so.1`), so matching on soname alone rewrites the wrong one.
    pub pkg_name: String,
    pub old_soname: String,
    pub new_soname: String,
    pub new_pkg_ver: String,
}

/// What `update_shlibs_file` actually did, so the caller can say so.
#[derive(Debug, Default, Clone)]
pub struct ShlibUpdateReport {
    pub rewritten: usize,
    /// Lines dropped because the rewrite produced a line that already existed.
    pub deduped: usize,
    /// Updates with no target line — reported rather than silently skipped.
    pub unmatched: Vec<String>,
}

/// Package name registered on a shlibs line: "libfoo.so.1 foo-1.2.3_1" -> "foo".
fn line_pkg_name(trimmed: &str) -> Option<&str> {
    let field = trimmed.split_whitespace().nth(1)?;
    field.rfind('-').map(|i| &field[..i])
}

/// Update common/shlibs with new SONAME entries.
///
/// Matches on soname **and** package name, and dedupes afterwards: rewriting
/// `libfoo.so.10 foo-0.11.0_1` to `libfoo.so.12 foo-0.13.1_1` can produce a
/// line that already exists further down. Both would then read as "provided",
/// so the badge clears and the duplicate becomes invisible and permanent.
pub fn update_shlibs_file(
    void_pkgs: &Path,
    updates: &[ShlibUpdate],
) -> Result<ShlibUpdateReport, std::io::Error> {
    let shlibs_path = void_pkgs.join("common/shlibs");
    let content = std::fs::read_to_string(&shlibs_path)?;

    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let mut report = ShlibUpdateReport::default();

    for up in updates {
        // "not found" is an orphaned entry: there is no new soname to write, and
        // the right fix is removing the line. Surfaced, not silently skipped.
        if up.new_soname == "not found" {
            report
                .unmatched
                .push(format!("{} ({}): needs manual removal", up.old_soname, up.pkg_name));
            continue;
        }

        let mut hit = false;
        for line in &mut lines {
            let trimmed = line.trim();
            if !trimmed.starts_with(&up.old_soname) {
                continue;
            }
            let after = &trimmed[up.old_soname.len()..];
            if !(after.starts_with(' ') || after.starts_with('\t')) {
                continue;
            }
            if line_pkg_name(trimmed) != Some(up.pkg_name.as_str()) {
                continue;
            }
            *line = format!("{} {}", up.new_soname, up.new_pkg_ver);
            hit = true;
            report.rewritten += 1;
        }
        if !hit {
            report
                .unmatched
                .push(format!("{} ({}): no matching line", up.old_soname, up.pkg_name));
        }
    }

    // Drop exact-duplicate entry lines, keeping the first. Comments and blanks
    // are left alone — only registration lines are meaningfully unique.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut deduped_lines: Vec<String> = Vec::with_capacity(lines.len());
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || seen.insert(trimmed) {
            deduped_lines.push(line.clone());
        } else {
            report.deduped += 1;
        }
    }

    let new_content = deduped_lines.join("\n");
    // Preserve trailing newline if original had one
    let final_content = if content.ends_with('\n') {
        format!("{}\n", new_content)
    } else {
        new_content
    };

    std::fs::write(&shlibs_path, final_content)?;
    Ok(report)
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

    /// A common/shlibs fixture in a temp void-packages tree.
    /// Same no-dependency idiom as `build.rs` tests — pid-tagged, caller cleans up.
    fn fixture(tag: &str, lines: &[&str]) -> std::path::PathBuf {
        let root = std::env::temp_dir()
            .join(format!("smith-shlibs-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("common")).unwrap();
        std::fs::write(root.join("common/shlibs"), format!("{}\n", lines.join("\n"))).unwrap();
        root
    }

    fn read(root: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(root.join("common/shlibs"))
            .unwrap()
            .lines()
            .map(|l| l.to_string())
            .collect()
    }

    fn update(pkg: &str, old: &str, new: &str, pkgver: &str) -> ShlibUpdate {
        ShlibUpdate {
            pkg_name: pkg.to_string(),
            old_soname: old.to_string(),
            new_soname: new.to_string(),
            new_pkg_ver: pkgver.to_string(),
        }
    }

    #[test]
    fn parse_keeps_the_registered_pkgver() {
        let root = fixture("pkgver", &["libfoo.so.1 foo-1.2.3_1"]);
        let map = parse_shlibs(&root);
        let entry = &map.get("foo").unwrap()[0];
        assert_eq!(entry.soname, "libfoo.so.1");
        assert_eq!(entry.pkgver, "foo-1.2.3_1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn same_soname_from_another_package_is_left_alone() {
        // libc.so, libjava.so and ld.so.1 all appear more than once upstream;
        // matching on soname alone rewrote whichever came first.
        let root = fixture("pkgmatch", &[
            "libjava.so openjdk8-8u1_1",
            "libjava.so openjdk17-17.0.1_1",
        ]);
        let report = update_shlibs_file(
            &root,
            &[update("openjdk17", "libjava.so", "libjava.so", "openjdk17-17.0.2_1")],
        )
        .unwrap();

        assert_eq!(report.rewritten, 1);
        assert_eq!(
            read(&root),
            vec!["libjava.so openjdk8-8u1_1", "libjava.so openjdk17-17.0.2_1"],
            "rewrote the wrong package's line"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rewrite_onto_an_existing_line_does_not_duplicate() {
        // The real case: hyprutils registered twice, the bump collapsing one
        // onto the other. Both then read as provided, so !so clears and the
        // duplicate becomes invisible and permanent.
        let root = fixture("dedupe", &[
            "libhyprutils.so.10 hyprutils-0.11.0_1",
            "libhyprutils.so.12 hyprutils-0.13.1_1",
        ]);
        let report = update_shlibs_file(
            &root,
            &[update("hyprutils", "libhyprutils.so.10", "libhyprutils.so.12", "hyprutils-0.13.1_1")],
        )
        .unwrap();

        assert_eq!(report.deduped, 1);
        assert_eq!(read(&root), vec!["libhyprutils.so.12 hyprutils-0.13.1_1"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn comments_and_blanks_survive_dedupe() {
        let root = fixture("comments", &["# header", "", "libfoo.so.1 foo-1_1", "", "# tail"]);
        update_shlibs_file(&root, &[]).unwrap();
        assert_eq!(read(&root), vec!["# header", "", "libfoo.so.1 foo-1_1", "", "# tail"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn orphaned_entry_is_reported_not_silently_skipped() {
        let root = fixture("orphan", &["libgone.so.1 foo-1_1"]);
        let report =
            update_shlibs_file(&root, &[update("foo", "libgone.so.1", "not found", "foo-2_1")])
                .unwrap();

        assert_eq!(report.rewritten, 0);
        assert_eq!(report.unmatched.len(), 1);
        assert!(report.unmatched[0].contains("needs manual removal"));
        assert_eq!(read(&root), vec!["libgone.so.1 foo-1_1"], "must not touch the line");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_update_with_no_target_line_is_reported() {
        let root = fixture("nomatch", &["libfoo.so.1 foo-1_1"]);
        let report =
            update_shlibs_file(&root, &[update("bar", "libbar.so.1", "libbar.so.2", "bar-2_1")])
                .unwrap();
        assert_eq!(report.rewritten, 0);
        assert_eq!(report.unmatched.len(), 1);
        assert!(report.unmatched[0].contains("no matching line"));
        let _ = std::fs::remove_dir_all(&root);
    }

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
