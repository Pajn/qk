//! Which installed packages a task's key covers, and which one a read in
//! `node_modules` belongs to, for the sandbox and input analysis alike.
//!
//! A key counts what its importers install and every installation of the
//! packages its `externalDependencies` name. A read is placed by the pnpm
//! store directory it lies in, through the links to it where the path names
//! one; see [`qk_lockfile::Store`] for how a directory is matched to the
//! snapshots it holds. A path outside the store, such as `node_modules/.bin`,
//! `.modules.yaml` or a tool's `node_modules/.cache`, and a directory no
//! snapshot matches belong to no package, so they are never reported.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use qk_lockfile::{Lockfile, Store, package_name, store_directory};

pub(crate) struct Installs {
    /// The workspace root, resolved.
    root: PathBuf,
    lockfile: Lockfile,
    store: Store,
    /// The store's directories on disk, with the snapshots each holds.
    directories: Vec<(PathBuf, Vec<String>)>,
}

impl Installs {
    /// `None` without a pnpm lockfile qk reads, when no key has installed sets
    /// and every package is covered.
    pub(crate) fn load(root: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(root.join("pnpm-lock.yaml")).ok()?;
        let lockfile = Lockfile::parse(&text).ok()?;
        let store = Store::new(&lockfile);
        let mut directories = Vec::new();
        let virtual_store = root.join("node_modules/.pnpm");
        for entry in std::fs::read_dir(&virtual_store).into_iter().flatten() {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) || name == "node_modules" {
                continue;
            }
            let keys: Vec<String> = store
                .snapshots(name)
                .into_iter()
                .map(str::to_owned)
                .collect();
            if !keys.is_empty() {
                directories.push((entry.path(), keys));
            }
        }
        directories.sort();
        Some(Self {
            root: root.to_owned(),
            lockfile,
            store,
            directories,
        })
    }

    /// The snapshots `resolved`'s key covers, or `None` when it covers every
    /// installed package: its lockfile is keyed whole.
    pub(crate) fn covered(&self, resolved: &qk_cache::Resolved) -> Option<BTreeSet<String>> {
        let importers = resolved.inferred.lockfile.as_ref()?;
        Some(self.lockfile.reached_snapshots(
            importers.iter().map(String::as_str),
            resolved.declared.external.iter().map(String::as_str),
        ))
    }

    /// The store directories on disk holding only installations `covered`
    /// leaves out.
    pub(crate) fn unkeyed_directories(&self, covered: &BTreeSet<String>) -> Vec<PathBuf> {
        self.directories
            .iter()
            .filter(|(_, keys)| !keys.iter().any(|key| covered.contains(key)))
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// The package a read of `path`, absolute, belongs to when `covered`
    /// leaves out its installation. A path naming a link into the store, such
    /// as an importer's `node_modules/<name>/…`, is followed to it.
    pub(crate) fn unkeyed(&self, covered: &BTreeSet<String>, path: &Path) -> Option<String> {
        let relative = |path: &Path| -> Option<String> {
            Some(
                path.strip_prefix(&self.root)
                    .ok()?
                    .to_str()?
                    .replace('\\', "/"),
            )
        };
        let named = relative(path)?;
        if !named.split('/').any(|part| part == "node_modules") {
            return None;
        }
        let directory = match store_directory(&named) {
            Some(directory) => directory.to_owned(),
            None => store_directory(&relative(&resolve(path))?)?.to_owned(),
        };
        let keys = self.store.snapshots(&directory);
        if keys.is_empty() || keys.iter().any(|key| covered.contains(*key)) {
            return None;
        }
        // Snapshots sharing a hashed name's kept part are, but for its last
        // few characters, the same name, so the same package.
        Some(package_name(keys[0]).to_owned())
    }
}

