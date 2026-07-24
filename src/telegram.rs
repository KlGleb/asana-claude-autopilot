//! Telegram-бот для управления автопилотом: статус, пауза/возобновление
//! проектов, приоритеты, просмотр логов — командами и inline-кнопками.
//!
//! Работает в потоке демона (long polling getUpdates). Все изменения идут через
//! реестр на диске, поэтому действуют и на демон, и видны CLI.
//!
//! Авторизация: только chat id из `telegram.allowed_chats` (~/.autopilot/config.yaml).
//! Чужим бот отвечает их chat id — чтобы его можно было внести в конфиг.

use crate::daemon::status_text;
use crate::registry::{tg_offset_file, Registry, TelegramCfg};
use crate::runner::{latest_session_log, tail_file};
use serde_json::{json, Value};
use std::time::Duration;

struct Bot {
    agent: ureq::Agent,
    base: String,
    cfg: TelegramCfg,
}

pub fn spawn(cfg: TelegramCfg) -> Option<std::thread::JoinHandle<()>> {
    let token = cfg.resolve_token()?;
    Some(std::thread::spawn(move || {
        let bot = Bot {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(90))
                .build(),
            base: format!("https://api.telegram.org/bot{token}"),
            cfg,
        };
        bot.poll_loop();
    }))
}

impl Bot {
    fn call(&self, method: &str, body: Value) -> Option<Value> {
        match self
            .agent
            .post(&format!("{}/{method}", self.base))
            .send_json(body)
        {
            Ok(r) => r.into_json().ok(),
            Err(ureq::Error::Status(code, r)) => {
                eprintln!(
                    "[telegram] {method} HTTP {code}: {}",
                    r.into_string().unwrap_or_default()
                );
                None
            }
            Err(e) => {
                eprintln!("[telegram] {method}: {e}");
                None
            }
        }
    }

    fn send(&self, chat_id: i64, text: &str, keyboard: Option<Value>) {
        let mut body = json!({"chat_id": chat_id, "text": text});
        if let Some(kb) = keyboard {
            body["reply_markup"] = kb;
        }
        self.call("sendMessage", body);
    }

    fn edit(&self, chat_id: i64, message_id: i64, text: &str, keyboard: Option<Value>) {
        let mut body = json!({"chat_id": chat_id, "message_id": message_id, "text": text});
        if let Some(kb) = keyboard {
            body["reply_markup"] = kb;
        }
        self.call("editMessageText", body);
    }

    fn answer_callback(&self, id: &str, text: &str) {
        self.call("answerCallbackQuery", json!({"callback_query_id": id, "text": text}));
    }

    fn allowed(&self, chat_id: i64) -> bool {
        self.cfg.allowed_chats.contains(&chat_id)
    }

    fn poll_loop(&self) {
        let mut offset: i64 = std::fs::read_to_string(tg_offset_file())
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        loop {
            let resp = self.call(
                "getUpdates",
                json!({
                    "timeout": 50,
                    "offset": offset,
                    "allowed_updates": ["message", "callback_query"]
                }),
            );
            let Some(resp) = resp else {
                std::thread::sleep(Duration::from_secs(10));
                continue;
            };
            let updates = resp["result"].as_array().cloned().unwrap_or_default();
            for u in updates {
                if let Some(id) = u["update_id"].as_i64() {
                    offset = id + 1;
                    let _ = std::fs::write(tg_offset_file(), offset.to_string());
                }
                self.handle_update(&u);
            }
        }
    }

    fn handle_update(&self, u: &Value) {
        if let Some(msg) = u.get("message").filter(|m| !m.is_null()) {
            let chat_id = msg["chat"]["id"].as_i64().unwrap_or(0);
            let text = msg["text"].as_str().unwrap_or("").trim().to_string();
            if chat_id == 0 || text.is_empty() {
                return;
            }
            if !self.allowed(chat_id) {
                self.send(
                    chat_id,
                    &format!(
                        "⛔ Не авторизован. Ваш chat id: {chat_id}\n\
                         Добавьте его в ~/.autopilot/config.yaml → telegram.allowed_chats \
                         и перезапустите демон (autopilot restart)."
                    ),
                    None,
                );
                return;
            }
            self.handle_command(chat_id, &text);
        }

        if let Some(cq) = u.get("callback_query").filter(|c| !c.is_null()) {
            let cq_id = cq["id"].as_str().unwrap_or("").to_string();
            let chat_id = cq["message"]["chat"]["id"].as_i64().unwrap_or(0);
            let message_id = cq["message"]["message_id"].as_i64().unwrap_or(0);
            let data = cq["data"].as_str().unwrap_or("").to_string();
            if !self.allowed(chat_id) {
                self.answer_callback(&cq_id, "Не авторизован");
                return;
            }
            self.handle_callback(&cq_id, chat_id, message_id, &data);
        }
    }

