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

#[derive(Debug, Clone)]
pub struct Periodic {
    pub path: String,        // e.g., ".loop/cleaner.md"
    pub name: String,        // derived from filename stem: "cleaner"
    pub cadence: u32,
    pub guards: Vec<String>, // from guard-after directives
}

#[derive(Debug)]
pub struct Config {
    pub max_tail: usize,
    pub log_dir: Option<String>,
    pub image: Option<String>,
    pub model: Option<String>,
    pub scope_rules: Vec<ScopeRule>,
    pub guards: Vec<String>,
    pub judge_every: Option<u32>,
    pub max_judge_failures: u32,
    pub periodics: Vec<Periodic>,
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
        let mut judge_every: Option<u32> = None;
        let mut max_judge_failures: u32 = 3;
        let mut periodics = Vec::new();
        let mut pending_guard_afters: Vec<(String, String, usize)> = Vec::new(); // (periodic_name, command, line_num)
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
                "judge-every" => {
                    let n = value.parse::<u32>().map_err(|_| {
                        format!(
                            "{}:{}: invalid judge-every value '{}'",
                            path.display(),
                            line_num + 1,
                            value
                        )
                    })?;
                    if n == 0 {
                        return Err(format!(
                            "{}:{}: judge-every must be > 0",
                            path.display(),
                            line_num + 1
                        ));
                    }
                    judge_every = Some(n);
                }
                "max-judge-failures" => {
                    let n = value.parse::<u32>().map_err(|_| {
                        format!(
                            "{}:{}: invalid max-judge-failures value '{}'",
                            path.display(),
                            line_num + 1,
                            value
                        )
                    })?;
                    if n == 0 {
                        return Err(format!(
                            "{}:{}: max-judge-failures must be > 0",
                            path.display(),
                            line_num + 1
                        ));
                    }
                    max_judge_failures = n;
                }
                "periodic" => {
                    // Split value at last whitespace → path + cadence
                    let trimmed = value.trim();
                    let split_pos = trimmed.rfind(char::is_whitespace).ok_or_else(|| {
                        format!(
                            "{}:{}: periodic requires '<path> <cadence>'",
                            path.display(),
                            line_num + 1
                        )
                    })?;
                    let ppath = trimmed[..split_pos].trim();
                    let cadence_str = trimmed[split_pos..].trim();
                    let cadence = cadence_str.parse::<u32>().map_err(|_| {
                        format!(
                            "{}:{}: invalid periodic cadence '{}'",
                            path.display(),
                            line_num + 1,
                            cadence_str
                        )
                    })?;
                    if cadence == 0 {
                        return Err(format!(
                            "{}:{}: periodic cadence must be > 0",
                            path.display(),
                            line_num + 1
                        ));
                    }
                    // Derive name from filename stem
                    let name = Path::new(ppath)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .ok_or_else(|| {
                            format!(
                                "{}:{}: cannot derive name from periodic path '{}'",
                                path.display(),
                                line_num + 1,
                                ppath
                            )
                        })?
                        .to_string();
                    periodics.push(Periodic {
                        path: ppath.to_string(),
                        name,
                        cadence,
                        guards: Vec::new(),
                    });
                }
                "guard-after" => {
                    // Split at first whitespace → periodic name + command
                    let trimmed = value.trim();
                    let split_pos = trimmed.find(char::is_whitespace).ok_or_else(|| {
                        format!(
                            "{}:{}: guard-after requires '<periodic-name> <command>'",
                            path.display(),
                            line_num + 1
                        )
                    })?;
                    let pname = trimmed[..split_pos].trim().to_string();
                    let cmd = trimmed[split_pos..].trim().to_string();
                    pending_guard_afters.push((pname, cmd, line_num + 1));
                }
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

        // Attach guard-after commands to their matching periodics
        for (pname, cmd, ln) in pending_guard_afters {
            let found = periodics.iter_mut().find(|p| p.name == pname);
            match found {
                Some(p) => p.guards.push(cmd),
                None => {
                    return Err(format!(
                        "{}:{}: guard-after references unknown periodic '{}'",
                        path.display(),
                        ln,
                        pname
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
            judge_every,
            max_judge_failures,
            periodics,
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
