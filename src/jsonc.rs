//! Sets one top-level key in a JSONC file (JSON with comments and trailing commas, as OpenCode's
//! `opencode.jsonc`), leaving everything else as written: comments, order and spacing.

/// `text` with top-level `key` set to `value` (already JSON), or `None` if `text` isn't a JSONC
/// object pai can read, in which case the caller leaves the file alone.
pub fn set_top_level(text: &str, key: &str, value: &str) -> Option<String> {
    let b = text.as_bytes();
    let open = skip_blank(b, 0)?;
    if b.get(open) != Some(&b'{') {
        return None;
    }
    let mut i = open + 1;
    loop {
        i = skip_blank(b, i)?;
        match b.get(i)? {
            b'}' => break,
            b'"' => {
                let key_end = skip_string(b, i)?;
                let name: String = serde_json::from_str(&text[i..key_end]).ok()?;
                i = skip_blank(b, key_end)?;
                if b.get(i) != Some(&b':') {
                    return None;
                }
                let start = skip_blank(b, i + 1)?;
                let end = skip_value(b, start)?;
                if name == key {
                    return Some(format!("{}{value}{}", &text[..start], &text[end..]));
                }
                i = skip_blank(b, end)?;
                match b.get(i)? {
                    b',' => i += 1,
                    b'}' => break,
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    // Not there: add it first, so a trailing comma or comment at the end doesn't matter.
    let empty = b.get(skip_blank(b, open + 1)?) == Some(&b'}');
    let entry = serde_json::to_string(key).ok()?;
    let comma = if empty { "" } else { "," };
    Some(format!("{}\n  {entry}: {value}{comma}{}", &text[..=open], &text[open + 1..]))
}

/// The index of the next byte that isn't whitespace or inside a comment. `None` for an
/// unterminated block comment.
fn skip_blank(b: &[u8], mut i: usize) -> Option<usize> {
    loop {
        match b.get(i) {
            Some(c) if c.is_ascii_whitespace() => i += 1,
            Some(b'/') if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            Some(b'/') if b.get(i + 1) == Some(&b'*') => {
                let rest = b.get(i + 2..)?;
                i += 2 + rest.windows(2).position(|w| w == b"*/")? + 2;
            }
            _ => return Some(i),
        }
    }
}

/// The index just past the string that starts at `i`.
fn skip_string(b: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    loop {
        match b.get(i)? {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
}

/// The index just past the value that starts at `i`.
fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i)? {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            loop {
                j = skip_blank(b, j)?;
                match b.get(j)? {
                    b'"' => j = skip_string(b, j)?,
                    b'{' | b'[' => {
                        depth += 1;
                        j += 1;
                    }
                    b'}' | b']' => {
                        depth -= 1;
                        j += 1;
                        if depth == 0 {
                            return Some(j);
                        }
                    }
                    _ => j += 1,
                }
            }
        }
        b',' | b'}' | b']' | b':' => None,
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']' | b'/') && !b[j].is_ascii_whitespace() {
                j += 1;
            }
            Some(j)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::set_top_level;

    const V: &str = r#""dawonai/qwen3.8-27b""#;

    #[test]
    fn replaces_an_existing_key_and_keeps_comments() {
        let text = "{\n  // my theme\n  \"theme\": \"tokyonight\",\n  /* old */ \"model\": \"anthropic/x\", // keep\n  \"x\": [1, {\"model\": 2}],\n}\n";
        let out = set_top_level(text, "model", V).unwrap();
        assert_eq!(out, text.replace("\"anthropic/x\"", V));
    }

    #[test]
    fn adds_a_missing_key_first() {
        let text = "// settings\n{\n  \"theme\": \"x\", // trailing\n  \"nested\": {\"model\": \"no\"},\n}\n";
        let out = set_top_level(text, "model", V).unwrap();
        assert_eq!(out, "// settings\n{\n  \"model\": \"dawonai/qwen3.8-27b\",\n  \"theme\": \"x\", // trailing\n  \"nested\": {\"model\": \"no\"},\n}\n");
        assert_eq!(set_top_level("{}", "model", V).unwrap(), "{\n  \"model\": \"dawonai/qwen3.8-27b\"}");
        assert_eq!(set_top_level("{ /* nothing */ }", "model", V).unwrap(), "{\n  \"model\": \"dawonai/qwen3.8-27b\" /* nothing */ }");
    }

    #[test]
    fn strings_with_brackets_and_comment_markers_are_skipped() {
        let text = r#"{"a": "}{ // \" /*", "url": "http://x/y", "model": null}"#;
        assert_eq!(set_top_level(text, "model", V).unwrap(), text.replace("null", V));
    }

    #[test]
    fn refuses_what_it_cant_read() {
        for bad in ["", "[]", "{\"a\" 1}", "{\"a\": 1 \"b\": 2}", "{ /* open", "{\"a\": \"open"] {
            assert_eq!(set_top_level(bad, "model", V), None, "{bad:?}");
        }
    }
}
