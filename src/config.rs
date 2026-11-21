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
    pub grind_passes: u32,
    pub periodics: Vec<Periodic>,
    pub hooks: Vec<String>,
    /// Resolved absolute path where NDJSON metrics rows are written.
    /// Defaults to `~/.yoke/metrics` so rows survive `yoke clean` / project resets.
    pub metrics_dir: PathBuf,
    pub metrics_enabled: bool,
}

struct ConfigBuilder {
    max_tail: usize,
    log_dir: Option<String>,
    image: Option<String>,
    model: Option<String>,
    thinking: Option<Thinking>,
    scope_rules: Vec<ScopeRule>,
    guards: Vec<String>,
    judge_every: Option<u32>,
    max_judge_failures: u32,
    grind_passes: u32,
    periodics: Vec<Periodic>,
    pending_guard_afters: Vec<(String, String, usize)>,
    hooks: Vec<String>,
    metrics_dir: Option<String>,
    metrics_enabled: bool,
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

impl ConfigBuilder {
    fn new() -> Self {
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
            grind_passes: 1,
            periodics: Vec::new(),
            pending_guard_afters: Vec::new(),
            hooks: Vec::new(),
            metrics_dir: None,
            metrics_enabled: true,
        }
    }

    #[allow(clippy::string_slice)]
    fn parse_line(
        &mut self,
        directive: &str,
        value: &str,
        path: &Path,
        line_num: usize,
    ) -> Result<(), String> {
        match directive {
            "max-tail" => {
                self.max_tail = value.parse::<usize>().map_err(|_| {
                    cfg_err(
                        path,
                        line_num,
                        &format!("invalid max-tail value '{}'", value),
                    )
                })?;
            }
            "log-dir" => self.log_dir = Some(value.to_string()),
            "image" => self.image = Some(value.to_string()),
            "model" => self.model = Some(value.to_string()),
            "thinking" => {
                self.thinking = Some(match value.trim() {
                    "off" => Thinking::Off,
                    "low" => Thinking::Low,
                    "medium" => Thinking::Medium,
                    "high" => Thinking::High,
                    "xhigh" => Thinking::XHigh,
                    "max" => Thinking::Max,
                    other => {
                        return Err(cfg_err(
                            path,
                            line_num,
                            &format!(
                                "thinking must be 'off', 'low', 'medium', 'high', 'xhigh', or 'max', got '{}'",
                                other
                            ),
                        ));
                    }
                });
            }
            "allow" => self.scope_rules.push(ScopeRule {
                tag: ScopeTag::Allow,
                prefix: value.to_string(),
            }),
            "add-only" => self.scope_rules.push(ScopeRule {
                tag: ScopeTag::AddOnly,
                prefix: value.to_string(),
            }),
            "no-modify" => self.scope_rules.push(ScopeRule {
                tag: ScopeTag::NoModify,
                prefix: value.to_string(),
            }),
            "guard" => self.guards.push(value.to_string()),
            "judge-every" => {
                self.judge_every = Some(parse_positive_u32(value, path, line_num, "judge-every")?)
            }
            "max-judge-failures" => {
                self.max_judge_failures =
                    parse_positive_u32(value, path, line_num, "max-judge-failures")?
            }
            "grind-passes" => {
                self.grind_passes = parse_positive_u32(value, path, line_num, "grind-passes")?
            }
            "periodic" => self.periodics.push(parse_periodic(value, path, line_num)?),
            "guard-after" => self
                .pending_guard_afters
                .push(parse_guard_after(value, path, line_num)?),
            "hook" => self.hooks.push(value.to_string()),
            "metrics-dir" => self.metrics_dir = Some(value.to_string()),
            "metrics" => match value.trim() {
                "on" => self.metrics_enabled = true,
                "off" => self.metrics_enabled = false,
                other => {
                    return Err(cfg_err(
                        path,
                        line_num,
                        &format!("metrics must be 'on' or 'off', got '{}'", other),
                    ));
                }
            },
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

    fn build(self, path: &Path) -> Result<Config, String> {
        let mut periodics = self.periodics;
        for (pname, cmd, ln) in self.pending_guard_afters {
            match periodics.iter_mut().find(|p| p.name == pname) {
                Some(p) => p.guards.push(cmd),
                None => {
                    return Err(cfg_err(
                        path,
                        ln,
                        &format!("guard-after references unknown periodic '{}'", pname),
                    ));
                }
            }
        }
        let metrics_dir = match self.metrics_dir {
            Some(raw) => crate::metrics::expand_tilde(&raw),
            None => crate::metrics::default_metrics_dir(),
        };
        Ok(Config {
            max_tail: self.max_tail,
            log_dir: self.log_dir,
            image: self.image,
            model: self.model,
            thinking: self.thinking,
            scope_rules: self.scope_rules,
            guards: self.guards,
            judge_every: self.judge_every,
            max_judge_failures: self.max_judge_failures,
            grind_passes: self.grind_passes,
            periodics,
            hooks: self.hooks,
            metrics_dir,
            metrics_enabled: self.metrics_enabled,
        })
    }
}

impl Config {
    #[allow(clippy::string_slice)] // all slices at ASCII delimiter positions from .find()
    pub fn load(path: &Path) -> Result<Config, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("failed to read config {}: {}", path.display(), e))?;

        let mut builder = ConfigBuilder::new();
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

            builder.parse_line(directive, value, path, line_num + 1)?;
        }

        builder.build(path)
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
}
