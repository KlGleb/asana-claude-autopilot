# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A single always-on daemon that feeds **Asana** tasks to **Claude Code** (`claude -p`) sessions across any number of local projects. It ships two binaries — `autopilot` (the daemon + registry/control CLI) and `asana` (a token-cheap Asana client used *inside* the sessions the daemon spawns). See `README.md` for the full user-facing feature tour; this file is the architecture map.

Note: source comments and most user-facing strings are in **Russian**. Match that language when editing existing modules.

## Build / run

```sh
cargo build --release          # both binaries -> target/release/{autopilot,asana}
cargo install --path .         # installs both into ~/.cargo/bin
cargo clippy                    # lint
cargo run --bin autopilot -- status    # run a subcommand without installing
cargo run --bin asana -- task <gid>
```

There are **no tests** in this repo. To exercise the daemon loop without backgrounding it, use `autopilot run` (runs the daemon body in the current terminal). `autopilot __daemon` is the hidden subcommand the backgrounded process actually execs — don't call it directly.

## The two-layer state model (read this first)

State lives in two distinct places, and confusing them is the most common source of bugs:

- **Global, one per machine** — `~/.autopilot/` (override with `AUTOPILOT_HOME` env, used for isolation): `projects.yaml` (the registry), `config.yaml` (daemon timings, role defaults, telegram), plus daemon runtime files `daemon.pid` / `daemon.log` / `STOP` / `status.json` / `telegram.offset`. All modeled in `src/registry.rs`.
- **Per-project** — `<project>/autopilot/`: `autopilot.yaml` (Asana identity: workspace gid, operator/agent role gids, section-name→role mapping), `prompt.md` (the session prompt), `logs/session-*.log`, `state/current-task.md`. The Asana identity config is modeled by `Settings` in `src/lib.rs`.

`Settings::find_dir()` locates the per-project config by walking **up** from the current directory (or `AUTOPILOT_DIR` env). This is why one globally-installed `asana` binary serves every project — the daemon sets `AUTOPILOT_DIR` when it spawns a session.

## Module map (`src/`)

- **`lib.rs`** (crate `autopilot_core`) — the shared Asana HTTP `Client`, the `Settings` config parser, and data models (`Task`, `Story`, `Placement`, …). Key methods: `build_task_context()` assembles the full markdown context handed to a session; `placement()`/`approved_placement()` resolve which project+section a task "belongs" to; `resolve_section_gid()` maps a role (e.g. `in_progress`) to a section gid **by matching section names per board** (cached in `section_cache`). All HTTP goes through `Client::request()` (3 retries on 429/5xx).
- **`daemon.rs`** — the infinite loop. Each iteration **re-reads** the registry and global config from disk (so `add/pause/priority` from CLI *or* Telegram take effect with no restart). Iterates active projects by **descending priority**; the first project with a runnable task gets one session, then the scan restarts from the top. Handles the model-fallback state machine on `Outcome::Limit`.
- **`runner.rs`** — `select_task()` (task selection + side effects) and `run_session()` (spawns `claude`). This is where the business rules live (see below).
- **`registry.rs`** — `Registry`/`ProjectEntry`, `GlobalConfig` (+ all serde defaults), `DaemonStatus`, PID/pgid helpers. `write_atomic()` (tmp-file + rename) is used for all state writes.
- **`initproj.rs`** — backs `autopilot add`: templates out `<project>/autopilot/`, writes a note into the project's **Claude Code auto-memory** (`~/.claude/projects/<munged-path>/memory/`), and optionally runs a one-shot `claude` (sonnet) session to fill `prompt.md` with the repo's real layout.
- **`telegram.rs`** — long-polling bot running in a daemon thread; registry mutations go through the on-disk registry (so CLI and bot stay consistent). Also exposes the daemon's **Claude account**: `/account` and `/usage` shell out via `claudecli`, and `/login` runs an interactive OAuth flow — it holds the spawned `claude auth login` child in `Bot.pending_login` (a `Mutex<Option<PendingLogin>>`) across poll iterations, sends the URL, then feeds the user's reply as the code to the child's stdin. A confirmation button gates login; state expires after `LOGIN_TTL`.
- **`claudecli.rs`** — thin wrappers over the `claude` CLI used by the bot: `auth_status()` (`claude auth status --json`), `usage()` (`claude -p "/usage" --output-format json`, a free/local call — parses percentages out of the `result` text), and `login_start()`/`login_finish()` (the URL-then-code flow; stdin is closed after the code to force EOF so a bad code can't hang the read).
- **`bin/autopilot.rs`** / **`bin/asana.rs`** — arg parsing + command dispatch only; logic lives in the library.

## Task-selection rules (`runner::select_task`)

Selection is **workspace-wide and assignment-driven**: `get_assigned_tasks()` fetches every incomplete task assigned to the agent's Asana account across the whole workspace — assignment *is* the filter for "which projects the agent owns". Sections are then resolved **by name** against the board each task lives in. The rules, in order:

- Subtasks and tasks with no project membership are skipped silently.
- Task in an **approved** section → candidate. Intra-project priority: `in_progress` → `todo` with due date ≤ today → `reopen` → other `todo`.
- Task in a known but **non-approved** section (qa/blocked/…, except `done`) → §2.1: comment + reassign to operator.
- Task whose section maps to **no** role → "sections don't match the process" → comment + reassign to operator.
- **Manual-action heuristic**: if the description contains any `heuristics.manual_keywords`, the task is auto-moved to `blocked` and reassigned to the operator — *unless* `was_manually_cleared()` finds operator activity after the bot's own block comment (tracked via the language-specific `manual_marker()` substring).

There is no local blocked-list: the Asana `blocked` section (plus reassignment to the operator) is the single source of truth for parked tasks.

## Running a session (`runner::run_session`)

1. Build full task context → write `state/current-task.md`.
2. Parse `model:`/`effort:` directives (Russian or English) from the task text via `extract_model_and_effort`; `model_override` from the daemon's fallback state wins.
3. Move task to `in_progress`, then spawn `claude -p <prompt> --model <m> --dangerously-skip-permissions --fallback-model <fb>` in the project dir with `AUTOPILOT_DIR` set and `MAX_THINKING_TOKENS` per effort (low=2048/medium=10000/high=31999).
4. Watchdog kills the child at `daemon.session_timeout`. On non-zero exit, the log tail is regex-matched for usage-limit phrases → `Outcome::Limit` (triggers model fallback, then `limit_sleep`); otherwise `Outcome::Error`.

The session itself does the real work by calling the `asana` CLI (comment / move / qa / done / block / subtask / download) — that binary is the session's only intended Asana surface, deliberately used instead of the Asana MCP server to save tokens.

## Process-group semantics

`autopilot start` spawns the daemon in its **own process group** (`process_group(0)`) so it survives terminal close. `autopilot stop` writes the `STOP` file (checked between loop steps for a graceful exit) *and* signals the whole group with TERM→KILL — deliberately killing any in-flight `claude` session (the task just stays `in_progress` and is re-picked next run). The daemon also spawns `caffeinate` to keep the Mac awake while alive.

## Conventions

- Never put the Asana token in a config file — only the **env-var name** (`asana.token_env`) is stored; `Settings::load_from` reads the actual token from the environment.
- All on-disk state writes go through `registry::write_atomic`.
- Config structs use a `RawX` (serde `Deserialize`, with `default` fns) → processed `X` split; add new config fields with a serde default so old files keep parsing.
- `templates/*.tmpl` are `include_str!`'d into `initproj.rs`; placeholders are `{{NAME}}`, substituted by `fill()`.
