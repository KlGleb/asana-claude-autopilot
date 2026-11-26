//! Тест-менеджмент gleb_tasks из CLI: `tracker test …` — тест-кейсы, прогоны,
//! результаты с видео/скриншотами и баги из упавших кейсов. Сессии автопилота
//! отчитываются о тестах сюда, а не комментариями к задаче.
//!
//! Здесь разбор аргументов (`parse`), выполнение (`execute`) и plain-text вывод;
//! сырые GraphQL-вызовы — в `gleb_tasks.rs`. Работает только с бэкендом
//! gleb_tasks (нужен модуль «Релизы» в workspace); для Asana — понятная ошибка.

use crate::gleb_tasks::GlebTasksApi;
use crate::{human_size, read_text, ApiError, Client, Result, Upload};
use serde_json::{json, Map, Value};
use std::path::Path;

pub const USAGE: &str = r#"tracker test — тест-кейсы и прогоны gleb_tasks (`tracker` и `asana` — один бинарь).
Только для tracker gleb_tasks; в workspace должен быть включён модуль «Релизы».
Каталог кейсов в трекере — источник истины; ключ кейса — S17, <run> — id прогона
(или его URL …/test-runs/<id>).

КОМАНДЫ:
  tracker test cases [--suite <имя>] [--key S17]…
                                       Список кейсов workspace проекта: ключ, название,
                                       платформы, автоматизация и путь к сценарию, набор.
                                       --suite — набор с поднаборами (имя или путь
                                       «Родитель / Набор»); --key — только эти ключи.
  tracker test case <key>              Кейс целиком: предусловия, шаги (действие / ожидание),
                                       платформы, автоматизация, путь к сценарию,
                                       последние результаты по платформам.
  tracker test upsert <file.json|->    Массовый upsert кейсов по key (`-` = stdin). JSON —
                                       массив кейсов (или {"cases": […]}):
                                       {"key": "S17", "title": "…", "suite": "Родитель / Набор",
                                        "preconditions": "…", "steps": [{"action": "…", "expected": "…"}],
                                        "platforms": ["web", "ios", "android"],
                                        "priority": "none|low|medium|high|urgent",
                                        "automation_status": "automated|partial|manual",
                                        "automation_ref": "e2e/login.yaml", "archived": false}
                                       Обязателен только key; не указанные поля не меняются.
  tracker test run start --platform <web|ios|android> --build <b>
        [--release <версия или id проекта>] [--title <t>] <key…|--suite <имя>…|--all>
                                       Начать прогон: по результату untested на каждый кейс.
                                       Кейсы — ключами, наборами (--suite, можно несколько)
                                       или --all (все активные кейсы платформы).
                                       Печатает id и URL прогона.
  tracker test result <run> <key> <passed|failed|blocked|skipped>
        [--step N=<status>[:<комментарий>]]… [--note <text|->] [--attach <file>]…
                                       Результат кейса в прогоне (кейс, которого в прогоне
                                       нет, добавляется). --step — статус шага N (с 1);
                                       --note — комментарий результата (`-` = stdin);
                                       --attach — видео/скриншоты/логи как вложения результата.
  tracker test run finish <run>        Завершить прогон (результаты становятся read-only),
                                       печатает счётчики.
  tracker test bug <run> <key> [--step N] [--project <id|имя>]
                                       Задача-баг из результата: шаги воспроизведения до шага N
                                       (по умолчанию — первый упавший), ссылка на прогон,
                                       тег bug, вложения результата. --project — куда положить
                                       (по умолчанию — текущий спринт). Печатает ключ задачи.

ПРИМЕР:
  tracker test run start --platform android --build "1.4 (77)" --release 1.4 S1 S2
  tracker test result <run> S1 passed --attach out/S1.mp4
  tracker test result <run> S2 failed --step 1=passed --step 2=failed:"белый экран" \
        --note "падает после логина" --attach out/S2.mp4 --attach out/S2.png
  tracker test bug <run> S2 --step 2
  tracker test run finish <run>
"#;

/// Ошибка разбора аргументов (печатается как есть).
pub type ParseResult<T> = std::result::Result<T, String>;

#[derive(Debug, PartialEq)]
pub enum Cmd {
    Cases { suite: Option<String>, keys: Vec<String> },
    Case { key: String },
    /// Путь к JSON-файлу или `-`.
    Upsert { source: String },
    RunStart(RunStart),
    RunFinish { run: String },
    Result(ResultArgs),
    Bug { run: String, key: String, step: Option<u32>, project: Option<String> },
}

#[derive(Debug, PartialEq)]
pub struct RunStart {
    /// WEB | IOS | ANDROID.
    pub platform: String,
    pub build: String,
    pub release: Option<String>,
    pub title: Option<String>,
    pub keys: Vec<String>,
    pub suites: Vec<String>,
    pub all: bool,
}

#[derive(Debug, PartialEq)]
pub struct ResultArgs {
    pub run: String,
    pub key: String,
    /// PASSED | FAILED | BLOCKED | SKIPPED | UNTESTED.
    pub status: String,
    pub steps: Vec<StepArg>,
    /// Текст или `-` (stdin).
    pub note: Option<String>,
    pub files: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub struct StepArg {
    /// Номер шага с 1.
    pub n: usize,
    pub status: String,
    pub comment: Option<String>,
}

// ============================ Разбор аргументов ============================

/// Позиционные аргументы и флаги: `--flag value`, `--flag=value`, `--` отключает флаги.
struct Split {
    pos: Vec<String>,
    opts: Vec<(String, String)>,
    switches: Vec<String>,
}

impl Split {
    fn new(args: &[String], value_flags: &[&str], switches: &[&str]) -> ParseResult<Split> {
        let mut out = Split { pos: Vec::new(), opts: Vec::new(), switches: Vec::new() };
        let mut flags_done = false;
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if !flags_done && a == "--" {
                flags_done = true;
            } else if !flags_done && a.starts_with("--") {
                let (name, inline) = match a.split_once('=') {
                    Some((n, v)) => (n, Some(v.to_string())),
                    None => (a.as_str(), None),
                };
                if value_flags.contains(&name) {
                    let v = match inline {
                        Some(v) => v,
                        None => {
                            i += 1;
                            args.get(i).cloned().ok_or(format!("{name}: нужно значение"))?
                        }
                    };
                    if v.trim().is_empty() {
                        return Err(format!("{name}: пустое значение"));
                    }
                    out.opts.push((name.to_string(), v));
                } else if switches.contains(&name) && inline.is_none() {
                    out.switches.push(name.to_string());
                } else {
                    return Err(format!("неизвестный флаг {name}"));
                }
            } else {
                out.pos.push(a.clone());
            }
            i += 1;
        }
        Ok(out)
    }

