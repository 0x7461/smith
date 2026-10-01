# vxpm — History

Split from `PLAN.md` 2026-09-02 (see `agent-docs/PLAN.md` → the `HISTORY.md` tier).
**Grep this, don't read it whole.** Newest first; one dated bullet per entry.

---

- **2026-10-01 — `topological_sort` reports cycles instead of dropping them.**
  It returned the partial Kahn order, so a cycle's packages never appeared in the build order while
  unrelated packages came through normally — a silent omission of exactly the kind the template
  work keeps hitting. Now `Result<Vec<String>, UnresolvedDeps>`: the length check against `forward`
  is the trigger, and `UnresolvedDeps.packages` names the cycle members *and* everything downstream
  of them (a dependent of a cycle also never reaches in_degree 0, so it is equally unbuildable).
  `app.rs::build_all_buildable` refuses the run and prints the names in the status line rather than
  building a subset. The pinned `a_dependency_cycle_silently_drops_its_members` is replaced by
  `a_dependency_cycle_is_reported_not_dropped` and `a_cycle_also_reports_what_depends_on_it`.
  37 tests.

- **2026-09-23 — `dep_graph.rs` 0 → 13 tests, and two surprises.**
  116 lines, no I/O, and it decides build order — the reason the tool exists ("bumping hyprutils
  requires rebuilding its dependents") — with nothing covering it. A wrong order does not crash; it
  links against a stale library and shows up later. Covered: `-devel` stripping (without it every
  edge in the hypr stack vanishes), external deps excluded, self-edges excluded, all three dependency
  lists read, and dependency-before-dependent ordering asserted **relatively** — `topological_sort`
  iterates a HashMap, so within-tier order is unstable and asserting a sequence would flake.
  **Two findings.** `topological_sort` drops cycle members silently while unrelated packages come
  through (filed in Backlog, pinned as current behaviour). And `reverse_dep_tree`'s `visited` set
  prunes the sub*tree*, not the node: in a diamond the shared dependent is listed once per path, and
  only the first occurrence expands its children — so a repeat renders as a leaf and understates what
  a bump rebuilds. Both pinned rather than changed. Mutation-verified: removing the `-devel` strip
  turns 5 red, removing the self-edge guard turns 1. 36 tests total.

- **2026-09-23 — the four common/shlibs bugs, fixed together.**
  They shared a root cause: `update_shlibs_file` worked on sonames alone and threw away everything
  else the data carried. **(1)** It matched on soname only, so a bump to `libjava.so` could rewrite
  openjdk8's line instead of openjdk17's — `common/shlibs` registers the same soname from several
  packages upstream. The package name was already in `shlib_updates`; `app.rs` dropped it when
  building the update vec. Now a typed `ShlibUpdate` carries it and matching requires both.
  **(2)** Rewriting in place could produce a line that already existed two rows down (the real
  `libhyprutils.so.10 -> .so.12` case). Both then read as provided, so `!so` cleared and the
  duplicate became invisible and permanent. Dedupe pass after replacement, comments and blanks
  exempt. **(3)** `"not found"` entries were `continue`d silently, leaving the badge lit with no
  in-tool path to resolution — they are orphans whose fix is *deleting* the line, so they now come
  back in a `ShlibUpdateReport` and the status line names them. **(4)** `parse_shlibs` discarded the
  registered `pkgname-version_revision`, which is exactly what a soname comparison cannot check;
  keeping it lets `check_soname_mismatches` catch a stale *version* on a correct soname — the case
  that let `libhyprlang.so.2` sit registered twice. Mismatches are now typed
  (`Bump` / `Orphaned` / `StaleVersion`) and the detail view names the action instead of "MISMATCH".
  6 tests, each verified by mutation. 23 total.

- **2026-07-05 — v0.7.0 (released)** — SONAME overhaul (see Decisions): `!so` checks now read `shlib-provides` metadata (installed pkgdb *and* built .xbps in local repos; built wins) — no more readelf per library, startup ~0.8s for 17 pkgs; pkglint `SONAME bump detected` build failures are parsed (`shlibs::parse_soname_bump_errors`) and staged into the `S`-apply flow with a rebuild hint (motivating case: hyprutils `.so.10→.so.12` needed two manual edit-rebuild cycles). Post-build success also re-checks against the fresh .xbps instead of startup data. README `s`→`S` keybind typo fixed. 3 new parser tests (17 total).
- **2026-07-05** — GCC gate removed (see Decisions). `src/gcc.rs` deleted; badge, build-block and bulk-build skip logic stripped from `app.rs`/`ui.rs`/`main.rs`; `gcc_requirements.toml` trashed. 14 tests + clippy clean. Unreleased (next tag picks it up). SONAME-check-on-built-xbps improvement added to Backlog.
- **2026-06-27 — v0.6.1** — Fixed the #2 source-build predicate. v0.6.0 compared *version only* and flagged a dep only when the local tree was *ahead* of the repo — but xbps-src builds the **exact** pkgver pinned in the local tree, so a tree *behind* the repo (e.g. local curl 8.20.0 vs remote 8.21.0) also triggers a source build, which v0.6.0 missed (false negative — real zed build compiled curl/sqlite/libxkbcommon/etc.). New predicate `binary_available_exact`: a dep builds from source unless a binary with its exact `version_revision` exists in the remote repos **or** `hostdir/binpkgs` (direction-agnostic, revision-aware, counts already-built deps). Test `local_binpkg_exact_match`.
- **2026-06-27** — Build pre-flight warnings (`build.rs::preflight`), shown as a dismissible modal before `b`/`B` launch. (1) **Dirty masterdir**: leftover `masterdir-*/builddir/` entries from an interrupted build (the `cannot access wrksrc` failure mode) → offers `c` = clean & build (`./xbps-src clean` + `remove-autodeps`). (2) **Source-build deps**: build-deps whose local template is ahead of the available binary, or have no binary — the surprise when a binary-repack package (zed/ollama) drags in a full deps compile (repo lag). Both warnings: `b` = build anyway, `Esc`/`q` = dismiss & do nothing (jobs dropped). Runs in a background thread + `poll_preflight` (the #2 `xbps-query -R` calls are ~1.5s for one build, would freeze the UI on `B`). Tests: `empty_builddir_is_not_dirty`, `leftover_builddir_entries_flagged`. (The #2 detection here was reworked in v0.6.1 — see above.)
- **2026-06-01** — Decoupled "upstream ahead" from the build lifecycle. Removed the `UpstreamAhead` `Status` variant (it was top-priority, masked the real build state, and blocked builds); replaced by the orthogonal `PackageState::upstream_newer()` flag → `↑` badge + `latest` column. Build no longer gates on upstream; only the bump key (`t` / `bump --all`) does. Tests `compute_status_ignores_upstream` + `upstream_newer_is_independent_of_status`.
- **2026-06-01** — Verified the `>renamed.tar.gz` bump-checksum report was *not* a hashing bug — already fixed and in HEAD since 2026-05-17. `resolve_distfiles_url` uses the `>rename` RHS only as the cache filename (`9c35ea5`); the `by_sha256` hardlink trap is defeated by force-download (`cb6cecc`). Live zed 1.4.2 sha matches the template; test `rename_suffix_becomes_cache_filename` passes. (Root cause of the May-28 mismatch was the stale-cache trap, now guarded.)
- **2026-05-25** — Non-interactive CLI: `vxpm check-updates [--json]` and `vxpm bump <pkg>|--all`. Exit 0/1/2 (grep convention). Implementation in `src/cli.rs` (~150 lines), reuses `version_check` + `template` primitives directly — no `App` refactor. Unblocks [[maint-watch]] vxpm-bumper service.
- **2026-05-17 — v0.4.2** — `download_and_checksum` gained `force: bool`; bump path passes `true` to defeat the `by_sha256` hardlink trap (see Internals). Also: `--version`/`-V` and `--help`/`-h` flags (intercept before TTY init, so `vxpm --version` no longer ENXIO-fails on non-TTY shells). Clippy cleanups (3 warnings: manual_strip in config.rs, unnecessary_sort_by + double_ended_iterator_last in template.rs).
- **2026-05-17 — v0.4.1 (botched)** — Released, but the GitHub asset matched v0.4.0 byte-for-byte at vxpm's build-time fetch (real cause: vxpm's by_sha256 hardlink trap, fixed in v0.4.2). The actual GitHub binary at `v0.4.1` is correct; the locally-built/installed "v0.4.1" was v0.4.0 mislabeled. Detection: SHA of cached tarball equaled the prior release's SHA, link count 3.
- **2026-05-16** — Two bug fixes (worktree, unreleased): (a) cache/config paths migrated `vpm/` → `vxpm/` with idempotent first-run migration of `~/.config/vpm` and `~/.cache/vpm`; (b) bump-flow distfile caching honors xbps-src's `URL>rename` syntax — previously cached by URL tail, which is version-agnostic for renamed distfiles (e.g. zed's `zed-linux-x86_64.tar.gz`), so every bump after the first re-hashed the prior release's tarball and wrote a stale checksum. Real-world hit: zed 1.1.7 → 1.2.6 bump produced sha of an Apr-16 tarball. Fix uses `>rename` value as cache filename. Tests added.
- **2026-05-09** — Doc-system rollout: AGENTS.md (~190 lines), CLAUDE.md shim, PLAN.md trimmed. PUBLISHING.md folded into `## Operations / Publishing`; REVIEW_FINDINGS.md folded into the 2026-03-06 history entry. Both shadow-docs deleted (per global no-shadow-docs rule).
- **2026-04-16 — v0.3.0** — Cancellable ops + quit cleanup, download caching to `hostdir/sources/`, GH Actions Node-24 update (checkout v6, cache v5, action-gh-release v3). Ollama template bumped (cuda_v13 added, revision=2).
- **Phase 6** — Keybind rework + help panel.
- **Phase 5** — Search/filter, SONAME tracking, GCC gate, bulk ops, shlibs auto-update, build log persistence, config file. (5f install integration skipped — sudo.)
- **Phase 4** — Git sync/rebase/push from TUI; discovery switched from `git diff` to `git log`; scrollable package list (TableState).
- **Phase 3** — Template bumping (version + checksum + revision=1) and build orchestration with auto-rebuild queue.
- **Phase 2** — Interactive dashboard with dep tree and full status pipeline.
- **Phase 1** — Template parser, version sources, package discovery.
- **2026-03-06 → 03-08** — Opus code review (28 findings; commit `47b0da3`). Notable bugs fixed: `env!("HOME")` baking `$HOME` at compile time → `std::env::var`; stderr deadlock in build thread (>64 KB stderr could deadlock against stdout reader) → concurrent stdout+stderr reading like `git.rs:run_streaming`; lexicographic version comparison on `.xbps` files (`"9.0_1" > "10.0_1"`) → use existing `version_newer`; GitHub API rate limit silently swallowed (60 req/hr × ~48 req/cycle) → user-visible feedback; cache race condition in `version_check.rs` → app-layer `checking_versions` guard. Suggestions intentionally left: hardcoded `master...custom` branch names (intentional per git workflow); log-filename parser strips 16-char timestamp prefix (brittle if format changes, OK for now); cancelled build still emits `QueueComplete` (accepted — downstream filters the succeeded list correctly).
