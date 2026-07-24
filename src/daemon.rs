//! Демон: бесконечный цикл по всем зарегистрированным проектам.
//!
//! Каждую итерацию реестр и глобальный конфиг перечитываются с диска, поэтому
//! `autopilot add/remove/pause/resume/priority` (и Telegram-бот) действуют на
//! работающий демон без рестарта. Проекты обходятся по убыванию приоритета;
//! первая найденная задача запускает сессию, после чего обход начинается заново
//! (высокоприоритетный проект всегда получает следующую сессию первым).

use crate::registry::{
    daemon_pid, global_dir, stop_file, DaemonStatus, GlobalConfig, Registry,
};
use crate::runner::{self, Outcome};
use crate::{telegram, Client, Settings};
use chrono::Local;
use std::process::Command;

fn log(msg: &str) {
    println!("[{}] {}", Local::now().format("%Y-%m-%d %H:%M:%S"), msg);
}

/// Текстовый статус для CLI и Telegram.
pub fn status_text() -> String {
    let reg = Registry::load().unwrap_or_default();
    let st = DaemonStatus::read();
    let mut out = String::new();

    match daemon_pid() {
        Some(pid) => out.push_str(&format!("Демон: РАБОТАЕТ (pid {pid})\n")),
        None => out.push_str("Демон: остановлен\n"),
    }

    if daemon_pid().is_some() {
        match st.state.as_str() {
            "session" => out.push_str(&format!(
                "Сейчас: сессия «{}» [{}] (модель {}, с {})\n",
                st.task_name.as_deref().unwrap_or("?"),
                st.project.as_deref().unwrap_or("?"),
                st.model.as_deref().unwrap_or("?"),
                st.updated_at
            )),
            "limit_sleep" => out.push_str(&format!("Сейчас: пауза из-за лимита (с {})\n", st.updated_at)),
            "idle" => out.push_str(&format!("Сейчас: простой, задач нет (проверка {})\n", st.updated_at)),
            _ => {}
        }
    }

    out.push_str("\nПроекты:\n");
    if reg.projects.is_empty() {
        out.push_str("  (нет — добавь: autopilot add <dir>)\n");
    }
    for p in reg.sorted_all() {
        let state = if p.paused { "⏸ пауза" } else { "▶️ активен" };
        out.push_str(&format!("  {} — {} (приоритет {})\n", p.name, state, p.priority));
        out.push_str(&format!("      {}\n", p.dir.display()));
    }
    out
}

/// Тело демона (вызывается скрытой командой `autopilot __daemon`).
pub fn run() {
    let _ = std::fs::create_dir_all(global_dir());
    let _ = std::fs::remove_file(stop_file());
    let _ = std::fs::write(crate::registry::pid_file(), std::process::id().to_string());

    log(&format!("Автопилот-демон запущен (pid {}).", std::process::id()));

    // Не даём Mac уснуть, пока жив демон.
    let caffeinate = Command::new("caffeinate")
        .args(["-dimsu", "-w", &std::process::id().to_string()])
        .spawn()
        .ok();
    if caffeinate.is_none() {
        log("caffeinate не найден — Mac может уснуть во время работы автопилота.");
    }

    // Telegram-бот (если настроен токен) — отдельный поток, живёт с процессом.
    match GlobalConfig::load() {
        Ok(cfg) => {
            if telegram::spawn(cfg.telegram.clone()).is_some() {
                log("Telegram-бот запущен.");
            } else {
                log("Telegram-бот не настроен (нет токена) — пропускаю.");
            }
        }
        Err(e) => log(&format!("config.yaml не читается: {e}")),
    }

    // При лимите на модели задачи переключаемся на fallback; сбрасывается после
    // успешной/ошибочной сессии.
    let mut model_override: Option<String> = None;

    'outer: loop {
        if runner::stop_requested() {
            break;
        }

        let gcfg = GlobalConfig::load().unwrap_or_default();
        let reg = match Registry::load() {
            Ok(r) => r,
            Err(e) => {
                log(&format!("projects.yaml не читается: {e}"));
                if runner::sleep_checking_stop(60) {
                    break;
                }
                continue;
            }
        };

        let mut ran_session = false;
        for project in reg.sorted_active() {
            if runner::stop_requested() {
                break 'outer;
            }
            let settings = match Settings::load_from(&project.autopilot_dir()) {
                Ok(s) => s,
                Err(e) => {
                    log(&format!("[{}] конфиг не загружен: {e}", project.name));
                    continue;
                }
            };
            let client = Client::new(settings);
            let Some((task, placement)) = runner::select_task(&client, project) else {
                continue;
            };

            let outcome = runner::run_session(
                project,
                &client,
                &gcfg,
                &task,
                &placement,
                model_override.as_deref(),
            );
            ran_session = true;
            match outcome {
                Outcome::Stopped => break 'outer,
                Outcome::Limit => {
                    if model_override.as_deref() != Some(gcfg.daemon.fallback_model.as_str()) {
                        log(&format!("Переключаюсь на fallback-модель {}.", gcfg.daemon.fallback_model));
                        model_override = Some(gcfg.daemon.fallback_model.clone());
                    } else {
                        log(&format!(
                            "Лимит и на fallback-модели — сплю {} мин.",
                            gcfg.daemon.limit_sleep / 60
                        ));
                        DaemonStatus::set("limit_sleep", |_| {});
                        if runner::sleep_checking_stop(gcfg.daemon.limit_sleep) {
                            break 'outer;
                        }
                    }
                }
                Outcome::Ok | Outcome::Error => {
                    model_override = None;
                }
            }
            // После сессии — обход заново, с самого приоритетного проекта.
            break;
        }

        if !ran_session {
            let gcfg = GlobalConfig::load().unwrap_or_default();
            DaemonStatus::set("idle", |_| {});
            log(&format!(
                "Задач нет ни в одном активном проекте. Сплю {} мин.",
                gcfg.daemon.no_task_sleep / 60
            ));
            if runner::sleep_checking_stop(gcfg.daemon.no_task_sleep) {
                break;
            }
        }
    }

    DaemonStatus::set("stopped", |_| {});
    log("Автопилот-демон остановлен.");
    let _ = std::fs::remove_file(crate::registry::pid_file());
    if let Some(mut c) = caffeinate {
        let _ = c.kill();
    }
}
