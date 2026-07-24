//! Глобальное состояние автопилота в `~/.autopilot/`:
//! - `projects.yaml` — реестр контролируемых проектов (имя, каталог, приоритет, пауза);
//! - `config.yaml`   — глобальные настройки (дефолты ролей, тайминги демона, telegram);
//! - `daemon.pid` / `daemon.log` / `STOP` / `status.json` — рантайм демона.
//!
//! Конфиги и логи КАЖДОГО проекта живут в `<проект>/autopilot/` — здесь только
//! то, что относится к демону в целом.

use crate::{ApiError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME is not set"))
}

/// Каталог глобального состояния (переопределяется env AUTOPILOT_HOME — для тестов).
pub fn global_dir() -> PathBuf {
    std::env::var("AUTOPILOT_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".autopilot"))
}

pub fn projects_file() -> PathBuf {
    global_dir().join("projects.yaml")
}
pub fn config_file() -> PathBuf {
    global_dir().join("config.yaml")
}
pub fn pid_file() -> PathBuf {
    global_dir().join("daemon.pid")
}
pub fn stop_file() -> PathBuf {
    global_dir().join("STOP")
}
pub fn status_file() -> PathBuf {
    global_dir().join("status.json")
}
pub fn daemon_log_file() -> PathBuf {
    global_dir().join("daemon.log")
}
pub fn tg_offset_file() -> PathBuf {
    global_dir().join("telegram.offset")
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)
        .map_err(|e| ApiError(format!("mkdir {}: {e}", dir.display())))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|e| ApiError(format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| ApiError(format!("rename {}: {e}", path.display())))?;
    Ok(())
}

// ============================ Реестр проектов ============================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub name: String,
    pub dir: PathBuf,
    /// Больше — важнее: демон сканирует проекты по убыванию приоритета.
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub paused: bool,
}

impl ProjectEntry {
    pub fn autopilot_dir(&self) -> PathBuf {
        self.dir.join("autopilot")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.autopilot_dir().join("logs")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.autopilot_dir().join("state")
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

impl Registry {
    pub fn load() -> Result<Registry> {
        let path = projects_file();
        if !path.exists() {
            return Ok(Registry::default());
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| ApiError(format!("read {}: {e}", path.display())))?;
        serde_yaml::from_str(&raw).map_err(|e| ApiError(format!("parse {}: {e}", path.display())))
    }

    pub fn save(&self) -> Result<()> {
        let yaml = serde_yaml::to_string(self)
            .map_err(|e| ApiError(format!("serialize registry: {e}")))?;
        write_atomic(&projects_file(), &yaml)
    }

    pub fn find(&self, name: &str) -> Option<&ProjectEntry> {
        self.projects.iter().find(|p| p.name == name)
    }

    pub fn find_mut(&mut self, name: &str) -> Option<&mut ProjectEntry> {
        self.projects.iter_mut().find(|p| p.name == name)
    }

    /// Активные проекты в порядке обхода демоном: приоритет по убыванию, затем имя.
    pub fn sorted_active(&self) -> Vec<&ProjectEntry> {
        let mut v: Vec<&ProjectEntry> = self.projects.iter().filter(|p| !p.paused).collect();
        v.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.name.cmp(&b.name)));
        v
    }

    /// Все проекты в том же порядке (для status/list).
    pub fn sorted_all(&self) -> Vec<&ProjectEntry> {
        let mut v: Vec<&ProjectEntry> = self.projects.iter().collect();
        v.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.name.cmp(&b.name)));
        v
    }

    pub fn names(&self) -> Vec<&str> {
        self.sorted_all().iter().map(|p| p.name.as_str()).collect()
    }
}

// ============================ Глобальный конфиг ============================

#[derive(Debug, Clone, Deserialize, Default)]
pub struct GlobalConfig {
    #[serde(default)]
    pub defaults: DefaultsCfg,
    #[serde(default)]
    pub daemon: DaemonCfg,
    #[serde(default)]
    pub telegram: TelegramCfg,
}

