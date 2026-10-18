//! Nx's `interpolate`, which resolves `{projectRoot}`, `{projectName}`,
//! `{workspaceRoot}` and `{project.*}` in the string values of a task's
//! command-line overrides as its task graph is built.

use anyhow::{Result, bail};
use serde_json::Value;

use crate::Project;

/// A part of a text: as written, or a token with its `??` alternatives.
enum Node {
    Text(String),
    Token { raw: String, parts: Vec<Part> },
}

enum Part {
    Reference { name: String, path: Option<String> },
    Literal(Vec<Node>),
}

const NAMES: &[&str] = &["workspaceRoot", "projectRoot", "projectName", "project"];

/// An override as Nx interpolates it for a task of `project`. `{workspaceRoot}`
/// counts as empty and is only valid at the start, where it goes with the
/// slash after it; a token with no value stays as written. A value with any
/// token is normalized as a path, as Nx joins it, and one starting with `/`
/// is left as it is.
pub fn interpolate_override(text: &str, project: &Project) -> Result<String> {
    if !has_token_shape(text) || text.starts_with('/') {
        return Ok(text.to_owned());
    }
    let nodes = parse(text)?;
    if !workspace_root_at_start(&nodes, true) {
        bail!(
            "Output '{text}' is invalid. {{workspaceRoot}} can only be used at the beginning of the expression."
        );
    }
    let data = serde_json::to_value(project)?;
    let mut segments = vec![String::new()];
    append(&nodes, false, project, &data, &mut segments);
    let joined = join(&segments);
    Ok(joined.replacen("{workspaceRoot}/", "", 1))
}

/// Nx's `/{[\s\S]+?}/`.
fn has_token_shape(text: &str) -> bool {
    text.find('{').is_some_and(|open| {
        text[open + 1..]
            .char_indices()
            .any(|(index, c)| c == '}' && index > 0)
    })
}

fn closing_brace(text: &str, open: usize) -> Option<usize> {
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

fn split_fallbacks(expression: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let bytes = expression.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'?' if depth == 0 && bytes.get(index + 1) == Some(&b'?') => {
                parts.push(expression[start..index].trim());
                start = index + 2;
                index += 1;
            }
            _ => {}
        }
        index += 1;
    }
    parts.push(expression[start..].trim());
    parts
}

fn reference(source: &str) -> Option<Part> {
    let (name, path) = match source.split_once('.') {
        Some((name, path)) => (name, Some(path.to_owned())),
        None => (source, None),
    };
    NAMES.contains(&name).then(|| Part::Reference {
        name: name.to_owned(),
        path,
    })
}

fn parse(template: &str) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    let mut text = String::new();
    let mut index = 0;
    while let Some(offset) = template[index..].find('{') {
        let open = index + offset;
        let token = match closing_brace(template, open) {
            Some(close) => parse_token(&template[open..=close])?.map(|token| (token, close)),
            None => None,
        };
        let Some((token, close)) = token else {
            text.push_str(&template[index..=open]);
            index = open + 1;
            continue;
        };
        text.push_str(&template[index..open]);
        if !text.is_empty() {
            nodes.push(Node::Text(std::mem::take(&mut text)));
        }
        nodes.push(token);
        index = close + 1;
    }
    text.push_str(&template[index..]);
    if !text.is_empty() {
        nodes.push(Node::Text(text));
    }
    Ok(nodes)
}

fn parse_token(raw: &str) -> Result<Option<Node>> {
    let sources = split_fallbacks(&raw[1..raw.len() - 1]);
    if reference(sources[0]).is_none() {
        return Ok(None);
    }
    let mut parts = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        if let Some(reference) = reference(source) {
            parts.push(reference);
        } else if index + 1 < sources.len() {
            bail!("{raw:?} has a fallback after the literal {source:?} that can never be used");
        } else {
            parts.push(Part::Literal(parse(source)?));
        }
    }
    Ok(Some(Node::Token {
        raw: raw.to_owned(),
        parts,
    }))
}

