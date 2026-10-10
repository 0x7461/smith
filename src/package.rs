use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::shlibs::{ShlibEntry, SonameMismatch};

#[derive(Debug, Clone, Serialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub revision: u32,
    pub short_desc: String,
    pub homepage: String,
    pub build_style: String,
    pub makedepends: Vec<String>,
    pub hostmakedepends: Vec<String>,
    pub depends: Vec<String>,
    pub distfiles: String,
    pub changelog: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageState {
    pub package: Package,
    pub installed: Option<String>,
    pub built: Option<String>,
    pub latest: Option<String>,
    pub status: Status,
    pub uncommitted: bool,
    #[serde(skip)]
    pub shlibs: Vec<ShlibEntry>,
    #[serde(skip)]
    pub soname_mismatches: Vec<SonameMismatch>,
    #[serde(skip)]
    pub build_log: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum Status {
    UpToDate,
    BuildOutdated,
    ReadyToInstall,
    BuildFailed,
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::UpToDate => "UP TO DATE",
            Status::BuildOutdated => "BUILD OUTDATED",
            Status::ReadyToInstall => "READY TO INSTALL",
            Status::BuildFailed => "BUILD FAILED",
        }
    }


}

impl PackageState {
    pub fn compute_status(
        package: &Package,
        installed: &Option<String>,
        built: &Option<String>,
    ) -> Status {
        let template_ver = format!("{}_{}",  package.version, package.revision);

        // Note: an available upstream release (latest > template) is deliberately
        // NOT a status here — it's orthogonal to the local build→install lifecycle
        // and would mask a genuinely pending build. It's surfaced separately via
        // `upstream_newer()` (badge + the `latest` column) and gates only the bump key.

        // Template → build gap: nothing built yet
        if built.is_none() && installed.is_none() {
            return Status::BuildOutdated;
        }

        // Build → system gap: built but never installed
        if installed.is_none() {
            return Status::ReadyToInstall;
        }

        let inst_ver = installed.as_ref().unwrap();

        if let Some(built_ver) = built {
            // Template → build gap: built version is stale
            if *built_ver != template_ver {
                return Status::BuildOutdated;
            }
            // Build → system gap: newer build waiting to be installed
            let inst_short = installed_to_ver_rev(inst_ver);
            if *built_ver != inst_short {
                return Status::ReadyToInstall;
            }
        } else {
            // No .xbps but package is installed — template may have moved ahead
            let inst_short = installed_to_ver_rev(inst_ver);
            if inst_short != template_ver {
                return Status::BuildOutdated;
            }
        }

        Status::UpToDate
    }

    /// One-line action hint shown in the detail panel.
    pub fn action_hint(&self) -> &'static str {
        // An upstream update is independent of the build lifecycle, so prompt for
        // a bump first when one is available — whatever the local build state.
        if self.upstream_newer() {
            return "Press t to bump the template to the upstream version (or b to build the current one).";
        }
        match &self.status {
            Status::UpToDate => "Nothing to do.",
            Status::BuildOutdated => "Press b to build the package.",
            Status::ReadyToInstall => "Run xi <pkg> to install the built package.",
            Status::BuildFailed => "Check the build log. Press b to retry.",
        }
    }

    /// Whether upstream has a release newer than the template version. Independent
    /// of `status` (the build→install lifecycle): a package can both need a build
    /// and have an upstream update available.
    pub fn upstream_newer(&self) -> bool {
        self.latest
            .as_ref()
            .is_some_and(|l| version_newer(l, &self.package.version))
    }
}

/// Public wrapper for version comparison (used by ui.rs for color coding).
pub fn version_newer_pub(a: &str, b: &str) -> bool {
    version_newer(a, b)
}

/// Split a void `pkgver` into (version, revision). The revision is the number
/// after the last '_' ("0.6.8_2" -> ("0.6.8", 2)); a bare version has revision 0.
fn split_rev(v: &str) -> (&str, u64) {
    match v.rsplit_once('_') {
        Some((base, rev)) if !rev.is_empty() && rev.chars().all(|c| c.is_ascii_digit()) => {
            (base, rev.parse().unwrap_or(0))
        }
        _ => (v, 0),
    }
}