    fn many(&self, name: &str) -> Vec<String> {
        self.opts.iter().filter(|(n, _)| n == name).map(|(_, v)| v.clone()).collect()
    }

    fn one(&self, name: &str) -> ParseResult<Option<String>> {
        let mut all = self.many(name);
        if all.len() > 1 {
            return Err(format!("{name} указан дважды"));
        }
        Ok(all.pop())
    }

    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }
}

/// Ключ кейса как его хранит трекер: без пробелов, в верхнем регистре.
fn case_key(s: &str) -> ParseResult<String> {
    let k = s.trim().to_ascii_uppercase();
    if k.is_empty() || k.len() > 32 || !k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err(format!("«{s}» — не ключ тест-кейса (ожидается вид S17)"));
    }
    Ok(k)
}

fn platform(s: &str) -> ParseResult<String> {
    match s.trim().to_ascii_lowercase().as_str() {
        p @ ("web" | "ios" | "android") => Ok(p.to_ascii_uppercase()),
        other => Err(format!("платформа «{other}»: ожидается web | ios | android")),
    }
}

fn status(s: &str) -> ParseResult<String> {
    match s.trim().to_ascii_lowercase().as_str() {
        p @ ("passed" | "failed" | "blocked" | "skipped" | "untested") => Ok(p.to_ascii_uppercase()),
        other => Err(format!("статус «{other}»: ожидается passed | failed | blocked | skipped")),
    }
}

/// `N=<status>[:<комментарий>]`.
fn step_arg(s: &str) -> ParseResult<StepArg> {
    let bad = || format!("--step «{s}»: ожидается N=<status>[:<комментарий>], например 2=failed:\"белый экран\"");
    let (n, rest) = s.split_once('=').ok_or_else(bad)?;
    let n: usize = n.trim().parse().ok().filter(|n| *n >= 1).ok_or_else(bad)?;
    let (st, comment) = match rest.split_once(':') {
        Some((st, c)) => (st, Some(c.trim().to_string()).filter(|c| !c.is_empty())),
        None => (rest, None),
    };
    Ok(StepArg { n, status: status(st)?, comment })
}

/// id прогона из аргумента: сам id либо URL `…/test-runs/<id>[?result=…]`.
pub fn run_id(arg: &str) -> String {
    let s = arg.trim();
    let s = match s.split_once("/test-runs/") {
        Some((_, tail)) => tail,
        None => s,
    };
    s.split(['?', '#', '/']).next().unwrap_or(s).to_string()
}

