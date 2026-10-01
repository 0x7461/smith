mod app;
mod build;
mod cli;
mod config;
mod dep_graph;
mod git;
mod package;
mod repo;
mod shlibs;
mod template;
mod ui;
mod version_check;

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if let Some(arg) = args.get(1).map(String::as_str) {
        match arg {
            "--version" | "-V" => {
                println!("smith {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            _ => {}
        }
    }

    config::migrate_legacy_paths();

    if let Some(sub) = args.get(1).map(String::as_str) {
        match sub {
            "dump" => return dump(&config::load()),
            "check-updates" | "bump" => {
                let code = cli::run(&args[1..])?;
                std::process::exit(code);
            }
            _ => {}
        }
    }

    run_tui(config::load())
}

fn print_help() {
    println!("smith {} — TUI for managing custom void-packages templates", env!("CARGO_PKG_VERSION"));
    println!();
    println!("USAGE:");
    println!("    smith                          Launch the interactive TUI");
    println!("    smith dump                     Print package state as JSON and exit");
    println!("    smith check-updates [--json]   List packages with upstream updates");
    println!("    smith bump <pkg>               Bump template + checksum for one package");
    println!("    smith bump --all               Bump every package with an upstream update");
    println!("    smith --version|-V             Print version and exit");
    println!("    smith --help|-h                Print this help and exit");
    println!();
    println!("In-TUI keybinds: press ? after launch, or see README.md.");
}

fn dump(cfg: &config::Config) -> Result<()> {
    let committed = repo::discover_custom_packages(&cfg.void_packages)?;
    let committed_set: std::collections::HashSet<String> = committed.iter().cloned().collect();
    let uncommitted = repo::discover_uncommitted_packages(&cfg.void_packages, &committed_set);
    let mut names = committed;
    names.extend(uncommitted.iter().cloned());
    names.sort();
    let uncommitted_set: std::collections::HashSet<String> = uncommitted.into_iter().collect();
    let packages = repo::load_packages(&cfg.void_packages, &names);
    let states = repo::build_package_states(&cfg.void_packages, packages, &uncommitted_set);

    let json = serde_json::to_string_pretty(&states)?;
    println!("{}", json);
    Ok(())
}

fn run_tui(cfg: config::Config) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = app::App::new(cfg.void_packages)?;

    loop {
        app.poll_build();
        app.poll_preflight();
        app.poll_version_check();
        app.poll_template_bump();
        app.poll_git();

        terminal.draw(|f| ui::draw(f, &mut app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                // Ctrl+C always quits
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
                {
                    app.request_quit();
                    if !app.should_quit {
                        // quit_confirm was set — keep running until user answers
                    } else {
                        break;
                    }
                }

                if app.filter_active {
                    match key.code {
                        KeyCode::Esc => app.stop_filter(true),
                        KeyCode::Enter => app.stop_filter(false),
                        KeyCode::Backspace => app.filter_backspace(),
                        KeyCode::Char(c) => app.filter_input(c),
                        _ => {}
                    }
                } else if app.build_preflight.is_some() {
                    // Build pre-flight modal: clean & build / build anyway / dismiss.
                    match key.code {
                        KeyCode::Char('c') if app.preflight_cleanable() => app.preflight_clean_and_build(),
                        KeyCode::Char('b') => app.preflight_proceed(),
                        KeyCode::Esc | KeyCode::Char('q') => app.preflight_dismiss(),
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Char('q') => app.request_quit(),
                        KeyCode::Char('y') if app.cancel_confirm.is_some() => app.confirm_cancel(),
                        KeyCode::Char('y') if app.quit_confirm => app.confirm_quit(),
                        KeyCode::Char('/') => app.start_filter(),
                        KeyCode::Char('j') | KeyCode::Down => {
                            if app.panel == app::PanelMode::BuildLog {
                                app.scroll_log_down();
                            } else if app.panel == app::PanelMode::BumpLog {
                                app.bump_log_scroll = app.bump_log_scroll.saturating_sub(1);
                            } else {
                                app.move_down();
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if app.panel == app::PanelMode::BuildLog {
                                app.scroll_log_up();
                            } else if app.panel == app::PanelMode::BumpLog {
                                app.bump_log_scroll += 1;
                            } else {
                                app.move_up();
                            }
                        }
                        KeyCode::Enter => app.toggle_detail(),
                        KeyCode::Tab => app.toggle_tree(),
                        // Upstream check: u = selected, U = all
                        KeyCode::Char('u') => app.check_version_selected(),
                        KeyCode::Char('U') => app.check_versions(),
                        // Template bump (no build): t = selected, T = all
                        KeyCode::Char('t') => app.bump_template_selected(),
                        KeyCode::Char('T') => app.bump_template_all(),
                        // Build (best-effort): b = selected, B = all buildable
                        KeyCode::Char('b') => app.build_selected(),
                        KeyCode::Char('B') => app.build_all_buildable(),
                        // Clean built packages: c = keep latest, C = remove all
                        KeyCode::Char('c') => app.clean_old_packages(),
                        KeyCode::Char('C') => app.clean_all_packages(),
                        // Other
                        KeyCode::Char('r') => app.refresh(),
                        KeyCode::Char('S') if !app.shlib_updates.is_empty() => {
                            app.apply_shlib_updates();
                        }
                        KeyCode::Char('g') => app.open_git_menu(),
                        KeyCode::Char('?') => {
                            app.panel = if app.panel == app::PanelMode::Help {
                                app::PanelMode::None
                            } else {
                                app::PanelMode::Help
                            };
                        }
                        KeyCode::Char('1') if app.panel == app::PanelMode::GitMenu && !app.git_op_active => {
                            app.git_sync_master();
                        }
                        KeyCode::Char('2') if app.panel == app::PanelMode::GitMenu && !app.git_op_active => {
                            app.git_rebase_custom();
                        }
                        KeyCode::Char('3') if app.panel == app::PanelMode::GitMenu && !app.git_op_active => {
                            app.git_push_custom();
                        }
                        KeyCode::Esc => {
                            if app.cancel_confirm.is_some() {
                                app.deny_cancel();
                            } else if app.quit_confirm {
                                app.deny_quit();
                            } else if app.any_op_active() {
                                app.request_cancel();
                            } else if !app.filter.is_empty() {
                                app.filter.clear();
                                app.selected = 0;
                            } else if app.panel != app::PanelMode::None {
                                app.panel = app::PanelMode::None;
                            } else if app.view == app::View::Tree {
                                app.view = app::View::List;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}
