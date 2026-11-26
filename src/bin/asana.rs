// Токен-экономная CLI трекера (Asana или gleb_tasks — по `tracker:` в конфиге
// проекта). Заменяет MCP-серверы трекеров в сессиях автопилота. Бинарь
// доступен под двумя именами: `asana` (историческое) и `tracker`.
// Вывод — минимальный plain text; длинные тексты можно подавать через stdin (`-`).
//
// Конфиг проекта (autopilot.yaml) ищется вверх от текущего каталога
// (`<dir>/autopilot/autopilot.yaml`) либо берётся из env AUTOPILOT_DIR —
// поэтому одна глобально установленная `asana` работает во всех проектах.

use autopilot_core::*;
use std::process::exit;

const USAGE: &str = r#"asana — лёгкая CLI трекера задач: Asana или gleb_tasks, по `tracker:` в конфиге
(autopilot/autopilot.yaml проекта, поиск вверх от cwd или AUTOPILOT_DIR). Алиас: `tracker`.
<gid> — gid задачи в Asana либо ключ/uuid задачи в gleb_tasks (напр. SAOBR-12).

КОМАНДЫ:
  asana task <gid>                     Полный контекст задачи: описание, сабтаски,
                                       комментарии, история, связанные задачи.
  asana comment <gid> <text|->         Добавить комментарий (`-` = читать из stdin).
  asana move <gid> <role>              Переместить в секцию по роли
                                       (todo|in_progress|reopen|qa|done|blocked — как в конфиге).
  asana assign <gid> <operator|agent|none>
                                       (также принимаются метки из конфига, напр. gleb|claude)
  asana complete <gid>                 Закрыть задачу/сабтаску (completed=true).
  asana subtask <parent_gid> <name> [--notes <text|->] [--assignee operator|agent]
                                       Создать сабтаску, печатает её GID.
  asana download <attachment_gid> [dir]
                                       Скачать вложение (скриншот и т.п.) в dir
                                       (по умолчанию — текущая папка), печатает путь.
                                       Asana: в get_asset?asset_id=NNN ссылках NNN = gid вложения.
                                       gleb_tasks: идентификатор вида <задача>/<id>, как в контексте.
  asana attach <gid> <file>… [--name <имя>]
                                       Приложить файл(ы) к задаче (видео прогона, скриншоты,
                                       логи); печатает id и имя каждого вложения. --name —
                                       имя вложения (только для одного файла). Content-Type —
                                       по расширению. Лимит: gleb_tasks 500 MB, Asana 100 MB.
  asana qa <gid> <comment|->           Комментарий + перенос в qa + assign оператору.
  asana done <gid> <comment|->         Комментарий + перенос в done + assign оператору.
  asana block <gid> <comment|->        Комментарий + assign оператору + перенос в blocked.
  asana fields                         (gleb_tasks) Создать в workspace поля «Model» и «Effort»
                                       (SELECT, для исполнителя Claude Code, необязательные) —
                                       из них демон берёт модель и эффорт сессии. Имена полей
                                       настраиваются в autopilot.yaml: fields: {model, effort}.
  asana test <…>                       (gleb_tasks) Тест-кейсы и прогоны: отчёт о тестах в
                                       прогон трекера вместо комментариев. Подробно:
                                       `asana test --help`.
      test cases [--suite <имя>] [--key S17]…       список кейсов
      test case <key>                               кейс целиком (шаги, платформы, сценарий)
      test upsert <file.json|->                     массовый upsert кейсов по key
      test run start --platform <web|ios|android> --build <b> [--release <версия|id>]
                     [--title <t>] <key…|--suite <имя>…|--all>   → id и URL прогона
      test result <run> <key> <passed|failed|blocked|skipped>
                     [--step N=<status>[:<комментарий>]]… [--note <text|->] [--attach <file>]…
      test run finish <run>                         завершить прогон
      test bug <run> <key> [--step N] [--project <id|имя>]       → ключ задачи-бага

Токен: env-переменная из autopilot.yaml (asana.token_env, по умолчанию ASANA_ACCESS_TOKEN;
для gleb_tasks — gleb_tasks.api_key_env, по умолчанию GLEB_TASKS_API_KEY).
"#;

