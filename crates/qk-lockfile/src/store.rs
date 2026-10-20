//! Where pnpm puts each installation: its virtual store,
//! `node_modules/.pnpm/<directory>/node_modules/<name>`, the directory named
//! after the snapshot key by pnpm's `depPathToFilename`. Importers'
//! `node_modules/<name>` and the hoisted `node_modules/.pnpm/node_modules`
//! are links into it.
//!
//! A directory is matched to the snapshots whose names pnpm would give it,
//! rather than to the name and version in its `package.json`: a name and
//! version can be installed against different peers or patches, each in its
//! own directory and by its own snapshot. pnpm writes a name as the key with
//! characters a file name cannot hold made `+`, and the peers' and patch's
//! parentheses `_`. A name that is too long, or has capitals, is cut and given
//! a hash of the whole; that hash differs between pnpm versions writing the
//! same lockfile (md5 in base32 in pnpm 9, a cut sha256 in hex since pnpm 10),
//! so a hashed directory is matched by the part of the name it keeps, to every
//! snapshot whose name starts so. A directory no snapshot matches, such as one
//! a later pnpm names otherwise or one left from an earlier install, belongs
//! to none.

use std::collections::BTreeMap;

use crate::Lockfile;

/// Each snapshot's store directory, as pnpm names it unhashed.
pub struct Store {
    names: BTreeMap<String, Vec<String>>,
}

impl Store {
    pub fn new(lockfile: &Lockfile) -> Self {
        let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for key in lockfile.snapshots.keys() {
            // pnpm 9 keeps `#`, as in a git dependency's commit; pnpm 10 and
            // later make it `+`.
            let mut variants = vec![filename(key, false)];
            if key.contains('#') {
                variants.push(filename(key, true));
            }
            for name in variants {
                names.entry(name).or_default().push(key.clone());
            }
        }
        Self { names }
    }

    /// The snapshot keys a store directory may hold: the one it is named
    /// after, or for a hashed name every one whose name starts as it does.
    /// Empty when it matches none.
    pub fn snapshots(&self, directory: &str) -> Vec<&str> {
        if let Some(keys) = self.names.get(directory) {
            return keys.iter().map(String::as_str).collect();
        }
        let Some(prefix) = unhashed(directory) else {
            return Vec::new();
        };
        self.names
            .range(prefix.to_owned()..)
            .take_while(|(name, _)| name.starts_with(prefix))
            .flat_map(|(_, keys)| keys.iter().map(String::as_str))
            .collect()
    }
}

/// The store directory a workspace-relative path lies in, as the path names
/// it: `<directory>` of `…node_modules/.pnpm/<directory>/…`. `None` outside the
/// store, and for the store's own `node_modules` and `lock.yaml`.
pub fn store_directory(path: &str) -> Option<&str> {
    const STORE: &str = "node_modules/.pnpm/";
    let mut start = 0;
    while let Some(found) = path[start..].find(STORE) {
        let at = start + found;
        if at == 0 || path.as_bytes()[at - 1] == b'/' {
            let rest = &path[at + STORE.len()..];
            let directory = rest.split('/').next().unwrap_or(rest);
            return (!directory.is_empty() && !matches!(directory, "node_modules" | "lock.yaml"))
                .then_some(directory);
        }
        start = at + 1;
    }
    None
}

/// pnpm's `depPathToFilename` before hashing: the characters a file name
/// cannot hold become `+`, the closing parenthesis at the end goes, and the
/// others, with `)(` as one, become `_`.
fn filename(key: &str, escape_hash: bool) -> String {
    let key = key.strip_prefix('/').unwrap_or(key);
    let escaped: String = key
        .chars()
        .map(|ch| match ch {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '+',
            '#' if escape_hash => '+',
            ch => ch,
        })
        .collect();
    if !escaped.contains('(') {
        return escaped;
    }
    let trimmed = escaped.strip_suffix(')').unwrap_or(&escaped);
    let mut name = String::with_capacity(trimmed.len());
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            ')' if chars.peek() == Some(&'(') => {
                chars.next();
                name.push('_');
            }
            '(' | ')' => name.push('_'),
            ch => name.push(ch),
        }
    }
    name
}

