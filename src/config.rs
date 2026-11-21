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
    pub scope_rules: Vec<ScopeRule>,
    pub guards: Vec<String>,
    pub judge_every: Option<u32>,
    pub max_judge_failures: u32,
    pub periodics: Vec<Periodic>,
    pub hooks: Vec<String>,
    /// Summarize every N parsed blocks. 0 disables the summary stream.
    pub summary_every: u32,
    /// Cheap model used only for summaries. Summaries are disabled without it.
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

fn parse_positive_u32(
    value: &str,
    path: &Path,
    line_num: usize,
    label: &str,
) -> Result<u32, String> {
    let n = value.parse::<u32>().map_err(|_| {
        cfg_err(
            path,
            line_num,
            &format!("invalid {} value '{}'", label, value),
        )
    })?;
    if n == 0 {
        return Err(cfg_err(path, line_num, &format!("{} must be > 0", label)));
    }
    Ok(n)
}

#[allow(clippy::string_slice)]
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
    let split_pos = trimmed
        .rfind(char::is_whitespace)
        .ok_or_else(|| cfg_err(path, line_num, "periodic requires '<path> <cadence>'"))?;
    let ppath = trimmed[..split_pos].trim();
    let cadence = parse_positive_u32(
        trimmed[split_pos..].trim(),
        path,
        line_num,
        "periodic cadence",
    )?;
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

#[allow(clippy::string_slice)]
fn parse_guard_after(
    value: &str,
    path: &Path,
    line_num: usize,
) -> Result<(String, String, usize), String> {
    let trimmed = value.trim();
    let split_pos = trimmed.find(char::is_whitespace).ok_or_else(|| {
        cfg_err(
            path,
            line_num,
            "guard-after requires '<periodic-name> <command>'",
        )
    })?;
    let pname = trimmed[..split_pos].trim().to_string();
    let cmd = trimmed[split_pos..].trim().to_string();
    Ok((pname, cmd, line_num))
}

