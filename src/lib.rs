//! Общий клиент трекера и конфиг для автопилота и CLI-утилиты `asana`/`tracker`.
//!
//! Два бэкенда: **Asana** (по умолчанию, `src/asana.rs`) и **gleb_tasks**
//! (`src/gleb_tasks.rs`, https://tasks.gleb.at). Какой использовать — поле
//! `tracker:` в `autopilot.yaml` проекта. Всё, что выше сырого API (роли,
//! маппинг секций по именам, выбор размещения, сборка контекста задачи),
//! живёт здесь в `Client` и одинаково для обоих бэкендов.
//!
//! Проект-независимый: вся идентичность (workspace, роли операторов, маппинг
//! секций) читается из `autopilot.yaml` в каталоге `autopilot/` проекта.
//! Автопилот работает по всему workspace — берёт задачи, назначенные на
//! аккаунт-агента, в любом проекте, а секции резолвит по имени внутри
//! проекта каждой задачи.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub mod asana;
pub mod claudecli;
pub mod daemon;
pub mod gleb_tasks;
pub mod initproj;
pub mod registry;
pub mod runner;
pub mod telegram;

pub use asana::BASE_URL;

// ============================ Ошибки ============================

#[derive(Debug)]
pub struct ApiError(pub String);

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ApiError {}

pub type Result<T> = std::result::Result<T, ApiError>;

// ============================ Конфиг ============================

/// Какой трекер стоит за проектом.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracker {
    Asana,
    GlebTasks,
}

impl Tracker {
    pub fn parse(s: &str) -> Result<Tracker> {
        match s.trim().to_lowercase().replace('-', "_").as_str() {
            "asana" => Ok(Tracker::Asana),
            "gleb_tasks" | "glebtasks" | "tasks" => Ok(Tracker::GlebTasks),
            other => Err(ApiError(format!("неизвестный tracker «{other}» (ожидается asana | gleb_tasks)"))),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Tracker::Asana => "asana",
            Tracker::GlebTasks => "gleb_tasks",
        }
    }
}

/// Сырой разбор `autopilot.yaml`.
#[derive(Debug, Deserialize)]
struct RawSettings {
    /// asana (по умолчанию) | gleb_tasks
    #[serde(default)]
    tracker: Option<String>,
    #[serde(default)]
    asana: Option<RawAsana>,
    #[serde(default)]
    gleb_tasks: Option<RawGlebTasks>,
    roles: RawRoles,
    /// role -> имя секции или список имён-синонимов.
    sections: HashMap<String, RawNames>,
    #[serde(default)]
    approved: Vec<String>,
    #[serde(default)]
    heuristics: RawHeuristics,
    #[serde(default = "default_prompt_file")]
    prompt_file: String,
    #[serde(default)]
    mismatch_comment: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawAsana {
    #[serde(default = "default_token_env")]
    token_env: String,
    workspace: String,
}

#[derive(Debug, Deserialize)]
struct RawGlebTasks {
    #[serde(default = "default_gleb_tasks_url")]
    url: String,
    #[serde(default = "default_gleb_tasks_key_env")]
    api_key_env: String,
    /// Ключ workspace (SAOBR) — ровно тот, к которому привязан API-ключ.
    workspace: String,
}

#[derive(Debug, Deserialize)]
struct RawRoles {
    operator: RawOperator,
    agent: RawAgent,
}

/// Человек-оператор: Asana — `gid`; gleb_tasks — `id` (uuid) или `username`.
#[derive(Debug, Deserialize)]
struct RawOperator {
    #[serde(default)]
    gid: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default = "default_operator_label")]
    label: String,
}

/// Аккаунт-агент: Asana — `gid` + `name`; gleb_tasks — можно ничего не
/// указывать (API-ключ сам «является» агентом Claude Code).
#[derive(Debug, Deserialize)]
struct RawAgent {
    #[serde(default)]
    gid: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    username: Option<String>,
    /// Отображаемое имя аккаунта-агента (для детекции авторства комментариев).
    #[serde(default)]
    name: Option<String>,
    #[serde(default = "default_agent_label")]
    label: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawHeuristics {
    #[serde(default)]
    manual_keywords: Vec<String>,
    #[serde(default = "default_language")]
    comment_language: String,
}

/// Значение секции: одно имя или список синонимов.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawNames {
    One(String),
    Many(Vec<String>),
}

