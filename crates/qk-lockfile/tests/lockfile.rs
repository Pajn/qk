//! Lockfile scenarios exercising cache keys and installation-aware changes.

use std::collections::BTreeMap;

use qk_lockfile::Lockfile;

/// Importers under `apps/` mapped to what each installs directly, and snapshot
/// keys mapped to their dependencies, as `name version` pairs.
fn lockfile(
    importers: &[(&str, &[(&str, &str)])],
    snapshots: &[(&str, &[(&str, &str)])],
) -> String {
    let mut lines = vec![
        "lockfileVersion: '9.0'".to_owned(),
        "settings:".into(),
        "  autoInstallPeers: true".into(),
        "importers:".into(),
        "  .: {}".into(),
    ];
    for (importer, dependencies) in importers {
        lines.push(format!("  apps/{importer}:"));
        lines.push("    dependencies:".into());
        for (name, version) in *dependencies {
            lines.push(format!("      '{name}':"));
            lines.push(format!("        specifier: '{version}'"));
            lines.push(format!("        version: '{version}'"));
        }
        if dependencies.is_empty() {
            lines.pop();
            lines.last_mut().unwrap().push_str(" {}");
        }
    }
    lines.push("packages:".into());
    let mut packages: Vec<_> = snapshots
        .iter()
        .map(|(key, _)| &key[..key.find('(').unwrap_or(key.len())])
        .collect();
    packages.dedup();
    for key in packages {
        lines.push(format!("  '{key}':"));
        lines.push(format!("    resolution: {{integrity: sha512-{key}}}"));
    }
    lines.push("snapshots:".into());
    for (key, dependencies) in snapshots {
        if dependencies.is_empty() {
            lines.push(format!("  '{key}': {{}}"));
            continue;
        }
        lines.push(format!("  '{key}':"));
        lines.push("    dependencies:".into());
        for (name, version) in *dependencies {
            lines.push(format!("      '{name}': '{version}'"));
        }
    }
    lines.join("\n") + "\n"
}

/// The importers under `apps/` whose installed set differs between the two.
fn reached(base: &str, head: &str) -> Vec<String> {
    let base = Lockfile::parse(base).unwrap();
    let head = Lockfile::parse(head).unwrap();
    let mut importers: BTreeMap<String, ()> = BTreeMap::new();
    for importer in ["app-a", "app-b", "app-c"] {
        let path = format!("apps/{importer}");
        if base.installed(&path) != head.installed(&path) {
            importers.insert(importer.to_owned(), ());
        }
    }
    importers.into_keys().collect()
}

#[test]
fn a_second_version_of_a_package_reaches_only_the_importer_installing_it() {
    let base = lockfile(
        &[("app-a", &[("tailwindcss", "3.4.17")]), ("app-b", &[])],
        &[
            ("arg@5.0.2", &[]),
            ("tailwindcss@3.4.17", &[("arg", "5.0.2")]),
        ],
    );
    let head = lockfile(
        &[
            ("app-a", &[("tailwindcss", "3.4.17")]),
            ("app-b", &[("expo-updates", "57.0.23")]),
        ],
        &[
            ("arg@4.1.3", &[]),
            ("arg@5.0.2", &[]),
            ("expo-updates@57.0.23", &[("arg", "4.1.3")]),
            ("tailwindcss@3.4.17", &[("arg", "5.0.2")]),
        ],
    );
    assert_eq!(reached(&base, &head), ["app-b"]);
}

#[test]
fn a_transitive_version_bump_reaches_the_importers_installing_it() {
    let importers: &[(&str, &[(&str, &str)])] = &[
        ("app-a", &[("lib", "1.0.0")]),
        ("app-c", &[("other", "1.0.0")]),
    ];
    let base = lockfile(
        importers,
        &[
            ("dep@1.0.0", &[]),
            ("lib@1.0.0", &[("dep", "1.0.0")]),
            ("other@1.0.0", &[]),
        ],
    );
    let head = lockfile(
        importers,
        &[
            ("dep@1.1.0", &[]),
            ("lib@1.0.0", &[("dep", "1.1.0")]),
            ("other@1.0.0", &[]),
        ],
    );
    assert_eq!(reached(&base, &head), ["app-a"]);
}

#[test]
fn dropping_one_of_two_versions_reaches_only_the_importer_that_moved() {
    // Comparing versions only by package name would also mark the unchanged importer.
    let importers: &[(&str, &[(&str, &str)])] = &[
        ("app-a", &[("lib", "1.0.0")]),
        ("app-b", &[("unrelated", "1.0.0")]),
        ("app-c", &[("other", "1.0.0")]),
    ];
    let base = lockfile(
        importers,
        &[
            ("dep@1.0.0", &[]),
            ("dep@2.0.0", &[]),
            ("lib@1.0.0", &[("dep", "1.0.0")]),
            ("other@1.0.0", &[("dep", "2.0.0")]),
            ("unrelated@1.0.0", &[]),
        ],
    );
    let head = lockfile(
        importers,
        &[
            ("dep@2.0.0", &[]),
            ("lib@1.0.0", &[("dep", "2.0.0")]),
            ("other@1.0.0", &[("dep", "2.0.0")]),
            ("unrelated@1.0.0", &[]),
        ],
    );
    assert_eq!(reached(&base, &head), ["app-a"]);
}

#[test]
fn a_package_dropped_with_the_version_requiring_it_reaches_only_its_importer() {
    let base = lockfile(
        &[
            ("app-a", &[("other", "1.0.0")]),
            ("app-b", &[("lib", "1.0.0")]),
        ],
        &[
            ("gone@1.0.0", &[]),
            ("lib@1.0.0", &[("gone", "1.0.0")]),
            ("other@1.0.0", &[]),
        ],
    );
    let head = lockfile(
        &[
            ("app-a", &[("other", "1.0.0")]),
            ("app-b", &[("lib", "1.0.1")]),
        ],
        &[("lib@1.0.1", &[]), ("other@1.0.0", &[])],
    );
    assert_eq!(reached(&base, &head), ["app-b"]);
}

