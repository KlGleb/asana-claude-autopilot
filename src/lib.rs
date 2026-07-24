//! Общий Asana-клиент и конфиг для автопилота и CLI-утилиты `asana`.
//!
//! Проект-независимый: вся идентичность (workspace, роли операторов, маппинг
//! секций) читается из `autopilot.yaml` в каталоге `autopilot/` проекта.
//! Автопилот работает по всему workspace — берёт задачи, назначенные на
//! аккаунт-агента, в любом проекте, а секции резолвит по имени внутри
//! проекта каждой задачи.

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub mod daemon;
pub mod initproj;
pub mod registry;
pub mod runner;
pub mod telegram;

pub const BASE_URL: &str = "https://app.asana.com/api/1.0";

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

/// Сырой разбор `autopilot.yaml`.
#[derive(Debug, Deserialize)]
struct RawSettings {
    asana: RawAsana,
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
struct RawRoles {
    operator: RawOperator,
    agent: RawAgent,
}

#[derive(Debug, Deserialize)]
struct RawOperator {
    gid: String,
    #[serde(default = "default_operator_label")]
    label: String,
}

#[derive(Debug, Deserialize)]
struct RawAgent {
    gid: String,
    /// Отображаемое имя аккаунта-агента в Asana (для детекции авторства).
    name: String,
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
#[derive(Debug, Clone)]
pub struct Settings {
    pub token: String,
    pub workspace: String,
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

    /// Загружает конфиг из `<dir>/autopilot.yaml`.
    pub fn load_from(dir: &Path) -> Result<Settings> {
        let path = dir.join("autopilot.yaml");
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| ApiError(format!("не могу прочитать {}: {e}", path.display())))?;
        let rs: RawSettings = serde_yaml::from_str(&raw)
            .map_err(|e| ApiError(format!("ошибка разбора {}: {e}", path.display())))?;

        let token = std::env::var(&rs.asana.token_env).map_err(|_| {
            ApiError(format!(
                "переменная окружения {} не задана (Asana-токен)",
                rs.asana.token_env
            ))
        })?;

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

        Ok(Settings {
            token,
            workspace: rs.asana.workspace,
            operator_gid: rs.roles.operator.gid,
            operator_label: rs.roles.operator.label,
            agent_gid: rs.roles.agent.gid,
            agent_name: rs.roles.agent.name,
            agent_label: rs.roles.agent.label,
            sections,
            approved: rs.approved,
            manual_keywords: rs
                .heuristics
                .manual_keywords
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
            comment_language: rs.heuristics.comment_language,
            prompt_file: rs.prompt_file,
            mismatch_comment: rs.mismatch_comment,
        })
    }

    /// Конфиг «текущего» проекта (для CLI `asana`): AUTOPILOT_DIR либо поиск вверх от cwd.
    pub fn load() -> Result<Settings> {
        Self::load_from(&Self::find_dir()?)
    }

    /// Роль по имени секции (обратный маппинг), регистронезависимо.
    pub fn role_of_section_name(&self, name: &str) -> Option<&str> {
        let n = name.trim().to_lowercase();
        self.sections
            .iter()
            .find(|(_, names)| names.iter().any(|w| *w == n))
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
            format!(
                "Не беру задачи из секции {section}, переместите её в одну из рабочих секций.",
            )
        } else {
            format!(
                "I do not take tasks from the {section} section, please move it to a working section.",
            )
        }
    }

    /// Комментарий при несоответствии секций процессу (секцию не сопоставить с ролью).
    pub fn msg_mismatch(&self) -> String {
        if let Some(c) = &self.mismatch_comment {
            return c.clone();
        }
        if self.is_russian() {
            "Секции в текущем проекте не соответствуют процессу, задача не может быть выполнена."
                .into()
        } else {
            "The sections in this project do not match the process; the task cannot be handled."
                .into()
        }
    }
}

// ============================ Модели ============================

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
    pub gid: String,
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
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Attachment {
    pub gid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: u64,
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
    /// Имя секции как в Asana.
    pub section_name: String,
    /// Роль секции (todo/in_progress/...), если сопоставилась.
    pub role: Option<String>,
}

// ============================ Клиент ============================

