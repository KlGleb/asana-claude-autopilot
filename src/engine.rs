//! Движок агентской сессии: **Claude Code** (`claude -p`, по умолчанию) или
//! **OpenAI Codex CLI** (`codex exec`).
//!
//! Движок задаётся в `autopilot.yaml` проекта (`engine: claude | codex`, плюс
//! необязательный блок `codex:`) и переопределяется на уровне задачи — custom
//! field «Engine» (gleb_tasks) или директивой в тексте («engine: codex»), так же
//! как модель и эффорт. Здесь — только выбор и сборка командной строки; запуск,
//! таймаут и разбор исхода общие (`runner::run_session`).

use regex::RegexBuilder;

/// Какой CLI-агент выполняет сессию.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Engine {
    #[default]
    Claude,
    Codex,
}

/// Допустимые значения поля/директивы движка.
pub const ENGINE_OPTIONS: &[&str] = &["claude", "codex"];

impl Engine {
    pub fn parse(s: &str) -> Option<Engine> {
        match s.trim().to_lowercase().as_str() {
            "claude" | "claude-code" | "claude_code" | "claude code" => Some(Engine::Claude),
            "codex" | "openai-codex" | "openai_codex" => Some(Engine::Codex),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Engine::Claude => "claude",
            Engine::Codex => "codex",
        }
    }

    /// Человеко-читаемое имя для логов.
    pub fn title(self) -> &'static str {
        match self {
            Engine::Claude => "Claude Code",
            Engine::Codex => "Codex",
        }
    }

    /// Годится ли значение модели для этого движка. Claude — только алиасы
    /// claude CLI (fable/opus/sonnet/haiku); Codex — то, что похоже на id модели
    /// OpenAI (gpt-…, o3, codex-…), алиасы Claude для него игнорируются.
    pub fn accepts_model(self, model: &str) -> bool {
        match self {
            Engine::Claude => crate::MODEL_OPTIONS.contains(&model),
            Engine::Codex => looks_like_openai_model(model),
        }
    }
}

/// Похоже ли значение на id модели OpenAI/Codex: `gpt-5.1-codex`, `o3`,
/// `o4-mini`, `codex-mini-latest`, … (нижний регистр, без пробелов).
pub fn looks_like_openai_model(model: &str) -> bool {
    let m = model.trim().to_lowercase();
    if m.is_empty() || !m.chars().all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c)) {
        return false;
    }
    let o_series = m.len() >= 2 && m.starts_with('o') && m.as_bytes()[1].is_ascii_digit();
    m.starts_with("gpt") || m.starts_with("chatgpt") || m.contains("codex") || o_series
}

/// Эффорт задачи (low/medium/high/xhigh/max) → `model_reasoning_effort` Codex.
/// xhigh/max сводятся к high: не все модели Codex их поддерживают.
pub fn codex_effort(effort: &str) -> Option<&'static str> {
    match effort {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" => Some("high"),
        _ => None,
    }
}

/// Настройки Codex из блока `codex:` в autopilot.yaml.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexSettings {
    /// Модель по умолчанию (`-m`); None — дефолт самого codex (~/.codex/config.toml).
    pub model: Option<String>,
    /// Доп. аргументы `codex exec` (вставляются перед -m/-c задачи).
    pub extra_args: Vec<String>,
}

/// Движок задачи. Источники по убыванию приоритета: custom field задачи
/// (`fields.engine`, по умолчанию «Engine») → директива в тексте («engine: codex»,
/// «движок: codex») → `engine:` проекта.
pub fn select_engine(field_value: Option<&str>, text: &str, project_default: Engine) -> Engine {
    if let Some(v) = field_value {
        match Engine::parse(v) {
            Some(e) => return e,
            None => crate::runner::log(&format!(
                "Поле движка = «{v}» не входит в {} — игнорирую",
                ENGINE_OPTIONS.join("/")
            )),
        }
    }
    let re = RegexBuilder::new(r"(?:движок|engine):\s*\**\s*(claude|codex)\b")
        .case_insensitive(true)
        .build()
        .unwrap();
    re.captures(text)
        .and_then(|c| Engine::parse(&c[1]))
        .unwrap_or(project_default)
}