/// Compare two version strings. Returns true if `a` is newer than `b`.
/// Splits on '.', compares numeric parts left to right. Parts with non-numeric
/// suffixes (e.g. "0beta1") are treated as prereleases, older than the same
/// number without a suffix ("0"). So "0.45.0" > "0.45.0beta1".
/// When base versions tie, the void revision (`_2`) is compared numerically:
/// treating it as a prerelease suffix made "0.6.8_2" and "0.6.8_1" compare
/// equal, so clean_old_packages could delete the newer revision.
fn version_newer(a: &str, b: &str) -> bool {
    let (base_a, rev_a) = split_rev(a);
    let (base_b, rev_b) = split_rev(b);

    // Returns (numeric_prefix, has_prerelease_suffix)
    let parse_part = |p: &str| -> (u64, bool) {
        let num_end = p.find(|c: char| !c.is_ascii_digit()).unwrap_or(p.len());
        let num: u64 = p[..num_end].parse().unwrap_or(0);
        (num, num_end < p.len())
    };
    let parts_a: Vec<&str> = base_a.split('.').collect();
    let parts_b: Vec<&str> = base_b.split('.').collect();
    for i in 0..parts_a.len().max(parts_b.len()) {
        let (na, pre_a) = parse_part(parts_a.get(i).copied().unwrap_or("0"));
        let (nb, pre_b) = parse_part(parts_b.get(i).copied().unwrap_or("0"));
        if na > nb {
            return true;
        }
        if na < nb {
            return false;
        }
        // Same numeric value: prerelease < release
        match (pre_a, pre_b) {
            (true, false) => return false, // a is prerelease, b is release
            (false, true) => return true,  // a is release, b is prerelease
            _ => {}
        }
    }
    rev_a > rev_b
}

/// Extract "version_revision" from xbps-query pkgver like "hyprutils-0.11.0_1"
fn installed_to_ver_rev(pkgver: &str) -> String {
    // Format: name-version_revision
    // Find the last '-' which separates name from version
    if let Some(idx) = pkgver.rfind('-') {
        pkgver[idx + 1..].to_string()
    } else {
        pkgver.to_string()
    }
}

