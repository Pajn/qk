//! The placeholder syntax Nx 24 shares between run-commands arguments and
//! `readyWhen`: `{name}` or `{name.path}`, with `??` fallbacks.

/// Where the brace opening at `open` closes, counting nested pairs.
pub(crate) fn closing_brace(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + index);
                }
            }
            _ => {}
        }
    }
    None
}

/// A token's `??` alternatives, trimmed, outside any nested braces.
pub(crate) fn fallbacks(token: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let bytes = token.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'?' if depth == 0 && bytes.get(index + 1) == Some(&b'?') => {
                parts.push(token[start..index].trim());
                start = index + 2;
                index += 1;
            }
            _ => {}
        }
        index += 1;
    }
    parts.push(token[start..].trim());
    parts
}

/// The variable an `env.NAME` reference names, as Nx reads one: any name a
/// dotenv file can define, and Windows names such as `ProgramFiles(x86)`.
pub(crate) fn env_name(source: &str) -> Option<&str> {
    source.strip_prefix("env.").filter(|name| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || "_.()-".contains(c))
    })
}