/// Аргументы `codex exec` для неинтерактивной сессии. Промпт читается из stdin
/// (последний аргумент `-`), чтобы длинный prompt.md не упирался в argv и не
/// принимался за флаг.
///
/// `--dangerously-bypass-approvals-and-sandbox` — аналог
/// `claude --dangerously-skip-permissions`: без него sandbox Codex не даёт сети
/// (git push, gh, CLI трекера), а подтверждать команды некому.
/// `--skip-git-repo-check` — каталог проекта может быть не git-репозиторием
/// (несколько репо внутри).
pub fn codex_args(
    project_dir: &str,
    model: Option<&str>,
    effort: Option<&str>,
    extra_args: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = [
        "exec",
        "--dangerously-bypass-approvals-and-sandbox",
        "--skip-git-repo-check",
        "--color",
        "never",
        "--cd",
        project_dir,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(extra_args.iter().cloned());
    if let Some(m) = model {
        args.push("-m".into());
        args.push(m.into());
    }
    if let Some(e) = effort.and_then(codex_effort) {
        args.push("-c".into());
        args.push(format!("model_reasoning_effort={e}"));
    }
    args.push("-".into());
    args
}

/// Аргументы `claude` для сессии (поведение до появления движков не изменилось).
pub fn claude_args(prompt: &str, model: &str, effort: &str, fallback_model: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        prompt.into(),
        "--model".into(),
        model.into(),
        "--dangerously-skip-permissions".into(),
    ];
    if model != fallback_model {
        args.push("--fallback-model".into());
        args.push(fallback_model.into());
    }
    // Эффорт — штатный флаг claude CLI (low/medium/high/xhigh/max); без него — дефолт CLI.
    if crate::EFFORT_OPTIONS.contains(&effort) {
        args.push("--effort".into());
        args.push(effort.into());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parse_engine_names() {
        assert_eq!(Engine::parse("Codex"), Some(Engine::Codex));
        assert_eq!(Engine::parse(" claude "), Some(Engine::Claude));
        assert_eq!(Engine::parse("gpt"), None);
        assert_eq!(Engine::default(), Engine::Claude);
    }

    #[test]
    fn engine_selection_precedence() {
        // Поле > директива > проект.
        assert_eq!(select_engine(Some("claude"), "engine: codex", Engine::Codex), Engine::Claude);
        assert_eq!(select_engine(None, "Сделай X.\nengine: codex", Engine::Claude), Engine::Codex);
        assert_eq!(select_engine(None, "движок: Claude", Engine::Codex), Engine::Claude);
        assert_eq!(select_engine(None, "просто текст", Engine::Codex), Engine::Codex);
        assert_eq!(select_engine(None, "", Engine::Claude), Engine::Claude);
        // Неизвестное значение поля — дальше по цепочке.
        assert_eq!(select_engine(Some("gemini"), "engine: codex", Engine::Claude), Engine::Codex);
    }

    #[test]
    fn openai_model_detection() {
        for m in ["gpt-5.1-codex", "gpt-5", "o3", "o4-mini", "codex-mini-latest", "GPT-5"] {
            assert!(looks_like_openai_model(m), "{m}");
        }
        for m in ["fable", "opus", "sonnet", "haiku", "", "gpt 5", "other"] {
            assert!(!looks_like_openai_model(m), "{m}");
        }
        assert!(Engine::Codex.accepts_model("gpt-5"));
        assert!(!Engine::Codex.accepts_model("opus"));
        assert!(Engine::Claude.accepts_model("opus"));
        assert!(!Engine::Claude.accepts_model("gpt-5"));
    }

    #[test]
    fn effort_mapping_for_codex() {
        assert_eq!(codex_effort("low"), Some("low"));
        assert_eq!(codex_effort("medium"), Some("medium"));
        assert_eq!(codex_effort("high"), Some("high"));
        assert_eq!(codex_effort("xhigh"), Some("high"));
        assert_eq!(codex_effort("max"), Some("high"));
        assert_eq!(codex_effort("-"), None);
    }

    #[test]
    fn codex_command_line_full() {
        let args = codex_args("/work/p", Some("gpt-5.1-codex"), Some("xhigh"), &s(&["--profile", "night"]));
        assert_eq!(
            args,
            s(&[
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--skip-git-repo-check",
                "--color",
                "never",
                "--cd",
                "/work/p",
                "--profile",
                "night",
                "-m",
                "gpt-5.1-codex",
                "-c",
                "model_reasoning_effort=high",
                "-",
            ])
        );
    }

    #[test]
    fn codex_command_line_defaults() {
        let args = codex_args("/work/p", None, Some("-"), &[]);
        assert_eq!(
            args,
            s(&[
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--skip-git-repo-check",
                "--color",
                "never",
                "--cd",
                "/work/p",
                "-",
            ])
        );
    }

    #[test]
    fn claude_command_line_unchanged() {
        assert_eq!(
            claude_args("P", "fable", "high", "opus"),
            s(&["-p", "P", "--model", "fable", "--dangerously-skip-permissions", "--fallback-model", "opus", "--effort", "high"])
        );
        assert_eq!(
            claude_args("P", "opus", "-", "opus"),
            s(&["-p", "P", "--model", "opus", "--dangerously-skip-permissions"])
        );
    }
}