fn default_token_env() -> String {
    "ASANA_ACCESS_TOKEN".into()
}
fn default_gleb_tasks_url() -> String {
    "https://tasks.gleb.at".into()
}
fn default_gleb_tasks_key_env() -> String {
    "GLEB_TASKS_API_KEY".into()
}
fn default_operator_label() -> String {
    "operator".into()
}
fn default_agent_label() -> String {
    "agent".into()
}
fn default_language() -> String {
    "en".into()
}
fn default_prompt_file() -> String {
    "prompt.md".into()
}

/// Обработанный конфиг автопилота одного проекта.
///
/// `*_gid` — идентификаторы в терминах трекера: gid Asana либо uuid gleb_tasks
/// (для gleb_tasks `workspace` — uuid workspace, `workspace_key` — его ключ).
#[derive(Debug, Clone)]
pub struct Settings {
    pub tracker: Tracker,
    /// Asana PAT либо API-ключ workspace gleb_tasks.
    pub token: String,
    /// Базовый URL gleb_tasks (для Asana пусто).
    pub base_url: String,
    pub workspace: String,
    pub workspace_key: String,
    pub operator_gid: String,
    pub operator_label: String,
    pub agent_gid: String,
    pub agent_name: String,
    pub agent_label: String,
    /// role -> список допустимых имён секций (нижний регистр).
    sections: Vec<(String, Vec<String>)>,
    approved: Vec<String>,
    pub manual_keywords: Vec<String>,
    pub comment_language: String,
    pub prompt_file: String,
    mismatch_comment: Option<String>,
}

impl Settings {
    /// Ищет каталог с `autopilot.yaml`: env `AUTOPILOT_DIR`, иначе вверх от
    /// текущего каталога (`<dir>/autopilot/autopilot.yaml` или `<dir>/autopilot.yaml`).
    pub fn find_dir() -> Result<PathBuf> {
        if let Ok(d) = std::env::var("AUTOPILOT_DIR") {
            return Ok(PathBuf::from(d));
        }
        let mut cur = std::env::current_dir()
            .map_err(|e| ApiError(format!("current_dir failed: {e}")))?;
        loop {
            let nested = cur.join("autopilot").join("autopilot.yaml");
            if nested.is_file() {
                return Ok(cur.join("autopilot"));
            }
            if cur.join("autopilot.yaml").is_file() {
                return Ok(cur.clone());
            }
            if !cur.pop() {
                break;
            }
        }
        Err(ApiError(
            "autopilot.yaml не найден: запусти команду из каталога проекта \
             (где есть autopilot/autopilot.yaml) или задай AUTOPILOT_DIR"
                .into(),
        ))
    }

    /// Загружает конфиг из `<dir>/autopilot.yaml`. Для gleb_tasks делает один
    /// GraphQL-запрос, чтобы превратить ключ workspace и username'ы ролей в id.
    pub fn load_from(dir: &Path) -> Result<Settings> {
        let path = dir.join("autopilot.yaml");
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| ApiError(format!("не могу прочитать {}: {e}", path.display())))?;
        let rs: RawSettings = serde_yaml::from_str(&raw)
            .map_err(|e| ApiError(format!("ошибка разбора {}: {e}", path.display())))?;

        let tracker = match &rs.tracker {
            Some(t) => Tracker::parse(t)?,
            None if rs.asana.is_none() && rs.gleb_tasks.is_some() => Tracker::GlebTasks,
            None => Tracker::Asana,
        };

        let sections: Vec<(String, Vec<String>)> = rs
            .sections
            .into_iter()
            .map(|(role, names)| {
                let list = match names {
                    RawNames::One(s) => vec![s],
                    RawNames::Many(v) => v,
                };
                let lowered = list.iter().map(|s| s.trim().to_lowercase()).collect();
                (role, lowered)
            })
            .collect();

