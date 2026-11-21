/// Unescape basic JSON string escape sequences into real characters.
pub fn unescape_json(s: &str) -> String {
    serde_json::from_str(&format!("\"{}\"", s)).unwrap_or_else(|_| s.to_string())
}

/// Extract the string value for a given key from a flat JSON line.
/// Looks for `"key": "value"` and returns the value (unescaped basic sequences).
/// Returns `None` if the key is not found or the value is not a string.
#[allow(clippy::string_slice)] // all slices at ASCII delimiter positions from .find()
pub fn extract_str<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    for (pos, _) in line.match_indices(&needle) {
        let rest = &line[pos + needle.len()..];
        if let Some(quoted) = after_colon(rest).and_then(|s| s.strip_prefix('"')) {
            let mut bytes = quoted.bytes().enumerate();
            while let Some((end, byte)) = bytes.next() {
                match byte {
                    b'\\' => {
                        bytes.next();
                    }
                    b'"' => return Some(&quoted[..end]),
                    _ => {}
                }
            }
            return None;
        }
    }
    None
}

fn after_colon(rest: &str) -> Option<&str> {
    Some(rest.trim_start().strip_prefix(':')?.trim_start())
}

fn first_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    after_colon(line.split_once(&needle)?.1)
}

/// Extract a boolean value for a given key from a flat JSON line.
/// Looks for `"key": true` or `"key": false`.
pub fn extract_bool(line: &str, key: &str) -> Option<bool> {
    let rest = first_value(line, key)?;

    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// Extract a numeric value for a given key from a flat JSON line.
/// Looks for `"key": 123.45` and returns the number.
/// Returns `None` if the key is not found or the value is not a number.
#[allow(clippy::string_slice)] // all slices at ASCII delimiter positions from .find()
pub fn extract_num(line: &str, key: &str) -> Option<f64> {
    let rest = first_value(line, key)?;
    let num_end = rest
        .find(|c: char| !c.is_ascii_digit() && !matches!(c, '.' | '-' | '+' | 'e' | 'E'))
        .unwrap_or(rest.len());
    rest[..num_end].parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_str_basic() {
        let line = r#"{"type": "assistant", "subtype": "text"}"#;
        assert_eq!(extract_str(line, "type"), Some("assistant"));
        assert_eq!(extract_str(line, "subtype"), Some("text"));
        assert_eq!(extract_str(line, "missing"), None);
    }

    #[test]
    fn test_extract_str_escaped() {
        let line = r#"{"path": "foo\"bar"}"#;
        assert_eq!(extract_str(line, "path"), Some(r#"foo\"bar"#));
    }

    #[test]
    fn test_extract_num_basic() {
        let line = r#"{"cost_usd": 0.42, "num_turns": 5}"#;
        assert!((extract_num(line, "cost_usd").unwrap() - 0.42).abs() < 1e-10);
        assert!((extract_num(line, "num_turns").unwrap() - 5.0).abs() < 1e-10);
        assert_eq!(extract_num(line, "missing"), None);
    }

    #[test]
    fn test_extract_bool() {
        let line = r#"{"guards_passed":true,"status_done":false}"#;
        assert_eq!(extract_bool(line, "guards_passed"), Some(true));
        assert_eq!(extract_bool(line, "status_done"), Some(false));
        assert_eq!(extract_bool(line, "missing"), None);
    }
}
