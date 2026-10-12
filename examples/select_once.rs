//! Один прогон выбора задачи (без запуска claude): для ручной проверки конфига.
//! AUTOPILOT_DIR=<dir> cargo run --example select_once
use autopilot_core::registry::ProjectEntry;
use autopilot_core::{runner, Client};

fn main() {
    let client = Client::load().unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        std::process::exit(1)
    });
    let dir = autopilot_core::Settings::find_dir().unwrap();
    let project = ProjectEntry { name: "select_once".into(), dir: dir.parent().unwrap().to_path_buf(), priority: 0, paused: false };
    match runner::select_task(&client, &project) {
        Some((t, p)) => println!("SELECTED: {} «{}» — проект «{}», секция {} (роль {:?})", t.display_id(), t.name, p.project_name, p.section_name, p.role),
        None => println!("NONE"),
    }
}