/// Parse a void-packages template file into a Package.
pub fn parse_template(path: &Path) -> Result<Package> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading template: {}", path.display()))?;

    let mut vars: HashMap<String, String> = HashMap::new();
    let mut in_multiline: Option<String> = None;
    let mut multiline_buf = String::new();
    // Assignments inside a function body apply to that function's build, not the
    // main package: a `short_desc+=` in `foo-terminfo_package()` must not be
    // appended to the package-wide short_desc (ghostty).
    let mut in_function = false;

    for line in content.lines() {
        // If we're accumulating a multiline value
        if let Some(ref varname) = in_multiline {
            if line.contains('"') {
                // Closing quote found
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

        // Track function scope: `name() {` opens, a lone `}` closes.
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

        // Skip comments, empty lines, functions, conditionals
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("if ")
            || trimmed.starts_with("fi")
            || trimmed.starts_with("then")
            || trimmed.starts_with("else")
            || trimmed.starts_with("vmove")
            || trimmed.starts_with("vlicense")
            || trimmed.starts_with("vinstall")
            || trimmed.starts_with("vbin")
            || trimmed.starts_with("vman")
            || trimmed.starts_with("vsed")
            || trimmed.starts_with("vcompletion")
            || trimmed.starts_with("vcopy")
            || trimmed.starts_with("ln ")
            || trimmed.starts_with("sed ")
            || trimmed.starts_with("chmod")
            || trimmed.starts_with("mkdir")
            || trimmed.starts_with("cat ")
            || trimmed.starts_with("install ")
            || trimmed.starts_with("rm ")
            || trimmed.starts_with("local ")
            || trimmed.starts_with("pkg_install")
            || (trimmed.starts_with("depends=") && trimmed.contains("sourcepkg"))
        {
            continue;
        }

        // Handle append: var+=" value"
        if let Some(idx) = trimmed.find("+=") {
            let varname = trimmed[..idx].trim();
            let rest = trimmed[idx + 2..].trim();
            if rest.starts_with('"') && !rest[1..].contains('"') {
                // Multiline append: opening quote but no closing
                let existing_val = vars.get(varname).cloned().unwrap_or_default();
                let initial = rest[1..].trim(); // strip opening quote
                multiline_buf = if existing_val.is_empty() {
                    initial.to_string()
                } else {
                    format!("{} {}", existing_val, initial)
                };
                in_multiline = Some(varname.to_string());
            } else {
                let val = unquote(rest);
                let existing = vars.entry(varname.to_string()).or_default();
                if !existing.is_empty() {
                    existing.push(' ');
                }
                existing.push_str(&val);
            }
            continue;
        }

        // Handle assignment: var=value or var="value" or var=$othervar
        if let Some(idx) = trimmed.find('=') {
            // Make sure this isn't inside a function or conditional
            let varname = trimmed[..idx].trim();
            if !varname.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let rest = trimmed[idx + 1..].trim();

            // Variable reference: var=$other
            if rest.starts_with('$') && !rest.contains('{') {
                let ref_name = rest.trim_start_matches('$');
                if let Some(ref_val) = vars.get(ref_name) {
                    vars.insert(varname.to_string(), ref_val.clone());
                }
                continue;
            }

            // Multiline: starts with " but no closing "
            let unq = rest.trim_matches('"');
            if rest.starts_with('"') && rest.len() > 1 && !rest[1..].contains('"') {
                in_multiline = Some(varname.to_string());
                multiline_buf = unq.trim().to_string();
                continue;
            }

            // Check for opening quote only (like just `"`)
            if rest == "\"" {
                in_multiline = Some(varname.to_string());
                multiline_buf.clear();
                continue;
            }

            vars.insert(varname.to_string(), unquote(rest));
        }
    }

    let version = vars.get("version").cloned().unwrap_or_default();

    // Resolve ${version} in distfiles
    let distfiles = vars
        .get("distfiles")
        .cloned()
        .unwrap_or_default()
        .replace("${version}", &version);

    Ok(Package {
        name: vars.get("pkgname").cloned().unwrap_or_default(),
        version,
        revision: vars
            .get("revision")
            .and_then(|r| r.parse().ok())
            .unwrap_or(1),
        short_desc: vars.get("short_desc").cloned().unwrap_or_default(),
        homepage: vars.get("homepage").cloned().unwrap_or_default(),
        build_style: vars.get("build_style").cloned().unwrap_or_default(),
        makedepends: split_deps(vars.get("makedepends").map(|s| s.as_str()).unwrap_or("")),
        hostmakedepends: split_deps(
            vars.get("hostmakedepends").map(|s| s.as_str()).unwrap_or(""),
        ),
        depends: split_deps(vars.get("depends").map(|s| s.as_str()).unwrap_or("")),
        distfiles,
        changelog: vars.get("changelog").cloned().unwrap_or_default(),
    })
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn split_deps(s: &str) -> Vec<String> {
    s.split_whitespace()
        .filter(|d| !d.is_empty())
        .map(|d| d.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_newer() {
        assert!(version_newer("0.12.0", "0.11.0"));
        assert!(version_newer("1.0.0", "0.99.99"));
        assert!(!version_newer("0.11.0", "0.11.0"));
        assert!(!version_newer("0.10.0", "0.11.0"));
        // Prerelease suffixes: release > prerelease of same number
        assert!(version_newer("0.45.0", "0.45.0beta1"));
        assert!(!version_newer("0.45.0beta1", "0.45.0"));
        assert!(!version_newer("0.45.0beta1", "0.45.0beta1"));
        // Cross-digit boundary (was broken with lexicographic comparison)
        assert!(version_newer("10.0.0", "9.0.0"));
        // Void revision suffix: a higher revision of the same version is newer
        assert!(version_newer("0.6.8_2", "0.6.8_1"));
        assert!(!version_newer("0.6.8_1", "0.6.8_2"));
        assert!(!version_newer("0.6.8_1", "0.6.8_1"));
        assert!(version_newer("0.6.10_1", "0.6.8_9"));
    }

    #[test]
    fn test_installed_to_ver_rev() {
        assert_eq!(installed_to_ver_rev("hyprutils-0.11.0_1"), "0.11.0_1");
        assert_eq!(
            installed_to_ver_rev("google-chrome-133.0.6943.53_1"),
            "133.0.6943.53_1"
        );
    }

    fn pkg(version: &str, revision: u32) -> Package {
        Package {
            name: "zig".into(),
            version: version.into(),
            revision,
            short_desc: String::new(),
            homepage: String::new(),
            build_style: String::new(),
            makedepends: vec![],
            hostmakedepends: vec![],
            depends: vec![],
            distfiles: String::new(),
            changelog: String::new(),
        }
    }

    fn state(package: Package, latest: Option<&str>) -> PackageState {
        let status = PackageState::compute_status(&package, &None, &None);
        PackageState {
            package,
            installed: None,
            built: None,
            latest: latest.map(String::from),
            status,
            uncommitted: false,
            shlibs: vec![],
            soname_mismatches: vec![],
            build_log: None,
        }
    }

    // compute_status reflects only the local build→install lifecycle; an available
    // upstream release must NOT mask a genuinely pending build.
    #[test]
    fn compute_status_ignores_upstream() {
        // Pinned template at 0.15.2, an older build present → still BUILD OUTDATED,
        // even though upstream (tracked separately) may be ahead.
        let p = pkg("0.15.2", 1);
        let s = PackageState::compute_status(&p, &Some("zig-0.15.1_1".into()), &Some("0.15.1_1".into()));
        assert_eq!(s, Status::BuildOutdated);

        // Built and installed match the template → UP TO DATE.
        let s = PackageState::compute_status(&p, &Some("zig-0.15.2_1".into()), &Some("0.15.2_1".into()));
        assert_eq!(s, Status::UpToDate);
    }

    // upstream_newer is an independent axis from status: a package can both need a
    // build and have an upstream update (the case the old single-status enum masked).
    #[test]
    fn upstream_newer_is_independent_of_status() {
        let s = state(pkg("0.15.2", 1), Some("0.16.0"));
        assert!(s.upstream_newer());
        assert_eq!(s.status, Status::BuildOutdated); // pinned, nothing built yet

        // No upstream info known, or upstream not newer → false.
        assert!(!state(pkg("0.15.2", 1), None).upstream_newer());
        assert!(!state(pkg("0.15.2", 1), Some("0.15.2")).upstream_newer());
        assert!(!state(pkg("0.15.2", 1), Some("0.15.1")).upstream_newer());
    }
}
