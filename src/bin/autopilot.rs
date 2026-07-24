//! CLI автопилота: управление реестром проектов и демоном.
//!
//! Демон один на машину; проекты добавляются/убираются на лету (демон
//! перечитывает реестр каждую итерацию). Конфиги и логи каждого проекта —
//! в `<проект>/autopilot/`, глобальное состояние — в `~/.autopilot/`.

use autopilot_core::registry::{
    daemon_log_file, daemon_pid, global_dir, pgid_of, pid_file, stop_file, DaemonStatus,
    GlobalConfig, ProjectEntry, Registry,
};
use autopilot_core::runner::{latest_session_log, tail_file};
use autopilot_core::{daemon, initproj};
use std::path::PathBuf;
use std::process::{exit, Command, Stdio};

const USAGE: &str = r#"autopilot — демон, раздающий Asana-задачи Claude Code по всем вашим проектам

ПРОЕКТЫ:
  autopilot add <dir> [--name N] [--priority P] [--workspace GID] [--no-init]
                                Добавить проект под контроль (создаёт <dir>/autopilot/
                                с конфигом и промптом; --no-init = без доводки промпта
                                через claude). Повторный add обновляет запись.
  autopilot remove <name>       Убрать проект из-под контроля (файлы проекта не трогает).
  autopilot list                Список проектов.
  autopilot pause <name>        Приостановить проект (текущая сессия доработает).
  autopilot resume <name>       Возобновить проект.
  autopilot priority <name> <P> Задать приоритет (больше = важнее).

ДЕМОН:
  autopilot start               Запустить демон в фоне.
  autopilot stop                Остановить демон (вместе с текущей claude-сессией!).
  autopilot restart             stop + start.
  autopilot run                 Запустить демон в текущем терминале (для отладки).
  autopilot status              Статус демона и проектов.
  autopilot logs [name] [-n N]  Хвост лога: без имени — лог демона,
                                с именем — последний лог сессии проекта.

ФАЙЛЫ:
  ~/.autopilot/projects.yaml    реестр проектов (имя, каталог, приоритет, пауза)
  ~/.autopilot/config.yaml      глобальные настройки (дефолты ролей, тайминги, telegram)
  <проект>/autopilot/           конфиг, промпт, логи и состояние конкретного проекта

Telegram-бот (управление с телефона): задай токен в config.yaml (telegram.token_env,
по умолчанию env AUTOPILOT_TG_TOKEN) и свой chat id в telegram.allowed_chats,
затем перезапусти демон. Команды: /status /pause /resume /priority /logs + кнопки.
"#;

fn die(msg: &str) -> ! {
    eprintln!("ОШИБКА: {msg}");
    exit(1);
}

fn load_registry() -> Registry {
    Registry::load().unwrap_or_else(|e| die(&format!("реестр не читается: {e}")))
}

fn save_registry(reg: &Registry) {
    reg.save().unwrap_or_else(|e| die(&format!("реестр не сохраняется: {e}")));
}

fn known_names(reg: &Registry) -> String {
    let names = reg.names();
    if names.is_empty() {
        "(реестр пуст)".into()
    } else {
        names.join(", ")
    }
}

fn require_project<'a>(reg: &'a Registry, name: &str) -> &'a ProjectEntry {
    reg.find(name).unwrap_or_else(|| {
        die(&format!("проект «{name}» не найден; есть: {}", known_names(reg)))
    })
}

