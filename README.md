# smith

A terminal UI for managing custom [void-packages](https://github.com/void-linux/void-packages) templates.

Built for the workflow of maintaining self-built packages on Void Linux — track versions, check upstream updates, rebuild dependencies in order, and manage the git workflow, all from a single TUI.

> Developed with [Claude Code](https://claude.ai/claude-code).

## Features

- Dashboard of all custom packages with live status (`UP TO DATE`, `UPSTREAM AHEAD`, `BUILD OUTDATED`, `READY TO INSTALL`)
- Upstream version checking against GitHub releases/tags
- One-key template bump + build
- Dependency-aware build ordering
- SONAME mismatch detection and `common/shlibs` auto-update — checks the freshly built `.xbps` (not just the installed package), and a pkglint `SONAME bump detected` build failure stages its own one-key fix
- Build log viewer with scrolling
- Git integration (ahead/behind count, commit workflow)
- Uncommitted template detection

## Keybinds

| Key | Action |
|-----|--------|
| `j`/`k` | Navigate |
| `/` | Search/filter |
| `u` | Check upstream versions (selected) |
| `U` | Check upstream versions (all) |
| `t` | Bump template to latest (selected) |
| `T` | Bump all upstream-ahead templates |
| `b` | Build selected |
| `B` | Build all outdated |
| `g` | Git status |
| `S` | Apply SONAME shlib updates |
| `Enter` | Detail panel |
| `?` | Help |
| `q` | Quit |

Before a build, smith runs pre-flight checks and shows a warning modal if the masterdir has leftover build state or dependencies would compile from source. In the modal: `c` = clean & build, `b` = build anyway, `Esc`/`q` = dismiss (do nothing).

## Non-interactive CLI

For cron/runit automation:

```sh
smith check-updates           # list "<name> <cur> -> <latest>" lines
smith check-updates --json    # same, as JSON
smith bump <pkg>              # bump one template + checksum (no build)
smith bump --all              # bump every pkg whose upstream version is newer than the template
```

Exit codes (grep convention): `0` = no updates / success, `1` = updates available / partial failure, `2` = GitHub rate-limited.

## Install

Download the binary from [releases](https://github.com/0x7461/smith/releases) or build from source:

```sh
cargo build --release
# Binary at target/release/smith
```

## Requirements

- Void Linux with [void-packages](https://github.com/void-linux/void-packages) checked out
- `xbps-query`, `xbps-src` in PATH
- Config: `~/.config/smith/config.toml` (auto-created on first run)

## License

MIT
