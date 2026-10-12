# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A single always-on daemon that feeds **Asana** or **gleb_tasks** (tasks.gleb.at, GraphQL) tasks to **Claude Code** (`claude -p`) sessions across any number of local projects. It ships the binaries `autopilot` (the daemon + registry/control CLI) and `asana` / `tracker` (the same token-cheap tracker CLI under two names, used *inside* the sessions the daemon spawns). The tracker is selected per project by `tracker:` in `autopilot.yaml` (default `asana`). See `README.md` for the full user-facing feature tour; this file is the architecture map.

Note: source comments and most user-facing strings are in **Russian**. Match that language when editing existing modules.

## Build / run

```sh
cargo build --release          # both binaries -> target/release/{autopilot,asana}
cargo install --path .         # installs both into ~/.cargo/bin
cargo clippy                    # lint
cargo run --bin autopilot -- status    # run a subcommand without installing
cargo run --bin asana -- task <gid>
```

Unit tests are few (`cargo test`: model/effort selection in `runner.rs`). To exercise the daemon loop without backgrounding it, use `autopilot run` (runs the daemon body in the current terminal). `autopilot __daemon` is the hidden subcommand the backgrounded process actually execs — don't call it directly.

## The two-layer state model (read this first)

State lives in two distinct places, and confusing them is the most common source of bugs:

- **Global, one per machine** — `~/.autopilot/` (override with `AUTOPILOT_HOME` env, used for isolation): `projects.yaml` (the registry), `config.yaml` (daemon timings, role defaults, telegram), plus daemon runtime files `daemon.pid` / `daemon.log` / `STOP` / `status.json` / `telegram.offset`. All modeled in `src/registry.rs`.
- **Per-project** — `<project>/autopilot/`: `autopilot.yaml` (tracker + identity: Asana workspace gid / gleb_tasks workspace key, operator/agent roles, section-name→role mapping), `prompt.md` (the session prompt), `logs/session-*.log`, `state/current-task.md`. The identity config is modeled by `Settings` in `src/lib.rs`; for gleb_tasks `Settings::load_from` makes one GraphQL request to turn the workspace key and role usernames into ids.

`Settings::find_dir()` locates the per-project config by walking **up** from the current directory (or `AUTOPILOT_DIR` env). This is why one globally-installed `asana` binary serves every project — the daemon sets `AUTOPILOT_DIR` when it spawns a session.

## Module map (`src/`)

- **`lib.rs`** (crate `autopilot_core`) — `Settings` (config parser, `Tracker` enum), the backend-neutral models (`Task` with `gid` + optional `key`/`display_id()`, `Story`, `Attachment`, `Placement`, …) and `Client`, which owns a `Backend` (`asana::AsanaApi` | `gleb_tasks::GlebTasksApi`) and delegates raw operations to it while keeping the process logic shared: `build_task_context()` assembles the full markdown context handed to a session; `placement()`/`approved_placement()` resolve which project+section a task "belongs" to; `resolve_section_gid()` maps a role (e.g. `in_progress`) to a section id **by matching section names per board** (cached in `section_cache`); `extract_linked_ids()` finds referenced tasks (Asana URLs, or `KEY-12` keys for gleb_tasks).
- **`asana.rs`** — the raw Asana REST client (`AsanaApi::request()`, 3 retries on 429/5xx) mapping straight into the shared models via serde.
- **`gleb_tasks.rs`** — the raw gleb_tasks GraphQL client (`GlebTasksApi`): `Authorization: Bearer <workspace api key>`, errors surfaced as `CODE: message` from `extensions.code`. Task ids accept uuid or key (`resolve_task_id`); comments + activity are merged into `Story` (activity rows humanized by `activity_text`); attachments are addressed as `<task>/<attachment id>` because the schema has no `attachment(id)` query.
- **`daemon.rs`** — the infinite loop. Each iteration **re-reads** the registry and global config from disk (so `add/pause/priority` from CLI *or* Telegram take effect with no restart). Iterates active projects by **descending priority**; the first project with a runnable task gets one session, then the scan restarts from the top. Handles the model-fallback state machine on `Outcome::Limit`. Polls every `daemon.no_task_sleep` seconds (default **60** — that is the max pickup delay for a new task); to keep the frequent poll quiet it caches each project's parsed `Settings` in `SettingsCache` keyed by `autopilot.yaml` mtime (the gleb_tasks parse is a network resolve), logs «задач нет» only on the transition into idle, and logs a repeating project-config error once until it changes.
- **`runner.rs`** — `select_task()` (task selection + side effects) and `run_session()` (spawns `claude`). This is where the business rules live (see below).
- **`registry.rs`** — `Registry`/`ProjectEntry`, `GlobalConfig` (+ all serde defaults), `DaemonStatus`, PID/pgid helpers. `write_atomic()` (tmp-file + rename) is used for all state writes.
- **`initproj.rs`** — backs `autopilot add`: templates out `<project>/autopilot/`, writes a note into the project's **Claude Code auto-memory** (`~/.claude/projects/<munged-path>/memory/`), and optionally runs a one-shot `claude` (sonnet) session to fill `prompt.md` with the repo's real layout.
- **`telegram.rs`** — long-polling bot running in a daemon thread; registry mutations go through the on-disk registry (so CLI and bot stay consistent). Also exposes the daemon's **Claude account**: `/account` and `/usage` shell out via `claudecli`, and `/login` runs an interactive OAuth flow — it holds the spawned `claude auth login` child in `Bot.pending_login` (a `Mutex<Option<PendingLogin>>`) across poll iterations, sends the URL, then feeds the user's reply as the code to the child's stdin. A confirmation button gates login; state expires after `LOGIN_TTL`.
- **`claudecli.rs`** — thin wrappers over the `claude` CLI used by the bot: `auth_status()` (`claude auth status --json`), `usage()` (`claude -p "/usage" --output-format json`, a free/local call — parses percentages out of the `result` text), and `login_start()`/`login_finish()` (the URL-then-code flow; stdin is closed after the code to force EOF so a bad code can't hang the read).
- **`bin/autopilot.rs`** / **`bin/asana.rs`** — arg parsing + command dispatch only; logic lives in the library. **`bin/tracker.rs`** is `include!("asana.rs")`: the same CLI under a tracker-neutral name (so `asana.rs` must not contain inner `//!` doc comments).
- **`examples/select_once.rs`** — runs `runner::select_task` once for the config in `AUTOPILOT_DIR` without spawning claude; the way to check a new project's config live.

## Task-selection rules (`runner::select_task`)

Selection is **workspace-wide and assignment-driven**: `get_assigned_tasks()` fetches every incomplete task assigned to the agent's account across the whole workspace (Asana: the agent's Asana user; gleb_tasks: the virtual "Claude Code" member, which is also who the API key acts as) — assignment *is* the filter for "which projects the agent owns". Sections are then resolved **by name** against the board each task lives in. The rules, in order:

