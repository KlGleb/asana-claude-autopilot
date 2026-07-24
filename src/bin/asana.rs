//! Токен-экономная CLI для Asana. Заменяет Asana MCP-сервер в сессиях автопилота.
//! Вывод — минимальный plain text; длинные тексты можно подавать через stdin (`-`).
//!
//! Конфиг проекта (autopilot.yaml) ищется вверх от текущего каталога
//! (`<dir>/autopilot/autopilot.yaml`) либо берётся из env AUTOPILOT_DIR —
//! поэтому одна глобально установленная `asana` работает во всех проектах.

use autopilot_core::*;
use std::io::Read;
use std::process::exit;

const USAGE: &str = r#"asana — лёгкая CLI для Asana (конфиг: autopilot/autopilot.yaml проекта, поиск вверх от cwd или AUTOPILOT_DIR)

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
                                       В get_asset?asset_id=NNN ссылках NNN = gid вложения.
  asana qa <gid> <comment|->           Комментарий + перенос в qa + assign оператору.
  asana done <gid> <comment|->         Комментарий + перенос в done + assign оператору.
  asana block <gid> <comment|->        Комментарий + assign оператору + перенос в blocked.

Токен: env-переменная из autopilot.yaml (asana.token_env, по умолчанию ASANA_ACCESS_TOKEN).
"#;

fn read_text(arg: &str) -> String {
    if arg == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).expect("stdin read failed");
        buf.trim().to_string()
    } else {
        arg.to_string()
    }
}

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
    if args.is_empty() || args[0] == "help" || args[0] == "--help" {
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
                println!("OK: сабтаска создана, GID {gid}");
            }
            "download" => {
                let gid = args.get(1).unwrap_or_else(|| die("usage: asana download <attachment_gid> [dir]"));
                let dir = std::path::PathBuf::from(args.get(2).map(|s| s.as_str()).unwrap_or("."));
                let path = client.download_attachment(gid, &dir)?;
                println!("OK: {}", path.display());
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
