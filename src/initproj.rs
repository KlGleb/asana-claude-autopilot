//! Инициализация проекта при `autopilot add`:
//!  - создаёт `<проект>/autopilot/` с autopilot.yaml и prompt.md из шаблонов;
//!  - пишет память о автопилоте в каталог auto-memory Claude Code этого проекта
//!    (чтобы из любого диалога в проекте можно было спросить про автопилот);
//!  - опционально запускает `claude -p` (sonnet, medium effort), чтобы тот
//!    заполнил в prompt.md реальную структуру репозиториев и git-процесс.

use crate::registry::{home_dir, GlobalConfig, ProjectEntry};
use crate::{ApiError, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const YAML_TMPL: &str = include_str!("../templates/autopilot.yaml.tmpl");
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
        let yaml = fill(
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
            ],
        );
        fs::write(&yaml_path, yaml)
            .map_err(|e| ApiError(format!("write {}: {e}", yaml_path.display())))?;
        println!("  создан {}", yaml_path.display());
        if workspace.is_none() {
            println!("  ⚠️ workspace не задан — впиши gid workspace в autopilot.yaml (или используй --workspace)");
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
            "- [Asana autopilot](asana-autopilot.md) — проект под контролем глобального демона autopilot; конфиг в ./autopilot/, CLI `asana`\n",
        );
        fs::write(&index_path, index)
            .map_err(|e| ApiError(format!("write {}: {e}", index_path.display())))?;
    }
    println!("  память записана: {}", mem_path.display());
    Ok(())
}

/// Доводка prompt.md силами Claude Code (sonnet, medium effort): вписать
/// реальные репозитории/команды/ветки. Логи — в autopilot/logs/init-*.log.
pub fn refine_prompt_with_claude(project: &ProjectEntry) -> Result<()> {
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

    println!("  запускаю claude (sonnet, medium) для заполнения prompt.md — лог: {}", log_path.display());
    let mut child = Command::new("claude")
        .arg("-p")
        .arg(&init_prompt)
        .arg("--model")
        .arg("sonnet")
        .arg("--dangerously-skip-permissions")
        .env("MAX_THINKING_TOKENS", "10000")
        .current_dir(&project.dir)
        .stdin(Stdio::null())
        .stdout(log_file.try_clone().unwrap())
        .stderr(log_file)
        .spawn()
        .map_err(|e| ApiError(format!("не удалось запустить claude: {e}")))?;

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
                    "claude завершился с ошибкой (код {:?}), см. {}",
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