impl Config {
    #[allow(clippy::string_slice)]
    fn parse_line(
        &mut self,
        directive: &str,
        value: &str,
        path: &Path,
        line_num: usize,
    ) -> Result<(), String> {
        if self.set_plain_field(directive, value) {
            return Ok(());
        }
        match directive {
            "max-tail" => {
                self.max_tail = value.parse::<usize>().map_err(|_| {
                    cfg_err(
                        path,
                        line_num,
                        &format!("invalid max-tail value '{}'", value),
                    )
                })?
            }
            "thinking" => self.thinking = Some(parse_thinking(value, path, line_num)?),
            "judge-every" => {
                self.judge_every = Some(parse_positive_u32(value, path, line_num, "judge-every")?)
            }
            "max-judge-failures" => {
                self.max_judge_failures =
                    parse_positive_u32(value, path, line_num, "max-judge-failures")?
            }
            "periodic" => self.periodics.push(parse_periodic(value, path, line_num)?),
            "summary-every" => {
                self.summary_every = value.trim().parse::<u32>().map_err(|_| {
                    cfg_err(
                        path,
                        line_num,
                        &format!("invalid summary-every value '{}'", value),
                    )
                })?
            }
            "summary-thinking" => self.summary_thinking = parse_thinking(value, path, line_num)?,
            "summary-queue-limit" => {
                self.summary_queue_limit =
                    parse_positive_u32(value, path, line_num, "summary-queue-limit")?
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
            other => {
                return Err(cfg_err(
                    path,
                    line_num,
                    &format!("unknown directive '{}'", other),
                ));
            }
        }
        Ok(())
    }

    /// Directives whose value is stored verbatim. Returns false when the
    /// directive isn't one of them.
    fn set_plain_field(&mut self, directive: &str, value: &str) -> bool {
        match directive {
            "log-dir" => self.log_dir = Some(value.to_string()),
            "image" => self.image = Some(value.to_string()),
            "model" => self.model = Some(value.to_string()),
            "allow" | "add-only" | "no-modify" => {
                let tag = match directive {
                    "allow" => ScopeTag::Allow,
                    "add-only" => ScopeTag::AddOnly,
                    _ => ScopeTag::NoModify,
                };
                self.scope_rules.push(ScopeRule {
                    tag,
                    prefix: value.to_string(),
                });
            }
            "guard" => self.guards.push(value.to_string()),
            "hook" => self.hooks.push(value.to_string()),
            "summary-model" => self.summary_model = Some(value.to_string()),
            "metrics-dir" => self.metrics_dir = crate::metrics::expand_tilde(value),
            _ => return false,
        }
        true
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
    #[allow(clippy::string_slice)] // all slices at ASCII delimiter positions from .find()
    pub fn load(path: &Path) -> Result<Config, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("failed to read config {}: {}", path.display(), e))?;

        let mut config = Config::default();
        let mut pending_guard_afters: Vec<(String, String, usize)> = Vec::new();
        for (line_num, raw_line) in content.lines().enumerate() {
            let line = match raw_line.find('#') {
                Some(pos) => &raw_line[..pos],
                None => raw_line,
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let (directive, value) = match line.find(char::is_whitespace) {
                Some(pos) => (&line[..pos], line[pos..].trim_start()),
                None => {
                    return Err(cfg_err(
                        path,
                        line_num + 1,
                        &format!("directive '{}' has no value", line),
                    ));
                }
            };

            if directive == "guard-after" {
                pending_guard_afters.push(parse_guard_after(value, path, line_num + 1)?);
            } else {
                config.parse_line(directive, value, path, line_num + 1)?;
            }
        }

        for (name, cmd, line) in pending_guard_afters {
            match config.periodics.iter_mut().find(|p| p.name == name) {
                Some(periodic) => periodic.guards.push(cmd),
                None => {
                    return Err(cfg_err(
                        path,
                        line,
                        &format!("guard-after references unknown periodic '{}'", name),
                    ));
                }
            }
        }
        Ok(config)
    }

    /// Resolve the most-specific scope tag for a file path.
    /// Returns None if no rule matches.
    pub fn resolve_tag(&self, file: &str) -> Option<&ScopeTag> {
        let mut best_tag: Option<&ScopeTag> = None;
        let mut best_len: usize = 0;

        for rule in &self.scope_rules {
            let matches = if rule.prefix == "." {
                true
            } else {
                file.starts_with(&rule.prefix)
            };

            if matches && rule.prefix.len() > best_len {
                best_len = rule.prefix.len();
                best_tag = Some(&rule.tag);
            }
        }

        // "." has length 1 but should match everything — already handled above.
        // If only "." matched, best_len is 1, which is correct.
        best_tag
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_with_thinking(value: &str) -> Result<Config, String> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("yoke.conf");
        fs::write(&path, format!("allow .\nthinking {value}\n")).unwrap();
        Config::load(&path)
    }

    #[test]
    fn thinking_max_parses_and_forwards() {
        let config = load_with_thinking("max").expect("thinking max should parse");
        assert_eq!(config.thinking, Some(Thinking::Max));
        assert_eq!(config.thinking.unwrap().as_omp_arg(), "max");
    }

    #[test]
    fn thinking_rejects_unknown_level() {
        let err = load_with_thinking("ultra").expect_err("unknown level should fail");
        assert!(
            err.contains("thinking must be 'off', 'low', 'medium', 'high', 'xhigh', or 'max'"),
            "error should list valid levels, got: {err}"
        );
    }

    #[test]
    fn summary_defaults_are_twelve_blocks_and_four_queued() {
        let config = load_with_thinking("off").expect("config loads");
        assert_eq!(config.summary_every, 12);
        assert_eq!(config.summary_queue_limit, 4);
        assert_eq!(config.summary_model, None);
        assert_eq!(config.summary_thinking, Thinking::Off);
    }

    #[test]
    fn summary_settings_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("yoke.conf");
        fs::write(
            &path,
            "allow .\nsummary-every 0\nsummary-model cheap/model\nsummary-thinking low\nsummary-queue-limit 2\n",
        )
        .unwrap();
        let config = Config::load(&path).expect("summary config");
        assert_eq!(config.summary_every, 0);
        assert_eq!(config.summary_model.as_deref(), Some("cheap/model"));
        assert_eq!(config.summary_thinking, Thinking::Low);
        assert_eq!(config.summary_queue_limit, 2);
    }

    #[test]
    fn summary_queue_limit_rejects_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("yoke.conf");
        fs::write(&path, "allow .\nsummary-queue-limit 0\n").unwrap();
        let err = Config::load(&path).expect_err("zero limit should fail");
        assert!(err.contains("summary-queue-limit must be > 0"), "{err}");
    }
}
