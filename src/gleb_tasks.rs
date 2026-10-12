//! Бэкенд gleb_tasks (https://tasks.gleb.at): GraphQL API по ключу workspace.
//! Отдаёт те же общие модели, что и Asana-клиент; идентификатор задачи —
//! uuid, «человеческий» id — ключ вида `SAOBR-12` (принимается везде, где
//! ожидается id). Вложения адресуются как `<задача>/<uuid вложения>`.

use crate::{ApiError, Attachment, FieldValue, Membership, Ref, Result, Story, Task, User};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Поля задачи, общие для всех запросов.
const TASK_FIELDS: &str = "id key title description completed dueDate \
    assignee { id displayName username } parent { id key } \
    projects { project { id name } section { id name } } \
    customFields { field { id name kind options { id name } assignees { id } } value }";

pub struct GlebTasksApi {
    agent: ureq::Agent,
    url: String,
    api_key: String,
    /// id workspace (резолвится при загрузке конфига).
    workspace_id: String,
    agent_id: String,
}

/// Что нужно знать о workspace, чтобы заполнить Settings.
pub struct Resolved {
    pub workspace_id: String,
    pub me_id: String,
    pub me_name: String,
    /// (id, username, displayName) всех участников.
    pub members: Vec<(String, String, String)>,
}

/// Ключ задачи вида SAOBR-12?
pub fn looks_like_key(s: &str) -> bool {
    let Some((prefix, num)) = s.split_once('-') else { return false };
    prefix.len() >= 2
        && prefix.len() <= 10
        && prefix.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && prefix.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && !num.is_empty()
        && num.chars().all(|c| c.is_ascii_digit())
}

impl GlebTasksApi {
    pub fn new(url: String, api_key: String, workspace_id: String, agent_id: String) -> Self {
        GlebTasksApi {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(60))
                .build(),
            url: url.trim_end_matches('/').to_string(),
            api_key,
            workspace_id,
            agent_id,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.url
    }

