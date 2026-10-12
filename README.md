# asana-claude-autopilot

A single always-on daemon that feeds **Asana** (or **gleb_tasks**, a self-hosted tracker at tasks.gleb.at) tasks to **[Claude Code](https://claude.com/claude-code)** sessions across any number of local projects — with a terminal CLI, per-project priorities, and a **Telegram bot** remote control (inline buttons included). The tracker is chosen per project (`tracker: asana | gleb_tasks` in `autopilot.yaml`); everything else — the process, the CLI, the prompts — is the same.

You assign a task to the agent's Asana account → the daemon picks it up, moves it to *In Progress*, builds the full task context (description, subtasks, comments, attachments, history), runs one `claude -p` session in the project directory, and the session delivers the work: feature branch, PR, Asana comments, move to *QA*/*Done*, or *Blocked* with questions for the operator.

## How it works

```
~/.autopilot/                     ← global state (one per machine)
  projects.yaml                   ← registry: name, dir, priority, paused
  config.yaml                     ← defaults, daemon timings, telegram
  daemon.pid / daemon.log / status.json

<your-project>/autopilot/         ← per-project (created by `autopilot add`)
  autopilot.yaml                  ← tracker (asana | gleb_tasks), workspace, roles, section names
  prompt.md                       ← the session prompt for this project
  logs/session-*.log              ← one log per Claude session
  state/current-task.md           ← context of the task being worked on
```

The daemon loops over registered projects **by priority (higher first)**. The first project with an available task gets a session; then the scan restarts from the top, so high-priority projects always win the next slot. The registry is re-read every iteration — `add`/`remove`/`pause`/`resume`/`priority` (from the CLI **or** the Telegram bot) take effect without restarting the daemon.

Task selection is workspace-wide and assignment-driven: the agent takes only tasks **assigned to its Asana account**, in any Asana project of the workspace. Sections are resolved **by name** per board (`todo` / `in progress` / `reopen` are picked up; tasks in other known sections are returned to the operator with a comment; boards whose sections don't match the process bounce the task back too).

## Install

```sh
git clone git@github.com:KlGleb/asana-claude-autopilot.git
cd asana-claude-autopilot
cargo install --path .   # installs `autopilot` and `asana` into ~/.cargo/bin
```

Requirements: Rust toolchain, `claude` CLI on PATH, and per tracker: an Asana Personal Access Token in `$ASANA_ACCESS_TOKEN` (the agent's account, not yours) and/or a gleb_tasks workspace API key in `$GLEB_TASKS_API_KEY` (Workspace settings → API & MCP; the key already acts as the virtual "Claude Code" member).

## Quick start

```sh
# one-time: global defaults so `add` can fill new project configs
mkdir -p ~/.autopilot && $EDITOR ~/.autopilot/config.yaml   # see below

autopilot add ~/work/myproject --workspace 1234567890 --priority 5                 # Asana
autopilot add ~/work/other --tracker gleb_tasks --workspace SAOBR --priority 3    # gleb_tasks (workspace = its key)
autopilot start          # launch the daemon (background, survives terminal close)
autopilot status         # daemon + per-project state
autopilot logs myproject # tail of the latest session log
autopilot pause myproject / resume myproject
autopilot priority myproject 10
autopilot stop           # stops the daemon AND the running claude session
```

`autopilot add` creates `<dir>/autopilot/` with a filled `autopilot.yaml` and a template `prompt.md`, then runs a short Claude Code session (sonnet, medium effort) that inspects your repos and writes the real workspace layout and git/PR flow into the prompt. It also drops a note into the project's Claude Code auto-memory, so any future Claude session in that project knows the autopilot exists. Skip the Claude step with `--no-init`.

## Global config (`~/.autopilot/config.yaml`)

```yaml
defaults:                # used by `autopilot add` to fill new project configs
  token_env: ASANA_ACCESS_TOKEN
  operator: {gid: "16086...", label: gleb}          # the human
  agent: {gid: "12167...", name: "Claude Code", label: claude}  # the bot account

daemon:
  session_timeout: 10800  # hard kill for a session, seconds
  no_task_sleep: 60       # poll interval when no project has tasks = max pickup delay for a new task
  limit_sleep: 1200       # sleep when usage limits are hit on the fallback model too
  fallback_model: opus

telegram:
  token_env: AUTOPILOT_TG_TOKEN   # or `token: "..."` literal (not recommended)
  allowed_chats: [123456789]      # who may control the bot
```

## gleb_tasks as the tracker

Per-project `autopilot.yaml` for a gleb_tasks workspace (template: `templates/autopilot.gleb_tasks.yaml.tmpl`):

```yaml
tracker: gleb_tasks
gleb_tasks:
  url: https://tasks.gleb.at
  api_key_env: GLEB_TASKS_API_KEY   # env var holding the workspace API key
  workspace: SAOBR                  # workspace key (task prefix); the API key is bound to it
roles:
  operator: { username: GlebKl, label: gleb }                      # or id: "<uuid>"
  agent:    { username: claude, name: "Claude Code", label: claude } # optional: the API key IS the agent
sections: { todo: [todo, Бэклог], in_progress: [in progress], qa: [qa], done: [done], blocked: [blocked], reopen: [reopen] }
approved: [todo, in_progress, reopen]
```

Differences from Asana, all handled by the CLI/daemon: task ids are keys (`SAOBR-12`, uuids also accepted), links are `https://tasks.gleb.at/t/SAOBR-12`, attachments are addressed as `<task>/<attachment id>` (printed that way in the task context), and the task history comes from the tracker's activity log. The daemon resolves the workspace key and role usernames to ids on every config load (one GraphQL request). Global defaults for `autopilot add --tracker gleb_tasks` live in `~/.autopilot/config.yaml`:

```yaml
defaults:
  gleb_tasks:
    url: https://tasks.gleb.at
    api_key_env: GLEB_TASKS_API_KEY
    operator: { username: GlebKl, label: gleb }
    agent: { username: claude, name: "Claude Code", label: claude }
```

## Telegram bot

1. Create a bot via [@BotFather](https://t.me/BotFather), put its token into the env var `AUTOPILOT_TG_TOKEN` (or `telegram.token`).
2. Start the daemon, message your bot — it replies with your chat id.
3. Put the id into `telegram.allowed_chats`, run `autopilot restart`.

The bot registers a command menu (Telegram's **Menu** button + autocomplete) and shows an inline **main menu** on `/start` / `/menu`. Commands:

- `/status` — daemon + per-project state (with inline buttons)
- `/account` — which Claude Code account the daemon is logged into (email, org, subscription)
- `/usage` — Claude limit consumption in **percent** (current session + weekly), straight from `claude`'s own `/usage`; it's a local, free call that doesn't spend tokens
- `/login` — re-authenticate the daemon's Claude account: the bot asks for confirmation (this changes billing/limits for every session), then sends the OAuth link; reply with the code from the browser and it completes the login (`/cancel` aborts). Login times out after 10 min.
- `/pause <name>` · `/resume <name>` · `/priority <name> <n>` · `/logs <name>`

Buttons per project: ⏸/▶️ toggle, 🔼/🔽 priority, 📜 logs. The account/usage/login features are thin wrappers over the `claude` CLI (`src/claudecli.rs`) — the daemon must have `claude` on PATH (it already does, since it runs the sessions).

## The `asana` CLI (alias `tracker`)

A token-cheap plain-text Asana client used inside Claude sessions instead of the Asana MCP server. It finds the project config by walking up from the current directory (or via `AUTOPILOT_DIR`), so one global binary serves every project:

```
asana task <gid>          # full task context
asana comment <gid> <text|->
asana move <gid> <role>   # todo|in_progress|reopen|qa|done|blocked
asana assign <gid> <operator|agent|none>
asana complete <gid>
asana subtask <parent> <name> [--notes <text|->] [--assignee operator|agent]
asana download <attachment_gid> [dir]
asana qa|done|block <gid> <comment|->   # comment + move + reassign in one call
asana fields              # gleb_tasks: create the Model / Effort custom fields in the workspace
```

## Model & effort per task

For a **gleb_tasks** project the task carries two custom fields, **Model** (`fable` / `opus` / `sonnet` / `haiku`) and **Effort** (`low` / `medium` / `high` / `xhigh` / `max`). Both are SELECT fields scoped to the Claude Code member — they show up in the "Fields for Claude Code" dialog when you hand a task over, and in the task card while it is assigned to the agent. Leave them empty for the defaults (model `fable`, the `claude` CLI's default effort). Create them once per workspace with `asana fields` (idempotent; run from the project directory or with `AUTOPILOT_DIR`); the names are configurable in `autopilot.yaml`:

```yaml
fields:
  model: Model
  effort: Effort
```

Text directives still work for both trackers (and are the only option for Asana): `model: sonnet`, `effort: medium` in the task description, Russian or English. Fields win over text. Effort is passed as `claude --effort <level>`; the default model has automatic fallback to `opus` when usage limits are hit.

## Safety notes

- The Asana token never lives in config files — only env var names do.
- Claude sessions run with `--dangerously-skip-permissions` in your project directory: use a dedicated Asana agent account, review PRs before merging, and keep the daemon on a machine you trust.
- `autopilot stop` kills the whole process group — including a claude session mid-task (the task simply stays in *In Progress* and is picked up next time).

## License

MIT — see [LICENSE](LICENSE).