    fn handle_command(&self, chat_id: i64, text: &str) {
        let mut parts = text.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        // Команды вида /status@MyBot тоже принимаем.
        let cmd = cmd.split('@').next().unwrap_or(cmd);
        match cmd {
            "/start" | "/help" => {
                self.send(
                    chat_id,
                    "Автопилот. Команды:\n\
                     /status — статус демона и проектов (с кнопками)\n\
                     /logs <проект> — хвост последнего лога сессии\n\
                     /pause <проект> | /resume <проект>\n\
                     /priority <проект> <число>\n\
                     Кнопки: ⏯ пауза/возобновление, 🔼/🔽 приоритет, 📜 логи.",
                    None,
                );
            }
            "/status" => {
                self.send(chat_id, &status_text(), Some(self.keyboard()));
            }
            "/logs" => match parts.next() {
                Some(name) => self.send_logs(chat_id, name),
                None => self.send(chat_id, "Использование: /logs <проект>", None),
            },
            "/pause" | "/resume" => {
                let Some(name) = parts.next() else {
                    self.send(chat_id, &format!("Использование: {cmd} <проект>"), None);
                    return;
                };
                let pause = cmd == "/pause";
                match set_paused(name, pause) {
                    Ok(()) => self.send(
                        chat_id,
                        &format!("{} {name}", if pause { "⏸ Приостановлен" } else { "▶️ Возобновлён" }),
                        None,
                    ),
                    Err(e) => self.send(chat_id, &format!("Ошибка: {e}"), None),
                }
            }
            "/priority" => {
                let (Some(name), Some(pr)) = (parts.next(), parts.next()) else {
                    self.send(chat_id, "Использование: /priority <проект> <число>", None);
                    return;
                };
                let Ok(pr) = pr.parse::<i64>() else {
                    self.send(chat_id, "Приоритет должен быть числом", None);
                    return;
                };
                match set_priority(name, pr) {
                    Ok(()) => self.send(chat_id, &format!("Приоритет {name} = {pr}"), None),
                    Err(e) => self.send(chat_id, &format!("Ошибка: {e}"), None),
                }
            }
            _ => {
                self.send(chat_id, "Не понял. /help — список команд.", None);
            }
        }
    }

    fn handle_callback(&self, cq_id: &str, chat_id: i64, message_id: i64, data: &str) {
        let (op, name) = match data.split_once('|') {
            Some((o, n)) => (o, n),
            None => (data, ""),
        };
        let note = match op {
            "t" => match toggle_paused(name) {
                Ok(paused) => {
                    if paused {
                        format!("⏸ {name} на паузе")
                    } else {
                        format!("▶️ {name} возобновлён")
                    }
                }
                Err(e) => format!("Ошибка: {e}"),
            },
            "u" => match bump_priority(name, 1) {
                Ok(p) => format!("Приоритет {name} = {p}"),
                Err(e) => format!("Ошибка: {e}"),
            },
            "d" => match bump_priority(name, -1) {
                Ok(p) => format!("Приоритет {name} = {p}"),
                Err(e) => format!("Ошибка: {e}"),
            },
            "l" => {
                self.answer_callback(cq_id, "Логи…");
                self.send_logs(chat_id, name);
                return;
            }
            "r" => "Обновлено".to_string(),
            _ => "Неизвестная кнопка".to_string(),
        };
        self.answer_callback(cq_id, &note);
        // Пауза/приоритет меняют статус — перерисовываем сообщение.
        self.edit(chat_id, message_id, &status_text(), Some(self.keyboard()));
    }

    fn send_logs(&self, chat_id: i64, name: &str) {
        let reg = Registry::load().unwrap_or_default();
        let Some(project) = reg.find(name) else {
            self.send(chat_id, &format!("Проект «{name}» не найден"), None);
            return;
        };
        match latest_session_log(project) {
            Some(path) => {
                let mut tail = tail_file(&path, 30);
                // Лимит Telegram — 4096 символов на сообщение.
                if tail.chars().count() > 3500 {
                    tail = tail.chars().skip(tail.chars().count() - 3500).collect();
                }
                let fname = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                self.send(chat_id, &format!("📜 {name} — {fname}:\n\n{tail}"), None);
            }
            None => self.send(chat_id, &format!("У «{name}» ещё нет логов сессий"), None),
        }
    }

    /// Inline-клавиатура: по строке на проект + строка «Обновить».
    fn keyboard(&self) -> Value {
        let reg = Registry::load().unwrap_or_default();
        let mut rows: Vec<Value> = Vec::new();
        for p in reg.sorted_all() {
            let toggle = if p.paused {
                format!("▶️ {}", p.name)
            } else {
                format!("⏸ {}", p.name)
            };
            rows.push(json!([
                {"text": toggle, "callback_data": format!("t|{}", p.name)},
                {"text": "🔼", "callback_data": format!("u|{}", p.name)},
                {"text": "🔽", "callback_data": format!("d|{}", p.name)},
                {"text": "📜", "callback_data": format!("l|{}", p.name)},
            ]));
        }
        rows.push(json!([{"text": "🔄 Обновить", "callback_data": "r|"}]));
        json!({"inline_keyboard": rows})
    }
}

// ---- Операции над реестром (общие для команд и кнопок) ----

fn set_paused(name: &str, paused: bool) -> crate::Result<()> {
    let mut reg = Registry::load()?;
    let p = reg
        .find_mut(name)
        .ok_or_else(|| crate::ApiError(format!("проект «{name}» не найден")))?;
    p.paused = paused;
    reg.save()
}

fn toggle_paused(name: &str) -> crate::Result<bool> {
    let mut reg = Registry::load()?;
    let p = reg
        .find_mut(name)
        .ok_or_else(|| crate::ApiError(format!("проект «{name}» не найден")))?;
    p.paused = !p.paused;
    let now = p.paused;
    reg.save()?;
    Ok(now)
}

fn bump_priority(name: &str, delta: i64) -> crate::Result<i64> {
    let mut reg = Registry::load()?;
    let p = reg
        .find_mut(name)
        .ok_or_else(|| crate::ApiError(format!("проект «{name}» не найден")))?;
    p.priority += delta;
    let now = p.priority;
    reg.save()?;
    Ok(now)
}

fn set_priority(name: &str, pr: i64) -> crate::Result<()> {
    let mut reg = Registry::load()?;
    let p = reg
        .find_mut(name)
        .ok_or_else(|| crate::ApiError(format!("проект «{name}» не найден")))?;
    p.priority = pr;
    reg.save()
}