#[test]
fn editing_a_patch_reaches_the_importers_installing_the_patched_package() {
    // pnpm records the patch applied to each installation in its key, whether
    // patchedDependencies names a version or the package alone.
    let patched = |hash: &str| {
        let version = format!("1.0.0(patch_hash={hash})");
        let key = format!("lib@{version}");
        lockfile(
            &[
                ("app-a", &[("lib", &version)]),
                ("app-c", &[("other", "1.0.0")]),
            ],
            &[(&key, &[]), ("other@1.0.0", &[])],
        )
    };
    let unpatched = lockfile(
        &[
            ("app-a", &[("lib", "1.0.0")]),
            ("app-c", &[("other", "1.0.0")]),
        ],
        &[("lib@1.0.0", &[]), ("other@1.0.0", &[])],
    );
    assert_eq!(reached(&patched("aaaa"), &patched("bbbb")), ["app-a"]);
    assert_eq!(reached(&unpatched, &patched("aaaa")), ["app-a"]);
}

#[test]
fn a_new_integrity_for_the_same_version_reaches_its_importers() {
    let importers: &[(&str, &[(&str, &str)])] = &[
        ("app-a", &[("lib", "1.0.0")]),
        ("app-c", &[("other", "1.0.0")]),
    ];
    let base = lockfile(importers, &[("lib@1.0.0", &[]), ("other@1.0.0", &[])]);
    let head = base.replace("sha512-lib@1.0.0", "sha512-republished");
    assert_eq!(reached(&base, &head), ["app-a"]);
}

#[test]
fn follows_aliases_and_skips_workspace_links() {
    let lockfile = Lockfile::parse(
        "lockfileVersion: '9.0'
importers:
  apps/app-a:
    dependencies:
      string-width-cjs:
        specifier: npm:string-width@^4.2.0
        version: string-width@4.2.3
      '@example/core':
        specifier: workspace:*
        version: link:../../packages/core
packages:
  string-width@4.2.3:
    resolution: {integrity: sha512-a}
  ansi-regex@5.0.1:
    resolution: {integrity: sha512-b}
snapshots:
  string-width@4.2.3:
    dependencies:
      ansi-regex: 5.0.1
  ansi-regex@5.0.1: {}
",
    )
    .unwrap();
    let installed = lockfile.installed("apps/app-a").unwrap();
    let text = installed.iter().cloned().collect::<Vec<_>>().join("\n");
    assert!(
        text.contains("string-width-cjs -> string-width@4.2.3"),
        "{text}"
    );
    assert!(text.contains(r#""key":"ansi-regex@5.0.1""#), "{text}");
    assert!(!text.contains("core"), "{text}");
    assert_eq!(installed.len(), 3);
    assert!(lockfile.installed("apps/missing").is_none());
}

#[test]
fn reads_the_workspace_document_after_the_lock_of_pnpm_itself() {
    let text = "---
lockfileVersion: '9.0'
importers:
  .:
    configDependencies: {}
    packageManagerDependencies:
      pnpm:
        specifier: 12.8.1
        version: 12.8.1
packages:
  pnpm@12.8.1:
    resolution: {integrity: sha512-pnpm}
snapshots:
  pnpm@12.8.1: {}

---
lockfileVersion: '9.0'
settings:
  autoInstallPeers: true
importers:
  .:
    devDependencies:
      lib:
        specifier: ^1.0.0
        version: 1.0.0
packages:
  lib@1.0.0:
    resolution: {integrity: sha512-lib}
snapshots:
  lib@1.0.0: {}
";
    let lockfile = Lockfile::parse(text).unwrap();
    assert_eq!(lockfile.installed(".").unwrap().len(), 2);
    assert_eq!(lockfile.global()["settings"]["autoInstallPeers"], true);
    // A pnpm upgrade changes what every installation depends on.
    let upgraded = Lockfile::parse(&text.replace("12.8.1", "12.9.0")).unwrap();
    assert_ne!(lockfile.global(), upgraded.global());
    assert_eq!(lockfile.installed("."), upgraded.installed("."));
}

#[test]
fn rejects_other_lockfile_versions() {
    for text in [
        "lockfileVersion: '6.0'\n",
        "importers: {}\n",
        "- not a lockfile\n",
    ] {
        assert!(Lockfile::parse(text).is_err(), "{text}");
    }
}

#[test]
fn finds_every_installation_of_a_package_by_name() {
    let text = lockfile(
        &[
            ("app-a", &[("@scope/tool", "1.0.0")]),
            ("app-b", &[("@scope/tool", "2.0.0(react@19.0.0)")]),
        ],
        &[
            ("@scope/tool@1.0.0", &[("dep", "1.0.0")]),
            ("@scope/tool@2.0.0(react@19.0.0)", &[]),
            ("@scope/tool-extra@1.0.0", &[]),
            ("dep@1.0.0", &[]),
        ],
    );
    let lockfile = Lockfile::parse(&text).unwrap();
    let keys: Vec<_> = lockfile
        .package("@scope/tool")
        .into_iter()
        .map(|fingerprint| {
            serde_json::from_str::<serde_json::Value>(&fingerprint).unwrap()["key"].clone()
        })
        .collect();
    assert_eq!(
        keys,
        [
            "@scope/tool@1.0.0",
            "@scope/tool@2.0.0(react@19.0.0)",
            "dep@1.0.0"
        ]
    );
    assert!(lockfile.package("missing").is_empty());
}