pub struct Client {
    agent: ureq::Agent,
    pub settings: Settings,
    /// project_gid -> список секций проекта (кэш для резолва имени в gid).
    section_cache: Mutex<HashMap<String, Vec<Ref>>>,
}

impl Client {
    pub fn new(settings: Settings) -> Self {
        Client {
            settings,
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(60))
                .build(),
            section_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Загружает конфиг текущего проекта (см. Settings::load) и создаёт клиент.
    pub fn load() -> Result<Self> {
        Ok(Self::new(Settings::load()?))
    }

    /// Один HTTP-запрос с 3 повторами на сетевые ошибки/429/5xx.
    pub fn request(&self, method: &str, url: &str, body: Option<Value>) -> Result<Value> {
        let mut last_err = String::new();
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(5 * attempt as u64));
            }
            let req = self
                .agent
                .request(method, url)
                .set("Authorization", &format!("Bearer {}", self.settings.token))
                .set("Accept", "application/json");
            let resp = match &body {
                Some(b) => req.send_json(b.clone()),
                None => req.call(),
            };
            match resp {
                Ok(r) => {
                    return r
                        .into_json::<Value>()
                        .map_err(|e| ApiError(format!("bad json from {url}: {e}")));
                }
                Err(ureq::Error::Status(code, r)) => {
                    let text = r.into_string().unwrap_or_default();
                    last_err = format!("HTTP {code} from {url}: {text}");
                    if code != 429 && code < 500 {
                        return Err(ApiError(last_err));
                    }
                }
                Err(e) => {
                    last_err = format!("network error for {url}: {e}");
                }
            }
        }
        Err(ApiError(last_err))
    }

    pub fn get_data(&self, url: &str) -> Result<Value> {
        Ok(self.request("GET", url, None)?["data"].take())
    }

    // ---- Задачи ----

    pub fn get_task(&self, gid: &str) -> Result<Task> {
        let v = self.get_data(&format!(
            "{BASE_URL}/tasks/{gid}?opt_fields=name,notes,completed,assignee.gid,assignee.name,due_on,permalink_url,memberships.project.gid,memberships.project.name,memberships.section.gid,memberships.section.name"
        ))?;
        serde_json::from_value(v).map_err(|e| ApiError(format!("parse task {gid}: {e}")))
    }

    /// Все НЕзакрытые задачи, назначенные на аккаунт-агента, во всём workspace.
    /// Это ядро проект-независимого выбора: агент видит только явно назначенное
    /// на него — назначение и есть фильтр «за какие проекты он отвечает».
    pub fn get_assigned_tasks(&self) -> Result<Vec<Task>> {
        let mut out: Vec<Task> = Vec::new();
        let mut url = format!(
            "{BASE_URL}/tasks?assignee={}&workspace={}&completed_since=now&limit=100&opt_fields=name,notes,completed,assignee.gid,due_on,permalink_url,parent.name,memberships.project.gid,memberships.project.name,memberships.section.gid,memberships.section.name",
            self.settings.agent_gid, self.settings.workspace
        );
        loop {
            let v = self.request("GET", &url, None)?;
            let batch: Vec<Task> = serde_json::from_value(v["data"].clone())
                .map_err(|e| ApiError(format!("parse assigned tasks: {e}")))?;
            out.extend(batch);
            match v["next_page"]["uri"].as_str() {
                Some(next) => url = next.to_string(),
                None => break,
            }
        }
        Ok(out)
    }

    pub fn get_subtasks(&self, gid: &str) -> Result<Vec<Task>> {
        let v = self.get_data(&format!(
            "{BASE_URL}/tasks/{gid}/subtasks?limit=100&opt_fields=name,notes,completed,assignee.gid,assignee.name,permalink_url"
        ))?;
        serde_json::from_value(v).map_err(|e| ApiError(format!("parse subtasks of {gid}: {e}")))
    }

    pub fn get_stories(&self, gid: &str) -> Result<Vec<Story>> {
        let v = self.get_data(&format!(
            "{BASE_URL}/tasks/{gid}/stories?limit=100&opt_fields=resource_subtype,text,created_at,created_by.name"
        ))?;
        serde_json::from_value(v).map_err(|e| ApiError(format!("parse stories of {gid}: {e}")))
    }

    pub fn add_comment(&self, gid: &str, text: &str) -> Result<()> {
        self.request(
            "POST",
            &format!("{BASE_URL}/tasks/{gid}/stories"),
            Some(json!({"data": {"text": text}})),
        )?;
        Ok(())
    }

    pub fn move_to_section(&self, task_gid: &str, section_gid: &str) -> Result<()> {
        self.request(
            "POST",
            &format!("{BASE_URL}/sections/{section_gid}/addTask"),
            Some(json!({"data": {"task": task_gid}})),
        )?;
        Ok(())
    }

    /// assignee: gid пользователя или None (снять).
    pub fn set_assignee(&self, task_gid: &str, assignee: Option<&str>) -> Result<()> {
        let val = match assignee {
            Some(g) => json!(g),
            None => Value::Null,
        };
        self.request(
            "PUT",
            &format!("{BASE_URL}/tasks/{task_gid}"),
            Some(json!({"data": {"assignee": val}})),
        )?;
        Ok(())
    }

    pub fn set_completed(&self, task_gid: &str, completed: bool) -> Result<()> {
        self.request(
            "PUT",
            &format!("{BASE_URL}/tasks/{task_gid}"),
            Some(json!({"data": {"completed": completed}})),
        )?;
        Ok(())
    }

    pub fn get_attachments(&self, task_gid: &str) -> Result<Vec<Attachment>> {
        let v = self.get_data(&format!(
            "{BASE_URL}/tasks/{task_gid}/attachments?opt_fields=name,size,resource_subtype"
        ))?;
        serde_json::from_value(v).map_err(|e| ApiError(format!("parse attachments: {e}")))
    }

    /// Метаданные вложения: имя + одноразовый signed download_url (S3, качается без auth).
    pub fn attachment_download_url(&self, attachment_gid: &str) -> Result<(String, String)> {
        let v = self.get_data(&format!(
            "{BASE_URL}/attachments/{attachment_gid}?opt_fields=name,download_url"
        ))?;
        let name = v["name"].as_str().unwrap_or("attachment").to_string();
        let url = v["download_url"]
            .as_str()
            .ok_or_else(|| ApiError(format!("attachment {attachment_gid} has no download_url")))?
            .to_string();
        Ok((name, url))
    }

    /// Скачивает вложение в файл, возвращает путь.
    pub fn download_attachment(
        &self,
        attachment_gid: &str,
        dest_dir: &std::path::Path,
    ) -> Result<std::path::PathBuf> {
        let (name, url) = self.attachment_download_url(attachment_gid)?;
        // S3 presigned URL: запрос БЕЗ Authorization-заголовка, иначе конфликт подписей
        let resp = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| ApiError(format!("download failed: {e}")))?;
        let mut bytes: Vec<u8> = Vec::new();
        resp.into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| ApiError(format!("download read failed: {e}")))?;
        let path = dest_dir.join(format!("{attachment_gid}-{name}"));
        std::fs::write(&path, &bytes)
            .map_err(|e| ApiError(format!("write {}: {e}", path.display())))?;
        Ok(path)
    }

    pub fn create_subtask(
        &self,
        parent_gid: &str,
        name: &str,
        notes: &str,
        assignee: Option<&str>,
    ) -> Result<String> {
        let mut data = json!({"name": name, "notes": notes});
        if let Some(a) = assignee {
            data["assignee"] = json!(a);
        }
        let v = self.request(
            "POST",
            &format!("{BASE_URL}/tasks/{parent_gid}/subtasks"),
            Some(json!({"data": data})),
        )?;
        Ok(v["data"]["gid"].as_str().unwrap_or_default().to_string())
    }

    // ---- Секции проекта (резолв роли -> gid по имени) ----

    /// Список секций проекта (кэшируется на время жизни клиента).
    pub fn project_sections(&self, project_gid: &str) -> Result<Vec<Ref>> {
        if let Some(s) = self.section_cache.lock().unwrap().get(project_gid) {
            return Ok(s.clone());
        }
        let v = self.get_data(&format!(
            "{BASE_URL}/projects/{project_gid}/sections?opt_fields=name&limit=100"
        ))?;
        let secs: Vec<Ref> = serde_json::from_value(v)
            .map_err(|e| ApiError(format!("parse sections of {project_gid}: {e}")))?;
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
        self.move_to_section(task_gid, &gid)
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

    /// Собирает полный markdown-контекст по задаче: описание, сабтаски,
    /// комментарии, история перемещений, связанные задачи.
    pub fn build_task_context(&self, task: &Task, section_name: &str) -> Result<String> {
        let subtasks = self.get_subtasks(&task.gid)?;
        let stories = self.get_stories(&task.gid)?;
        let agent_gid = self.settings.agent_gid.as_str();
        let agent_name = self.settings.agent_name.as_str();

        let mut out = String::new();
        out.push_str(&format!(
            "# Текущая задача\n**Название:** {}\n**GID:** {}\n**Ссылка:** {}\n**Секция:** {}\n",
            task.name,
            task.gid,
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
        out.push_str(if task.notes.is_empty() {
            "Нет описания"
        } else {
            &task.notes
        });
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
                    .map(|a| {
                        if a.gid == agent_gid {
                            agent_name
                        } else {
                            a.name.as_str()
                        }
                    })
                    .unwrap_or("никто");
                let mut flag = String::new();
                if !st.completed && st.assignee_gid() == Some(agent_gid) {
                    open_for_agent += 1;
                    flag = " ⚠️ ОТКРЫТА И НАЗНАЧЕНА НА ТЕБЯ — ОБЯЗАТЕЛЬНА К ВЫПОЛНЕНИЮ".to_string();
                }
                out.push_str(&format!(
                    "- [{mark}] {} — «{}» (исполнитель: {assignee}){flag}\n",
                    st.gid, st.name
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
                     (`asana complete <gid>`).**\n"
                ));
            }
        }

        // --- Вложения ---
        let attachments = self.get_attachments(&task.gid).unwrap_or_default();
        if !attachments.is_empty() {
            out.push_str("\n## Вложения\n");
            for a in &attachments {
                out.push_str(&format!("- {} — {} ({} байт)\n", a.gid, a.name, a.size));
            }
            out.push_str(
                "Скачать: `asana download <gid> [dir]` — затем открой файл инструментом Read \
                 (картинки Claude Code видит). Ссылки вида `get_asset?asset_id=NNN` в тексте: NNN — это тот же gid, \
                 качается той же командой.\n",
            );
        }

        // --- Комментарии ---
        out.push_str("\n## Комментарии (в хронологическом порядке)\n");
        let comments: Vec<&Story> = stories.iter().filter(|s| s.is_comment()).collect();
        if comments.is_empty() {
            out.push_str("Комментариев нет.\n");
        } else {
            for c in &comments {
                out.push_str(&format!(
                    "**{} ({})**: {}\n\n",
                    c.author(),
                    c.short_date(),
                    c.text
                ));
            }
        }

        // --- Системная история ---
        out.push_str("\n## История (системные события Asana, НЕ комментарии)\n");
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
        let subtask_gids: Vec<&str> = subtasks.iter().map(|s| s.gid.as_str()).collect();
        let linked: Vec<String> = extract_linked_gids(&full_text)
            .into_iter()
            .filter(|g| g != &task.gid && !subtask_gids.contains(&g.as_str()))
            .collect();
        if !linked.is_empty() {
            out.push_str(
                "\n## Связанные задачи (ТОЛЬКО справочный контекст — это НЕ твоя задача и НЕ причина переоткрытия)\n",
            );
            for gid in linked {
                if let Ok(lt) = self.get_task(&gid) {
                    out.push_str(&format!("### {} (GID: {})\n{}\n\n", lt.name, gid, lt.notes));
                }
            }
        }

        Ok(out)
    }
}

/// Извлекает GID задач из ссылок Asana в тексте (старый /0/… и новый /1/…/task/… форматы).
pub fn extract_linked_gids(text: &str) -> Vec<String> {
    let re = regex::Regex::new(
        r"app\.asana\.com/(?:0/\d+/(\d+)|1/\d+/(?:project/\d+/)?task/(\d+))",
    )
    .unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(text) {
        let gid = cap
            .get(1)
            .or_else(|| cap.get(2))
            .map(|m| m.as_str().to_string());
        if let Some(g) = gid {
            if !out.contains(&g) {
                out.push(g);
            }
        }
    }
    out
}
