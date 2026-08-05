//! Тонкие обёртки над CLI `claude`: статус авторизации, usage-лимиты и
//! интерактивный флоу логина (ссылка + код). Используются Telegram-ботом,
//! чтобы управлять аккаунтом Claude Code, которым работает демон, с телефона.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Результат `claude auth status --json`.
#[derive(Debug, Default)]
pub struct AuthStatus {
    pub logged_in: bool,
    pub email: Option<String>,
    pub org_name: Option<String>,
    pub subscription: Option<String>,
    pub auth_method: Option<String>,
}

/// Кто сейчас залогинен в `claude` (аккаунт, организация, тип подписки).
pub fn auth_status() -> Result<AuthStatus, String> {
    let out = Command::new("claude")
        .args(["auth", "status", "--json"])
        .output()
        .map_err(|e| format!("не удалось запустить claude: {e}"))?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("не удалось разобрать вывод `claude auth status`: {e}"))?;
    Ok(AuthStatus {
        logged_in: v["loggedIn"].as_bool().unwrap_or(false),
        email: v["email"].as_str().map(String::from),
        org_name: v["orgName"].as_str().map(String::from),
        subscription: v["subscriptionType"].as_str().map(String::from),
        auth_method: v["authMethod"].as_str().map(String::from),
    })
}

/// Текст `/usage` с процентами израсходованных лимитов (сессия/неделя) и
/// разбивкой по тому, что их наполняет. Операция локальная: не тратит токены
/// и не жжёт лимит (в JSON-обёртке `total_cost_usd = 0`).
pub fn usage() -> Result<String, String> {
    let out = Command::new("claude")
        .args(["-p", "/usage", "--output-format", "json"])
        .output()
        .map_err(|e| format!("не удалось запустить claude: {e}"))?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("не удалось разобрать вывод `/usage`: {e}"))?;
    if v["is_error"].as_bool().unwrap_or(false) {
        return Err(format!(
            "claude вернул ошибку: {}",
            v["result"].as_str().unwrap_or("?")
        ));
    }
    let text = v["result"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        return Err("пустой вывод /usage".into());
    }
    Ok(text)
}

/// Запущенный `claude auth login`, ждущий код авторизации на stdin.
pub struct LoginProc {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Запускает `claude auth login --claudeai`, вычитывает из stdout ссылку
/// авторизации и возвращает (процесс, url). Процесс остаётся жить и ждёт код на
/// stdin — передайте его в [`login_finish`]. Строку `Paste code here >` НЕ
/// вычитываем (у неё нет перевода строки — read_line на ней бы заблокировался).
pub fn login_start() -> Result<(LoginProc, String), String> {
    let mut child = Command::new("claude")
        .args(["auth", "login", "--claudeai"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("не удалось запустить `claude auth login`: {e}"))?;
    let stdin = child.stdin.take().ok_or("нет stdin у claude")?;
    let stdout = child.stdout.take().ok_or("нет stdout у claude")?;
    let mut reader = BufReader::new(stdout);

    let mut url = String::new();
    // Ссылка появляется в первых строках; читаем ограниченно, чтобы не зависнуть.
    for _ in 0..10 {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF — процесс завершился, ссылки не будет
            Ok(_) => {
                if let Some(idx) = line.find("https://") {
                    url = line[idx..].trim().to_string();
                    break;
                }
            }
            Err(e) => return Err(format!("чтение вывода claude: {e}")),
        }
    }
    if url.is_empty() {
        let _ = child.kill();
        let _ = child.wait();
        return Err("не удалось получить ссылку авторизации из вывода claude".into());
    }
    Ok((
        LoginProc {
            child,
            stdin,
            stdout: reader,
        },
        url,
    ))
}

/// Дописывает код в stdin процесса логина, закрывает stdin (чтобы claude увидел
/// EOF и не завис на повторном запросе) и дожидается завершения. Возвращает
/// хвост вывода claude.
pub fn login_finish(mut proc: LoginProc, code: &str) -> Result<String, String> {
    writeln!(proc.stdin, "{}", code.trim()).map_err(|e| format!("запись кода: {e}"))?;
    let _ = proc.stdin.flush();
    drop(proc.stdin); // EOF на stdin

    let mut rest = String::new();
    for line in proc.stdout.lines() {
        match line {
            Ok(l) => {
                rest.push_str(l.trim());
                rest.push('\n');
            }
            Err(_) => break,
        }
    }
    let status = proc
        .child
        .wait()
        .map_err(|e| format!("ожидание claude: {e}"))?;
    let rest = rest.trim().to_string();
    if status.success() {
        Ok(rest)
    } else {
        Err(format!(
            "`claude auth login` завершился с ошибкой (код {:?}). Вывод:\n{}",
            status.code(),
            if rest.is_empty() { "—" } else { &rest }
        ))
    }
}

impl LoginProc {
    /// Прервать флоу логина (по /cancel или при старте нового).
    pub fn abort(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