fn run_arg(s: &str) -> ParseResult<String> {
    let id = run_id(s);
    if !is_uuid(&id) {
        return Err(format!("«{s}» — не id прогона (ожидается uuid из `test run start` или URL …/test-runs/<id>)"));
    }
    Ok(id)
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Разбор `test <…>` (args — без самого слова `test`).
pub fn parse(args: &[String]) -> ParseResult<Cmd> {
    let (sub, rest) = args.split_first().ok_or("usage: tracker test <cases|case|upsert|run|result|bug> … (см. tracker test --help)")?;
    match sub.as_str() {
        "cases" => {
            let s = Split::new(rest, &["--suite", "--key"], &[])?;
            if let Some(extra) = s.pos.first() {
                return Err(format!("лишний аргумент «{extra}»\nusage: tracker test cases [--suite <имя>] [--key S17]…"));
            }
            let mut keys = Vec::new();
            for k in s.many("--key") {
                for part in k.split(',').filter(|p| !p.trim().is_empty()) {
                    keys.push(case_key(part)?);
                }
            }
            Ok(Cmd::Cases { suite: s.one("--suite")?, keys })
        }
        "case" => {
            let s = Split::new(rest, &[], &[])?;
            match s.pos.as_slice() {
                [key] => Ok(Cmd::Case { key: case_key(key)? }),
                _ => Err("usage: tracker test case <key>".into()),
            }
        }
        "upsert" => match rest {
            [source] => Ok(Cmd::Upsert { source: source.clone() }),
            _ => Err("usage: tracker test upsert <file.json|->".into()),
        },
        "run" => {
            let (action, rest) = rest.split_first().ok_or("usage: tracker test run <start|finish> …")?;
            match action.as_str() {
                "start" => parse_run_start(rest),
                "finish" => {
                    let s = Split::new(rest, &[], &[])?;
                    match s.pos.as_slice() {
                        [run] => Ok(Cmd::RunFinish { run: run_arg(run)? }),
                        _ => Err("usage: tracker test run finish <run>".into()),
                    }
                }
                other => Err(format!("неизвестная команда «test run {other}» (ожидается start | finish)")),
            }
        }
        "result" => parse_result(rest),
        "bug" => {
            const U: &str = "usage: tracker test bug <run> <key> [--step N] [--project <id|имя>]";
            let s = Split::new(rest, &["--step", "--project"], &[])?;
            let [run, key] = s.pos.as_slice() else { return Err(U.into()) };
            let step = match s.one("--step")? {
                Some(v) => Some(
                    v.trim()
                        .parse::<u32>()
                        .ok()
                        .filter(|n| *n >= 1)
                        .ok_or(format!("--step «{v}»: ожидается номер шага с 1"))?,
                ),
                None => None,
            };
            Ok(Cmd::Bug { run: run_arg(run)?, key: case_key(key)?, step, project: s.one("--project")? })
        }
        other => Err(format!("неизвестная команда «test {other}» (см. tracker test --help)")),
    }
}

fn parse_run_start(args: &[String]) -> ParseResult<Cmd> {
    const U: &str = "usage: tracker test run start --platform <web|ios|android> --build <b> \
                     [--release <версия или id проекта>] [--title <t>] <key…|--suite <имя>…|--all>";
    let s = Split::new(args, &["--platform", "--build", "--release", "--title", "--suite"], &["--all"])?;
    let platform = platform(&s.one("--platform")?.ok_or(format!("нужен --platform\n{U}"))?)?;
    let build = s.one("--build")?.ok_or(format!("нужен --build\n{U}"))?.trim().to_string();
    let keys = s.pos.iter().map(|k| case_key(k)).collect::<ParseResult<Vec<_>>>()?;
    let suites = s.many("--suite");
    let all = s.has("--all");
    if all && (!keys.is_empty() || !suites.is_empty()) {
        return Err("--all нельзя совмещать с ключами кейсов и --suite".into());
    }
    if !all && keys.is_empty() && suites.is_empty() {
        return Err(format!(
            "не выбраны кейсы: укажи ключи, --suite <имя> или --all (все активные кейсы платформы)\n{U}"
        ));
    }
    Ok(Cmd::RunStart(RunStart {
        platform,
        build,
        release: s.one("--release")?.map(|r| r.trim().to_string()),
        title: s.one("--title")?.map(|t| t.trim().to_string()),
        keys,
        suites,
        all,
    }))
}

fn parse_result(args: &[String]) -> ParseResult<Cmd> {
    const U: &str = "usage: tracker test result <run> <key> <passed|failed|blocked|skipped> \
                     [--step N=<status>[:<комментарий>]]… [--note <text|->] [--attach <file>]…";
    let s = Split::new(args, &["--step", "--note", "--attach"], &[])?;
    let [run, key, st] = s.pos.as_slice() else { return Err(U.into()) };
    let mut steps: Vec<StepArg> = Vec::new();
    for raw in s.many("--step") {
        let step = step_arg(&raw)?;
        if steps.iter().any(|x| x.n == step.n) {
            return Err(format!("--step {} указан дважды", step.n));
        }
        steps.push(step);
    }
    Ok(Cmd::Result(ResultArgs {
        run: run_arg(run)?,
        key: case_key(key)?,
        status: status(st)?,
        steps,
        note: s.one("--note")?,
        files: s.many("--attach"),
    }))
}

// ============================ Upsert: нормализация JSON ============================

/// JSON каталога → массив `TestCaseInput`. Принимает массив кейсов или
/// `{"cases": […]}`; имена полей — snake_case (как в MCP) или camelCase;
/// `suite` — путь набора; значения enum — в любом регистре; шаг — объект
/// `{action, expected}` или просто строка-действие. Неизвестные поля — ошибка
/// (чтобы опечатка не терялась молча).
pub fn normalize_cases(input: &Value) -> ParseResult<Vec<Value>> {
    let list = match input {
        Value::Array(a) => a,
        Value::Object(o) => o
            .get("cases")
            .and_then(Value::as_array)
            .ok_or("ожидается JSON-массив кейсов или объект {\"cases\": […]}")?,
        _ => return Err("ожидается JSON-массив кейсов или объект {\"cases\": […]}".into()),
    };
    if list.is_empty() {
        return Err("в файле нет ни одного кейса".into());
    }
    list.iter().enumerate().map(|(i, c)| normalize_case(c).map_err(|e| format!("кейс #{}: {e}", i + 1))).collect()
}

fn upper_enum(v: &Value, field: &str, allowed: &[&str]) -> ParseResult<Value> {
    let s = v.as_str().map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
    if !allowed.contains(&s.as_str()) {
        return Err(format!("{field}: «{}» — ожидается {}", v.as_str().unwrap_or(&v.to_string()), allowed.join(" | ")));
    }
    Ok(json!(s.to_ascii_uppercase()))
}

fn normalize_case(c: &Value) -> ParseResult<Value> {
    let obj = c.as_object().ok_or("ожидается объект")?;
    let mut out = Map::new();
    for (name, v) in obj {
        let target = match name.as_str() {
            "key" => "key",
            "title" => "title",
            "suite" | "suite_path" | "suitePath" => "suitePath",
            "suite_id" | "suiteId" => "suiteId",
            "preconditions" => "preconditions",
            "steps" => "steps",
            "platforms" => "platforms",
            "priority" => "priority",
            "automation_status" | "automationStatus" => "automationStatus",
            "automation_ref" | "automationRef" => "automationRef",
            "archived" => "archived",
            other => return Err(format!("неизвестное поле «{other}»")),
        };
        // null = «не менять» — кроме suiteId, где null значит «без набора».
        if v.is_null() && target != "suiteId" {
            continue;
        }
        let value = match target {
            "key" => json!(case_key(v.as_str().ok_or("key: ожидается строка")?)?),
            "platforms" => {
                let list = v.as_array().ok_or("platforms: ожидается массив")?;
                Value::Array(
                    list.iter()
                        .map(|p| upper_enum(p, "platforms", &["web", "ios", "android"]))
                        .collect::<ParseResult<Vec<_>>>()?,
                )
            }
            "priority" => upper_enum(v, "priority", &["none", "low", "medium", "high", "urgent"])?,
            "automationStatus" => upper_enum(v, "automation_status", &["automated", "partial", "manual"])?,
            "steps" => {
                let list = v.as_array().ok_or("steps: ожидается массив")?;
                Value::Array(list.iter().enumerate().map(|(i, s)| normalize_step(s, i + 1)).collect::<ParseResult<Vec<_>>>()?)
            }
            _ => v.clone(),
        };
        out.insert(target.to_string(), value);
    }
    if !out.contains_key("key") {
        return Err("нет поля key (upsert идёт по ключу, напр. \"S17\")".into());
    }
    Ok(Value::Object(out))
}

fn normalize_step(s: &Value, n: usize) -> ParseResult<Value> {
    match s {
        Value::String(action) => Ok(json!({ "action": action })),
        Value::Object(o) => {
            if let Some(extra) = o.keys().find(|k| !matches!(k.as_str(), "action" | "expected")) {
                return Err(format!("шаг {n}: неизвестное поле «{extra}»"));
            }
            let action = o.get("action").and_then(Value::as_str).ok_or(format!("шаг {n}: нет поля action"))?;
            let mut step = json!({ "action": action });
            if let Some(e) = o.get("expected").filter(|e| !e.is_null()) {
                step["expected"] = json!(e.as_str().ok_or(format!("шаг {n}: expected — ожидается строка"))?);
            }
            Ok(step)
        }
        _ => Err(format!("шаг {n}: ожидается объект {{action, expected}} или строка")),
    }
}

// ============================ Резолв имён в id ============================

fn err<T>(msg: String) -> Result<T> {
    Err(ApiError(msg))
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Путь набора «Родитель / Набор» по плоскому списку с parentId.
fn suite_path(suites: &[Value], suite: &Value) -> String {
    let mut parts = vec![suite["name"].as_str().unwrap_or_default().to_string()];
    let mut parent = suite["parentId"].as_str();
    // Ограничение глубины — на случай цикла в данных.
    for _ in 0..32 {
        let Some(pid) = parent else { break };
        let Some(p) = suites.iter().find(|s| s["id"].as_str() == Some(pid)) else { break };
        parts.push(p["name"].as_str().unwrap_or_default().to_string());
        parent = p["parentId"].as_str();
    }
    parts.reverse();
    parts.join(" / ")
}

/// id набора по имени или пути «Родитель / Набор» (без учёта регистра).
pub fn resolve_suite(suites: &[Value], name: &str) -> Result<String> {
    let wanted: Vec<String> = name.split('/').map(norm).filter(|p| !p.is_empty()).collect();
    let matches: Vec<&Value> = suites
        .iter()
        .filter(|s| {
            if wanted.len() > 1 {
                suite_path(suites, s).split(" / ").map(norm).collect::<Vec<_>>() == wanted
            } else {
                Some(&norm(s["name"].as_str().unwrap_or_default())) == wanted.first()
            }
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok(one["id"].as_str().unwrap_or_default().to_string()),
        [] => {
            let known: Vec<String> = suites.iter().map(|s| suite_path(suites, s)).collect();
            err(if known.is_empty() {
                format!("набор «{name}» не найден: в workspace нет наборов кейсов")
            } else {
                format!("набор «{name}» не найден. Есть наборы: {}", known.join("; "))
            })
        }
        many => err(format!(
            "набор «{name}» неоднозначен: {} — укажи путь «Родитель / Набор»",
            many.iter().map(|s| suite_path(suites, s)).collect::<Vec<_>>().join("; ")
        )),
    }
}

/// id проекта-релиза по версии или имени проекта (как при импорте из CI):
/// сначала неархивные, затем совпадение по версии, затем самый новый.
pub fn resolve_release(projects: &[Value], release: &str) -> Result<String> {
    let want = norm(release);
    let version_hit = |p: &Value| p["release"]["version"].as_str().map(norm).as_deref() == Some(want.as_str());
    let mut found: Vec<&Value> = projects
        .iter()
        .filter(|p| p["group"]["kind"].as_str() == Some("RELEASES"))
        .filter(|p| version_hit(p) || norm(p["name"].as_str().unwrap_or_default()) == want)
        .collect();
    found.sort_by(|a, b| {
        let rank = |p: &Value| (!p["archivedAt"].is_null(), !version_hit(p));
        rank(a)
            .cmp(&rank(b))
            .then_with(|| b["createdAt"].as_str().unwrap_or_default().cmp(a["createdAt"].as_str().unwrap_or_default()))
    });
    match found.first() {
        Some(p) => Ok(p["id"].as_str().unwrap_or_default().to_string()),
        None => {
            let known: Vec<String> = projects
                .iter()
                .filter(|p| p["group"]["kind"].as_str() == Some("RELEASES") && p["archivedAt"].is_null())
                .map(|p| match p["release"]["version"].as_str().filter(|v| !v.is_empty()) {
                    Some(v) => format!("{v} ({})", p["name"].as_str().unwrap_or_default()),
                    None => p["name"].as_str().unwrap_or_default().to_string(),
                })
                .collect();
            err(if known.is_empty() {
                format!("релиз «{release}» не найден: в workspace нет релизов (проектов группы «Релизы»)")
            } else {
                format!("релиз «{release}» не найден. Есть релизы: {}", known.join("; "))
            })
        }
    }
}

/// id проекта по точному имени (без учёта регистра) среди неархивных.
pub fn resolve_project(projects: &[Value], name: &str) -> Result<String> {
    let want = norm(name);
    let found: Vec<&Value> = projects
        .iter()
        .filter(|p| p["archivedAt"].is_null() && norm(p["name"].as_str().unwrap_or_default()) == want)
        .collect();
    match found.as_slice() {
        [one] => Ok(one["id"].as_str().unwrap_or_default().to_string()),
        [] => err(format!("проект «{name}» не найден в workspace (укажи точное имя или id)")),
        _ => err(format!("проектов с именем «{name}» несколько — укажи id проекта")),
    }
}

// ============================ Вывод ============================

fn low(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_lowercase()
}

fn platforms_label(v: &Value) -> String {
    let list: Vec<String> = v.as_array().into_iter().flatten().map(low).collect();
    if list.is_empty() {
        "все платформы".into()
    } else {
        list.join(",")
    }
}

fn automation_label(c: &Value) -> String {
    let st = low(&c["automationStatus"]);
    match c["automationRef"].as_str().filter(|r| !r.is_empty()) {
        Some(r) => format!("{st} → {r}"),
        None => st,
    }
}

/// Набор кейса полным путём («Родитель / Набор»); без списка наборов — просто имя.
fn suite_label(suites: &[Value], c: &Value) -> Option<String> {
    let id = c["suite"]["id"].as_str()?;
    match suites.iter().find(|s| s["id"].as_str() == Some(id)) {
        Some(s) => Some(suite_path(suites, s)),
        None => c["suite"]["name"].as_str().map(str::to_string),
    }
}

/// Строка списка: `S17 · Название · web,ios · automated → e2e/x.yaml · набор: Заказы / Оплата`.
pub fn case_line(c: &Value, suites: &[Value]) -> String {
    let mut parts = vec![
        c["key"].as_str().unwrap_or_default().to_string(),
        c["title"].as_str().unwrap_or_default().to_string(),
        platforms_label(&c["platforms"]),
        automation_label(c),
    ];
    if let Some(s) = suite_label(suites, c) {
        parts.push(format!("набор: {s}"));
    }
    parts.join(" · ")
}

/// Кейс целиком (markdown).
pub fn case_text(c: &Value, suites: &[Value]) -> String {
    let mut out = format!(
        "# {} — {} (v{})\n",
        c["key"].as_str().unwrap_or_default(),
        c["title"].as_str().unwrap_or_default(),
        c["version"].as_i64().unwrap_or(1)
    );
    if !c["archivedAt"].is_null() {
        out.push_str("**В архиве**\n");
    }
    if let Some(s) = suite_label(suites, c) {
        out.push_str(&format!("**Набор:** {s}\n"));
    }
    out.push_str(&format!("**Платформы:** {}\n", platforms_label(&c["platforms"])));
    out.push_str(&format!("**Приоритет:** {}\n", low(&c["priority"])));
    out.push_str(&format!("**Автоматизация:** {}\n", low(&c["automationStatus"])));
    if let Some(r) = c["automationRef"].as_str().filter(|r| !r.is_empty()) {
        out.push_str(&format!("**Сценарий:** {r}\n"));
    }
    if let Some(p) = c["preconditions"].as_str().map(str::trim).filter(|p| !p.is_empty()) {
        out.push_str(&format!("\n## Предусловия\n{p}\n"));
    }
    out.push_str("\n## Шаги\n");
    let steps = c["steps"].as_array().cloned().unwrap_or_default();
    if steps.is_empty() {
        out.push_str("Шагов нет.\n");
    }
    for (i, s) in steps.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, s["action"].as_str().unwrap_or_default().trim()));
        if let Some(e) = s["expected"].as_str().map(str::trim).filter(|e| !e.is_empty()) {
            out.push_str(&format!("   → ожидается: {e}\n"));
        }
    }
    let last = c["lastResults"].as_array().cloned().unwrap_or_default();
    if !last.is_empty() {
        out.push_str("\n## Последние результаты\n");
        for r in &last {
            let build = r["run"]["build"].as_str().filter(|b| !b.is_empty()).map(|b| format!(", build {b}")).unwrap_or_default();
            let date: String = r["updatedAt"].as_str().unwrap_or_default().replace('T', " ").chars().take(16).collect();
            out.push_str(&format!(
                "- {}: {} ({date}{build}, прогон {})\n",
                low(&r["run"]["platform"]),
                low(&r["status"]),
                r["run"]["id"].as_str().unwrap_or_default()
            ));
        }
    }
    out
}