/// The part of a hashed store directory's name kept before `_<hash>`: a
/// 26-character base32 md5 (pnpm 9) or 32 hex characters of sha256 (pnpm 10
/// and later). `None` for a name without such a suffix.
fn unhashed(directory: &str) -> Option<&str> {
    let (prefix, hash) = directory.rsplit_once('_')?;
    let base32 = hash.len() == 26
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte));
    let hex = hash.len() == 32
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    (!prefix.is_empty() && (base32 || hex)).then_some(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_directories_as_pnpm_does() {
        for (key, name) in [
            ("react@19.2.3", "react@19.2.3"),
            ("@babel/core@7.24.0", "@babel+core@7.24.0"),
            (
                "react-dom@19.2.3(react@19.2.3)",
                "react-dom@19.2.3_react@19.2.3",
            ),
            (
                "@typescript-eslint/eslint-plugin@8.0.0(@typescript-eslint/parser@8.0.0(eslint@9.0.0)(typescript@5.5.0))(eslint@9.0.0)",
                "@typescript-eslint+eslint-plugin@8.0.0_@typescript-eslint+parser@8.0.0_eslint@9.0.0_typescript@5.5.0__eslint@9.0.0",
            ),
            (
                "foo@1.0.0(patch_hash=abc123)",
                "foo@1.0.0_patch_hash=abc123",
            ),
            ("foo@file:packages/foo", "foo@file+packages+foo"),
        ] {
            assert_eq!(filename(key, true), name, "{key}");
        }
        assert_eq!(
            filename("foo@git+https://host/foo.git#abc", false),
            "foo@git+https+++host+foo.git#abc"
        );
        assert_eq!(
            filename("foo@git+https://host/foo.git#abc", true),
            "foo@git+https+++host+foo.git+abc"
        );
    }

    #[test]
    fn finds_the_store_directory_of_a_path() {
        for (path, directory) in [
            (
                "node_modules/.pnpm/fallow@1.0.0/node_modules/fallow/index.js",
                Some("fallow@1.0.0"),
            ),
            (
                "node_modules/.pnpm/@scope+tool@2.0.0_react@19.0.0/node_modules/@scope/tool/package.json",
                Some("@scope+tool@2.0.0_react@19.0.0"),
            ),
            ("node_modules/.pnpm/fallow@1.0.0", Some("fallow@1.0.0")),
            ("nested/node_modules/.pnpm/a@1.0.0/x", Some("a@1.0.0")),
            ("node_modules/.pnpm/node_modules/fallow/index.js", None),
            ("node_modules/.pnpm/lock.yaml", None),
            ("node_modules/.modules.yaml", None),
            ("node_modules/fallow/index.js", None),
            ("not_node_modules/.pnpm/a@1.0.0/x", None),
        ] {
            assert_eq!(store_directory(path), directory, "{path}");
        }
    }

    #[test]
    fn matches_directories_to_snapshots_hashed_or_not() {
        let long = format!("a{}", "b".repeat(150));
        let lockfile = Lockfile::parse(&format!(
            "lockfileVersion: '9.0'
importers:
  .: {{}}
packages:
  react@19.0.0: {{resolution: {{integrity: sha512-r}}}}
  '@scope/tool@2.0.0': {{resolution: {{integrity: sha512-t}}}}
  JSONStream@1.3.5: {{resolution: {{integrity: sha512-j}}}}
  {long}@1.0.0: {{resolution: {{integrity: sha512-l}}}}
snapshots:
  react@19.0.0: {{}}
  '@scope/tool@2.0.0(react@19.0.0)': {{}}
  '@scope/tool@2.0.0': {{}}
  JSONStream@1.3.5: {{}}
  {long}@1.0.0(react@19.0.0): {{}}
"
        ))
        .unwrap();
        let store = Store::new(&lockfile);
        assert_eq!(store.snapshots("react@19.0.0"), ["react@19.0.0"]);
        // Peers in the name keep apart what the version alone does not.
        assert_eq!(
            store.snapshots("@scope+tool@2.0.0_react@19.0.0"),
            ["@scope/tool@2.0.0(react@19.0.0)"]
        );
        assert_eq!(store.snapshots("@scope+tool@2.0.0"), ["@scope/tool@2.0.0"]);
        // Capitals are hashed, in pnpm 9's base32 or pnpm 10's hex.
        assert_eq!(
            store.snapshots("JSONStream@1.3.5_ty4dlt2jxkuqxqjgqvzsxyhf4e"),
            ["JSONStream@1.3.5"]
        );
        // A long name is cut, at 120 characters by default, before its hash.
        let cut = &format!("{long}@1.0.0_react@19.0.0")[..120 - 33];
        assert_eq!(
            store.snapshots(&format!("{cut}_0123456789abcdef0123456789abcdef")),
            [format!("{long}@1.0.0(react@19.0.0)")]
        );
        assert!(store.snapshots("react@18.0.0").is_empty());
        assert!(store.snapshots("react@19.0.0_notahash").is_empty());
    }
}
