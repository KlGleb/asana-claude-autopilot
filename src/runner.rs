//! Выбор задачи и прогон одной claude-сессии для одного проекта.
//!
//! Логика выбора: по всему workspace берутся задачи, назначенные на агента;
//!   - approved-секция → кандидат на выполнение;
//!   - известная не-approved секция (qa/blocked/…, кроме done) → §2.1: вернуть оператору;
//!   - секцию не сопоставить с ролью → «секции не соответствуют процессу» → вернуть оператору;
//!   - сабтаски и задачи без проекта пропускаются молча.
//! Приоритет внутри проекта: in_progress → todo с дедлайном → reopen → todo.

use crate::registry::{stop_file, GlobalConfig, ProjectEntry};
use crate::{Client, Placement, Task};
use chrono::Local;
use regex::RegexBuilder;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub enum Outcome {
    Ok,
    Error,
    Limit,
    Stopped,
}

fn log(msg: &str) {
    println!("[{}] {}", Local::now().format("%Y-%m-%d %H:%M:%S"), msg);
}

pub fn stop_requested() -> bool {
    stop_file().exists()
}

/// Сон с периодической проверкой STOP-файла. true = STOP найден.
pub fn sleep_checking_stop(secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if stop_requested() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    stop_requested()
}

fn append_line(path: &Path, line: &str) {
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Проверяет, разобрался ли человек с задачей после авто-блокировки: ищет наш
/// «ручной» комментарий и любую более позднюю историю (комментарий/реассайн),
/// оставленную НЕ агентом. Если такая есть — блокировка снята вручную.
fn was_manually_cleared(client: &Client, task_gid: &str) -> bool {
    let stories = match client.get_stories(task_gid) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let agent = client.settings.agent_name.as_str();
    let marker = client.settings.manual_marker();
    let Some(block_at) = stories
        .iter()
        .filter(|s| s.is_comment() && s.author() == agent && s.text.contains(marker))
        .map(|s| s.created_at.clone())
        .max()
    else {
        return false;
    };
    stories
        .iter()
        .any(|s| s.created_at > block_at && s.author() != agent)
}

/// Выбор задачи проекта (с побочными эффектами: возврат оператору задач из
/// неправильных секций, авто-blocked по эвристике ручных действий).
pub fn select_task(client: &Client, project: &ProjectEntry) -> Option<(Task, Placement)> {
    let today = Local::now().format("%Y-%m-%d").to_string();
    let settings = &client.settings;

    let tasks = match client.get_assigned_tasks() {
        Ok(t) => t,
        Err(e) => {
            log(&format!("[{}] Не удалось получить задачи агента: {e}", project.name));
            return None;
        }
    };

    let mut runnable: Vec<(Task, Placement)> = Vec::new();
    for t in tasks {
        if t.completed {
            continue;
        }
        // Запрос уже фильтрует по assignee, но подстрахуемся.
        if t.assignee_gid() != Some(settings.agent_gid.as_str()) {
            continue;
        }
        // Сабтаска обрабатывается через контекст родителя, а не сама по себе.
        if t.is_subtask() {
            continue;
        }

        if let Some(p) = client.approved_placement(&t) {
            runnable.push((t, p));
            continue;
        }

        // Не в рабочей секции: разбираем причину.
        match client.placement(&t) {
            // Не карточка доски (задача без проекта) — не наша забота, не трогаем.
            None => continue,
            Some(p) if p.role.is_some() => {
                let role = p.role.as_deref().unwrap();
                // done трогать бессмысленно — работы по ней никто не ждёт.
                if role == "done" {
                    continue;
                }
                log(&format!(
                    "[{}] §2.1: «{}» в секции {} → возвращаю оператору",
                    project.name, t.name, p.section_name
                ));
                let _ = client.add_comment(&t.gid, &settings.msg_not_approved(&p.section_name));
                let _ = client.set_assignee(&t.gid, Some(&settings.operator_gid));
            }
            // Есть проект, но секция не сопоставлена с ролью → секции не соответствуют процессу.
            Some(_) => {
                log(&format!(
                    "[{}] Секции не соответствуют процессу: «{}» → возвращаю оператору",
                    project.name, t.name
                ));
                let _ = client.add_comment(&t.gid, &settings.msg_mismatch());
                let _ = client.set_assignee(&t.gid, Some(&settings.operator_gid));
            }
        }
    }

    // --- Приоритеты внутри проекта ---
    let role_is = |p: &Placement, r: &str| p.role.as_deref() == Some(r);
    let mut queue: Vec<(Task, Placement)> = Vec::new();

    // 1) in_progress
    for (t, p) in runnable.iter().filter(|(_, p)| role_is(p, "in_progress")) {
        queue.push((t.clone(), p.clone()));
    }
    // 2) todo с дедлайном <= сегодня (раньше — первее)
    let mut due: Vec<&(Task, Placement)> = runnable
        .iter()
        .filter(|(t, p)| {
            role_is(p, "todo") && t.due_on.as_deref().map(|d| d <= today.as_str()).unwrap_or(false)
        })
        .collect();
    due.sort_by(|a, b| a.0.due_on.cmp(&b.0.due_on));
    for (t, p) in &due {
        queue.push((t.clone(), p.clone()));
    }
    // 3) reopen
    for (t, p) in runnable.iter().filter(|(_, p)| role_is(p, "reopen")) {
        queue.push((t.clone(), p.clone()));
    }
    // 4) остальные todo
    for (t, p) in runnable
        .iter()
        .filter(|(t, p)| role_is(p, "todo") && !due.iter().any(|d| d.0.gid == t.gid))
    {
        queue.push((t.clone(), p.clone()));
    }

    for (task, placement) in queue {
        // Эвристика: явные ручные действия → сразу в blocked
        let notes_lower = task.notes.to_lowercase();
        let manual_trigger = !settings.manual_keywords.is_empty()
            && settings
                .manual_keywords
                .iter()
                .any(|w| notes_lower.contains(w));
        if manual_trigger {
            if was_manually_cleared(client, &task.gid) {
                log(&format!(
                    "[{}] «{}» триггерит эвристику ручных действий, но уже переоткрыта с комментарием после блокировки → беру в работу",
                    project.name, task.name
                ));
            } else {
                log(&format!("[{}] «{}» требует ручных действий → blocked", project.name, task.name));
                let _ = client.add_comment(&task.gid, settings.msg_manual());
                let _ = client.set_assignee(&task.gid, Some(&settings.operator_gid));
                let _ = client.move_task_to_role(&task.gid, &placement.project_gid, "blocked");
                continue;
            }
        }
        return Some((task, placement));
    }
    None
}

fn extract_model_and_effort(text: &str) -> (String, String) {
    let re = |p: &str| RegexBuilder::new(p).case_insensitive(true).build().unwrap();
    // Директивы «модель: sonnet» / «model: sonnet», «эффорт: medium» / «effort: medium»
    // и свободная форма «Sonnet, medium effort».
    let model_re = re(r"(?:модель|model):\s*(opus|sonnet|haiku|fable)");
    let effort_re = re(r"(?:эффорт|effort):\s*(low|medium|high)");
    let free_re = re(r"\b(opus|sonnet|haiku|fable)\b[\s,]+(low|medium|high)\s+effort");

    let free = free_re.captures(text);
    let model = model_re
        .captures(text)
        .map(|c| c[1].to_lowercase())
        .or_else(|| free.as_ref().map(|c| c[1].to_lowercase()))
        .unwrap_or_else(|| "opus".into());
    let effort = effort_re
        .captures(text)
        .map(|c| c[1].to_lowercase())
        .or_else(|| free.as_ref().map(|c| c[2].to_lowercase()))
        .unwrap_or_else(|| "-".into());
    (model, effort)
}

/// Прогоняет одну claude-сессию по уже выбранной задаче. Пишет status.json,
/// лог сессии — в `<проект>/autopilot/logs/session-*.log`.
pub fn run_session(
    project: &ProjectEntry,
    client: &Client,
    gcfg: &GlobalConfig,
    task: &Task,
    placement: &Placement,
    model_override: Option<&str>,
) -> Outcome {
    use crate::registry::DaemonStatus;

    let apdir = project.autopilot_dir();
    let _ = fs::create_dir_all(project.state_dir());
    let _ = fs::create_dir_all(project.logs_dir());

    let role = placement.role.clone().unwrap_or_else(|| "todo".into());

    // Полный контекст: описание + сабтаски + комментарии + история + связанные задачи
    let full_task = client.get_task(&task.gid).unwrap_or_else(|_| task.clone());
    let context = match client.build_task_context(&full_task, &role) {
        Ok(c) => c,
        Err(e) => {
            log(&format!("[{}] Не удалось собрать контекст задачи {}: {e}", project.name, task.gid));
            return Outcome::Error;
        }
    };
    if fs::write(project.state_dir().join("current-task.md"), &context).is_err() {
        log(&format!("[{}] Не удалось записать current-task.md", project.name));
        return Outcome::Error;
    }

    let (mut model, effort) = extract_model_and_effort(&context);
    if let Some(m) = model_override {
        model = m.to_string();
    }
    log(&format!(
        "[{}] Выбрана задача: {} ({}) — проект «{}», секция {}",
        project.name, task.name, task.gid, placement.project_name, placement.section_name
    ));
    log(&format!("[{}] Модель: {model}, Эффорт: {effort}", project.name));

    if role != "in_progress" {
        if let Err(e) = client.move_task_to_role(&task.gid, &placement.project_gid, "in_progress") {
            log(&format!("[{}] Не удалось переместить в in_progress: {e}", project.name));
        }
    }

    let prompt_path = apdir.join(&client.settings.prompt_file);
    let prompt = match fs::read_to_string(&prompt_path) {
        Ok(p) => p,
        Err(e) => {
            log(&format!("[{}] Нет промпта {}: {e}", project.name, prompt_path.display()));
            return Outcome::Error;
        }
    };

    let log_path = project
        .logs_dir()
        .join(format!("session-{}.log", Local::now().format("%Y%m%d-%H%M%S")));
    let log_file = match fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => {
            log(&format!("[{}] Не могу создать лог сессии: {e}", project.name));
            return Outcome::Error;
        }
    };

    let mut cmd = Command::new("claude");
    cmd.arg("-p")
        .arg(&prompt)
        .arg("--model")
        .arg(&model)
        .arg("--dangerously-skip-permissions")
        .current_dir(&project.dir)
        .env("AUTOPILOT_DIR", &apdir) // чтобы глобальная `asana` находила конфиг проекта
        .stdin(Stdio::null());
    if model != gcfg.daemon.fallback_model {
        cmd.arg("--fallback-model").arg(&gcfg.daemon.fallback_model);
    }
    match effort.as_str() {
        "low" => {
            cmd.env("MAX_THINKING_TOKENS", "2048");
        }
        "medium" => {
            cmd.env("MAX_THINKING_TOKENS", "10000");
        }
        "high" => {
            cmd.env("MAX_THINKING_TOKENS", "31999");
        }
        _ => {}
    }
    cmd.stdout(log_file.try_clone().unwrap()).stderr(log_file);

    log(&format!("[{}] Запуск Claude Code (модель: {model}) -> {}", project.name, log_path.display()));
    DaemonStatus::set("session", |st| {
        st.project = Some(project.name.clone());
        st.task_gid = Some(task.gid.clone());
        st.task_name = Some(task.name.clone());
        st.model = Some(model.clone());
        st.session_log = Some(log_path.display().to_string());
    });

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log(&format!("[{}] Не удалось запустить claude: {e}", project.name));
            return Outcome::Error;
        }
    };

    // Ожидание с хард-таймаутом
    let timeout = gcfg.daemon.session_timeout;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let exit_code: i32 = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {
                if Instant::now() >= deadline {
                    log(&format!("[{}] Сессия превысила {timeout}с — убита по таймауту.", project.name));
                    let _ = child.kill();
                    let _ = child.wait();
                    append_line(
                        &log_path,
                        &format!("\n== WATCHDOG: сессия превысила {timeout}с, убита =="),
                    );
                    break -1;
                }
                std::thread::sleep(Duration::from_secs(5));
            }
            Err(e) => {
                log(&format!("[{}] try_wait error: {e}", project.name));
                break -1;
            }
        }
    };

    let log_content = fs::read_to_string(&log_path).unwrap_or_default();
    let tail: String = {
        let lines: Vec<&str> = log_content.lines().collect();
        let start = lines.len().saturating_sub(10);
        lines[start..].join("\n")
    };
    println!("{tail}");

    if exit_code == 0 {
        log(&format!("[{}] Сессия успешно завершена.", project.name));
        std::thread::sleep(Duration::from_secs(10));
        return Outcome::Ok;
    }

    let limit_re = RegexBuilder::new(
        r"usage limit|limit reached|rate.?limit|session limit|hit your|out of usage credits|usage credits|out of credits|credit balance",
    )
    .case_insensitive(true)
    .build()
    .unwrap();
    if limit_re.is_match(&tail) {
        log(&format!("[{}] Похоже, упёрлись в лимит трат — не считаем сессию продуктивной.", project.name));
        return Outcome::Limit;
    }

    log(&format!("[{}] Claude Code завершился с ошибкой (код {exit_code}), не похоже на лимит.", project.name));
    if sleep_checking_stop(300) {
        return Outcome::Stopped;
    }
    Outcome::Error
}

/// Последний лог сессии проекта (для status/logs/telegram).
pub fn latest_session_log(project: &ProjectEntry) -> Option<PathBuf> {
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(project.logs_dir())
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with("session-") && n.ends_with(".log")
        })
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((meta.modified().ok()?, e.path()))
        })
        .collect();
    logs.sort_by_key(|(t, _)| *t);
    logs.pop().map(|(_, p)| p)
}

/// Хвост файла в N строк.
pub fn tail_file(path: &Path, n: usize) -> String {
    let content = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
