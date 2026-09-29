//! npm's own answers: `npm-ranges.json` holds what the `semver` package Nx
//! uses answers for each version and range.

#[test]
fn ranges_are_satisfied_as_npm_satisfies_them() {
    let vectors: serde_json::Value = serde_json::from_str(include_str!("npm-ranges.json")).unwrap();
    let mut wrong = Vec::new();
    for case in vectors["cases"].as_array().unwrap() {
        let (version, range, expected) = (
            case[0].as_str().unwrap(),
            case[1].as_str().unwrap(),
            case[2].as_bool().unwrap(),
        );
        if qk_graph::satisfies(version, range) != expected {
            wrong.push(format!("{version:?} in {range:?}: npm says {expected}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of the cases differ:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}