fn cmd_add(args: &[String]) {
    let mut dir: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut priority: i64 = 0;
    let mut workspace: Option<String> = None;
    let mut no_init = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                i += 1;
                name = Some(args.get(i).unwrap_or_else(|| die("--name требует значение")).clone());
            }
            "--priority" => {
                i += 1;
                priority = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| die("--priority требует число"));
            }
            "--workspace" => {
                i += 1;
                workspace = Some(args.get(i).unwrap_or_else(|| die("--workspace требует gid")).clone());
            }
            "--no-init" => no_init = true,
            other if dir.is_none() && !other.starts_with("--") => {
                dir = Some(PathBuf::from(other));
            }
            other => die(&format!("неизвестный аргумент {other}")),
        }
        i += 1;
    }

    let dir = dir.unwrap_or_else(|| die("usage: autopilot add <dir> [--name N] [--priority P] [--workspace GID] [--no-init]"));
    let dir = dir
        .canonicalize()
        .unwrap_or_else(|e| die(&format!("каталог {} не существует: {e}", dir.display())));
    let name = name.unwrap_or_else(|| {
        dir.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| die("не могу вывести имя из пути, задай --name"))
    });

    let mut reg = load_registry();
    // Имя должно быть уникальным; каталог — тоже (иначе демон возьмёт задачу дважды).
    if let Some(existing) = reg.projects.iter().find(|p| p.dir == dir && p.name != name) {
        die(&format!("каталог уже зарегистрирован под именем «{}»", existing.name));
    }

    let entry = ProjectEntry { name: name.clone(), dir: dir.clone(), priority, paused: false };
    let updated = if let Some(p) = reg.find_mut(&name) {
        p.dir = dir.clone();
        p.priority = priority;
        true
    } else {
        reg.projects.push(entry.clone());
        false
    };
    save_registry(&reg);
    println!(
        "{} проект «{name}» ({}), приоритет {priority}",
        if updated { "Обновлён" } else { "Добавлен" },
        dir.display()
    );

    let gcfg = GlobalConfig::load().unwrap_or_default();
    let prompt_created = initproj::ensure_files(&entry, &gcfg, workspace.as_deref())
        .unwrap_or_else(|e| die(&format!("инициализация файлов: {e}")));
    if let Err(e) = initproj::write_memory(&entry) {
        eprintln!("  ⚠️ память Claude Code не записана: {e}");
    }

    if prompt_created && !no_init {
        match initproj::refine_prompt_with_claude(&entry) {
            Ok(()) => {}
            Err(e) => eprintln!(
                "  ⚠️ доводка промпта не удалась ({e}) — отредактируй {} вручную",
                entry.autopilot_dir().join("prompt.md").display()
            ),
        }
    } else if !prompt_created {
        println!("  промпт уже был — доводку через claude пропускаю");
    }

    if daemon_pid().is_some() {
        println!("Демон работает — проект подхватится на следующей итерации.");
    } else {
        println!("Демон не запущен. Запуск: autopilot start");
    }
}

fn cmd_remove(name: &str) {
    let mut reg = load_registry();
    let before = reg.projects.len();
    reg.projects.retain(|p| p.name != name);
    if reg.projects.len() == before {
        die(&format!("проект «{name}» не найден; есть: {}", known_names(&reg)));
    }
    save_registry(&reg);
    println!("Проект «{name}» убран из-под контроля (файлы в каталоге проекта не тронуты).");
}

fn cmd_pause(name: &str, paused: bool) {
    let mut reg = load_registry();
    {
        let p = reg
            .find_mut(name)
            .unwrap_or_else(|| die(&format!("проект «{name}» не найден")));
        p.paused = paused;
    }
    save_registry(&reg);
    if paused {
        println!("⏸ «{name}» приостановлен. Если по нему сейчас идёт сессия — она доработает до конца.");
    } else {
        println!("▶️ «{name}» возобновлён.");
    }
}

fn cmd_priority(name: &str, pr: i64) {
    let mut reg = load_registry();
    {
        let p = reg
            .find_mut(name)
            .unwrap_or_else(|| die(&format!("проект «{name}» не найден")));
        p.priority = pr;
    }
    save_registry(&reg);
    println!("Приоритет «{name}» = {pr} (больше = важнее).");
}

fn cmd_list() {
    let reg = load_registry();
    if reg.projects.is_empty() {
        println!("Реестр пуст. Добавь проект: autopilot add <dir>");
        return;
    }
    println!("{:<16} {:>9}  {:<10} КАТАЛОГ", "ИМЯ", "ПРИОРИТЕТ", "СОСТОЯНИЕ");
    for p in reg.sorted_all() {
        println!(
            "{:<16} {:>9}  {:<10} {}",
            p.name,
            p.priority,
            if p.paused { "пауза" } else { "активен" },
            p.dir.display()
        );
    }
}

fn cmd_status() {
    print!("{}", daemon::status_text());
    // Последняя активность по каждому проекту.
    let reg = load_registry();
    for p in reg.sorted_all() {
        if let Some(log) = latest_session_log(p) {
            let name = log.file_name().unwrap_or_default().to_string_lossy().to_string();
            println!("\n[{}] последняя сессия: {name}", p.name);
            for line in tail_file(&log, 3).lines() {
                println!("    | {line}");
            }
        }
    }
}

