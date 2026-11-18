//! Инициализация проекта при `autopilot add`:
//!  - создаёт `<проект>/autopilot/` с autopilot.yaml и prompt.md из шаблонов;
//!  - пишет память о автопилоте в каталог auto-memory Claude Code этого проекта
//!    (чтобы из любого диалога в проекте можно было спросить про автопилот);
//!  - опционально запускает агента (`claude -p` sonnet/medium или `codex exec`
//!    с medium-эффортом — по движку проекта), чтобы тот заполнил в prompt.md
//!    реальную структуру репозиториев и git-процесс.

use crate::registry::{home_dir, GlobalConfig, ProjectEntry};
use crate::engine::{self, Engine};
use crate::{ApiError, Result, Tracker};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const YAML_TMPL: &str = include_str!("../templates/autopilot.yaml.tmpl");
const YAML_GLEB_TASKS_TMPL: &str = include_str!("../templates/autopilot.gleb_tasks.yaml.tmpl");
const PROMPT_TMPL: &str = include_str!("../templates/prompt.md.tmpl");
const MEMORY_TMPL: &str = include_str!("../templates/memory.md.tmpl");
const INIT_PROMPT_TMPL: &str = include_str!("../templates/init-prompt.md.tmpl");

fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

/// Каталог auto-memory Claude Code для данного пути проекта:
/// ~/.claude/projects/<путь с / и . заменёнными на ->/memory
pub fn claude_memory_dir(project_dir: &Path) -> PathBuf {
    let munged: String = project_dir
        .display()
        .to_string()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect();
    home_dir()
        .join(".claude")
        .join("projects")
        .join(munged)
        .join("memory")
}

/// Создаёт файлы автопилота в проекте (ничего не перезаписывает).
/// Возвращает true, если prompt.md был создан только что (кандидат на доводку claude).
pub fn ensure_files(
    project: &ProjectEntry,
    gcfg: &GlobalConfig,
    workspace: Option<&str>,
    tracker: Tracker,
    engine: Engine,
) -> Result<bool> {
    let apdir = project.autopilot_dir();
    fs::create_dir_all(project.logs_dir())
        .and_then(|_| fs::create_dir_all(project.state_dir()))
        .map_err(|e| ApiError(format!("mkdir {}: {e}", apdir.display())))?;

    let d = &gcfg.defaults;
    let (op_gid, op_label) = d
        .operator
        .as_ref()
        .map(|o| (o.gid.clone(), if o.label.is_empty() { "operator".into() } else { o.label.clone() }))
        .unwrap_or(("FILL_ME".into(), "operator".into()));
    let (ag_gid, ag_name, ag_label) = d
        .agent
        .as_ref()
        .map(|a| {
            (
                a.gid.clone(),
                a.name.clone(),
                if a.label.is_empty() { "agent".into() } else { a.label.clone() },
            )
        })
        .unwrap_or(("FILL_ME".into(), "Claude Code".into(), "agent".into()));

    let yaml_path = apdir.join("autopilot.yaml");
    if !yaml_path.exists() {
        let yaml = match tracker {
            Tracker::Asana => fill(
                YAML_TMPL,
                &[
                    ("PROJECT_NAME", project.name.as_str()),
                    ("TOKEN_ENV", d.token_env.as_str()),
                    ("WORKSPACE", workspace.unwrap_or("FILL_ME")),
                    ("OPERATOR_GID", op_gid.as_str()),
                    ("OPERATOR_LABEL", op_label.as_str()),
                    ("AGENT_GID", ag_gid.as_str()),
                    ("AGENT_NAME", ag_name.as_str()),
                    ("AGENT_LABEL", ag_label.as_str()),
                    ("ENGINE", engine.name()),
                ],
            ),
            Tracker::GlebTasks => {
                let g = d.gleb_tasks.clone().unwrap_or(crate::registry::GlebTasksDefaults {
                    url: "https://tasks.gleb.at".into(),
                    api_key_env: "GLEB_TASKS_API_KEY".into(),
                    operator: None,
                    agent: None,
                });
                let (op_user, op_label) = g
                    .operator
                    .as_ref()
                    .map(|o| (o.username.clone(), if o.label.is_empty() { "operator".into() } else { o.label.clone() }))
                    .unwrap_or(("FILL_ME".into(), "operator".into()));
                let (ag_user, ag_name, ag_label) = g
                    .agent
                    .as_ref()
                    .map(|a| {
                        (
                            if a.username.is_empty() { "claude".into() } else { a.username.clone() },
                            if a.name.is_empty() { "Claude Code".into() } else { a.name.clone() },
                            if a.label.is_empty() { "agent".into() } else { a.label.clone() },
                        )
                    })
                    .unwrap_or(("claude".into(), "Claude Code".into(), "agent".into()));
                fill(
                    YAML_GLEB_TASKS_TMPL,
                    &[
                        ("PROJECT_NAME", project.name.as_str()),
                        ("URL", g.url.as_str()),
                        ("API_KEY_ENV", g.api_key_env.as_str()),
                        ("WORKSPACE", workspace.unwrap_or("FILL_ME")),
                        ("OPERATOR_USERNAME", op_user.as_str()),
                        ("OPERATOR_LABEL", op_label.as_str()),
                        ("AGENT_USERNAME", ag_user.as_str()),
                        ("AGENT_NAME", ag_name.as_str()),
                        ("AGENT_LABEL", ag_label.as_str()),
                        ("ENGINE", engine.name()),
                    ],
                )
            }
        };
        fs::write(&yaml_path, yaml)
            .map_err(|e| ApiError(format!("write {}: {e}", yaml_path.display())))?;
        println!(
            "  создан {} (tracker: {}, engine: {})",
            yaml_path.display(),
            tracker.name(),
            engine.name()
        );
        if workspace.is_none() {
            println!("  ⚠️ workspace не задан — впиши его в autopilot.yaml (или используй --workspace)");
        }
    } else {
        println!("  {} уже существует — не трогаю", yaml_path.display());
    }

    let prompt_path = apdir.join("prompt.md");
    let prompt_created = if !prompt_path.exists() {
        let prompt = fill(PROMPT_TMPL, &[("PROJECT_NAME", project.name.as_str())]);
        fs::write(&prompt_path, prompt)
            .map_err(|e| ApiError(format!("write {}: {e}", prompt_path.display())))?;
        println!("  создан {}", prompt_path.display());
        true
    } else {
        println!("  {} уже существует — не трогаю", prompt_path.display());
        false
    };

    Ok(prompt_created)
}