- Subtasks and tasks with no project membership are skipped silently.
- Task in an **approved** section → candidate. Intra-project priority: `in_progress` → `todo` with due date ≤ today → `reopen` → other `todo`.
- Task in a known but **non-approved** section (qa/blocked/…, except `done`) → §2.1: comment + reassign to operator.
- Task whose section maps to **no** role → "sections don't match the process" → comment + reassign to operator.
- **Manual-action heuristic**: if the description contains any `heuristics.manual_keywords`, the task is auto-moved to `blocked` and reassigned to the operator — *unless* `was_manually_cleared()` finds operator activity after the bot's own block comment (tracked via the language-specific `manual_marker()` substring).

There is no local blocked-list: the Asana `blocked` section (plus reassignment to the operator) is the single source of truth for parked tasks.

## Running a session (`runner::run_session`)

1. Build full task context → write `state/current-task.md`.
2. Pick model/effort via `extract_model_and_effort`: the task's custom fields first (`Task.custom_fields`, names from `fields:` in `autopilot.yaml`, default «Model»/«Effort», gleb_tasks only — `asana fields` creates them as SELECT fields scoped to the agent), then `model:`/`effort:` text directives (Russian or English), then defaults (`fable`, no effort flag); `model_override` from the daemon's fallback state wins over all.
3. Move task to `in_progress`, then spawn `claude -p <prompt> --model <m> --dangerously-skip-permissions --fallback-model <fb> [--effort <level>]` in the project dir with `AUTOPILOT_DIR` set.
4. Watchdog kills the child at `daemon.session_timeout`. On non-zero exit, the log tail is regex-matched for usage-limit phrases → `Outcome::Limit` (triggers model fallback, then `limit_sleep`); otherwise `Outcome::Error`.

The session itself does the real work by calling the `asana` CLI (comment / move / qa / done / block / subtask / download) — that binary is the session's only intended tracker surface, deliberately used instead of the trackers' MCP servers to save tokens. It serves both trackers: ids are Asana gids or gleb_tasks keys.

## Process-group semantics

`autopilot start` spawns the daemon in its **own process group** (`process_group(0)`) so it survives terminal close. `autopilot stop` writes the `STOP` file (checked between loop steps for a graceful exit) *and* signals the whole group with TERM→KILL — deliberately killing any in-flight `claude` session (the task just stays `in_progress` and is re-picked next run). The daemon also spawns `caffeinate` to keep the Mac awake while alive.

## Conventions

- Never put a token in a config file — only the **env-var name** (`asana.token_env` / `gleb_tasks.api_key_env`) is stored; `Settings::load_from` reads the actual token from the environment.
- Keep `autopilot.yaml` files without `tracker:` working (they are Asana); every new config field needs a serde default.
- All on-disk state writes go through `registry::write_atomic`.
- Config structs use a `RawX` (serde `Deserialize`, with `default` fns) → processed `X` split; add new config fields with a serde default so old files keep parsing.
- `templates/*.tmpl` are `include_str!`'d into `initproj.rs`; placeholders are `{{NAME}}`, substituted by `fill()`.
