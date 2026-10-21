//! Decode strace's path-oriented output. Unknown/truncated records are surfaced.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

use crate::{Access, Category, Operation};
use anyhow::Result;

pub fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(part.as_os_str()),
        }
    }
    result
}

/// How much of a trace is read, and which accesses are kept.
pub struct Limits<'a> {
    /// Lines kept, and their bytes; past either the trace is cut short.
    pub records: usize,
    pub bytes: usize,
    /// Accesses kept; past it the trace is cut short.
    pub accesses: usize,
    /// Whether an access to an absolute path is kept; `None` keeps all.
    pub keep: Option<&'a dyn Fn(&Path) -> bool>,
}

/// What `inputs analyze` reads of a trace: every access, within a budget
/// that keeps its report readable.
pub const ANALYSIS: Limits<'static> = Limits {
    records: 200_000,
    bytes: 32 * 1024 * 1024,
    accesses: 100_000,
    keep: None,
};

/// A cwd no path can be in, to tell a line whose paths depend on the
/// process's working directory from one that names its paths whole: deep
/// enough that no `..` climbs out of it. A path relative to a descriptor,
/// `AT_FDCWD` included, which strace names, does not depend on it.
static UNKNOWN_CWD: std::sync::LazyLock<PathBuf> =
    std::sync::LazyLock::new(|| std::iter::once("/").chain(["\0"; 64]).collect());

/// Whether a trace line can matter under `keep`: it changes a process's
/// working directory or makes a process, names a path relative to the
/// working directory, cannot be decoded, or names a kept path.
fn relevant(process: u32, call: &str, keep: &dyn Fn(&Path) -> bool) -> bool {
    let unknown = UNKNOWN_CWD.as_path();
    let Some((events, change, child)) = decode(process, call, unknown) else {
        return true;
    };
    change.is_some()
        || child.is_some()
        || events.iter().any(|event| {
            let path = Path::new(&event.path);
            // Still somewhere below the unknown cwd, after any `..`.
            path.components()
                .any(|component| component.as_os_str() == "\0")
                || keep(path)
        })
}

pub fn collect(directory: &Path, cwd: &Path) -> Result<(Vec<Access>, Vec<String>)> {
    collect_within(directory, cwd, &ANALYSIS)
}