/// `всего 5: passed 3, failed 1, blocked 0, skipped 1, untested 0`.
pub fn counts_line(c: &Value) -> String {
    let n = |k: &str| c[k].as_i64().unwrap_or(0);
    format!(
        "всего {}: passed {}, failed {}, blocked {}, skipped {}, untested {}",
        n("total"),
        n("passed"),
        n("failed"),
        n("blocked"),
        n("skipped"),
        n("untested")
    )
}

/// Шаги результата одной строкой: `1 passed · 2 failed (белый экран) · 3 untested`.
fn steps_line(result: &Value) -> Option<String> {
    let steps = result["steps"].as_array()?;
    if steps.iter().all(|s| s["status"].as_str() == Some("UNTESTED")) {
        return None;
    }
    Some(
        steps
            .iter()
            .map(|s| {
                let comment = s["comment"].as_str().filter(|c| !c.is_empty()).map(|c| format!(" ({c})")).unwrap_or_default();
                format!("{} {}{comment}", s["step"].as_i64().unwrap_or(0), low(&s["status"]))
            })
            .collect::<Vec<_>>()
            .join(" · "),
    )
}

fn run_url(api: &GlebTasksApi, run: &str) -> String {
    format!("{}/test-runs/{run}", api.base_url())
}

// ============================ Выполнение ============================

