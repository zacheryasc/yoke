use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Thinking {
    Off,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Thinking {
    /// Value to forward to `omp --thinking`.
    pub fn as_omp_arg(self) -> &'static str {
        match self {
            Thinking::Off => "off",
            Thinking::Low => "low",
            Thinking::Medium => "medium",
            Thinking::High => "high",
            Thinking::XHigh => "xhigh",
            Thinking::Max => "max",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScopeTag {
    Allow,
    AddOnly,
    NoModify,
}

#[derive(Debug, Clone)]
pub struct ScopeRule {
    pub tag: ScopeTag,
    pub prefix: String,
}

#[derive(Debug, Clone)]
pub struct Periodic {
    pub path: String, // e.g., ".loop/cleaner.md"
    pub name: String, // derived from filename stem: "cleaner"
    pub cadence: u32,
    pub guards: Vec<String>, // from guard-after directives
}

#[derive(Debug)]
pub struct Config {
    pub max_tail: usize,
    pub log_dir: Option<String>,
    pub image: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<Thinking>,
    pub judge_thinking: Option<String>,
    pub scope_rules: Vec<ScopeRule>,
    pub guards: Vec<String>,
    pub judge_every: Option<u32>,
    pub max_judge_failures: u32,
    pub periodics: Vec<Periodic>,
    pub hooks: Vec<String>,
    /// Summarize every N parsed blocks. 0 disables the summary stream.
    pub summary_every: u32,
    /// Optional override for the built-in summary model.
    pub summary_model: Option<String>,
    pub summary_thinking: Thinking,
    /// Pause the worker when this many summary batches are outstanding.
    pub summary_queue_limit: u32,
    /// Resolved absolute path where NDJSON metrics rows are written.
    /// Defaults to `~/.yoke/metrics` so rows survive `yoke clean` / project resets.
    pub metrics_dir: PathBuf,
    pub metrics_enabled: bool,
}

fn cfg_err(path: &Path, line_num: usize, msg: &str) -> String {
    format!("{}:{}: {}", path.display(), line_num, msg)
}

fn parse_number<T: std::str::FromStr>(
    value: &str,
    path: &Path,
    line_num: usize,
    label: &str,
) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| cfg_err(path, line_num, &format!("invalid {label} value '{value}'")))
}

fn parse_positive_u32(
    value: &str,
    path: &Path,
    line_num: usize,
    label: &str,
) -> Result<u32, String> {
    match parse_number(value, path, line_num, label)? {
        0 => Err(cfg_err(path, line_num, &format!("{label} must be > 0"))),
        n => Ok(n),
    }
}

fn parse_thinking(value: &str, path: &Path, line_num: usize) -> Result<Thinking, String> {
    match value.trim() {
        "off" => Ok(Thinking::Off),
        "low" => Ok(Thinking::Low),
        "medium" => Ok(Thinking::Medium),
        "high" => Ok(Thinking::High),
        "xhigh" => Ok(Thinking::XHigh),
        "max" => Ok(Thinking::Max),
        other => Err(cfg_err(
            path,
            line_num,
            &format!(
                "thinking must be 'off', 'low', 'medium', 'high', 'xhigh', or 'max', got '{}'",
                other
            ),
        )),
    }
}

fn parse_periodic(value: &str, path: &Path, line_num: usize) -> Result<Periodic, String> {
    let trimmed = value.trim();
    let (ppath, cadence) = trimmed
        .rsplit_once(char::is_whitespace)
        .ok_or_else(|| cfg_err(path, line_num, "periodic requires '<path> <cadence>'"))?;
    let ppath = ppath.trim();
    let cadence = parse_positive_u32(cadence.trim(), path, line_num, "periodic cadence")?;
    let name = Path::new(ppath)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            cfg_err(
                path,
                line_num,
                &format!("cannot derive name from periodic path '{}'", ppath),
            )
        })?
        .to_string();
    Ok(Periodic {
        path: ppath.to_string(),
        name,
        cadence,
        guards: Vec::new(),
    })
}

