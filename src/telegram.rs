//! Telegram-бот для управления автопилотом: статус, пауза/возобновление
//! проектов, приоритеты, просмотр логов — командами и inline-кнопками.
//!
//! Работает в потоке демона (long polling getUpdates). Все изменения идут через
//! реестр на диске, поэтому действуют и на демон, и видны CLI.
//!
//! Авторизация: только chat id из `telegram.allowed_chats` (~/.autopilot/config.yaml).
//! Чужим бот отвечает их chat id — чтобы его можно было внести в конфиг.

use crate::claudecli::{self, LoginProc};
use crate::daemon::status_text;
use crate::registry::{tg_offset_file, Registry, TelegramCfg};
use crate::runner::{latest_session_log, tail_file};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Незавершённый флоу `/login`: процесс claude ждёт код от конкретного чата.
struct PendingLogin {
    chat_id: i64,
    proc: LoginProc,
    started: Instant,
}

/// Живёт не дольше этого — потом считаем брошенным и убиваем процесс claude.
const LOGIN_TTL: Duration = Duration::from_secs(600);

struct Bot {
    agent: ureq::Agent,
    base: String,
    cfg: TelegramCfg,
    /// Ожидающий код авторизации логин (не больше одного за раз).
    pending_login: Mutex<Option<PendingLogin>>,
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
            pending_login: Mutex::new(None),
        };
        bot.register_commands();
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
        // Если ждём код авторизации от этого чата — любой не-командный текст = код.
        if !text.starts_with('/') {
            if let Some(pending) = self.take_pending_login_for(chat_id) {
                self.submit_login_code(chat_id, pending, text);
                return;
            }
        }

        let mut parts = text.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        // Команды вида /status@MyBot тоже принимаем.
        let cmd = cmd.split('@').next().unwrap_or(cmd);
        match cmd {
            "/start" | "/help" | "/menu" => {
                self.send(chat_id, &help_text(), Some(self.main_menu()));
            }
            "/status" => {
                self.send(chat_id, &status_text(), Some(self.keyboard()));
            }
            "/whoami" | "/account" => self.send_account(chat_id),
            "/usage" => self.send_usage(chat_id),
            "/login" => self.send_login_confirm(chat_id),
            "/cancel" => {
                if let Some(p) = self.take_pending_login() {
                    p.proc.abort();
                    self.send(chat_id, "Вход отменён.", None);
                } else {
                    self.send(chat_id, "Отменять нечего.", None);
                }
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
        // Кнопки главного меню и подтверждения логина (без разделителя '|').
        match data {
            "m_status" => {
                self.answer_callback(cq_id, "Статус");
                self.send(chat_id, &status_text(), Some(self.keyboard()));
                return;
            }
            "m_account" => {
                self.answer_callback(cq_id, "Аккаунт");
                self.send_account(chat_id);
                return;
            }
            "m_usage" => {
                self.answer_callback(cq_id, "Usage…");
                self.send_usage(chat_id);
                return;
            }
            "m_login" => {
                self.answer_callback(cq_id, "Вход");
                self.send_login_confirm(chat_id);
                return;
            }
            "login_go" => {
                self.answer_callback(cq_id, "Запускаю…");
                self.edit(chat_id, message_id, "🔐 Запускаю вход…", None);
                self.start_login(chat_id);
                return;
            }
            "login_cancel" => {
                self.answer_callback(cq_id, "Отменено");
                if let Some(p) = self.take_pending_login() {
                    p.proc.abort();
                }
                self.edit(chat_id, message_id, "Вход отменён.", None);
                return;
            }
            _ => {}
        }

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

    // ---- Аккаунт Claude / usage / логин ----

    fn send_account(&self, chat_id: i64) {
        match claudecli::auth_status() {
            Ok(s) if s.logged_in => {
                let text = format!(
                    "👤 Аккаунт Claude Code\n\
                     Email: {}\n\
                     Организация: {}\n\
                     Подписка: {}\n\
                     Метод входа: {}",
                    s.email.as_deref().unwrap_or("—"),
                    s.org_name.as_deref().unwrap_or("—"),
                    s.subscription.as_deref().unwrap_or("—"),
                    s.auth_method.as_deref().unwrap_or("—"),
                );
                self.send(chat_id, &text, None);
            }
            Ok(_) => self.send(
                chat_id,
                "❌ Не залогинен. Нажми «🔐 Войти» или пришли /login.",
                None,
            ),
            Err(e) => self.send(chat_id, &format!("Ошибка `claude auth status`: {e}"), None),
        }
    }

    fn send_usage(&self, chat_id: i64) {
        match claudecli::usage() {
            Ok(mut text) => {
                if text.chars().count() > 3800 {
                    text = text.chars().take(3800).collect();
                }
                self.send(chat_id, &format!("📈 Лимиты Claude\n\n{text}"), None);
            }
            Err(e) => self.send(chat_id, &format!("Не удалось получить usage: {e}"), None),
        }
    }

    fn send_login_confirm(&self, chat_id: i64) {
        let kb = json!({"inline_keyboard": [[
            {"text": "✅ Да, войти", "callback_data": "login_go"},
            {"text": "Отмена", "callback_data": "login_cancel"},
        ]]});
        self.send(
            chat_id,
            "⚠️ Это перелогинит аккаунт Claude, которым работает демон \
             (влияет на лимиты и биллинг всех сессий). Продолжить?",
            Some(kb),
        );
    }

    /// Стартует `claude auth login`, шлёт ссылку и ждёт код ответным сообщением.
    fn start_login(&self, chat_id: i64) {
        // Прерываем прежний незавершённый вход, если был.
        if let Some(p) = self.take_pending_login() {
            p.proc.abort();
        }
        match claudecli::login_start() {
            Ok((proc, url)) => {
                *self.pending_login.lock().unwrap() = Some(PendingLogin {
                    chat_id,
                    proc,
                    started: Instant::now(),
                });
                self.send(
                    chat_id,
                    &format!(
                        "🔗 Открой ссылку, авторизуйся и пришли мне код ответным сообщением \
                         (или /cancel):\n\n{url}"
                    ),
                    None,
                );
            }
            Err(e) => self.send(chat_id, &format!("Не удалось начать вход: {e}"), None),
        }
    }

    fn submit_login_code(&self, chat_id: i64, pending: PendingLogin, code: &str) {
        self.send(chat_id, "⏳ Проверяю код…", None);
        match claudecli::login_finish(pending.proc, code) {
            Ok(rest) => {
                let acc = claudecli::auth_status()
                    .ok()
                    .filter(|s| s.logged_in)
                    .and_then(|s| s.email)
                    .map(|e| format!("\nТеперь залогинен как: {e}"))
                    .unwrap_or_default();
                let tail = if rest.is_empty() { String::new() } else { format!("\n\n{rest}") };
                self.send(chat_id, &format!("✅ Вход выполнен.{acc}{tail}"), None);
            }
            Err(e) => self.send(chat_id, &format!("❌ Вход не удался: {e}"), None),
        }
    }

    // ---- Состояние ожидающего логина ----

    /// Забрать pending-login целиком (например, для /cancel), очистив слот.
    fn take_pending_login(&self) -> Option<PendingLogin> {
        self.pending_login.lock().unwrap().take()
    }

    /// Забрать pending-login, если он принадлежит этому чату и не протух.
    /// Протухший убивается, свежий чужого чата остаётся на месте.
    fn take_pending_login_for(&self, chat_id: i64) -> Option<PendingLogin> {
        let mut guard = self.pending_login.lock().unwrap();
        match guard.as_ref() {
            Some(p) if p.started.elapsed() > LOGIN_TTL => {
                guard.take().unwrap().proc.abort();
                None
            }
            Some(p) if p.chat_id == chat_id => guard.take(),
            _ => None,
        }
    }

    /// Регистрирует список команд (кнопка «Menu» и автодополнение в Telegram).
    fn register_commands(&self) {
        let commands = json!([
            {"command": "status", "description": "Статус демона и проектов"},
            {"command": "menu", "description": "Главное меню"},
            {"command": "account", "description": "Аккаунт Claude Code"},
            {"command": "usage", "description": "Лимиты Claude (%)"},
            {"command": "login", "description": "Войти в аккаунт Claude"},
            {"command": "logs", "description": "Лог последней сессии проекта"},
            {"command": "pause", "description": "Приостановить проект"},
            {"command": "resume", "description": "Возобновить проект"},
            {"command": "priority", "description": "Задать приоритет проекта"},
        ]);
        self.call("setMyCommands", json!({"commands": commands}));
    }

    /// Главное inline-меню (для /start, /menu, /help).
    fn main_menu(&self) -> Value {
        json!({"inline_keyboard": [
            [
                {"text": "📊 Статус", "callback_data": "m_status"},
                {"text": "👤 Аккаунт", "callback_data": "m_account"},
            ],
            [
                {"text": "📈 Usage", "callback_data": "m_usage"},
                {"text": "🔐 Войти", "callback_data": "m_login"},
            ],
        ]})
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

fn help_text() -> String {
    "Автопилот — управление с телефона.\n\n\
     Кнопки меню ниже или команды:\n\
     /status — статус демона и проектов (с кнопками)\n\
     /account — аккаунт Claude Code (кто залогинен, подписка)\n\
     /usage — лимиты Claude в процентах\n\
     /login — войти в аккаунт Claude (пришлёт ссылку)\n\
     /logs <проект> — хвост последнего лога сессии\n\
     /pause <проект> | /resume <проект>\n\
     /priority <проект> <число>\n\
     Кнопки проектов: ⏯ пауза, 🔼/🔽 приоритет, 📜 логи."
        .to_string()
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
