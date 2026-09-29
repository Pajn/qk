//! Nx's `jsonDiff`: every path present in either document, as added, deleted
//! or modified. Array elements are addressed by their index, as in JavaScript.

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Added,
    Deleted,
    Modified,
}

#[derive(Debug)]
pub struct Change {
    pub kind: Kind,
    pub path: Vec<String>,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

pub fn diff(before: &Value, after: &Value) -> Vec<Change> {
    let mut changes = Vec::new();
    walk(
        before,
        &mut Vec::new(),
        &mut |path, value| match lookup(after, path) {
            None => changes.push(Change {
                kind: Kind::Deleted,
                path: path.to_vec(),
                before: Some(value.clone()),
                after: None,
            }),
            Some(other) if other != value => changes.push(Change {
                kind: Kind::Modified,
                path: path.to_vec(),
                before: Some(value.clone()),
                after: Some(other.clone()),
            }),
            Some(_) => {}
        },
    );
    walk(after, &mut Vec::new(), &mut |path, value| {
        if lookup(before, path).is_none() {
            changes.push(Change {
                kind: Kind::Added,
                path: path.to_vec(),
                before: None,
                after: Some(value.clone()),
            });
        }
    });
    changes
}

fn children(value: &Value) -> Vec<(String, &Value)> {
    match value {
        Value::Object(object) => object
            .iter()
            .map(|(key, value)| (key.clone(), value))
            .collect(),
        Value::Array(array) => array
            .iter()
            .enumerate()
            .map(|(index, value)| (index.to_string(), value))
            .collect(),
        _ => Vec::new(),
    }
}

fn walk(value: &Value, path: &mut Vec<String>, visit: &mut impl FnMut(&[String], &Value)) {
    for (key, child) in children(value) {
        path.push(key);
        visit(path, child);
        walk(child, path, visit);
        path.pop();
    }
}

fn lookup<'a>(mut value: &'a Value, path: &[String]) -> Option<&'a Value> {
    for key in path {
        value = match value {
            Value::Object(object) => object.get(key)?,
            Value::Array(array) => array.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reports_every_changed_path_like_nx() {
        let changes = diff(
            &json!({"a": {"b": 1, "c": [1, 2]}, "gone": true}),
            &json!({"a": {"b": 2, "c": [1]}, "new": null}),
        );
        let summary: Vec<_> = changes
            .iter()
            .map(|change| (change.kind, change.path.join(".")))
            .collect();
        assert_eq!(
            summary,
            [
                (Kind::Modified, "a".into()),
                (Kind::Modified, "a.b".into()),
                (Kind::Modified, "a.c".into()),
                (Kind::Deleted, "a.c.1".into()),
                (Kind::Deleted, "gone".into()),
                (Kind::Added, "new".into()),
            ]
        );
    }
}
