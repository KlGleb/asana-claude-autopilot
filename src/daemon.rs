//! Демон: бесконечный цикл по всем зарегистрированным проектам.
//!
//! Каждую итерацию реестр и глобальный конфиг перечитываются с диска, поэтому
//! `autopilot add/remove/pause/resume/priority` (и Telegram-бот) действуют на
//! работающий демон без рестарта. Проекты обходятся по убыванию приоритета;
//! первая найденная задача запускает сессию, после чего обход начинается заново
//! (высокоприоритетный проект всегда получает следующую сессию первым).
//!
//! Опрос трекеров частый (`daemon.no_task_sleep`, по умолчанию 60 с), чтобы новая
//! задача бралась в работу в течение минуты. Чтобы частый опрос не шумел:
//! - разобранный `Settings` каждого проекта кэшируется до изменения его
//!   `autopilot.yaml` (для gleb_tasks разбор — это сетевой запрос резолва id);
//! - «задач нет» пишется в лог один раз при переходе в простой, а не на каждом круге;
//! - повторяющаяся ошибка конфига проекта логируется один раз, пока не изменится.

use crate::registry::{
    daemon_pid, global_dir, stop_file, DaemonStatus, GlobalConfig, Registry,
};
use crate::runner::{self, Outcome};
use crate::{telegram, Client, Settings};
use chrono::Local;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn log(msg: &str) {
    println!("[{}] {}", Local::now().format("%Y-%m-%d %H:%M:%S"), msg);
}

/// Кэш разобранных конфигов проектов: ключ — каталог `autopilot/`, значение —
/// mtime его `autopilot.yaml` на момент разбора и сам `Settings`. Перечитывается,
/// как только файл изменился, так что правки конфига подхватываются без рестарта.
#[derive(Default)]
struct SettingsCache {
    entries: HashMap<PathBuf, (Option<SystemTime>, Settings)>,
    /// Последняя залогированная ошибка конфига по проекту — чтобы не повторять её
    /// каждую минуту.
    last_errors: HashMap<String, String>,
}

impl SettingsCache {
    fn mtime(dir: &Path) -> Option<SystemTime> {
        std::fs::metadata(dir.join("autopilot.yaml")).and_then(|m| m.modified()).ok()
    }

    /// `Settings` проекта из кэша или заново; при ошибке — None (ошибка уже залогирована).
    fn get(&mut self, project: &crate::registry::ProjectEntry) -> Option<Settings> {
        let dir = project.autopilot_dir();
        let mtime = Self::mtime(&dir);
        if let Some((cached_mtime, settings)) = self.entries.get(&dir) {
            if *cached_mtime == mtime {
                return Some(settings.clone());
            }
        }
        match Settings::load_from(&dir) {
            Ok(s) => {
                self.entries.insert(dir, (mtime, s.clone()));
                if self.last_errors.remove(&project.name).is_some() {
                    log(&format!("[{}] конфиг снова загружается.", project.name));
                }
                Some(s)
            }
            Err(e) => {
                self.entries.remove(&dir);
                let msg = e.to_string();
                if self.last_errors.get(&project.name) != Some(&msg) {
                    log(&format!("[{}] конфиг не загружен: {msg}", project.name));
                    self.last_errors.insert(project.name.clone(), msg);
                }
                None
            }
        }
    }
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
    let mut settings_cache = SettingsCache::default();
    // true, пока демон в простое: «задач нет» логируется один раз на вход в простой.
    let mut idle_logged = false;

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
            let Some(settings) = settings_cache.get(project) else {
                continue;
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
            idle_logged = false;
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
            // status.json обновляется каждый круг (updated_at = время последней проверки),
            // а в лог простой попадает один раз — иначе при опросе раз в минуту он
            // забивался бы одинаковыми строками.
            DaemonStatus::set("idle", |_| {});
            if !idle_logged {
                log(&format!(
                    "Задач нет ни в одном активном проекте. Проверяю трекеры каждые {}.",
                    human_secs(gcfg.daemon.no_task_sleep)
                ));
                idle_logged = true;
            }
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

/// Интервал в человеко-читаемом виде: «45 с», «1 мин», «2 мин 30 с».
fn human_secs(secs: u64) -> String {
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s} с"),
        (m, 0) => format!("{m} мин"),
        (m, s) => format!("{m} мин {s} с"),
    }
}