impl Config {
    fn parse_line(
        &mut self,
        directive: &str,
        value: &str,
        path: &Path,
        line_num: usize,
    ) -> Result<(), String> {
        match directive {
            "max-tail" => self.max_tail = parse_number(value, path, line_num, directive)?,
            "thinking" => self.thinking = Some(parse_thinking(value, path, line_num)?),
            "judge-thinking" => self.judge_thinking = Some(value.to_owned()),
            "judge-every" => {
                self.judge_every = Some(parse_positive_u32(value, path, line_num, directive)?)
            }
            "max-judge-failures" => {
                self.max_judge_failures = parse_positive_u32(value, path, line_num, directive)?
            }
            "periodic" => self.periodics.push(parse_periodic(value, path, line_num)?),
            "summary-every" => {
                self.summary_every = parse_number(value.trim(), path, line_num, directive)?
            }
            "summary-thinking" => self.summary_thinking = parse_thinking(value, path, line_num)?,
            "summary-queue-limit" => {
                self.summary_queue_limit = parse_positive_u32(value, path, line_num, directive)?
            }
            "metrics" => {
                self.metrics_enabled = match value.trim() {
                    "on" => true,
                    "off" => false,
                    other => {
                        return Err(cfg_err(
                            path,
                            line_num,
                            &format!("metrics must be 'on' or 'off', got '{}'", other),
                        ));
                    }
                }
            }
            "log-dir" => self.log_dir = Some(value.to_owned()),
            "image" => self.image = Some(value.to_owned()),
            "model" => self.model = Some(value.to_owned()),
            "allow" | "add-only" | "no-modify" => {
                let tag = match directive {
                    "allow" => ScopeTag::Allow,
                    "add-only" => ScopeTag::AddOnly,
                    _ => ScopeTag::NoModify,
                };
                self.scope_rules.push(ScopeRule {
                    tag,
                    prefix: value.to_owned(),
                });
            }
            "guard" => self.guards.push(value.to_owned()),
            "hook" => self.hooks.push(value.to_owned()),
            "summary-model" => self.summary_model = Some(value.to_owned()),
            "metrics-dir" => self.metrics_dir = crate::metrics::expand_tilde(value),
            _ => {
                return Err(cfg_err(
                    path,
                    line_num,
                    &format!("unknown directive '{directive}'"),
                ));
            }
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_tail: 200,
            log_dir: None,
            image: None,
            model: None,
            thinking: None,
            judge_thinking: None,
            scope_rules: Vec::new(),
            guards: Vec::new(),
            judge_every: None,
            max_judge_failures: 3,
            periodics: Vec::new(),
            hooks: Vec::new(),
            summary_every: 12,
            summary_model: None,
            summary_thinking: Thinking::Off,
            summary_queue_limit: 4,
            metrics_dir: crate::metrics::default_metrics_dir(),
            metrics_enabled: true,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("failed to read config {}: {}", path.display(), e))?;

        let mut config = Config::default();
        let mut pending_guard_afters = Vec::new();
        for (line_num, raw_line) in content.lines().enumerate() {
            let line = raw_line
                .split_once('#')
                .map_or(raw_line, |(line, _)| line)
                .trim();
            if line.is_empty() {
                continue;
            }

            let (directive, value) = line.split_once(char::is_whitespace).ok_or_else(|| {
                cfg_err(
                    path,
                    line_num + 1,
                    &format!("directive '{line}' has no value"),
                )
            })?;
            let value = value.trim_start();

            if directive == "guard-after" {
                let (name, cmd) = value.split_once(char::is_whitespace).ok_or_else(|| {
                    cfg_err(
                        path,
                        line_num + 1,
                        "guard-after requires '<periodic-name> <command>'",
                    )
                })?;
                pending_guard_afters.push((name, cmd.trim(), line_num + 1));
            } else {
                config.parse_line(directive, value, path, line_num + 1)?;
            }
        }

        for (name, cmd, line) in pending_guard_afters {
            let periodic = config
                .periodics
                .iter_mut()
                .find(|p| p.name == name)
                .ok_or_else(|| {
                    cfg_err(
                        path,
                        line,
                        &format!("guard-after references unknown periodic '{name}'"),
                    )
                })?;
            periodic.guards.push(cmd.to_owned());
        }
        Ok(config)
    }

    /// Resolve the most-specific scope tag for a file path.
    /// Returns None if no rule matches.
    pub fn resolve_tag(&self, file: &str) -> Option<&ScopeTag> {
        self.scope_rules
            .iter()
            .filter(|rule| {
                !rule.prefix.is_empty() && (rule.prefix == "." || file.starts_with(&rule.prefix))
            })
            .rev()
            .max_by_key(|rule| rule.prefix.len())
            .map(|rule| &rule.tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_fixture(content: &str) -> Result<Config, String> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("yoke.conf");
        fs::write(&path, content).unwrap();
        Config::load(&path)
    }

    #[test]
    fn thinking_max_parses_and_forwards() {
        let config = load_fixture("allow .\nthinking max\n").expect("thinking max should parse");
        assert_eq!(config.thinking, Some(Thinking::Max));
        assert_eq!(config.thinking.unwrap().as_omp_arg(), "max");
    }

    #[test]
    fn thinking_rejects_unknown_level() {
        let err = load_fixture("allow .\nthinking ultra\n").expect_err("unknown level should fail");
        assert!(
            err.contains("thinking must be 'off', 'low', 'medium', 'high', 'xhigh', or 'max'"),
            "error should list valid levels, got: {err}"
        );
    }

    #[test]
    fn summary_defaults_are_twelve_blocks_and_four_queued() {
        let config = load_fixture("allow .\nthinking off\n").expect("config loads");
        assert_eq!(config.summary_every, 12);
        assert_eq!(config.summary_queue_limit, 4);
        assert_eq!(config.summary_model, None);
        assert_eq!(config.summary_thinking, Thinking::Off);
    }

    #[test]
    fn summary_settings_parse() {
        let config = load_fixture(
            "allow .\nsummary-every 0\nsummary-model cheap/model\nsummary-thinking low\nsummary-queue-limit 2\n",
        ).expect("summary config");
        assert_eq!(config.summary_every, 0);
        assert_eq!(config.summary_model.as_deref(), Some("cheap/model"));
        assert_eq!(config.summary_thinking, Thinking::Low);
        assert_eq!(config.summary_queue_limit, 2);
    }

    #[test]
    fn summary_queue_limit_rejects_zero() {
        let err =
            load_fixture("allow .\nsummary-queue-limit 0\n").expect_err("zero limit should fail");
        assert!(err.contains("summary-queue-limit must be > 0"), "{err}");
    }
}