/// The accesses of a trace within `limits`, in the order they were made,
/// and what kept them from being complete.
pub fn collect_within(
    directory: &Path,
    cwd: &Path,
    limits: &Limits,
) -> Result<(Vec<Access>, Vec<String>)> {
    let mut records = Vec::new();
    let mut diagnostics = Vec::new();
    let mut pending = BTreeMap::<u32, (f64, String)>::new();
    let mut bytes_read = 0usize;
    let mut files = std::fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    files.sort_by_key(|entry| entry.file_name());
    'files: for entry in &files {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(process) = name
            .rsplit('.')
            .next()
            .and_then(|id| id.parse::<u32>().ok())
        else {
            continue;
        };
        let reader = BufReader::new(std::fs::File::open(entry.path())?);
        for (sequence, line) in reader.lines().enumerate() {
            let line = line?;
            if bytes_read > limits.bytes || records.len() >= limits.records {
                diagnostics.push(format!(
                    "trace collection exceeded its {} MiB/{}-record budget; report truncated",
                    limits.bytes / 1024 / 1024,
                    limits.records
                ));
                break 'files;
            }
            let Some((time, call)) = line.split_once(' ') else {
                continue;
            };
            let Ok(mut time) = time.parse::<f64>() else {
                diagnostics.push("trace line has an invalid timestamp".into());
                continue;
            };
            let call = call.trim();
            if call.starts_with("---") || call.starts_with("+++") {
                continue;
            }
            if let Some(prefix) = call.strip_suffix("<unfinished ...>") {
                pending.insert(process, (time, prefix.to_owned()));
                continue;
            }
            let call = if call.starts_with("<...") {
                match (pending.remove(&process), call.split_once("resumed>")) {
                    (Some((started, prefix)), Some((_, suffix))) => {
                        time = started;
                        format!("{prefix}{suffix}")
                    }
                    _ => {
                        diagnostics
                            .push(format!("unmatched resumed syscall for process {process}"));
                        continue;
                    }
                }
            } else {
                call.to_owned()
            };
            if limits
                .keep
                .is_some_and(|keep| !relevant(process, &call, keep))
            {
                continue;
            }
            bytes_read = bytes_read.saturating_add(call.len());
            records.push((time, process, sequence, call));
        }
    }
    if files.is_empty() {
        diagnostics.push("no syscall trace files were produced".into());
    }
    if !pending.is_empty() {
        diagnostics.push("some syscalls did not finish before recording stopped".into());
    }
    records.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut directories = BTreeMap::new();
    let mut accesses = Vec::new();
    let mut undecoded = 0;
    let mut undecoded_names = std::collections::BTreeSet::new();
    for (_, process, _, call) in records {
        let current = directories
            .entry(process)
            .or_insert_with(|| cwd.to_owned())
            .clone();
        match decode(process, &call, &current) {
            Some((events, change, child)) => {
                accesses.extend(
                    events.into_iter().filter(|event| {
                        limits.keep.is_none_or(|keep| keep(Path::new(&event.path)))
                    }),
                );
                if let Some(directory) = change {
                    directories.insert(process, directory);
                }
                if let Some(child) = child {
                    directories.insert(child, current);
                }
            }
            None => {
                undecoded += 1;
                undecoded_names.insert(call.split('(').next().unwrap_or("unknown").to_owned());
            }
        }
        if accesses.len() > limits.accesses {
            accesses.truncate(limits.accesses);
            diagnostics.push(format!(
                "recording exceeded {} accesses; report truncated",
                limits.accesses
            ));
            break;
        }
    }
    if undecoded > 0 {
        diagnostics.push(format!(
            "{undecoded} syscall records could not be decoded: {}",
            undecoded_names.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    diagnostics.sort();
    diagnostics.dedup();
    Ok((accesses, diagnostics))
}

type Decoded = (Vec<Access>, Option<PathBuf>, Option<u32>);

fn decode(process: u32, call: &str, cwd: &Path) -> Option<Decoded> {
    let (name, tail) = call.split_once('(')?;
    let (args, result) = tail.rsplit_once(" = ")?;
    let args = args.trim_end().strip_suffix(')')?;
    let args = arguments(args);
    let success = !result.starts_with("-1") && !result.starts_with('?');
    let status = if result.starts_with("-1 ENOENT") {
        "missing"
    } else if result.starts_with("-1") {
        result.split_whitespace().nth(1).unwrap_or("failed")
    } else if success {
        "success"
    } else {
        "unknown"
    };
    let mut events = Vec::new();
    let mut change = None;
    let mut child = None;
    let mut emit = |path: PathBuf, operation| {
        events.push(Access {
            process,
            path: path.to_string_lossy().into_owned(),
            resolved_path: None,
            keyed_path: None,
            package: None,
            operation,
            result: status.to_owned(),
            category: Category::Uncovered,
        });
    };
    let path = |index: usize, dir: Option<usize>| -> Option<PathBuf> {
        let value = quoted(args.get(index)?)?;
        let value = Path::new(&value);
        if value.is_absolute() {
            return Some(normalize(value));
        }
        let base = match dir {
            Some(index) => descriptor(args[index])
                .or_else(|| args[index].starts_with("AT_FDCWD").then(|| cwd.to_owned()))?,
            _ => cwd.to_owned(),
        };
        Some(normalize(&base.join(value)))
    };
    match name {
        "clone" | "clone3" | "fork" | "vfork" => {
            if success {
                child = result.split_whitespace().next()?.parse().ok();
            }
        }
        "chdir" => {
            if success {
                change = Some(path(0, None)?);
            }
        }
        "fchdir" => {
            if success {
                change = Some(descriptor(args.first()?)?);
            }
        }
        "getcwd" => {
            if success {
                change = Some(PathBuf::from(quoted(args.first()?)?));
            }
        }
        "open" | "openat" | "openat2" | "creat" => {
            let at = name.starts_with("openat");
            let file = path(usize::from(at), at.then_some(0))?;
            let flags = args.get(usize::from(at) + 1).copied().unwrap_or("");
            if name != "creat" && !flags.contains("O_WRONLY") && !flags.contains("O_PATH") {
                emit(file.clone(), Operation::OpenRead);
            } else if flags.contains("O_PATH") {
                emit(file.clone(), Operation::Metadata);
            }
            if name == "creat"
                || flags.contains("O_WRONLY")
                || flags.contains("O_RDWR")
                || flags.contains("O_CREAT")
                || flags.contains("O_TRUNC")
            {
                emit(file, Operation::Write);
            }
            // -yy gives the fd's actual target without resolving the path later.
            let resolved = success.then(|| descriptor(result)).flatten();
            for event in &mut events {
                event.resolved_path = resolved
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned());
            }
        }
        "getdents64" => emit(descriptor(args.first()?)?, Operation::ListDirectory),
        "readlink" => emit(path(0, None)?, Operation::ReadLink),
        "readlinkat" => emit(path(1, Some(0))?, Operation::ReadLink),
        "stat" | "lstat" | "stat64" | "lstat64" | "access" | "statfs" | "statfs64" | "getxattr"
        | "lgetxattr" | "listxattr" | "llistxattr" => emit(path(0, None)?, Operation::Metadata),
        "newfstatat" | "fstatat64" | "statx" | "faccessat" | "faccessat2" | "name_to_handle_at" => {
            // With an empty path, the call reads the descriptor itself: a
            // pipe, socket or other descriptor that is no file names no path.
            let unnamed = quoted(args.get(1)?).is_some_and(|path| path.is_empty())
                && args[0].contains('<')
                && descriptor(args[0]).is_none();
            if !unnamed {
                emit(path(1, Some(0))?, Operation::Metadata);
            }
        }
        "inotify_add_watch" => emit(path(1, None)?, Operation::Metadata),
        "unlink" | "rmdir" => emit(path(0, None)?, Operation::Delete),
        "unlinkat" => emit(path(1, Some(0))?, Operation::Delete),
        "rename" => {
            emit(path(0, None)?, Operation::Delete);
            emit(path(1, None)?, Operation::Rename);
        }
        "renameat" | "renameat2" => {
            emit(path(1, Some(0))?, Operation::Delete);
            emit(path(3, Some(2))?, Operation::Rename);
        }
        "mkdir" | "truncate" | "chmod" | "chown" | "lchown" | "utime" | "utimes" | "mknod"
        | "setxattr" | "lsetxattr" | "removexattr" | "lremovexattr" => {
            emit(path(0, None)?, Operation::Write)
        }
        "mkdirat" | "fchmodat" | "fchmodat2" | "fchownat" | "utimensat" | "mknodat"
        | "futimesat" => emit(path(1, Some(0))?, Operation::Write),
        "symlink" => emit(path(1, None)?, Operation::Write),
        "symlinkat" => emit(path(2, Some(1))?, Operation::Write),
        "link" => {
            emit(path(0, None)?, Operation::Metadata);
            emit(path(1, None)?, Operation::Write);
        }
        "linkat" => {
            emit(path(1, Some(0))?, Operation::Metadata);
            emit(path(3, Some(2))?, Operation::Write);
        }
        "execve" | "execveat" | "exit" | "exit_group" | "wait4" | "waitid" | "close" | "dup"
        | "dup2" | "dup3" | "kill" | "tkill" | "tgkill" | "rt_sigqueueinfo"
        | "rt_tgsigqueueinfo" | "pidfd_send_signal" | "pidfd_open" => {}
        _ => return None,
    }
    Some((events, change, child))
}

fn descriptor(arg: &str) -> Option<PathBuf> {
    let (_, path) = arg.split_once('<')?;
    let path = path.strip_suffix('>')?;
    let path = path.strip_suffix(" (deleted)").unwrap_or(path);
    path.starts_with('/').then(|| PathBuf::from(path))
}

/// Separate syscall arguments without splitting strings, structures or fd paths.
fn arguments(text: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let (mut start, mut nesting, mut string, mut escaped) = (0, 0usize, false, false);
    for (index, ch) in text.char_indices() {
        if string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                string = false;
            }
            continue;
        }
        match ch {
            '"' => string = true,
            '(' | '[' | '{' | '<' => nesting += 1,
            ')' | ']' | '}' | '>' => nesting = nesting.saturating_sub(1),
            ',' if nesting == 0 => {
                result.push(text[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    result.push(text[start..].trim());
    result
}

fn quoted(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut result = Vec::new();
    let mut index = 1;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        if byte == b'"' {
            if text[index..].trim().starts_with("...") {
                return None;
            }
            return String::from_utf8(result).ok();
        }
        if byte != b'\\' {
            result.push(byte);
            continue;
        }
        let escaped = *bytes.get(index)?;
        index += 1;
        result.push(match escaped {
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'\\' => b'\\',
            b'"' => b'"',
            b'0'..=b'7' => {
                let mut value = u16::from(escaped - b'0');
                for _ in 0..2 {
                    if let Some(next @ b'0'..=b'7') = bytes.get(index) {
                        value = value * 8 + u16::from(*next - b'0');
                        index += 1;
                    } else {
                        break;
                    }
                }
                u8::try_from(value).ok()?
            }
            _ => return None,
        });
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn resolves_cwd_dirfds_and_failed_probes_without_confusing_writes() {
        let cwd = Path::new("/repo/app");
        let (events, _, _) = decode(
            1,
            "openat(3</repo/shared>, \"a b.json\", O_RDONLY|O_CLOEXEC) = 4</repo/shared/a b.json>",
            cwd,
        )
        .unwrap();
        assert_eq!(events[0].path, "/repo/shared/a b.json");
        let (events, _, _) = decode(1, "newfstatat(AT_FDCWD, \"../missing\", 0x123, 0) = -1 ENOENT (No such file or directory)", cwd).unwrap();
        assert_eq!(events[0].result, "missing");
        assert_eq!(events[0].path, "/repo/missing");
        let (events, _, _) = decode(
            1,
            "openat(AT_FDCWD, \"out\", O_WRONLY|O_CREAT|O_TRUNC, 0666) = 5</repo/app/out>",
            cwd,
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].operation, Operation::Write);
        let (events, _, _) = decode(
            1,
            "getdents64(5</repo/app/templates>, [{d_name=\"a\"}], 4096) = 24",
            cwd,
        )
        .unwrap();
        assert_eq!(events[0].operation, Operation::ListDirectory);
    }

    #[test]
    fn detects_truncated_strings_and_decodes_escaped_paths() {
        assert_eq!(quoted(r#""a\040b\"c""#), Some("a b\"c".into()));
        assert_eq!(quoted(r#""long"..."#), None);
        assert_eq!(
            arguments(r#"3</a,b>, "c,d", {flags=O_RDONLY, mode=0}"#).len(),
            3
        );
    }

    #[test]
    fn accepts_padded_exit_records_and_uses_annotated_cwd() {
        assert!(decode(1, "exit_group(0)         = ?", Path::new("/repo")).is_some());
        let (events, _, _) = decode(
            2,
            "openat(AT_FDCWD</repo/sub>, \"file\", O_RDONLY) = 3</repo/sub/file>",
            Path::new("/repo"),
        )
        .unwrap();
        assert_eq!(events[0].path, "/repo/sub/file");
    }

    #[test]
    fn signals_and_path_metadata_do_not_disable_successful_collection() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("command-0.123"), concat!(
            "1.000 tgkill(123, 124, SIGURG) = 0\n",
            "1.001 kill(125, SIGTERM) = 0\n",
            "1.002 lgetxattr(\"f\", \"security.selinux\", 0x0, 0) = -1 ENODATA (No data available)\n",
            "1.003 inotify_add_watch(3, \"src\", IN_MODIFY) = 1\n",
            "1.004 setxattr(\"f\", \"user.test\", 0x123, 1, 0) = 0\n",
            "1.005 futimesat(AT_FDCWD, \"f\", NULL) = 0\n",
            "1.006 name_to_handle_at(AT_FDCWD, \"f\", 0x123, 0x124, 0) = 0\n",
        )).unwrap();
        let (events, diagnostics) = collect(directory.path(), Path::new("/repo")).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].path, "/repo/f");
        assert_eq!(events[0].operation, Operation::Metadata);
        assert_eq!(events[0].result, "ENODATA");
        assert_eq!(events[1].path, "/repo/src");
        assert_eq!(events[1].operation, Operation::Metadata);
        assert_eq!(events[2].operation, Operation::Write);
        assert_eq!(events[3].operation, Operation::Write);
        assert_eq!(events[4].operation, Operation::Metadata);
        assert!(decode(123, "chroot(\"/other\") = 0", Path::new("/repo")).is_none());
    }

    #[test]
    fn a_filter_keeps_only_what_can_matter() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("command-0.10"),
            concat!(
                "1.000 openat(AT_FDCWD</repo>, \"node_modules/x/index.js\", O_RDONLY) = 3</repo/node_modules/x/index.js>\n",
                "1.001 openat(AT_FDCWD</repo>, \"src/a.ts\", O_RDONLY) = 3</repo/src/a.ts>\n",
                "1.002 openat(AT_FDCWD</repo>, \"/usr/lib/libc.so\", O_RDONLY) = 3</usr/lib/libc.so>\n",
                "1.003 chdir(\"lib\") = 0\n",
                // Relative to the working directory, which the filter cannot
                // know: kept, and placed once it is known.
                "1.004 stat(\"../src/b.ts\", {...}) = 0\n",
            ),
        )
        .unwrap();
        let keep =
            |path: &Path| path.starts_with("/repo") && !path.starts_with("/repo/node_modules");
        let limits = Limits {
            keep: Some(&keep),
            ..ANALYSIS
        };
        let (events, diagnostics) =
            collect_within(directory.path(), Path::new("/repo"), &limits).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let paths: Vec<&str> = events.iter().map(|event| event.path.as_str()).collect();
        assert_eq!(paths, ["/repo/src/a.ts", "/repo/src/b.ts"]);
        // Only kept accesses count toward the budget.
        let tight = Limits {
            accesses: 2,
            keep: Some(&keep),
            ..ANALYSIS
        };
        let (_, diagnostics) =
            collect_within(directory.path(), Path::new("/repo"), &tight).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn stats_of_pipes_and_sockets_name_no_path() {
        let cwd = Path::new("/repo");
        for call in [
            "newfstatat(1<pipe:[123]>, \"\", {st_mode=S_IFIFO|0600, ...}, AT_EMPTY_PATH) = 0",
            "statx(17<socket:[456]>, \"\", AT_STATX_SYNC_AS_STAT|AT_EMPTY_PATH, STATX_ALL, {...}) = 0",
        ] {
            let (events, _, _) = decode(1, call, cwd).unwrap();
            assert!(events.is_empty(), "{call}");
        }
        // A file's descriptor still names its file.
        let (events, _, _) = decode(
            1,
            "newfstatat(3</repo/a.txt>, \"\", {st_mode=S_IFREG|0644, ...}, AT_EMPTY_PATH) = 0",
            cwd,
        )
        .unwrap();
        assert_eq!(events[0].path, "/repo/a.txt");
    }
}
