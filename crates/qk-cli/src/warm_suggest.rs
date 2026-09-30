//! `qk warm suggest`: candidate warm paths from what a task wrote outside its
//! declared outputs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A directory the task wrote into, and how many of the files it wrote are
/// below it.
#[derive(Debug, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub files: usize,
}

/// Each written path's topmost ancestor directory that holds no source file,
/// as a build keeps its own directories apart from what the checkout tracks.
/// A write with no such ancestor, into a directory of sources, is left out:
/// that directory is not the task's to keep.
pub fn candidates(writes: &BTreeSet<String>, sources: &BTreeSet<String>) -> Vec<Candidate> {
    // Every directory that holds a source file, at any depth.
    let mut holds_sources = BTreeSet::new();
    for source in sources {
        for ancestor in Path::new(source).ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() || !holds_sources.insert(ancestor.to_owned()) {
                break;
            }
        }
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for write in writes {
        let path = Path::new(write);
        let mut ancestors: Vec<&Path> = path
            .ancestors()
            .skip(1)
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .collect();
        ancestors.reverse();
        if let Some(directory) = ancestors
            .into_iter()
            .find(|ancestor| !holds_sources.contains(*ancestor))
            .and_then(Path::to_str)
        {
            *counts.entry(directory.to_owned()).or_default() += 1;
        }
    }
    let mut candidates: Vec<Candidate> = counts
        .into_iter()
        .map(|(path, files)| Candidate { path, files })
        .collect();
    candidates.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.path.cmp(&b.path)));
    candidates
}

/// Bytes below `path`, not following links.
pub fn size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn writes_gather_under_the_topmost_directory_without_sources() {
        let sources = set(&["android/build.gradle", "src/index.ts"]);
        let writes = set(&[
            "android/app/build/intermediates/a.class",
            "android/app/build/tmp/b",
            "android/.gradle/history.bin",
            ".cache/tool/entry",
        ]);
        assert_eq!(
            candidates(&writes, &sources),
            [
                Candidate {
                    path: "android/app".into(),
                    files: 2
                },
                Candidate {
                    path: ".cache".into(),
                    files: 1
                },
                Candidate {
                    path: "android/.gradle".into(),
                    files: 1
                },
            ]
        );
    }

    #[test]
    fn writes_beside_sources_are_left_out() {
        let sources = set(&["src/index.ts"]);
        assert!(candidates(&set(&["src/generated.ts"]), &sources).is_empty());
    }
}