fn die(msg: &str) -> ! {
    eprintln!("ERROR: {msg}");
    exit(1);
}

/// Разбор метки исполнителя: operator|agent|none + настраиваемые алиасы из конфига.
/// Возвращает Some(Some(gid)) / Some(None=снять) / None (неизвестно).
fn resolve_assignee(s: &Settings, who: &str) -> Option<Option<String>> {
    let w = who.to_lowercase();
    if w == "none" {
        Some(None)
    } else if w == "operator" || w == s.operator_label.to_lowercase() {
        Some(Some(s.operator_gid.clone()))
    } else if w == "agent" || w == s.agent_label.to_lowercase() {
        Some(Some(s.agent_gid.clone()))
    } else {
        None
    }
}

/// Аргументы `attach`: задача, файлы, необязательное имя вложения.
#[derive(Debug, PartialEq)]
struct AttachArgs {
    task: String,
    files: Vec<String>,
    name: Option<String>,
}

const ATTACH_USAGE: &str = "usage: asana attach <gid> <file>… [--name <имя>]";

/// Разбор `attach <gid> <file>… [--name <имя>]` (args — без самой команды).
/// `--name` можно указать в любом месте; `--` отключает разбор флагов дальше.
fn parse_attach_args(args: &[String]) -> std::result::Result<AttachArgs, String> {
    let mut positional: Vec<String> = Vec::new();
    let mut name: Option<String> = None;
    let mut flags_done = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if !flags_done && a == "--" {
            flags_done = true;
        } else if !flags_done && (a == "--name" || a.starts_with("--name=")) {
            let v = match a.strip_prefix("--name=") {
                Some(v) => v.to_string(),
                None => {
                    i += 1;
                    args.get(i).cloned().ok_or("--name needs a value")?
                }
            };
            if v.trim().is_empty() {
                return Err("--name: пустое имя".into());
            }
            if name.replace(v).is_some() {
                return Err("--name указан дважды".into());
            }
        } else if !flags_done && a.starts_with("--") {
            return Err(format!("unknown flag {a}"));
        } else {
            positional.push(a.clone());
        }
        i += 1;
    }
    let mut it = positional.into_iter();
    let task = it.next().ok_or(ATTACH_USAGE)?;
    let files: Vec<String> = it.collect();
    if files.is_empty() {
        return Err(format!("нужен хотя бы один файл\n{ATTACH_USAGE}"));
    }
    if name.is_some() && files.len() > 1 {
        return Err("--name можно указать только при одном файле".into());
    }
    Ok(AttachArgs { task, files, name })
}

