//! Сырой HTTP-клиент Asana: только запросы к REST API и разбор ответов в
//! общие модели (`Task`, `Story`, `Attachment`, `Ref`). Логика процесса
//! (роли, секции, контекст) живёт в `Client` (lib.rs) и не знает про Asana.

use crate::{ApiError, Attachment, Ref, Result, Story, Task};
use serde_json::{json, Value};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const BASE_URL: &str = "https://app.asana.com/api/1.0";

pub struct AsanaApi {
    agent: ureq::Agent,
    token: String,
    workspace: String,
    agent_gid: String,
}

impl AsanaApi {
    pub fn new(token: String, workspace: String, agent_gid: String) -> Self {
        AsanaApi {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(60))
                .build(),
            token,
            workspace,
            agent_gid,
        }
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
                .set("Authorization", &format!("Bearer {}", self.token))
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
            "{BASE_URL}/tasks/{gid}?opt_fields=name,notes,completed,assignee.gid,assignee.name,due_on,permalink_url,parent.gid,parent.name,memberships.project.gid,memberships.project.name,memberships.section.gid,memberships.section.name"
        ))?;
        serde_json::from_value(v).map_err(|e| ApiError(format!("parse task {gid}: {e}")))
    }

    /// Все НЕзакрытые задачи, назначенные на аккаунт-агента, во всём workspace.
    pub fn get_assigned_tasks(&self) -> Result<Vec<Task>> {
        let mut out: Vec<Task> = Vec::new();
        let mut url = format!(
            "{BASE_URL}/tasks?assignee={}&workspace={}&completed_since=now&limit=100&opt_fields=name,notes,completed,assignee.gid,due_on,permalink_url,parent.name,memberships.project.gid,memberships.project.name,memberships.section.gid,memberships.section.name",
            self.agent_gid, self.workspace
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
    pub fn download_attachment(&self, attachment_gid: &str, dest_dir: &Path) -> Result<PathBuf> {
        let (name, url) = self.attachment_download_url(attachment_gid)?;
        download_signed(&self.agent, &url, dest_dir, &format!("{attachment_gid}-{name}"))
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

    pub fn project_sections(&self, project_gid: &str) -> Result<Vec<Ref>> {
        let v = self.get_data(&format!(
            "{BASE_URL}/projects/{project_gid}/sections?opt_fields=name&limit=100"
        ))?;
        serde_json::from_value(v).map_err(|e| ApiError(format!("parse sections of {project_gid}: {e}")))
    }
}

/// Скачать по подписанной ссылке (S3/GCS): запрос БЕЗ Authorization-заголовка,
/// иначе конфликт подписей. Возвращает путь к записанному файлу.
pub fn download_signed(agent: &ureq::Agent, url: &str, dest_dir: &Path, file_name: &str) -> Result<PathBuf> {
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| ApiError(format!("download failed: {e}")))?;
    let mut bytes: Vec<u8> = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| ApiError(format!("download read failed: {e}")))?;
    let path = dest_dir.join(file_name);
    std::fs::write(&path, &bytes).map_err(|e| ApiError(format!("write {}: {e}", path.display())))?;
    Ok(path)
}