        let common = |token, base_url, workspace, workspace_key, operator_gid, agent_gid, agent_name| Settings {
            tracker,
            token,
            base_url,
            workspace,
            workspace_key,
            operator_gid,
            operator_label: rs.roles.operator.label.clone(),
            agent_gid,
            agent_name,
            agent_label: rs.roles.agent.label.clone(),
            sections,
            approved: rs.approved.clone(),
            manual_keywords: rs.heuristics.manual_keywords.iter().map(|s| s.to_lowercase()).collect(),
            comment_language: rs.heuristics.comment_language.clone(),
            prompt_file: rs.prompt_file.clone(),
            mismatch_comment: rs.mismatch_comment.clone(),
        };

        match tracker {
            Tracker::Asana => {
                let a = rs.asana.as_ref().ok_or_else(|| {
                    ApiError(format!("{}: для tracker asana нужен блок asana: {{ workspace }}", path.display()))
                })?;
                let token = std::env::var(&a.token_env).map_err(|_| {
                    ApiError(format!("переменная окружения {} не задана (Asana-токен)", a.token_env))
                })?;
                let operator_gid = rs.roles.operator.gid.clone().ok_or_else(|| {
                    ApiError(format!("{}: roles.operator.gid обязателен для Asana", path.display()))
                })?;
                let agent_gid = rs.roles.agent.gid.clone().ok_or_else(|| {
                    ApiError(format!("{}: roles.agent.gid обязателен для Asana", path.display()))
                })?;
                let agent_name = rs.roles.agent.name.clone().ok_or_else(|| {
                    ApiError(format!("{}: roles.agent.name обязателен для Asana", path.display()))
                })?;
                Ok(common(token, String::new(), a.workspace.clone(), a.workspace.clone(), operator_gid, agent_gid, agent_name))
            }
            Tracker::GlebTasks => {
                let g = rs.gleb_tasks.as_ref().ok_or_else(|| {
                    ApiError(format!(
                        "{}: для tracker gleb_tasks нужен блок gleb_tasks: {{ url, api_key_env, workspace }}",
                        path.display()
                    ))
                })?;
                let token = std::env::var(&g.api_key_env).map_err(|_| {
                    ApiError(format!(
                        "переменная окружения {} не задана (API-ключ workspace gleb_tasks)",
                        g.api_key_env
                    ))
                })?;
                let r = gleb_tasks::GlebTasksApi::resolve(&g.url, &token, &g.workspace)?;
                let find = |id: &Option<String>, username: &Option<String>, what: &str| -> Result<String> {
                    if let Some(id) = id {
                        return Ok(id.clone());
                    }
                    if let Some(u) = username {
                        let u = u.trim().trim_start_matches('@');
                        return r
                            .members
                            .iter()
                            .find(|(_, un, _)| un.eq_ignore_ascii_case(u))
                            .map(|(id, _, _)| id.clone())
                            .ok_or_else(|| {
                                ApiError(format!("в workspace {} нет участника @{u} ({what})", g.workspace))
                            });
                    }
                    Err(ApiError(format!("{}: для roles.{what} укажи id или username", path.display())))
                };
                let operator_gid = find(&rs.roles.operator.id, &rs.roles.operator.username, "operator")?;
                // Агент по умолчанию — «я» (владелец API-ключа и есть Claude Code).
                let agent_gid = if rs.roles.agent.id.is_some() || rs.roles.agent.username.is_some() {
                    find(&rs.roles.agent.id, &rs.roles.agent.username, "agent")?
                } else {
                    r.me_id.clone()
                };
                let agent_name = rs.roles.agent.name.clone().unwrap_or_else(|| {
                    r.members
                        .iter()
                        .find(|(id, _, _)| *id == agent_gid)
                        .map(|(_, _, n)| n.clone())
                        .unwrap_or_else(|| r.me_name.clone())
                });
                Ok(common(
                    token,
                    g.url.trim_end_matches('/').to_string(),
                    r.workspace_id,
                    g.workspace.to_uppercase(),
                    operator_gid,
                    agent_gid,
                    agent_name,
                ))
            }
        }
    }

    /// Конфиг «текущего» проекта (для CLI): AUTOPILOT_DIR либо поиск вверх от cwd.
    pub fn load() -> Result<Settings> {
        Self::load_from(&Self::find_dir()?)
    }

    /// Роль по имени секции (обратный маппинг), регистронезависимо.
    pub fn role_of_section_name(&self, name: &str) -> Option<&str> {
        let n = name.trim().to_lowercase();
        self.sections
            .iter()
            .find(|(_, names)| names.contains(&n))
            .map(|(role, _)| role.as_str())
    }

    /// Допустимые имена секции для роли (для резолва gid при перемещении).
    fn section_names(&self, role: &str) -> Option<&[String]> {
        self.sections
            .iter()
            .find(|(r, _)| r == role)
            .map(|(_, names)| names.as_slice())
    }

    pub fn is_approved(&self, role: &str) -> bool {
        self.approved.iter().any(|r| r == role)
    }

    pub fn is_russian(&self) -> bool {
        self.comment_language.eq_ignore_ascii_case("ru")
    }

    /// Комментарий при задаче с признаками ручных действий.
    pub fn msg_manual(&self) -> &'static str {
        if self.is_russian() {
            "Автопилот: в описании упомянуты ручные действия (доступы/оплата/аккаунт/устройства), \
             перевожу в blocked до вмешательства оператора."
        } else {
            "Autopilot: the description mentions manual actions (access/payment/account/devices); \
             moving to blocked until the operator steps in."
        }
    }

    /// Устойчивая подстрока нашего собственного «ручного» комментария — для
    /// детекции того, что человек уже разобрался с блокировкой (см. was_manually_cleared).
    pub fn manual_marker(&self) -> &'static str {
        if self.is_russian() {
            "упомянуты ручные действия"
        } else {
            "mentions manual actions"
        }
    }

    /// Комментарий §2.1: задача в известной, но неразрешённой секции.
    pub fn msg_not_approved(&self, section: &str) -> String {
        if self.is_russian() {
            format!("Не беру задачи из секции {section}, переместите её в одну из рабочих секций.",)
        } else {
            format!("I do not take tasks from the {section} section, please move it to a working section.",)
        }
    }

    /// Комментарий при несоответствии секций процессу (секцию не сопоставить с ролью).
    pub fn msg_mismatch(&self) -> String {
        if let Some(c) = &self.mismatch_comment {
            return c.clone();
        }
        if self.is_russian() {
            "Секции в текущем проекте не соответствуют процессу, задача не может быть выполнена.".into()
        } else {
            "The sections in this project do not match the process; the task cannot be handled.".into()
        }
    }
}