/// `path` with its links resolved, as far up as it exists.
fn resolve(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut current = path;
    loop {
        if let Ok(resolved) = current.canonicalize() {
            return rest
                .iter()
                .rev()
                .fold(resolved, |path, part| path.join(part));
        }
        let (Some(parent), Some(name)) = (current.parent(), current.file_name()) else {
            return path.to_owned();
        };
        rest.push(name);
        current = parent;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const LOCKFILE: &str = "lockfileVersion: '9.0'
importers:
  .:
    devDependencies:
      fallow:
        specifier: ^1.0.0
        version: 1.0.0
      '@scope/tool':
        specifier: ^2.0.0
        version: 2.0.0(react@19.0.0)
  apps/app:
    dependencies:
      react:
        specifier: ^19.0.0
        version: 19.0.0
packages:
  fallow@1.0.0: {resolution: {integrity: sha512-f}}
  helper@1.0.0: {resolution: {integrity: sha512-h}}
  react@19.0.0: {resolution: {integrity: sha512-r}}
  '@scope/tool@2.0.0': {resolution: {integrity: sha512-t}}
snapshots:
  fallow@1.0.0:
    dependencies:
      helper: 1.0.0
  helper@1.0.0: {}
  react@19.0.0: {}
  '@scope/tool@2.0.0(react@19.0.0)':
    dependencies:
      react: 19.0.0
";

    /// A workspace laid out as pnpm installs `LOCKFILE`.
    fn workspace() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("pnpm-lock.yaml"), LOCKFILE).unwrap();
        for (directory, name) in [
            ("fallow@1.0.0", "fallow"),
            ("helper@1.0.0", "helper"),
            ("react@19.0.0", "react"),
            ("@scope+tool@2.0.0_react@19.0.0", "@scope/tool"),
        ] {
            let package = root.join(format!(
                "node_modules/.pnpm/{directory}/node_modules/{name}"
            ));
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(package.join("index.js"), "").unwrap();
        }
        std::fs::create_dir_all(root.join("node_modules/.pnpm/node_modules")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/@scope")).unwrap();
        std::fs::create_dir_all(root.join("apps/app/node_modules")).unwrap();
        for (target, link) in [
            (
                ".pnpm/fallow@1.0.0/node_modules/fallow",
                "node_modules/fallow",
            ),
            (
                "../.pnpm/@scope+tool@2.0.0_react@19.0.0/node_modules/@scope/tool",
                "node_modules/@scope/tool",
            ),
            (
                "../../../node_modules/.pnpm/react@19.0.0/node_modules/react",
                "apps/app/node_modules/react",
            ),
            (
                "../helper@1.0.0/node_modules/helper",
                "node_modules/.pnpm/node_modules/helper",
            ),
        ] {
            std::os::unix::fs::symlink(target, root.join(link)).unwrap();
        }
        temp
    }

    /// The key of a task in `apps/app` naming `external`, without the root
    /// importer, as a task with `externalDependencies` is keyed.
    fn covered(installs: &Installs, external: &[&str]) -> BTreeSet<String> {
        installs
            .lockfile
            .reached_snapshots(["apps/app"], external.iter().copied())
    }

    #[test]
    fn places_reads_by_their_store_directory_or_the_link_to_it() {
        let temp = workspace();
        let root = temp.path().canonicalize().unwrap();
        let installs = Installs::load(&root).unwrap();
        let none = covered(&installs, &[]);
        let unkeyed = |path: &str| installs.unkeyed(&none, &root.join(path));
        for (path, package) in [
            (
                "node_modules/.pnpm/fallow@1.0.0/node_modules/fallow/index.js",
                Some("fallow"),
            ),
            // Through an importer's link, and the hoisted one.
            ("node_modules/fallow/index.js", Some("fallow")),
            (
                "node_modules/.pnpm/node_modules/helper/index.js",
                Some("helper"),
            ),
            // A scoped package installed against a peer.
            ("node_modules/@scope/tool/index.js", Some("@scope/tool")),
            (
                "node_modules/.pnpm/@scope+tool@2.0.0_react@19.0.0/node_modules/@scope/tool/missing.js",
                Some("@scope/tool"),
            ),
            // What the task's own importer installs.
            ("apps/app/node_modules/react/index.js", None),
            (
                "node_modules/.pnpm/react@19.0.0/node_modules/react/index.js",
                None,
            ),
            // Outside the store, or in a directory no snapshot names.
            ("node_modules/.modules.yaml", None),
            ("node_modules/.pnpm/lock.yaml", None),
            (
                "node_modules/.pnpm/stale@0.1.0/node_modules/stale/index.js",
                None,
            ),
            ("apps/app/src/index.ts", None),
        ] {
            assert_eq!(unkeyed(path).as_deref(), package, "{path}");
        }
        assert_eq!(
            installs.unkeyed_directories(&none),
            [
                root.join("node_modules/.pnpm/@scope+tool@2.0.0_react@19.0.0"),
                root.join("node_modules/.pnpm/fallow@1.0.0"),
                root.join("node_modules/.pnpm/helper@1.0.0"),
            ]
        );
        // Naming a package covers it and what it reaches.
        let fallow = covered(&installs, &["fallow"]);
        assert_eq!(
            installs.unkeyed(&fallow, &root.join("node_modules/fallow/index.js")),
            None
        );
        assert_eq!(
            installs.unkeyed(
                &fallow,
                &root.join("node_modules/.pnpm/node_modules/helper/index.js")
            ),
            None
        );
        assert_eq!(
            installs.unkeyed_directories(&fallow),
            [root.join("node_modules/.pnpm/@scope+tool@2.0.0_react@19.0.0")]
        );
    }
}