fn cmd_logs(args: &[String]) {
    let mut name: Option<String> = None;
    let mut n: usize = 40;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                i += 1;
                n = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| die("-n требует число"));
            }
            other if name.is_none() && !other.starts_with('-') => name = Some(other.to_string()),
            other => die(&format!("неизвестный аргумент {other}")),
        }
        i += 1;
    }

    match name {
        None => {
            let path = daemon_log_file();
            if !path.exists() {
                println!("Лог демона ещё не создан ({}).", path.display());
                return;
            }
            println!("== {} ==", path.display());
            println!("{}", tail_file(&path, n));
        }
        Some(name) => {
            let reg = load_registry();
            let p = require_project(&reg, &name);
            match latest_session_log(p) {
                Some(path) => {
                    println!("== {} ==", path.display());
                    println!("{}", tail_file(&path, n));
                }
                None => println!("У «{name}» ещё нет логов сессий ({}).", p.logs_dir().display()),
            }
        }
    }
}

fn cmd_start() {
    if let Some(pid) = daemon_pid() {
        println!("Демон уже работает (pid {pid}).");
        return;
    }
    std::fs::create_dir_all(global_dir()).unwrap_or_else(|e| die(&format!("mkdir ~/.autopilot: {e}")));
    let _ = std::fs::remove_file(stop_file());

    let exe = std::env::current_exe().unwrap_or_else(|e| die(&format!("current_exe: {e}")));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(daemon_log_file())
        .unwrap_or_else(|e| die(&format!("открыть daemon.log: {e}")));

    let mut cmd = Command::new(exe);
    cmd.arg("__daemon")
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    // Своя process group: демон переживает закрытие терминала, а stop может
    // убить всю группу (демон + claude-сессии + caffeinate) одним сигналом.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().unwrap_or_else(|e| die(&format!("не удалось запустить демон: {e}")));
    println!("Демон запущен (pid {}). Лог: {}", child.id(), daemon_log_file().display());
}

fn cmd_stop() {
    let Some(pid) = daemon_pid() else {
        println!("Демон не запущен.");
        let _ = std::fs::remove_file(pid_file());
        let _ = std::fs::remove_file(stop_file());
        return;
    };
    // Мягкий сигнал (демон проверяет STOP между шагами) + жёсткое завершение группы.
    let _ = std::fs::write(stop_file(), "");
    let group_kill = pgid_of(pid) == Some(pid);
    let target = if group_kill {
        format!("-{pid}")
    } else {
        format!("{pid}")
    };
    let _ = Command::new("kill").args(["-TERM", "--", &target]).status();
    for _ in 0..5 {
        if daemon_pid().is_none() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    if daemon_pid().is_some() {
        println!("Демон не завершился по TERM — шлю KILL.");
        let _ = Command::new("kill").args(["-KILL", "--", &target]).status();
    }
    let _ = std::fs::remove_file(pid_file());
    let _ = std::fs::remove_file(stop_file());
    DaemonStatus::set("stopped", |_| {});
    println!("Демон остановлен (был pid {pid}).");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("");

    match cmd {
        "" | "help" | "--help" | "-h" => print!("{USAGE}"),
        "__daemon" => daemon::run(),
        "add" => cmd_add(&args[1..]),
        "remove" | "rm" => {
            let name = args.get(1).unwrap_or_else(|| die("usage: autopilot remove <name>"));
            cmd_remove(name);
        }
        "list" | "ls" => cmd_list(),
        "pause" => {
            let name = args.get(1).unwrap_or_else(|| die("usage: autopilot pause <name>"));
            cmd_pause(name, true);
        }
        "resume" => {
            let name = args.get(1).unwrap_or_else(|| die("usage: autopilot resume <name>"));
            cmd_pause(name, false);
        }
        "priority" => {
            let name = args.get(1).unwrap_or_else(|| die("usage: autopilot priority <name> <P>"));
            let pr = args
                .get(2)
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| die("приоритет должен быть числом"));
            cmd_priority(name, pr);
        }
        "status" => cmd_status(),
        "logs" => cmd_logs(&args[1..]),
        "start" => cmd_start(),
        "stop" => cmd_stop(),
        "restart" => {
            cmd_stop();
            cmd_start();
        }
        "run" => {
            std::fs::create_dir_all(global_dir()).ok();
            daemon::run();
        }
        other => {
            eprintln!("Неизвестная команда: {other}\n");
            print!("{USAGE}");
            exit(1);
        }
    }
}
