/// Unescape basic JSON string escape sequences into real characters.
pub fn unescape_json(s: &str) -> String {
    serde_json::from_str(&format!("\"{}\"", s)).unwrap_or_else(|_| s.to_string())
}

/// Extract the string value for a given key from a flat JSON line.
/// Looks for `"key": "value"` and returns the value (unescaped basic sequences).
/// Returns `None` if the key is not found or the value is not a string.
#[allow(clippy::string_slice)] // all slices at ASCII delimiter positions from .find()
pub fn extract_str<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = {
        let mut pat = String::with_capacity(key.len() + 3);
        pat.push('"');
        pat.push_str(key);
        pat.push('"');
        pat
    };

    let mut search_from = 0;
    loop {
        let rel_pos = line[search_from..].find(&needle)?;
        let key_start = search_from + rel_pos;
        let after_key = key_start + needle.len();
        let rest = &line[after_key..];

        // Skip whitespace and colon
        let rest = rest.trim_start();
        let rest = match rest.strip_prefix(':') {
            Some(r) => r,
            None => {
                // This was a value, not a key — advance and retry
                search_from = after_key;
                continue;
            }
        };
        let rest = rest.trim_start();

        // Expect opening quote
        let rest = match rest.strip_prefix('"') {
            Some(r) => r,
            None => {
                search_from = after_key;
                continue;
            }
        };

        // Find closing quote (handle escaped quotes)
        let mut end = 0;
        let bytes = rest.as_bytes();
        while end < bytes.len() {
            if bytes[end] == b'\\' {
                end += 2;
            } else if bytes[end] == b'"' {
                return Some(&rest[..end]);
            } else {
                end += 1;
            }
        }

        return None;
    }
}

/// Extract a boolean value for a given key from a flat JSON line.
/// Looks for `"key": true` or `"key": false`.
#[allow(clippy::string_slice)] // all slices at ASCII delimiter positions from .find()
pub fn extract_bool(line: &str, key: &str) -> Option<bool> {
    let needle = {
        let mut pat = String::with_capacity(key.len() + 3);
        pat.push('"');
        pat.push_str(key);
        pat.push('"');
        pat
    };

    let key_start = line.find(&needle)?;
    let after_key = key_start + needle.len();
    let rest = line[after_key..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();

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
    let needle = {
        let mut pat = String::with_capacity(key.len() + 3);
        pat.push('"');
        pat.push_str(key);
        pat.push('"');
        pat
    };

    let key_start = line.find(&needle)?;
    let after_key = key_start + needle.len();
    let rest = &line[after_key..];

    // Skip whitespace and colon
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':')?;
    let rest = rest.trim_start();

    // Collect numeric chars: digits, '.', '-', '+', 'e', 'E'
    let num_end = rest
        .find(|c: char| {
            !c.is_ascii_digit() && c != '.' && c != '-' && c != '+' && c != 'e' && c != 'E'
        })
        .unwrap_or(rest.len());

    if num_end == 0 {
        return None;
    }

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
