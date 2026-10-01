use std::path::PathBuf;

pub struct Config {
    pub void_packages: PathBuf,
}

/// One-time migration from the legacy paths to `smith/`: `vxpm/` (renamed 2026-10-01) and the
/// older `vpm/`. Idempotent: only renames if the source exists and the destination does not, so
/// the newest legacy name wins when both exist.
pub fn migrate_legacy_paths() {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };
    for (old, new) in [
        (".config/vxpm", ".config/smith"),
        (".cache/vxpm", ".cache/smith"),
        (".config/vpm", ".config/smith"),
        (".cache/vpm", ".cache/smith"),
    ] {
        let old_path = PathBuf::from(&home).join(old);
        let new_path = PathBuf::from(&home).join(new);
        if old_path.exists() && !new_path.exists() {
            let _ = std::fs::rename(&old_path, &new_path);
        }
    }
}

pub fn load() -> Config {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let config_path = PathBuf::from(&home).join(".config/smith/config.toml");

    if !config_path.exists() {
        bootstrap(&config_path, &home);
    }

    let void_packages = match std::fs::read_to_string(&config_path) {
        Ok(content) => {
            let table: toml::Table = content.parse().unwrap_or_default();
            table
                .get("void_packages")
                .and_then(|v| v.as_str())
                .map(|s| expand_tilde(s, &home))
                .unwrap_or_else(|| PathBuf::from(&home).join("void-packages"))
        }
        Err(_) => PathBuf::from(&home).join("void-packages"),
    };

    Config { void_packages }
}

fn expand_tilde(path: &str, home: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        PathBuf::from(home).join(stripped)
    } else if path == "~" {
        PathBuf::from(home)
    } else {
        PathBuf::from(path)
    }
}

fn bootstrap(path: &PathBuf, home: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let default = format!(
        r#"# VPM configuration
void_packages = "{}/void-packages"
"#,
        home
    );

    let _ = std::fs::write(path, default);
}