// ============================ Модели ============================
// Общие для обоих трекеров. Asana десериализуется в них напрямую (serde),
// gleb_tasks заполняет их руками из GraphQL-ответа.

#[derive(Debug, Deserialize, Clone, Default)]
pub struct User {
    pub gid: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Ref {
    pub gid: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Membership {
    pub project: Option<Ref>,
    pub section: Option<Ref>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Task {
    /// Идентификатор в трекере: gid Asana либо uuid gleb_tasks.
    pub gid: String,
    /// Человеческий идентификатор (gleb_tasks: `SAOBR-12`); у Asana нет.
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub completed: bool,
    pub assignee: Option<User>,
    pub due_on: Option<String>,
    pub permalink_url: Option<String>,
    /// Родитель, если задача — сабтаска (тогда стандалон её не обрабатываем).
    pub parent: Option<Ref>,
    #[serde(default)]
    pub memberships: Vec<Membership>,
}

impl Task {
    pub fn assignee_gid(&self) -> Option<&str> {
        self.assignee.as_ref().map(|a| a.gid.as_str())
    }
    /// true, если это сабтаска (обрабатывается через контекст родителя, не сама по себе).
    pub fn is_subtask(&self) -> bool {
        self.parent.is_some()
    }
    /// Что показывать человеку и передавать в CLI: ключ (SAOBR-12) либо gid.
    pub fn display_id(&self) -> &str {
        self.key.as_deref().unwrap_or(&self.gid)
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Attachment {
    pub gid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: u64,
    /// gleb_tasks: подписанная ссылка на скачивание (15 минут). Asana: нет.
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Story {
    #[serde(default)]
    pub resource_subtype: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub created_at: String,
    pub created_by: Option<User>,
}

impl Story {
    pub fn is_comment(&self) -> bool {
        self.resource_subtype == "comment_added"
    }
    pub fn author(&self) -> &str {
        self.created_by
            .as_ref()
            .map(|u| u.name.as_str())
            .filter(|n| !n.is_empty())
            .unwrap_or("Unknown")
    }
    /// Дата в коротком виде: 2026-07-21 22:54
    pub fn short_date(&self) -> String {
        let s = self.created_at.replace('T', " ");
        s.chars().take(16).collect()
    }
}

/// Куда автопилот отнёс задачу текущего проекта (проект + роль/секция).
#[derive(Debug, Clone)]
pub struct Placement {
    pub project_gid: String,
    pub project_name: String,
    /// Имя секции как в трекере.
    pub section_name: String,
    /// Роль секции (todo/in_progress/...), если сопоставилась.
    pub role: Option<String>,
}

// ============================ Клиент ============================

/// Сырой API выбранного трекера.
enum Backend {
    Asana(asana::AsanaApi),
    GlebTasks(gleb_tasks::GlebTasksApi),
}

/// Клиент трекера: сырые операции делегирует бэкенду, логика процесса общая.
pub struct Client {
    backend: Backend,
    pub settings: Settings,
    /// project_gid -> список секций проекта (кэш для резолва имени в gid).
    section_cache: Mutex<HashMap<String, Vec<Ref>>>,
}

impl Client {
    pub fn new(settings: Settings) -> Self {
        let backend = match settings.tracker {
            Tracker::Asana => Backend::Asana(asana::AsanaApi::new(
                settings.token.clone(),
                settings.workspace.clone(),
                settings.agent_gid.clone(),
            )),
            Tracker::GlebTasks => Backend::GlebTasks(gleb_tasks::GlebTasksApi::new(
                settings.base_url.clone(),
                settings.token.clone(),
                settings.workspace.clone(),
                settings.agent_gid.clone(),
            )),
        };
        Client {
            backend,
            settings,
            section_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Загружает конфиг текущего проекта (см. Settings::load) и создаёт клиент.
    pub fn load() -> Result<Self> {
        Ok(Self::new(Settings::load()?))
    }

    pub fn tracker(&self) -> Tracker {
        self.settings.tracker
    }

    // ---- Сырые операции (делегирование) ----

    pub fn get_task(&self, id: &str) -> Result<Task> {
        match &self.backend {
            Backend::Asana(a) => a.get_task(id),
            Backend::GlebTasks(g) => g.get_task(id),
        }
    }

    /// Все НЕзакрытые задачи, назначенные на аккаунт-агента, во всём workspace.
    /// Это ядро проект-независимого выбора: агент видит только явно назначенное
    /// на него — назначение и есть фильтр «за какие проекты он отвечает».
    pub fn get_assigned_tasks(&self) -> Result<Vec<Task>> {
        match &self.backend {
            Backend::Asana(a) => a.get_assigned_tasks(),
            Backend::GlebTasks(g) => g.get_assigned_tasks(),
        }
    }

    pub fn get_subtasks(&self, id: &str) -> Result<Vec<Task>> {
        match &self.backend {
            Backend::Asana(a) => a.get_subtasks(id),
            Backend::GlebTasks(g) => g.get_subtasks(id),
        }
    }

    pub fn get_stories(&self, id: &str) -> Result<Vec<Story>> {
        match &self.backend {
            Backend::Asana(a) => a.get_stories(id),
            Backend::GlebTasks(g) => g.get_stories(id),
        }
    }

    pub fn add_comment(&self, id: &str, text: &str) -> Result<()> {
        match &self.backend {
            Backend::Asana(a) => a.add_comment(id, text),
            Backend::GlebTasks(g) => g.add_comment(id, text),
        }
    }

    /// Перенос в конкретную секцию проекта (gid/id секции уже известен).
    pub fn move_to_section(&self, task_id: &str, project_id: &str, section_id: &str) -> Result<()> {
        match &self.backend {
            Backend::Asana(a) => a.move_to_section(task_id, section_id),
            Backend::GlebTasks(g) => g.move_to_section(task_id, project_id, section_id),
        }
    }

    /// assignee: id пользователя или None (снять).
    pub fn set_assignee(&self, task_id: &str, assignee: Option<&str>) -> Result<()> {
        match &self.backend {
            Backend::Asana(a) => a.set_assignee(task_id, assignee),
            Backend::GlebTasks(g) => g.set_assignee(task_id, assignee),
        }
    }

    pub fn set_completed(&self, task_id: &str, completed: bool) -> Result<()> {
        match &self.backend {
            Backend::Asana(a) => a.set_completed(task_id, completed),
            Backend::GlebTasks(g) => g.set_completed(task_id, completed),
        }
    }

    pub fn get_attachments(&self, task_id: &str) -> Result<Vec<Attachment>> {
        match &self.backend {
            Backend::Asana(a) => a.get_attachments(task_id),
            Backend::GlebTasks(g) => g.get_attachments(task_id),
        }
    }

    /// Скачивает вложение в файл, возвращает путь. Asana: gid вложения;
    /// gleb_tasks: `<задача>/<id вложения>`.
    pub fn download_attachment(&self, attachment_id: &str, dest_dir: &Path) -> Result<PathBuf> {
        match &self.backend {
            Backend::Asana(a) => a.download_attachment(attachment_id, dest_dir),
            Backend::GlebTasks(g) => g.download_attachment(attachment_id, dest_dir),
        }
    }

    /// Создать сабтаску; возвращает её идентификатор (gid Asana / ключ gleb_tasks).
    pub fn create_subtask(&self, parent: &str, name: &str, notes: &str, assignee: Option<&str>) -> Result<String> {
        match &self.backend {
            Backend::Asana(a) => a.create_subtask(parent, name, notes, assignee),
            Backend::GlebTasks(g) => g.create_subtask(parent, name, notes, assignee),
        }
    }

    /// Как в контексте печатать идентификатор вложения, чтобы его можно было
    /// скормить `download` как есть.
    fn attachment_handle(&self, task: &Task, a: &Attachment) -> String {
        match self.settings.tracker {
            Tracker::Asana => a.gid.clone(),
            Tracker::GlebTasks => format!("{}/{}", task.display_id(), a.gid),
        }
    }

    // ---- Секции проекта (резолв роли -> gid по имени) ----

    /// Список секций проекта (кэшируется на время жизни клиента).
    pub fn project_sections(&self, project_gid: &str) -> Result<Vec<Ref>> {
        if let Some(s) = self.section_cache.lock().unwrap().get(project_gid) {
            return Ok(s.clone());
        }
        let secs = match &self.backend {
            Backend::Asana(a) => a.project_sections(project_gid)?,
            Backend::GlebTasks(g) => g.project_sections(project_gid)?,
        };
        self.section_cache
            .lock()
            .unwrap()
            .insert(project_gid.to_string(), secs.clone());
        Ok(secs)
    }

    /// gid секции в конкретном проекте по роли (todo/in_progress/...).
    /// Матч по имени секции из конфига; ошибка, если проект не содержит такой секции.
    pub fn resolve_section_gid(&self, project_gid: &str, role: &str) -> Result<String> {
        let names = self.settings.section_names(role).ok_or_else(|| {
            ApiError(format!("роль '{role}' не описана в autopilot.yaml (sections)"))
        })?;
        let secs = self.project_sections(project_gid)?;
        for want in names {
            for s in &secs {
                if s.name.trim().to_lowercase() == *want {
                    return Ok(s.gid.clone());
                }
            }
        }
        Err(ApiError(format!(
            "в проекте {project_gid} нет секции для роли '{role}' (искал имена: {names:?})"
        )))
    }

    /// Переместить задачу в секцию по роли, резолвя её в проекте задачи.
    pub fn move_task_to_role(&self, task_gid: &str, project_gid: &str, role: &str) -> Result<()> {
        let gid = self.resolve_section_gid(project_gid, role)?;
        self.move_to_section(task_gid, project_gid, &gid)
    }

    /// Определяет «управляющее» размещение задачи: membership, чья секция
    /// сопоставилась с ролью (если такой нет — первый membership с проектом).
    pub fn placement(&self, task: &Task) -> Option<Placement> {
        let mut fallback: Option<Placement> = None;
        for m in &task.memberships {
            let Some(project) = &m.project else { continue };
            let section = m.section.clone().unwrap_or_default();
            let role = self.settings.role_of_section_name(&section.name);
            let p = Placement {
                project_gid: project.gid.clone(),
                project_name: project.name.clone(),
                section_name: section.name.clone(),
                role: role.map(|r| r.to_string()),
            };
            if p.role.is_some() {
                return Some(p);
            }
            if fallback.is_none() {
                fallback = Some(p);
            }
        }
        fallback
    }

    /// Управляющее размещение, предпочитая approved-роль (для задач, лежащих
    /// одновременно в approved и не-approved секциях разных проектов).
    pub fn approved_placement(&self, task: &Task) -> Option<Placement> {
        for m in &task.memberships {
            let Some(project) = &m.project else { continue };
            let Some(section) = &m.section else { continue };
            if let Some(role) = self.settings.role_of_section_name(&section.name) {
                if self.settings.is_approved(role) {
                    return Some(Placement {
                        project_gid: project.gid.clone(),
                        project_name: project.name.clone(),
                        section_name: section.name.clone(),
                        role: Some(role.to_string()),
                    });
                }
            }
        }
        None
    }

    /// Короткая метка секции задачи для отображения (роль либо сырое имя).
    pub fn task_section_label(&self, task: &Task) -> String {
        match self.placement(task) {
            Some(p) => p.role.unwrap_or_else(|| {
                if p.section_name.is_empty() {
                    "?".into()
                } else {
                    p.section_name
                }
            }),
            None => "?".into(),
        }
    }

    /// Идентификаторы задач, на которые ссылается текст: ссылки Asana либо
    /// (для gleb_tasks) ключи `SAOBR-12` и ссылки `<url>/t/SAOBR-12`.
    pub fn extract_linked_ids(&self, text: &str) -> Vec<String> {
        match self.settings.tracker {
            Tracker::Asana => extract_linked_gids(text),
            Tracker::GlebTasks => extract_task_keys(text, &self.settings.workspace_key),
        }
    }

    /// Собирает полный markdown-контекст по задаче: описание, сабтаски,
    /// комментарии, история перемещений, связанные задачи.
    pub fn build_task_context(&self, task: &Task, section_name: &str) -> Result<String> {
        let subtasks = self.get_subtasks(&task.gid)?;
        let stories = self.get_stories(&task.gid)?;
        let agent_gid = self.settings.agent_gid.as_str();
        let agent_name = self.settings.agent_name.as_str();
        let id_label = match self.settings.tracker {
            Tracker::Asana => "GID",
            Tracker::GlebTasks => "Ключ",
        };

        let mut out = String::new();
        out.push_str(&format!(
            "# Текущая задача\n**Название:** {}\n**{id_label}:** {}\n**Ссылка:** {}\n**Секция:** {}\n",
            task.name,
            task.display_id(),
            task.permalink_url.as_deref().unwrap_or("-"),
            section_name
        ));
        if section_name == "reopen" {
            out.push_str(
                "\n⚠️ **ЗАДАЧА ПЕРЕОТКРЫТА.** Причину ищи в НЕЗАКРЫТЫХ САБТАСКАХ и в комментариях, \
                 оставленных ПОСЛЕ последнего перемещения в reopen (см. историю ниже). \
                 Не выдумывай причину из связанных задач.\n",
            );
        }

        out.push_str("\n## Описание\n");
        out.push_str(if task.notes.is_empty() { "Нет описания" } else { &task.notes });
        out.push('\n');

        // --- Сабтаски ---
        out.push_str("\n## Сабтаски\n");
        if subtasks.is_empty() {
            out.push_str("Нет сабтасков.\n");
        } else {
            let mut open_for_agent = 0;
            for st in &subtasks {
                let mark = if st.completed { "x" } else { " " };
                let assignee = st
                    .assignee
                    .as_ref()
                    .map(|a| if a.gid == agent_gid { agent_name } else { a.name.as_str() })
                    .unwrap_or("никто");
                let mut flag = String::new();
                if !st.completed && st.assignee_gid() == Some(agent_gid) {
                    open_for_agent += 1;
                    flag = " ⚠️ ОТКРЫТА И НАЗНАЧЕНА НА ТЕБЯ — ОБЯЗАТЕЛЬНА К ВЫПОЛНЕНИЮ".to_string();
                }
                out.push_str(&format!(
                    "- [{mark}] {} — «{}» (исполнитель: {assignee}){flag}\n",
                    st.display_id(),
                    st.name
                ));
                if !st.notes.is_empty() {
                    for line in st.notes.lines() {
                        out.push_str(&format!("    {line}\n"));
                    }
                }
            }
            if open_for_agent > 0 {
                out.push_str(&format!(
                    "\n**ВНИМАНИЕ: {open_for_agent} незакрытых сабтасков назначено на тебя. \
                     Задача НЕ считается выполненной, пока они не сделаны и не закрыты \
                     (`asana complete <id>`).**\n"
                ));
            }
        }

        // --- Вложения ---
        let attachments = self.get_attachments(&task.gid).unwrap_or_default();
        if !attachments.is_empty() {
            out.push_str("\n## Вложения\n");
            for a in &attachments {
                out.push_str(&format!("- {} — {} ({} байт)\n", self.attachment_handle(task, a), a.name, a.size));
            }
            match self.settings.tracker {
                Tracker::Asana => out.push_str(
                    "Скачать: `asana download <gid> [dir]` — затем открой файл инструментом Read \
                     (картинки Claude Code видит). Ссылки вида `get_asset?asset_id=NNN` в тексте: NNN — это тот же gid, \
                     качается той же командой.\n",
                ),
                Tracker::GlebTasks => out.push_str(
                    "Скачать: `asana download <задача>/<id> [dir]` (идентификатор — как напечатан выше) — \
                     затем открой файл инструментом Read (картинки Claude Code видит).\n",
                ),
            }
        }

        // --- Комментарии ---
        out.push_str("\n## Комментарии (в хронологическом порядке)\n");
        let comments: Vec<&Story> = stories.iter().filter(|s| s.is_comment()).collect();
        if comments.is_empty() {
            out.push_str("Комментариев нет.\n");
        } else {
            for c in &comments {
                out.push_str(&format!("**{} ({})**: {}\n\n", c.author(), c.short_date(), c.text));
            }
        }

        // --- Системная история ---
        out.push_str("\n## История (системные события трекера, НЕ комментарии)\n");
        for s in stories.iter().filter(|s| !s.is_comment()) {
            if !s.text.is_empty() {
                out.push_str(&format!("- {} {}: {}\n", s.short_date(), s.author(), s.text));
            }
        }

        // --- Связанные задачи ---
        let mut full_text = task.notes.clone();
        for c in &comments {
            full_text.push('\n');
            full_text.push_str(&c.text);
        }
        let own: Vec<&str> = std::iter::once(task.display_id())
            .chain(std::iter::once(task.gid.as_str()))
            .chain(subtasks.iter().map(|s| s.display_id()))
            .chain(subtasks.iter().map(|s| s.gid.as_str()))
            .collect();
        let linked: Vec<String> = self
            .extract_linked_ids(&full_text)
            .into_iter()
            .filter(|g| !own.contains(&g.as_str()))
            .collect();
        if !linked.is_empty() {
            out.push_str(
                "\n## Связанные задачи (ТОЛЬКО справочный контекст — это НЕ твоя задача и НЕ причина переоткрытия)\n",
            );
            for id in linked {
                if let Ok(lt) = self.get_task(&id) {
                    out.push_str(&format!("### {} ({id_label}: {})\n{}\n\n", lt.name, lt.display_id(), lt.notes));
                }
            }
        }

        Ok(out)
    }
}

/// Извлекает GID задач из ссылок Asana в тексте (старый /0/… и новый /1/…/task/… форматы).
pub fn extract_linked_gids(text: &str) -> Vec<String> {
    let re = regex::Regex::new(r"app\.asana\.com/(?:0/\d+/(\d+)|1/\d+/(?:project/\d+/)?task/(\d+))").unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(text) {
        let gid = cap.get(1).or_else(|| cap.get(2)).map(|m| m.as_str().to_string());
        if let Some(g) = gid {
            if !out.contains(&g) {
                out.push(g);
            }
        }
    }
    out
}

/// Ключи задач workspace (`SAOBR-12`) в тексте — голые и внутри ссылок `/t/SAOBR-12`.
pub fn extract_task_keys(text: &str, workspace_key: &str) -> Vec<String> {
    let re = regex::Regex::new(&format!(r"\b({}-\d+)\b", regex::escape(workspace_key))).unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(text) {
        let k = cap[1].to_string();
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out
}