fn workspace_root_at_start(nodes: &[Node], at_start: bool) -> bool {
    nodes.iter().enumerate().all(|(index, node)| {
        let start = at_start && index == 0;
        match node {
            Node::Text(text) => {
                let from = if start {
                    text.chars().next().map_or(0, char::len_utf8)
                } else {
                    0
                };
                !text[from..].contains("{workspaceRoot}")
            }
            Node::Token { parts, .. } => parts.iter().all(|part| match part {
                Part::Literal(nodes) => workspace_root_at_start(nodes, start),
                Part::Reference { name, .. } => name != "workspaceRoot" || start,
            }),
        }
    })
}

fn read(name: &str, path: Option<&str>, project: &Project, data: &Value) -> Option<String> {
    let value = match name {
        "workspaceRoot" => Value::String(String::new()),
        "projectRoot" => Value::String(project.root.clone()),
        "projectName" => Value::String(project.name.clone()),
        _ => data.clone(),
    };
    let mut value = &value;
    if let Some(path) = path {
        for key in path.split('.') {
            value = value.as_object()?.get(key)?;
        }
    }
    match value {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => Some(
            items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(","),
        ),
        Value::Object(_) => Some("[object Object]".into()),
        other => Some(other.to_string()),
    }
}

/// Nx's `joinSegments`: the text split at each `/`, which [`join`] puts
/// back together.
fn append(
    nodes: &[Node],
    literal: bool,
    project: &Project,
    data: &Value,
    segments: &mut Vec<String>,
) {
    for (index, node) in nodes.iter().enumerate() {
        match node {
            Node::Text(text) => {
                let root = if literal && index == 0 && text.starts_with('/') {
                    "/"
                } else {
                    ""
                };
                let mut pieces = text[root.len()..].split('/');
                let first = pieces.next().unwrap_or_default();
                let last = segments.last_mut().unwrap();
                last.push_str(root);
                last.push_str(first);
                segments.extend(pieces.map(str::to_owned));
            }
            Node::Token { raw, parts } => {
                // A root project's `{projectRoot}` joins away.
                if project.root == "." && raw == "{projectRoot}" {
                    continue;
                }
                let mut value = None;
                for part in parts {
                    match part {
                        Part::Reference { name, path } => {
                            if let Some(found) = read(name, path.as_deref(), project, data) {
                                value = Some(found);
                                break;
                            }
                        }
                        Part::Literal(nodes) => {
                            append(nodes, true, project, data, segments);
                            value = Some(String::new());
                            break;
                        }
                    }
                }
                let value = value.unwrap_or_else(|| raw.clone());
                segments.last_mut().unwrap().push_str(&value);
            }
        }
    }
}

/// Node's POSIX `path.join`.
fn join(segments: &[String]) -> String {
    let joined = segments
        .iter()
        .filter(|segment| !segment.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        return ".".into();
    }
    let absolute = joined.starts_with('/');
    let trailing = joined.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            part => parts.push(part),
        }
    }
    let mut result = parts.join("/");
    if result.is_empty() && !absolute {
        result.push('.');
    }
    if !result.is_empty() && trailing {
        result.push('/');
    }
    if absolute {
        result.insert(0, '/');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(root: &str) -> Project {
        serde_json::from_value(serde_json::json!({
            "name": "ui", "root": root, "sourceRoot": format!("{root}/src")
        }))
        .unwrap()
    }

    #[test]
    fn overrides_resolve_like_nx_interpolate() {
        let ui = project("libs/ui");
        for (text, expected) in [
            ("--out={projectRoot}/dist", "--out=libs/ui/dist"),
            ("{workspaceRoot}/dist/{projectName}", "dist/ui"),
            ("{project.sourceRoot}/main.ts", "libs/ui/src/main.ts"),
            ("{missing} and {projectRoot}//x", "{missing} and libs/ui/x"),
            ("plain//text", "plain//text"),
            ("/abs/{projectRoot}", "/abs/{projectRoot}"),
            ("{project.nothing ?? fallback}/a", "fallback/a"),
            ("{workspaceRoot}", "{workspaceRoot}"),
        ] {
            assert_eq!(interpolate_override(text, &ui).unwrap(), expected, "{text}");
        }
        let root = project(".");
        assert_eq!(
            interpolate_override("{projectRoot}/dist", &root).unwrap(),
            "dist"
        );
        assert!(interpolate_override("x/{workspaceRoot}", &ui).is_err());
    }
}