/// Пишет память об автопилоте в auto-memory Claude Code проекта.
pub fn write_memory(project: &ProjectEntry) -> Result<()> {
    let mem_dir = claude_memory_dir(&project.dir);
    fs::create_dir_all(&mem_dir)
        .map_err(|e| ApiError(format!("mkdir {}: {e}", mem_dir.display())))?;

    let mem_path = mem_dir.join("asana-autopilot.md");
    let content = fill(
        MEMORY_TMPL,
        &[
            ("PROJECT_NAME", project.name.as_str()),
            ("PROJECT_DIR", &project.dir.display().to_string()),
        ],
    );
    fs::write(&mem_path, content)
        .map_err(|e| ApiError(format!("write {}: {e}", mem_path.display())))?;

    // Индексная строка в MEMORY.md (если её ещё нет).
    let index_path = mem_dir.join("MEMORY.md");
    let mut index = fs::read_to_string(&index_path).unwrap_or_default();
    if !index.contains("asana-autopilot.md") {
        if index.trim().is_empty() {
            index = "# Memory Index\n\n".to_string();
        }
        if !index.ends_with('\n') {
            index.push('\n');
        }
        index.push_str(
            "- [Asana autopilot](asana-autopilot.md) — проект под контролем глобального демона autopilot (Asana или gleb_tasks); конфиг в ./autopilot/, CLI `asana`/`tracker`\n",
        );
        fs::write(&index_path, index)
            .map_err(|e| ApiError(format!("write {}: {e}", index_path.display())))?;
    }
    println!("  память записана: {}", mem_path.display());
    Ok(())
}

