use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Backend {
    Claude,
    OpenCode,
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

#[derive(Debug)]
pub struct Config {
    pub max_tail: usize,
    pub log_dir: Option<String>,
    pub image: Option<String>,
    pub model: Option<String>,
    pub scope_rules: Vec<ScopeRule>,
    pub guards: Vec<String>,
}

impl Config {
    #[allow(clippy::string_slice)] // all slices at ASCII delimiter positions from .find()
    pub fn load(path: &Path) -> Result<Config, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("failed to read config {}: {}", path.display(), e))?;

        let mut max_tail: usize = 200;
        let mut log_dir: Option<String> = None;
        let mut image: Option<String> = None;
        let mut model: Option<String> = None;
        let mut scope_rules = Vec::new();
        let mut guards = Vec::new();
        for (line_num, raw_line) in content.lines().enumerate() {
            // Strip comments
            let line = match raw_line.find('#') {
                Some(pos) => &raw_line[..pos],
                None => raw_line,
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // Split into directive and value at first whitespace
            let (directive, value) = match line.find(char::is_whitespace) {
                Some(pos) => (&line[..pos], line[pos..].trim_start()),
                None => {
                    return Err(format!(
                        "{}:{}: directive '{}' has no value",
                        path.display(),
                        line_num + 1,
                        line
                    ));
                }
            };

            match directive {
                "max-tail" => {
                    max_tail = value.parse::<usize>().map_err(|_| {
                        format!(
                            "{}:{}: invalid max-tail value '{}'",
                            path.display(),
                            line_num + 1,
                            value
                        )
                    })?;
                }
                "log-dir" => {
                    log_dir = Some(value.to_string());
                }
                "image" => {
                    image = Some(value.to_string());
                }
                "model" => {
                    model = Some(value.to_string());
                }
                "allow" => scope_rules.push(ScopeRule {
                    tag: ScopeTag::Allow,
                    prefix: value.to_string(),
                }),
                "add-only" => scope_rules.push(ScopeRule {
                    tag: ScopeTag::AddOnly,
                    prefix: value.to_string(),
                }),
                "no-modify" => scope_rules.push(ScopeRule {
                    tag: ScopeTag::NoModify,
                    prefix: value.to_string(),
                }),
                "guard" => guards.push(value.to_string()),
                other => {
                    return Err(format!(
                        "{}:{}: unknown directive '{}'",
                        path.display(),
                        line_num + 1,
                        other
                    ));
                }
            }
        }

        Ok(Config {
            max_tail,
            log_dir,
            image,
            model,
            scope_rules,
            guards,
        })
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

    /// Determine which backend to use based on config.
    /// If `model` is set, use OpenCode; otherwise default to Claude CLI.
    pub fn backend(&self) -> Backend {
        if self.model.is_some() {
            Backend::OpenCode
        } else {
            Backend::Claude
        }
    }
}
