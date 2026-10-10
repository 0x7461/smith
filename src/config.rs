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
            let table: toml::Table = match content.parse() {
                Ok(t) => t,
                Err(e) => {
                    // Fall back, but say so: a silent default sent the wrong path
                    // to every subcommand when the file was malformed.
                    eprintln!(
                        "Warning: {} is not valid TOML ({}); using ~/void-packages",
                        config_path.display(),
                        e
                    );
                    toml::Table::new()
                }
            };
            table
                .get("void_packages")
                .and_then(|v| v.as_str())
                .map(|s| expand_tilde(s, &home))
                .unwrap_or_else(|| PathBuf::from(&home).join("void-packages"))
        }
        Err(e) => {
            eprintln!(
                "Warning: cannot read {} ({}); using ~/void-packages",
                config_path.display(),
                e
            );
            PathBuf::from(&home).join("void-packages")
        }
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
        r#"# smith configuration
void_packages = "{}/void-packages"
"#,
        home
    );

    let _ = std::fs::write(path, default);
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_document_into_table() {
        // toml 1.x changed `FromStr for Value` to parse a value, not a document;
        // `Table` must still parse a whole document, which `load()` relies on.
        let table: toml::Table =
            "# smith configuration\nvoid_packages = \"/home/u/void-packages\"\n"
                .parse()
                .unwrap();
        assert_eq!(
            table.get("void_packages").and_then(|v| v.as_str()),
            Some("/home/u/void-packages")
        );
    }
}