/// Дефолты, которыми `autopilot add` заполняет autopilot.yaml нового проекта.
#[derive(Debug, Clone, Deserialize)]
pub struct DefaultsCfg {
    #[serde(default = "d_token_env")]
    pub token_env: String,
    pub operator: Option<PersonCfg>,
    pub agent: Option<AgentCfg>,
}

impl Default for DefaultsCfg {
    fn default() -> Self {
        DefaultsCfg {
            token_env: d_token_env(),
            operator: None,
            agent: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PersonCfg {
    pub gid: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentCfg {
    pub gid: String,
    pub name: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DaemonCfg {
    /// Хард-таймаут одной claude-сессии, сек.
    #[serde(default = "d_session_timeout")]
    pub session_timeout: u64,
    /// Сон после упирания в лимит на fallback-модели, сек.
    #[serde(default = "d_limit_sleep")]
    pub limit_sleep: u64,
    /// Сон, когда ни в одном проекте нет задач, сек.
    #[serde(default = "d_no_task_sleep")]
    pub no_task_sleep: u64,
    #[serde(default = "d_fallback_model")]
    pub fallback_model: String,
}

impl Default for DaemonCfg {
    fn default() -> Self {
        DaemonCfg {
            session_timeout: d_session_timeout(),
            limit_sleep: d_limit_sleep(),
            no_task_sleep: d_no_task_sleep(),
            fallback_model: d_fallback_model(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramCfg {
    /// Токен бота литералом (не рекомендуется — лучше token_env).
    pub token: Option<String>,
    #[serde(default = "d_tg_token_env")]
    pub token_env: String,
    /// Кому разрешено управлять ботом (chat id). Пусто = бот только сообщает id.
    #[serde(default)]
    pub allowed_chats: Vec<i64>,
}

impl Default for TelegramCfg {
    fn default() -> Self {
        TelegramCfg {
            token: None,
            token_env: d_tg_token_env(),
            allowed_chats: Vec::new(),
        }
    }
}

impl TelegramCfg {
    pub fn resolve_token(&self) -> Option<String> {
        if let Some(t) = &self.token {
            if !t.trim().is_empty() {
                return Some(t.trim().to_string());
            }
        }
        std::env::var(&self.token_env).ok().filter(|t| !t.trim().is_empty())
    }
}

fn d_token_env() -> String {
    "ASANA_ACCESS_TOKEN".into()
}
fn d_tg_token_env() -> String {
    "AUTOPILOT_TG_TOKEN".into()
}
fn d_session_timeout() -> u64 {
    10800
}
fn d_limit_sleep() -> u64 {
    1200
}
fn d_no_task_sleep() -> u64 {
    600
}
fn d_fallback_model() -> String {
    "opus".into()
}

impl GlobalConfig {
    pub fn load() -> Result<GlobalConfig> {
        let path = config_file();
        if !path.exists() {
            return Ok(GlobalConfig::default());
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| ApiError(format!("read {}: {e}", path.display())))?;
        serde_yaml::from_str(&raw).map_err(|e| ApiError(format!("parse {}: {e}", path.display())))
    }
}

// ============================ Статус демона ============================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonStatus {
    /// idle | session | limit_sleep | stopped
    #[serde(default)]
    pub state: String,
    pub project: Option<String>,
    pub task_gid: Option<String>,
    pub task_name: Option<String>,
    pub model: Option<String>,
    pub session_log: Option<String>,
    #[serde(default)]
    pub updated_at: String,
}

impl DaemonStatus {
    pub fn write(&self) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = write_atomic(&status_file(), &s);
        }
    }

    pub fn read() -> DaemonStatus {
        std::fs::read_to_string(status_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn set(state: &str, f: impl FnOnce(&mut DaemonStatus)) {
        let mut st = DaemonStatus {
            state: state.into(),
            updated_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            ..Default::default()
        };
        f(&mut st);
        st.write();
    }
}

// ============================ PID-хелперы ============================

pub fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_file())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

pub fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// PID работающего демона, если он жив.
pub fn daemon_pid() -> Option<u32> {
    read_pid().filter(|p| pid_alive(*p))
}

/// Группа процессов PID'а (для убийства демона вместе с детьми).
pub fn pgid_of(pid: u32) -> Option<u32> {
    let out = std::process::Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}