/// Доводка prompt.md агентом: вписать реальные репозитории/команды/ветки.
/// Claude — sonnet, medium effort; Codex — модель из `codex.model` (или дефолт
/// codex), эффорт medium. Логи — в autopilot/logs/init-*.log.
pub fn refine_prompt(project: &ProjectEntry, engine: Engine, codex: &crate::CodexSettings) -> Result<()> {
    let init_prompt = fill(
        INIT_PROMPT_TMPL,
        &[("PROJECT_DIR", &project.dir.display().to_string())],
    );

    let log_path = project.logs_dir().join(format!(
        "init-{}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    let log_file = fs::File::create(&log_path)
        .map_err(|e| ApiError(format!("create {}: {e}", log_path.display())))?;

    let mut cmd = match engine {
        Engine::Claude => {
            println!("  запускаю claude (sonnet, medium) для заполнения prompt.md — лог: {}", log_path.display());
            let mut c = Command::new("claude");
            c.arg("-p")
                .arg(&init_prompt)
                .arg("--model")
                .arg("sonnet")
                .arg("--dangerously-skip-permissions")
                .env("MAX_THINKING_TOKENS", "10000")
                .stdin(Stdio::null());
            c
        }
        Engine::Codex => {
            println!("  запускаю codex (effort medium) для заполнения prompt.md — лог: {}", log_path.display());
            let mut c = Command::new("codex");
            c.args(engine::codex_args(
                &project.dir.display().to_string(),
                codex.model.as_deref(),
                Some("medium"),
                &codex.extra_args,
            ))
            .stdin(Stdio::piped());
            c
        }
    };
    let mut child = cmd
        .current_dir(&project.dir)
        .env("AUTOPILOT_DIR", project.autopilot_dir())
        .stdout(log_file.try_clone().unwrap())
        .stderr(log_file)
        .spawn()
        .map_err(|e| ApiError(format!("не удалось запустить {}: {e}", engine.name())))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write as _;
        let _ = stdin.write_all(init_prompt.as_bytes());
        // stdin закрывается при drop → EOF для codex.
    }

    // Инициализация не должна длиться дольше 15 минут.
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    println!("  prompt.md заполнен по содержимому проекта.");
                    return Ok(());
                }
                return Err(ApiError(format!(
                    "{} завершился с ошибкой (код {:?}), см. {}",
                    engine.name(),
                    status.code(),
                    log_path.display()
                )));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ApiError(format!(
                        "инициализация превысила 15 минут и была прервана, см. {}",
                        log_path.display()
                    )));
                }
                std::thread::sleep(Duration::from_secs(3));
            }
            Err(e) => return Err(ApiError(format!("try_wait: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("autopilot-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn templates_carry_engine_and_parse() {
        for (i, tmpl) in [YAML_TMPL, YAML_GLEB_TASKS_TMPL].iter().enumerate() {
            let dir = tmp_dir(&format!("tmpl{i}"));
            let yaml = fill(tmpl, &[("ENGINE", "codex"), ("WORKSPACE", "X")]);
            fs::write(dir.join("autopilot.yaml"), yaml).unwrap();
            let (engine, codex) = crate::load_engine(&dir).unwrap();
            assert_eq!(engine, Engine::Codex);
            assert_eq!(codex, crate::CodexSettings::default());
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn engine_config_defaults_and_codex_block() {
        let dir = tmp_dir("cfg");
        // Старый конфиг без engine: — claude.
        fs::write(dir.join("autopilot.yaml"), "asana: {workspace: \"1\"}\n").unwrap();
        assert_eq!(crate::load_engine(&dir).unwrap(), (Engine::Claude, Default::default()));
        fs::write(
            dir.join("autopilot.yaml"),
            "engine: codex\ncodex:\n  model: gpt-5.1-codex\n  extra_args: [\"--profile\", \"night\"]\n",
        )
        .unwrap();
        let (engine, codex) = crate::load_engine(&dir).unwrap();
        assert_eq!(engine, Engine::Codex);
        assert_eq!(codex.model.as_deref(), Some("gpt-5.1-codex"));
        assert_eq!(codex.extra_args, vec!["--profile".to_string(), "night".to_string()]);
        fs::write(dir.join("autopilot.yaml"), "engine: gemini\n").unwrap();
        assert!(crate::load_engine(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