    /// Один GraphQL-запрос с 3 повторами на сетевые ошибки/5xx. Ошибки GraphQL
    /// (`errors[]`) возвращаются как ApiError с кодом из `extensions.code`.
    pub fn graphql(agent: &ureq::Agent, url: &str, api_key: &str, query: &str, variables: Value) -> Result<Value> {
        let endpoint = format!("{url}/graphql");
        let body = json!({ "query": query, "variables": variables });
        let mut last_err = String::new();
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(5 * attempt as u64));
            }
            let resp = agent
                .post(&endpoint)
                .set("Authorization", &format!("Bearer {api_key}"))
                .set("Accept", "application/json")
                .send_json(body.clone());
            match resp {
                Ok(r) => {
                    let v: Value = r
                        .into_json()
                        .map_err(|e| ApiError(format!("bad json from {endpoint}: {e}")))?;
                    if let Some(errs) = v.get("errors").and_then(Value::as_array) {
                        if let Some(e) = errs.first() {
                            let code = e["extensions"]["code"].as_str().unwrap_or("ERROR");
                            let msg = e["message"].as_str().unwrap_or("unknown error");
                            return Err(ApiError(format!("{code}: {msg}")));
                        }
                    }
                    return Ok(v["data"].clone());
                }
                Err(ureq::Error::Status(code, r)) => {
                    let text = r.into_string().unwrap_or_default();
                    last_err = format!("HTTP {code} from {endpoint}: {text}");
                    if code == 401 || code == 403 {
                        return Err(ApiError(format!("{last_err} (проверь API-ключ workspace)")));
                    }
                    if code < 500 {
                        return Err(ApiError(last_err));
                    }
                }
                Err(e) => last_err = format!("network error for {endpoint}: {e}"),
            }
        }
        Err(ApiError(last_err))
    }

    fn gql(&self, query: &str, variables: Value) -> Result<Value> {
        Self::graphql(&self.agent, &self.url, &self.api_key, query, variables)
    }

    /// Workspace по ключу + участники + «я» (агент) — для Settings::load_from.
    pub fn resolve(url: &str, api_key: &str, workspace_key: &str) -> Result<Resolved> {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).build();
        let v = Self::graphql(
            &agent,
            url.trim_end_matches('/'),
            api_key,
            "{ me { id displayName } workspaces { id key members { user { id username displayName } } } }",
            json!({}),
        )?;
        let ws = v["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| w["key"].as_str().map(|k| k.eq_ignore_ascii_case(workspace_key)).unwrap_or(false))
            .ok_or_else(|| {
                ApiError(format!(
                    "workspace «{workspace_key}» недоступен по этому API-ключу (ключ привязан к одному workspace)"
                ))
            })?;
        let members = ws["members"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| {
                (
                    m["user"]["id"].as_str().unwrap_or_default().to_string(),
                    m["user"]["username"].as_str().unwrap_or_default().to_string(),
                    m["user"]["displayName"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        Ok(Resolved {
            workspace_id: ws["id"].as_str().unwrap_or_default().to_string(),
            me_id: v["me"]["id"].as_str().unwrap_or_default().to_string(),
            me_name: v["me"]["displayName"].as_str().unwrap_or_default().to_string(),
            members,
        })
    }

    // ---- Маппинг ----

    fn user_of(v: &Value) -> Option<User> {
        v.as_object().map(|_| User {
            gid: v["id"].as_str().unwrap_or_default().to_string(),
            name: v["displayName"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// Значение custom field → текст: SELECT/MULTI_SELECT — имена опций,
    /// CHECKBOX — да/нет, остальное — как есть. None, если значение пустое.
    fn field_value_of(cf: &Value) -> Option<FieldValue> {
        let field = &cf["field"];
        let value = &cf["value"];
        if value.is_null() {
            return None;
        }
        let kind = field["kind"].as_str().unwrap_or_default();
        let option_name = |id: &str| -> String {
            field["options"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|o| o["id"].as_str() == Some(id))
                .and_then(|o| o["name"].as_str())
                .unwrap_or(id)
                .to_string()
        };
        let text = match (kind, value) {
            ("SELECT", Value::String(id)) => option_name(id),
            ("MULTI_SELECT", Value::Array(ids)) => ids
                .iter()
                .filter_map(Value::as_str)
                .map(option_name)
                .collect::<Vec<_>>()
                .join(", "),
            ("CHECKBOX", Value::Bool(b)) => (if *b { "да" } else { "нет" }).to_string(),
            (_, Value::String(x)) => x.trim().to_string(),
            (_, other) => other.to_string(),
        };
        if text.is_empty() {
            return None;
        }
        Some(FieldValue {
            field_id: field["id"].as_str().unwrap_or_default().to_string(),
            name: field["name"].as_str().unwrap_or_default().to_string(),
            kind: kind.to_string(),
            value: text,
            scoped: field["assignees"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
        })
    }

    fn task_of(&self, v: &Value) -> Task {
        let key = v["key"].as_str().map(str::to_string);
        Task {
            gid: v["id"].as_str().unwrap_or_default().to_string(),
            key: key.clone(),
            name: v["title"].as_str().unwrap_or_default().to_string(),
            notes: v["description"].as_str().unwrap_or_default().to_string(),
            completed: v["completed"].as_bool().unwrap_or(false),
            assignee: Self::user_of(&v["assignee"]),
            due_on: v["dueDate"].as_str().map(str::to_string),
            permalink_url: key.as_ref().map(|k| format!("{}/t/{k}", self.url)),
            parent: v["parent"].as_object().map(|p| Ref {
                gid: p["id"].as_str().unwrap_or_default().to_string(),
                name: p["key"].as_str().unwrap_or_default().to_string(),
            }),
            memberships: v["projects"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|l| Membership {
                    project: Some(Ref {
                        gid: l["project"]["id"].as_str().unwrap_or_default().to_string(),
                        name: l["project"]["name"].as_str().unwrap_or_default().to_string(),
                    }),
                    section: Some(Ref {
                        gid: l["section"]["id"].as_str().unwrap_or_default().to_string(),
                        name: l["section"]["name"].as_str().unwrap_or_default().to_string(),
                    }),
                })
                .collect(),
            custom_fields: v["customFields"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Self::field_value_of)
                .collect(),
        }
    }

    /// uuid задачи по uuid или ключу (`SAOBR-12`).
    pub fn resolve_task_id(&self, id_or_key: &str) -> Result<String> {
        if looks_like_key(id_or_key) {
            let v = self.gql("query($k: String) { task(key: $k) { id } }", json!({ "k": id_or_key }))?;
            return Ok(v["task"]["id"].as_str().unwrap_or_default().to_string());
        }
        Ok(id_or_key.to_string())
    }

    // ---- Задачи ----

    pub fn get_task(&self, id_or_key: &str) -> Result<Task> {
        let (arg, var) = if looks_like_key(id_or_key) { ("key: $v", "String") } else { ("id: $v", "ID") };
        let q = format!("query($v: {var}) {{ task({arg}) {{ {TASK_FIELDS} }} }}");
        let v = self.gql(&q, json!({ "v": id_or_key }))?;
        Ok(self.task_of(&v["task"]))
    }

    pub fn get_assigned_tasks(&self) -> Result<Vec<Task>> {
        let q = format!(
            "query($w: ID!, $a: ID!) {{ tasks(workspaceId: $w, filter: {{ assigneeId: $a, completed: false }}, \
             sort: CREATED_ASC, limit: 500) {{ items {{ {TASK_FIELDS} }} }} }}"
        );
        let v = self.gql(&q, json!({ "w": self.workspace_id, "a": self.agent_id }))?;
        Ok(v["tasks"]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| self.task_of(t))
            .collect())
    }

    pub fn get_subtasks(&self, id_or_key: &str) -> Result<Vec<Task>> {
        let id = self.resolve_task_id(id_or_key)?;
        let q = format!("query($id: ID) {{ task(id: $id) {{ subtasks {{ {TASK_FIELDS} }} }} }}");
        let v = self.gql(&q, json!({ "id": id }))?;
        Ok(v["task"]["subtasks"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| self.task_of(t))
            .collect())
    }

    /// Комментарии + история как единый список «сторис» (как в Asana):
    /// комментарии — `comment_added`, записи истории — `kind` из activity.
    pub fn get_stories(&self, id_or_key: &str) -> Result<Vec<Story>> {
        let id = self.resolve_task_id(id_or_key)?;
        let v = self.gql(
            "query($id: ID) { task(id: $id) { \
                comments { body createdAt author { id displayName } } \
                activity { kind data createdAt actor { id displayName } } } }",
            json!({ "id": id }),
        )?;
        let mut out: Vec<Story> = Vec::new();
        for c in v["task"]["comments"].as_array().into_iter().flatten() {
            out.push(Story {
                resource_subtype: "comment_added".into(),
                text: c["body"].as_str().unwrap_or_default().to_string(),
                created_at: c["createdAt"].as_str().unwrap_or_default().to_string(),
                created_by: Self::user_of(&c["author"]),
            });
        }
        for a in v["task"]["activity"].as_array().into_iter().flatten() {
            let kind = a["kind"].as_str().unwrap_or_default();
            if kind == "comment.created" {
                continue; // сам комментарий уже выше
            }
            out.push(Story {
                resource_subtype: kind.to_string(),
                text: activity_text(kind, &a["data"]),
                created_at: a["createdAt"].as_str().unwrap_or_default().to_string(),
                created_by: Self::user_of(&a["actor"]),
            });
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    pub fn add_comment(&self, id_or_key: &str, text: &str) -> Result<()> {
        let id = self.resolve_task_id(id_or_key)?;
        self.gql(
            "mutation($t: ID!, $b: String!) { createComment(taskId: $t, body: $b) { id } }",
            json!({ "t": id, "b": text }),
        )?;
        Ok(())
    }

    pub fn move_to_section(&self, id_or_key: &str, project_id: &str, section_id: &str) -> Result<()> {
        let id = self.resolve_task_id(id_or_key)?;
        self.gql(
            "mutation($t: ID!, $p: ID!, $s: ID!) { moveTask(taskId: $t, projectId: $p, sectionId: $s) { id } }",
            json!({ "t": id, "p": project_id, "s": section_id }),
        )?;
        Ok(())
    }

    pub fn set_assignee(&self, id_or_key: &str, assignee: Option<&str>) -> Result<()> {
        let id = self.resolve_task_id(id_or_key)?;
        let input = match assignee {
            Some(a) => json!({ "assigneeId": a }),
            None => json!({ "clearAssignee": true }),
        };
        self.gql(
            "mutation($id: ID!, $i: UpdateTaskInput!) { updateTask(id: $id, input: $i) { id } }",
            json!({ "id": id, "i": input }),
        )?;
        Ok(())
    }

    pub fn set_completed(&self, id_or_key: &str, completed: bool) -> Result<()> {
        let id = self.resolve_task_id(id_or_key)?;
        self.gql(
            "mutation($id: ID!, $i: UpdateTaskInput!) { updateTask(id: $id, input: $i) { id } }",
            json!({ "id": id, "i": { "completed": completed } }),
        )?;
        Ok(())
    }

    pub fn get_attachments(&self, id_or_key: &str) -> Result<Vec<Attachment>> {
        let id = self.resolve_task_id(id_or_key)?;
        let v = self.gql(
            "query($id: ID) { task(id: $id) { attachments { id filename size url } } }",
            json!({ "id": id }),
        )?;
        Ok(v["task"]["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| Attachment {
                gid: a["id"].as_str().unwrap_or_default().to_string(),
                name: a["filename"].as_str().unwrap_or_default().to_string(),
                size: a["size"].as_u64().unwrap_or(0),
                url: a["url"].as_str().map(str::to_string),
            })
            .collect())
    }

    /// `<задача>/<uuid вложения>` → файл в dest_dir. Ссылка на вложение
    /// подписана и живёт 15 минут, поэтому она всегда запрашивается заново.
    pub fn download_attachment(&self, composite: &str, dest_dir: &Path) -> Result<PathBuf> {
        let (task, att) = composite.split_once('/').ok_or_else(|| {
            ApiError(
                "для gleb_tasks вложение адресуется как <задача>/<id вложения>, например SAOBR-12/3f2a… \
                 (так оно и напечатано в контексте задачи)"
                    .into(),
            )
        })?;
        let a = self
            .get_attachments(task)?
            .into_iter()
            .find(|a| a.gid == att)
            .ok_or_else(|| ApiError(format!("у задачи {task} нет вложения {att}")))?;
        let url = a.url.ok_or_else(|| ApiError("вложение без ссылки (хранилище не настроено)".into()))?;
        let short: String = a.gid.chars().take(8).collect();
        crate::asana::download_signed(&self.agent, &url, dest_dir, &format!("{short}-{}", a.name))
    }

    /// Создать подзадачу; возвращает её ключ (SAOBR-13).
    pub fn create_subtask(&self, parent: &str, name: &str, notes: &str, assignee: Option<&str>) -> Result<String> {
        let parent_id = self.resolve_task_id(parent)?;
        let mut input = json!({
            "workspaceId": self.workspace_id,
            "title": name,
            "description": notes,
            "parentId": parent_id,
        });
        if let Some(a) = assignee {
            input["assigneeId"] = json!(a);
        }
        let v = self.gql(
            "mutation($i: CreateTaskInput!) { createTask(input: $i) { id key } }",
            json!({ "i": input }),
        )?;
        Ok(v["createTask"]["key"]
            .as_str()
            .or(v["createTask"]["id"].as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// Создаёт (или дополняет опциями) SELECT-поле `name`, привязанное к агенту,
    /// необязательное. Возвращает строку-отчёт для CLI.
    pub fn ensure_select_field(&self, name: &str, options: &[&str]) -> Result<String> {
        let v = self.gql(
            "query($w: ID!) { workspace(id: $w) { customFields { id name kind options { id name } assignees { id } } } }",
            json!({ "w": self.workspace_id }),
        )?;
        let existing = v["workspace"]["customFields"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| f["name"].as_str().map(|n| n.trim().eq_ignore_ascii_case(name)).unwrap_or(false))
            .cloned();
        let Some(f) = existing else {
            let opts: Vec<Value> = options.iter().map(|o| json!({ "name": o })).collect();
            let r = self.gql(
                "mutation($w: ID!, $n: String!, $o: [CustomFieldOptionInput!], $a: [ID!]) { \
                    createCustomField(workspaceId: $w, name: $n, kind: SELECT, options: $o, assigneeIds: $a, required: false) { id } }",
                json!({ "w": self.workspace_id, "n": name, "o": opts, "a": [self.agent_id] }),
            )?;
            return Ok(format!(
                "создано поле «{name}» (SELECT: {}; для исполнителя-агента) id {}",
                options.join("/"),
                r["createCustomField"]["id"].as_str().unwrap_or_default()
            ));
        };
        let id = f["id"].as_str().unwrap_or_default().to_string();
        if f["kind"].as_str() != Some("SELECT") {
            return Err(ApiError(format!(
                "поле «{name}» уже есть, но его тип {} — нужен SELECT; переименуй его или укажи другое имя в autopilot.yaml (fields:)",
                f["kind"].as_str().unwrap_or("?")
            )));
        }
        let have: Vec<Value> = f["options"].as_array().cloned().unwrap_or_default();
        let missing: Vec<&str> = options
            .iter()
            .copied()
            .filter(|o| !have.iter().any(|h| h["name"].as_str().map(|n| n.eq_ignore_ascii_case(o)).unwrap_or(false)))
            .collect();
        let has_agent = f["assignees"]
            .as_array()
            .map(|a| a.iter().any(|u| u["id"].as_str() == Some(self.agent_id.as_str())))
            .unwrap_or(false);
        if missing.is_empty() && has_agent {
            return Ok(format!("поле «{name}» уже настроено (id {id})"));
        }
        let mut opts: Vec<Value> = have.iter().map(|h| json!({ "id": h["id"], "name": h["name"] })).collect();
        opts.extend(missing.iter().map(|o| json!({ "name": o })));
        let mut assignees: Vec<Value> = f["assignees"].as_array().cloned().unwrap_or_default().iter().map(|u| u["id"].clone()).collect();
        if !has_agent {
            assignees.push(json!(self.agent_id));
        }
        self.gql(
            "mutation($id: ID!, $o: [CustomFieldOptionInput!], $a: [ID!]) { \
                updateCustomField(id: $id, options: $o, assigneeIds: $a) { id } }",
            json!({ "id": id, "o": opts, "a": assignees }),
        )?;
        let mut what = Vec::new();
        if !missing.is_empty() {
            what.push(format!("добавлены опции {}", missing.join("/")));
        }
        if !has_agent {
            what.push("привязано к исполнителю-агенту".to_string());
        }
        Ok(format!("поле «{name}» обновлено: {} (id {id})", what.join(", ")))
    }

    pub fn project_sections(&self, project_id: &str) -> Result<Vec<Ref>> {
        let v = self.gql(
            "query($id: ID!) { project(id: $id) { sections { id name } } }",
            json!({ "id": project_id }),
        )?;
        Ok(v["project"]["sections"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| Ref {
                gid: s["id"].as_str().unwrap_or_default().to_string(),
                name: s["name"].as_str().unwrap_or_default().to_string(),
            })
            .collect())
    }
}

/// Человекочитаемая строка записи истории (kind + data из activity).
fn activity_text(kind: &str, d: &Value) -> String {
    let s = |k: &str| d[k].as_str().unwrap_or("—").to_string();
    let v = |k: &str| match &d[k] {
        Value::Null => "—".to_string(),
        Value::String(x) => x.clone(),
        other => other.to_string(),
    };
    match kind {
        "task.created" => "задача создана".into(),
        "task.completed" => "задача выполнена".into(),
        "task.uncompleted" => "задача снова открыта".into(),
        "task.deleted" => "задача удалена".into(),
        "task.field_changed" => format!("{}: {} → {}", s("field"), v("from"), v("to")),
        "task.moved" => format!(
            "перемещена в «{}» → «{}» (проект {})",
            s("fromSectionName"),
            s("toSectionName"),
            s("projectName")
        ),
        "task.added_to_project" => format!("добавлена в проект {} / {}", s("projectName"), s("sectionName")),
        "task.removed_from_project" => format!("убрана из проекта {}", s("projectName")),
        "task.tags_changed" => format!("теги: +{} −{}", v("added"), v("removed")),
        "task.custom_field_changed" => format!(
            "{}: {} → {}",
            s("fieldName"),
            d["fromText"].as_str().unwrap_or(&v("from")),
            d["toText"].as_str().unwrap_or(&v("to"))
        ),
        "comment.deleted" => "комментарий удалён".into(),
        "attachment.added" => format!("вложение добавлено: {}", s("filename")),
        "attachment.deleted" => format!("вложение удалено: {}", s("filename")),
        other => other.to_string(),
    }
}