/// Управляющий проект задачи (для резолва целевой секции по роли).
fn project_of(client: &Client, gid: &str) -> Result<String> {
    let task = client.get_task(gid)?;
    client
        .placement(&task)
        .map(|p| p.project_gid)
        .ok_or_else(|| ApiError(format!("задача {gid} не состоит ни в одном проекте")))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `asana test …` — тест-менеджмент gleb_tasks: своя справка, а аргументы
    // разбираются до загрузки конфига (ошибка в них не должна стоить запроса в сеть).
    if args.first().is_some_and(|a| a == "test") {
        let rest = &args[1..];
        let help = rest.iter().take_while(|a| *a != "--").any(|a| a == "--help" || a == "-h");
        if rest.is_empty() || rest[0] == "help" || help {
            print!("{}", testing::USAGE);
            return;
        }
        let cmd = testing::parse(rest).unwrap_or_else(|e| die(&e));
        let client = Client::load().unwrap_or_else(|e| die(&e.to_string()));
        if let Err(e) = testing::execute(&client, cmd) {
            die(&e.to_string());
        }
        return;
    }
    // `asana <команда> --help|-h` — та же общая справка (до загрузки конфига).
    let sub_help = args.get(1).is_some_and(|a| a == "--help" || a == "-h");
    if args.is_empty() || args[0] == "help" || args[0] == "--help" || args[0] == "-h" || sub_help {
        print!("{USAGE}");
        return;
    }
    let client = Client::load().unwrap_or_else(|e| die(&e.to_string()));
    let s = client.settings.clone();
    let cmd = args[0].as_str();

    let result: Result<()> = (|| {
        match cmd {
            "task" => {
                let gid = args.get(1).unwrap_or_else(|| die("usage: asana task <gid>"));
                let task = client.get_task(gid)?;
                let section = client.task_section_label(&task);
                let ctx = client.build_task_context(&task, &section)?;
                print!("{ctx}");
            }
            "comment" => {
                let gid = args.get(1).unwrap_or_else(|| die("usage: asana comment <gid> <text|->"));
                let text = read_text(args.get(2).unwrap_or_else(|| die("comment text required")));
                client.add_comment(gid, &text)?;
                println!("OK: комментарий добавлен к {gid}");
            }
            "move" => {
                let gid = args.get(1).unwrap_or_else(|| die("usage: asana move <gid> <role>"));
                let role = args.get(2).unwrap_or_else(|| die("role required"));
                let proj = project_of(&client, gid)?;
                client.move_task_to_role(gid, &proj, role)?;
                println!("OK: {gid} → {role}");
            }
            "assign" => {
                let gid = args
                    .get(1)
                    .unwrap_or_else(|| die("usage: asana assign <gid> <operator|agent|none>"));
                let who = args.get(2).unwrap_or_else(|| die("assignee required"));
                let target = resolve_assignee(&s, who)
                    .unwrap_or_else(|| die("assignee must be operator|agent|none (или метка из конфига)"));
                client.set_assignee(gid, target.as_deref())?;
                println!("OK: {gid} assignee = {who}");
            }
            "complete" => {
                let gid = args.get(1).unwrap_or_else(|| die("usage: asana complete <gid>"));
                client.set_completed(gid, true)?;
                println!("OK: {gid} закрыта");
            }
            "subtask" => {
                let parent = args.get(1).unwrap_or_else(|| {
                    die("usage: asana subtask <parent_gid> <name> [--notes <text|->] [--assignee operator|agent]")
                });
                let name = args.get(2).unwrap_or_else(|| die("subtask name required"));
                let mut notes = String::new();
                let mut assignee: Option<String> = None;
                let mut i = 3;
                while i < args.len() {
                    match args[i].as_str() {
                        "--notes" => {
                            i += 1;
                            notes = read_text(args.get(i).unwrap_or_else(|| die("--notes needs a value")));
                        }
                        "--assignee" => {
                            i += 1;
                            let who = args.get(i).unwrap_or_else(|| die("--assignee needs a value"));
                            assignee = match resolve_assignee(&s, who) {
                                Some(Some(gid)) => Some(gid),
                                Some(None) | None => die("assignee must be operator|agent (или метка из конфига)"),
                            };
                        }
                        other => die(&format!("unknown flag {other}")),
                    }
                    i += 1;
                }
                let gid = client.create_subtask(parent, name, &notes, assignee.as_deref())?;
                println!("OK: сабтаска создана, ID {gid}");
            }
            "download" => {
                let gid = args.get(1).unwrap_or_else(|| die("usage: asana download <attachment_gid> [dir]"));
                let dir = std::path::PathBuf::from(args.get(2).map(|s| s.as_str()).unwrap_or("."));
                let path = client.download_attachment(gid, &dir)?;
                println!("OK: {}", path.display());
            }
            "attach" => {
                let a = parse_attach_args(&args[1..]).unwrap_or_else(|e| die(&e));
                // Сначала проверяем, что все файлы на месте, чтобы не загрузить половину.
                for f in &a.files {
                    let p = std::path::Path::new(f);
                    if !p.is_file() {
                        die(&format!("{f}: файл не найден"));
                    }
                }
                for f in &a.files {
                    let att = client.upload_attachment(&a.task, std::path::Path::new(f), a.name.as_deref())?;
                    println!(
                        "OK: {} — {} ({})",
                        client.attachment_ref(&a.task, &att),
                        att.name,
                        human_size(att.size)
                    );
                }
            }
            "qa" => {
                let gid = args.get(1).unwrap_or_else(|| die("usage: asana qa <gid> <comment|->"));
                let text = read_text(args.get(2).unwrap_or_else(|| die("comment required")));
                let proj = project_of(&client, gid)?;
                client.add_comment(gid, &text)?;
                client.move_task_to_role(gid, &proj, "qa")?;
                client.set_assignee(gid, Some(&s.operator_gid))?;
                println!("OK: {gid} → qa, assignee оператор, комментарий добавлен");
            }
            "done" => {
                let gid = args.get(1).unwrap_or_else(|| die("usage: asana done <gid> <comment|->"));
                let text = read_text(args.get(2).unwrap_or_else(|| die("comment required")));
                let proj = project_of(&client, gid)?;
                client.add_comment(gid, &text)?;
                client.move_task_to_role(gid, &proj, "done")?;
                client.set_assignee(gid, Some(&s.operator_gid))?;
                println!("OK: {gid} → done, assignee оператор, комментарий добавлен");
            }
            "block" => {
                let gid = args.get(1).unwrap_or_else(|| die("usage: asana block <gid> <comment|->"));
                let text = read_text(args.get(2).unwrap_or_else(|| die("comment required")));
                let proj = project_of(&client, gid)?;
                client.add_comment(gid, &text)?;
                client.set_assignee(gid, Some(&s.operator_gid))?;
                client.move_task_to_role(gid, &proj, "blocked")?;
                println!("OK: {gid} → blocked, assignee оператор, комментарий добавлен");
            }
            "fields" => {
                for line in client.ensure_session_fields()? {
                    println!("OK: {line}");
                }
                println!(
                    "Пустое поле = значения по умолчанию (модель fable, эффорт — дефолт claude CLI)."
                );
            }
            _ => {
                eprintln!("Unknown command: {cmd}\n");
                print!("{USAGE}");
                exit(1);
            }
        }
        Ok(())
    })();

    if let Err(e) = result {
        die(&e.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn attach_single_file() {
        assert_eq!(
            parse_attach_args(&v(&["AUTOPILOT-3", "run.mp4"])).unwrap(),
            AttachArgs { task: "AUTOPILOT-3".into(), files: v(&["run.mp4"]), name: None }
        );
    }

    #[test]
    fn attach_many_files() {
        let a = parse_attach_args(&v(&["123", "a.png", "b.png", "e2e.log"])).unwrap();
        assert_eq!(a.task, "123");
        assert_eq!(a.files, v(&["a.png", "b.png", "e2e.log"]));
    }

    #[test]
    fn attach_name_anywhere() {
        let want = AttachArgs { task: "T-1".into(), files: v(&["x.mp4"]), name: Some("Прогон".into()) };
        assert_eq!(parse_attach_args(&v(&["T-1", "x.mp4", "--name", "Прогон"])).unwrap(), want);
        assert_eq!(parse_attach_args(&v(&["--name", "Прогон", "T-1", "x.mp4"])).unwrap(), want);
        assert_eq!(parse_attach_args(&v(&["T-1", "--name=Прогон", "x.mp4"])).unwrap(), want);
    }

    #[test]
    fn attach_double_dash() {
        let a = parse_attach_args(&v(&["T-1", "--", "--name"])).unwrap();
        assert_eq!(a.files, v(&["--name"]));
        assert_eq!(a.name, None);
    }

    #[test]
    fn attach_errors() {
        assert!(parse_attach_args(&v(&[])).is_err());
        assert!(parse_attach_args(&v(&["T-1"])).is_err());
        assert!(parse_attach_args(&v(&["T-1", "x.mp4", "--name"])).is_err());
        assert!(parse_attach_args(&v(&["T-1", "x.mp4", "--name", " "])).is_err());
        assert!(parse_attach_args(&v(&["T-1", "a", "b", "--name", "n"])).is_err());
        assert!(parse_attach_args(&v(&["T-1", "a", "--name", "n", "--name", "m"])).is_err());
        assert!(parse_attach_args(&v(&["T-1", "a", "--bogus"])).is_err());
    }
}