/// Полный список `stepResults` для `setTestResult`: текущие статусы шагов
/// результата с наложенными `--step`. Ошибка, если шага с таким номером нет.
pub fn merge_steps(current: &Value, steps: &[StepArg], key: &str) -> Result<Value> {
    let mut list: Vec<Value> = current
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| json!({ "status": s["status"].as_str().unwrap_or("UNTESTED"), "comment": s["comment"].as_str().unwrap_or("") }))
        .collect();
    for st in steps {
        let total = list.len();
        let slot = list
            .get_mut(st.n - 1)
            .ok_or_else(|| ApiError(format!("--step {}: у кейса {key} шагов: {total}", st.n)))?;
        slot["status"] = json!(st.status);
        if let Some(c) = &st.comment {
            slot["comment"] = json!(c);
        }
    }
    Ok(Value::Array(list))
}

pub fn execute(client: &Client, cmd: Cmd) -> Result<()> {
    let api = client.gleb_tasks_api("`test` (тест-кейсы и прогоны)")?;
    match cmd {
        Cmd::Cases { suite, keys } => {
            let suites = api.test_suites()?;
            let suite_id = match &suite {
                Some(name) => Some(resolve_suite(&suites, name)?),
                None => None,
            };
            let mut cases = api.test_cases(suite_id.as_deref())?;
            if !keys.is_empty() {
                cases.retain(|c| keys.iter().any(|k| c["key"].as_str() == Some(k.as_str())));
            }
            if cases.is_empty() {
                println!("Кейсов нет.");
                return Ok(());
            }
            for c in &cases {
                println!("{}", case_line(c, &suites));
            }
            println!("Всего: {}", cases.len());
        }
        Cmd::Case { key } => {
            let case = api.test_case(&key)?;
            // Наборы нужны только ради полного пути набора в заголовке.
            let suites = if case["suite"].is_null() { Vec::new() } else { api.test_suites()? };
            print!("{}", case_text(&case, &suites));
        }
        Cmd::Upsert { source } => {
            let text = if source == "-" {
                read_text("-")
            } else {
                std::fs::read_to_string(&source).map_err(|e| ApiError(format!("{source}: {e}")))?
            };
            let json: Value = serde_json::from_str(&text).map_err(|e| ApiError(format!("{source}: невалидный JSON: {e}")))?;
            let cases = normalize_cases(&json).map_err(|e| ApiError(format!("{source}: {e}")))?;
            let r = api.upsert_test_cases(Value::Array(cases))?;
            let n = |k: &str| r[k].as_i64().unwrap_or(0);
            println!("OK: создано {}, обновлено {}, без изменений {}", n("created"), n("updated"), n("unchanged"));
            let list: Vec<String> = r["cases"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| format!("{} v{}", c["key"].as_str().unwrap_or_default(), c["version"].as_i64().unwrap_or(1)))
                .collect();
            println!("Кейсы: {}", list.join(", "));
        }
        Cmd::RunStart(a) => {
            // AGENT: прогон ведёт сессия автопилота (не человек и не CI).
            let mut input = json!({ "platform": a.platform, "build": a.build, "source": "AGENT" });
            if let Some(t) = a.title.filter(|t| !t.is_empty()) {
                input["title"] = json!(t);
            }
            if let Some(r) = a.release.filter(|r| !r.is_empty()) {
                input["releaseProjectId"] = json!(if is_uuid(&r) {
                    r
                } else {
                    let ws = api.workspace_projects()?;
                    resolve_release(ws["projects"].as_array().map(Vec::as_slice).unwrap_or_default(), &r)?
                });
            }
            if !a.keys.is_empty() {
                input["caseKeys"] = json!(a.keys);
            }
            if !a.suites.is_empty() {
                let suites = api.test_suites()?;
                let ids = a.suites.iter().map(|s| resolve_suite(&suites, s)).collect::<Result<Vec<_>>>()?;
                input["suiteIds"] = json!(ids);
            }
            let run = api.create_test_run(input)?;
            let id = run["id"].as_str().unwrap_or_default();
            let total = run["counts"]["total"].as_i64().unwrap_or(0);
            println!("OK: прогон создан — {}, кейсов: {total}", run["title"].as_str().unwrap_or_default());
            println!("ID: {id}");
            println!("URL: {}", run_url(api, id));
            if total == 0 {
                println!("ВНИМАНИЕ: в прогон не попал ни один кейс (нет активных кейсов под выбор).");
            }
        }
        Cmd::RunFinish { run } => {
            let r = api.finish_test_run(&run)?;
            println!("OK: прогон завершён — {}", counts_line(&r["counts"]));
            println!("URL: {}", run_url(api, &run));
        }
        Cmd::Result(a) => {
            // Сначала проверяем файлы, чтобы не поставить результат без вложений.
            for f in &a.files {
                if !Path::new(f).is_file() {
                    return err(format!("{f}: файл не найден"));
                }
            }
            let note = a.note.as_deref().map(read_text);
            let mut vars = json!({ "runId": a.run, "caseKey": a.key, "status": a.status, "comment": note });
            if !a.steps.is_empty() {
                // Шаги задаются списком целиком: берём текущий результат (пустой вызов
                // заодно добавит кейс в прогон) и накладываем на него --step.
                let cur = api.set_test_result(json!({ "runId": a.run, "caseKey": a.key }))?;
                vars = json!({
                    "resultId": cur["id"],
                    "status": a.status,
                    "comment": note,
                    "stepResults": merge_steps(&cur["steps"], &a.steps, &a.key)?,
                });
            }
            let r = api.set_test_result(vars)?;
            let id = r["id"].as_str().unwrap_or_default();
            println!(
                "OK: {} «{}» → {}",
                r["key"].as_str().unwrap_or(&a.key),
                r["title"].as_str().unwrap_or_default(),
                low(&r["status"])
            );
            if let Some(steps) = steps_line(&r) {
                println!("Шаги: {steps}");
            }
            for f in &a.files {
                let att = api.upload_result_attachment(id, Upload::open(Path::new(f), None)?)?;
                println!("OK: вложение {} ({})", att.name, human_size(att.size));
            }
            println!("URL: {}?result={id}", run_url(api, &a.run));
        }
        Cmd::Bug { run, key, step, project } => {
            let r = api.test_run(&run)?;
            let result = r["results"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|x| x["key"].as_str() == Some(key.as_str()))
                .ok_or_else(|| ApiError(format!("в прогоне {run} нет результата кейса {key}")))?;
            // Повторный вызов (ретрай сессии) не должен плодить дубликаты.
            if let Some(existing) = result["bugTask"]["key"].as_str() {
                println!("OK: по результату {key} уже заведён баг {existing} (новый не создаю)");
                println!("URL: {}/t/{existing}", api.base_url());
                return Ok(());
            }
            let project_id = match project {
                Some(p) if is_uuid(p.trim()) => p.trim().to_string(),
                other => {
                    let ws = api.workspace_projects()?;
                    match other {
                        Some(name) => resolve_project(ws["projects"].as_array().map(Vec::as_slice).unwrap_or_default(), &name)?,
                        None => ws["currentSprint"]["id"].as_str().map(str::to_string).ok_or_else(|| {
                            ApiError("в workspace нет текущего спринта — укажи проект для бага: --project <id|имя>".into())
                        })?,
                    }
                }
            };
            let bug = api.create_bug_from_result(result["id"].as_str().unwrap_or_default(), step, &project_id)?;
            let bug_key = bug["key"].as_str().unwrap_or_default();
            println!("OK: баг {bug_key} — {}", bug["title"].as_str().unwrap_or_default());
            println!("URL: {}/t/{bug_key}", api.base_url());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN: &str = "655cb3d6-1111-4222-8333-444455556666";

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn p(args: &[&str]) -> ParseResult<Cmd> {
        parse(&v(args))
    }

    #[test]
    fn cases_filters() {
        assert_eq!(p(&["cases"]).unwrap(), Cmd::Cases { suite: None, keys: vec![] });
        assert_eq!(
            p(&["cases", "--suite", "Заказы / Оплата", "--key", "s17", "--key=S2,s3"]).unwrap(),
            Cmd::Cases { suite: Some("Заказы / Оплата".into()), keys: v(&["S17", "S2", "S3"]) }
        );
        assert!(p(&["cases", "S17"]).is_err(), "позиционных аргументов нет");
        assert!(p(&["cases", "--suite"]).is_err());
        assert!(p(&["cases", "--suite", "a", "--suite", "b"]).is_err());
        assert!(p(&["cases", "--platform", "web"]).is_err(), "неизвестный флаг");
    }

    #[test]
    fn case_and_upsert() {
        assert_eq!(p(&["case", "s17"]).unwrap(), Cmd::Case { key: "S17".into() });
        assert!(p(&["case"]).is_err());
        assert!(p(&["case", "S1", "S2"]).is_err());
        assert!(p(&["case", "S 17"]).is_err());
        assert_eq!(p(&["upsert", "cases.json"]).unwrap(), Cmd::Upsert { source: "cases.json".into() });
        assert_eq!(p(&["upsert", "-"]).unwrap(), Cmd::Upsert { source: "-".into() });
        assert!(p(&["upsert"]).is_err());
        assert!(p(&["upsert", "a.json", "b.json"]).is_err());
    }

    #[test]
    fn run_start_keys() {
        assert_eq!(
            p(&["run", "start", "--platform", "Android", "--build", "1.4 (77)", "--release", "1.4", "--title=Смоук", "s1", "S2"])
                .unwrap(),
            Cmd::RunStart(RunStart {
                platform: "ANDROID".into(),
                build: "1.4 (77)".into(),
                release: Some("1.4".into()),
                title: Some("Смоук".into()),
                keys: v(&["S1", "S2"]),
                suites: vec![],
                all: false,
            })
        );
    }

    #[test]
    fn run_start_suites_and_all() {
        let Cmd::RunStart(a) = p(&["run", "start", "--suite", "Авторизация", "--platform=web", "--build", "abc123", "--suite", "Заказы"]).unwrap()
        else {
            panic!("ожидался RunStart")
        };
        assert_eq!(a.suites, v(&["Авторизация", "Заказы"]));
        assert!(a.keys.is_empty() && !a.all);
        let Cmd::RunStart(a) = p(&["run", "start", "--platform", "ios", "--build", "7", "--all"]).unwrap() else {
            panic!("ожидался RunStart")
        };
        assert!(a.all);
    }

    #[test]
    fn run_start_errors() {
        assert!(p(&["run", "start", "--build", "1", "S1"]).is_err(), "нет --platform");
        assert!(p(&["run", "start", "--platform", "web", "S1"]).is_err(), "нет --build");
        assert!(p(&["run", "start", "--platform", "windows", "--build", "1", "S1"]).is_err());
        assert!(p(&["run", "start", "--platform", "web", "--build", "1"]).is_err(), "кейсы не выбраны");
        assert!(p(&["run", "start", "--platform", "web", "--build", "1", "--all", "S1"]).is_err());
        assert!(p(&["run", "start", "--platform", "web", "--build", "1", "--all=yes"]).is_err());
        assert!(p(&["run", "start", "--platform", "web", "--platform", "ios", "--build", "1", "S1"]).is_err());
        assert!(p(&["run"]).is_err());
        assert!(p(&["run", "pause", RUN]).is_err());
    }

    #[test]
    fn run_finish_and_run_id() {
        assert_eq!(p(&["run", "finish", RUN]).unwrap(), Cmd::RunFinish { run: RUN.into() });
        let url = format!("https://tasks.gleb.at/test-runs/{RUN}?result=abc&step=2");
        assert_eq!(p(&["run", "finish", &url]).unwrap(), Cmd::RunFinish { run: RUN.into() });
        assert!(p(&["run", "finish"]).is_err());
        assert!(p(&["run", "finish", "S17"]).is_err(), "не uuid");
        assert_eq!(run_id(&format!("https://x.test/test-runs/{RUN}/")), RUN);
        assert_eq!(run_id(&format!(" {RUN} ")), RUN);
    }

    #[test]
    fn result_full() {
        let got = p(&[
            "result", RUN, "s2", "Failed", "--step", "1=passed", "--step", "2=failed:белый экран: после логина", "--note", "-",
            "--attach", "out/S2.mp4", "--attach=out/S2.png",
        ])
        .unwrap();
        assert_eq!(
            got,
            Cmd::Result(ResultArgs {
                run: RUN.into(),
                key: "S2".into(),
                status: "FAILED".into(),
                steps: vec![
                    StepArg { n: 1, status: "PASSED".into(), comment: None },
                    StepArg { n: 2, status: "FAILED".into(), comment: Some("белый экран: после логина".into()) },
                ],
                note: Some("-".into()),
                files: v(&["out/S2.mp4", "out/S2.png"]),
            })
        );
    }

    #[test]
    fn result_minimal_and_errors() {
        let Cmd::Result(a) = p(&["result", RUN, "S1", "passed"]).unwrap() else { panic!("ожидался Result") };
        assert!(a.steps.is_empty() && a.note.is_none() && a.files.is_empty());
        assert!(p(&["result", RUN, "S1"]).is_err(), "нет статуса");
        assert!(p(&["result", RUN, "S1", "ok"]).is_err(), "неизвестный статус");
        assert!(p(&["result", "not-a-run", "S1", "passed"]).is_err());
        assert!(p(&["result", RUN, "S1", "passed", "--step", "0=passed"]).is_err());
        assert!(p(&["result", RUN, "S1", "passed", "--step", "x=passed"]).is_err());
        assert!(p(&["result", RUN, "S1", "passed", "--step", "2"]).is_err());
        assert!(p(&["result", RUN, "S1", "passed", "--step", "2=broken"]).is_err());
        assert!(p(&["result", RUN, "S1", "passed", "--step", "2=passed", "--step", "2=failed"]).is_err());
        assert!(p(&["result", RUN, "S1", "passed", "--note", "a", "--note", "b"]).is_err());
        assert!(p(&["result", RUN, "S1", "passed", "--attach"]).is_err());
    }

    #[test]
    fn bug_args() {
        assert_eq!(
            p(&["bug", RUN, "s2", "--step", "2", "--project", "Спринт 12"]).unwrap(),
            Cmd::Bug { run: RUN.into(), key: "S2".into(), step: Some(2), project: Some("Спринт 12".into()) }
        );
        assert_eq!(p(&["bug", RUN, "S2"]).unwrap(), Cmd::Bug { run: RUN.into(), key: "S2".into(), step: None, project: None });
        assert!(p(&["bug", RUN]).is_err());
        assert!(p(&["bug", RUN, "S2", "--step", "0"]).is_err());
        assert!(p(&["bug", RUN, "S2", "--step", "два"]).is_err());
    }

    #[test]
    fn unknown_subcommand() {
        assert!(p(&[]).is_err());
        assert!(p(&["runs"]).is_err());
    }

    #[test]
    fn upsert_normalization() {
        let input = json!({ "cases": [{
            "key": "s1", "title": "Вход", "suite": "Авторизация / Вход", "preconditions": null,
            "steps": ["Открыть приложение", { "action": "Ввести пароль", "expected": "Главный экран" }],
            "platforms": ["Web", "ANDROID"], "priority": "High",
            "automation_status": "automated", "automationRef": "e2e/login.yaml", "archived": false
        }] });
        assert_eq!(
            normalize_cases(&input).unwrap(),
            vec![json!({
                "key": "S1", "title": "Вход", "suitePath": "Авторизация / Вход",
                "steps": [{ "action": "Открыть приложение" }, { "action": "Ввести пароль", "expected": "Главный экран" }],
                "platforms": ["WEB", "ANDROID"], "priority": "HIGH",
                "automationStatus": "AUTOMATED", "automationRef": "e2e/login.yaml", "archived": false
            })]
        );
        // Голый массив и suite_id: null (= без набора) тоже принимаются.
        assert_eq!(normalize_cases(&json!([{ "key": "S2", "suite_id": null }])).unwrap(), vec![json!({ "key": "S2", "suiteId": null })]);
    }

    #[test]
    fn upsert_errors() {
        assert!(normalize_cases(&json!("S1")).is_err());
        assert!(normalize_cases(&json!([])).is_err());
        assert!(normalize_cases(&json!({ "items": [] })).is_err());
        let e = normalize_cases(&json!([{ "key": "S1" }, { "title": "без ключа" }])).unwrap_err();
        assert!(e.contains("кейс #2") && e.contains("key"), "{e}");
        assert!(normalize_cases(&json!([{ "key": "S1", "titel": "опечатка" }])).unwrap_err().contains("titel"));
        assert!(normalize_cases(&json!([{ "key": "S1", "platforms": ["windows"] }])).is_err());
        assert!(normalize_cases(&json!([{ "key": "S1", "priority": "critical" }])).is_err());
        assert!(normalize_cases(&json!([{ "key": "S1", "steps": [{ "expected": "нет действия" }] }])).is_err());
        assert!(normalize_cases(&json!([{ "key": "S1", "steps": [{ "action": "a", "result": "x" }] }])).is_err());
    }

    fn suites() -> Vec<Value> {
        vec![
            json!({ "id": "a", "parentId": null, "name": "Авторизация" }),
            json!({ "id": "o", "parentId": null, "name": "Заказы" }),
            json!({ "id": "o-pay", "parentId": "o", "name": "Оплата" }),
            json!({ "id": "a-pay", "parentId": "a", "name": "Оплата" }),
        ]
    }

    #[test]
    fn suite_by_name_and_path() {
        let s = suites();
        assert_eq!(resolve_suite(&s, "авторизация").unwrap(), "a");
        assert_eq!(resolve_suite(&s, "Заказы / Оплата").unwrap(), "o-pay");
        assert_eq!(resolve_suite(&s, "авторизация/оплата").unwrap(), "a-pay");
        let e = resolve_suite(&s, "Оплата").unwrap_err().0;
        assert!(e.contains("неоднозначен") && e.contains("Заказы / Оплата"), "{e}");
        let e = resolve_suite(&s, "Профиль").unwrap_err().0;
        assert!(e.contains("не найден") && e.contains("Авторизация"), "{e}");
        assert!(resolve_suite(&[], "Профиль").is_err());
    }

    #[test]
    fn release_by_version_or_name() {
        let projects = vec![
            json!({ "id": "sprint", "name": "1.4", "archivedAt": null, "createdAt": "2026-11-01", "group": { "kind": "SPRINTS" }, "release": null }),
            json!({ "id": "old", "name": "Релиз 1.4", "archivedAt": "2026-10-01", "createdAt": "2026-09-01", "group": { "kind": "RELEASES" }, "release": { "version": "1.4" } }),
            json!({ "id": "named", "name": "1.4", "archivedAt": null, "createdAt": "2026-11-20", "group": { "kind": "RELEASES" }, "release": { "version": "1.4.1" } }),
            json!({ "id": "cur", "name": "Ноябрьский", "archivedAt": null, "createdAt": "2026-11-10", "group": { "kind": "RELEASES" }, "release": { "version": "1.4" } }),
        ];
        // Неархивный с совпавшей версией важнее совпадения по имени и архивного.
        assert_eq!(resolve_release(&projects, "1.4").unwrap(), "cur");
        assert_eq!(resolve_release(&projects, "ноябрьский").unwrap(), "cur");
        assert_eq!(resolve_release(&projects, "1.4.1").unwrap(), "named");
        assert_eq!(resolve_release(&projects, "Релиз 1.4").unwrap(), "old");
        let e = resolve_release(&projects, "2.0").unwrap_err().0;
        assert!(e.contains("1.4 (Ноябрьский)"), "{e}");
    }

    #[test]
    fn project_by_name() {
        let projects = vec![
            json!({ "id": "s1", "name": "Sprint", "archivedAt": null }),
            json!({ "id": "s0", "name": "Sprint", "archivedAt": "2026-10-01" }),
            json!({ "id": "d1", "name": "Дубль", "archivedAt": null }),
            json!({ "id": "d2", "name": "дубль", "archivedAt": null }),
        ];
        assert_eq!(resolve_project(&projects, "sprint").unwrap(), "s1");
        assert!(resolve_project(&projects, "Дубль").is_err());
        assert!(resolve_project(&projects, "Backlog").is_err());
    }

    #[test]
    fn steps_merge() {
        let current = json!([
            { "step": 1, "status": "PASSED", "comment": "ок" },
            { "step": 2, "status": "UNTESTED", "comment": "" },
            { "step": 3, "status": "UNTESTED", "comment": "" },
        ]);
        let steps = vec![
            StepArg { n: 2, status: "FAILED".into(), comment: Some("белый экран".into()) },
            StepArg { n: 1, status: "SKIPPED".into(), comment: None },
        ];
        assert_eq!(
            merge_steps(&current, &steps, "S1").unwrap(),
            json!([
                { "status": "SKIPPED", "comment": "ок" },
                { "status": "FAILED", "comment": "белый экран" },
                { "status": "UNTESTED", "comment": "" },
            ])
        );
        let e = merge_steps(&current, &[StepArg { n: 4, status: "PASSED".into(), comment: None }], "S1").unwrap_err().0;
        assert!(e.contains("S1") && e.contains('3'), "{e}");
    }

    #[test]
    fn rendering() {
        let c = json!({
            "key": "S17", "title": "Оплата заказа", "version": 3, "archivedAt": null,
            "platforms": ["WEB", "IOS"], "priority": "HIGH",
            "automationStatus": "AUTOMATED", "automationRef": "e2e/pay.yaml",
            "suite": { "id": "x", "name": "Заказы" }, "preconditions": "Есть заказ",
            "steps": [{ "action": "Открыть заказ", "expected": "" }, { "action": "Нажать «Оплатить»", "expected": "Чек" }],
            "lastResults": [{ "status": "FAILED", "updatedAt": "2026-11-25T10:00:12Z", "run": { "id": RUN, "platform": "IOS", "build": "77" } }]
        });
        assert_eq!(case_line(&c, &[]), "S17 · Оплата заказа · web,ios · automated → e2e/pay.yaml · набор: Заказы");
        let tree = vec![json!({ "id": "p", "parentId": null, "name": "Магазин" }), json!({ "id": "x", "parentId": "p", "name": "Заказы" })];
        assert!(case_line(&c, &tree).ends_with("набор: Магазин / Заказы"));
        let text = case_text(&c, &tree);
        assert!(text.contains("**Набор:** Магазин / Заказы\n"), "{text}");
        assert!(text.starts_with("# S17 — Оплата заказа (v3)\n"), "{text}");
        assert!(text.contains("1. Открыть заказ\n2. Нажать «Оплатить»\n   → ожидается: Чек\n"), "{text}");
        assert!(text.contains(&format!("- ios: failed (2026-11-25 10:00, build 77, прогон {RUN})")), "{text}");
        let bare = json!({ "key": "S1", "title": "Вход", "platforms": [], "automationStatus": "MANUAL", "automationRef": "", "suite": null });
        assert_eq!(case_line(&bare, &tree), "S1 · Вход · все платформы · manual");
        assert_eq!(
            counts_line(&json!({ "total": 5, "passed": 3, "failed": 1, "blocked": 0, "skipped": 1, "untested": 0 })),
            "всего 5: passed 3, failed 1, blocked 0, skipped 1, untested 0"
        );
    }
}
