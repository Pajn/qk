//! Decode strace's path-oriented output. Unknown/truncated records are surfaced.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use qk_input_analysis::{Access, Category, Operation};

pub(super) fn normalize(path: &Path) -> PathBuf {
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

pub(super) fn collect(directory: &Path, cwd: &Path) -> Result<(Vec<Access>, Vec<String>)> {
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
            bytes_read = bytes_read.saturating_add(line.len());
            if bytes_read > 32 * 1024 * 1024 || records.len() >= 200_000 {
                diagnostics.push(
                    "trace collection exceeded its 32 MiB/200000-record budget; report truncated"
                        .into(),
                );
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
                accesses.extend(events);
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
        if accesses.len() > 100_000 {
            accesses.truncate(100_000);
            diagnostics.push("recording exceeded 100000 accesses; report truncated".into());
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
        "stat" | "lstat" | "stat64" | "lstat64" | "access" | "statfs" | "statfs64" => {
            emit(path(0, None)?, Operation::Metadata)
        }
        "newfstatat" | "fstatat64" | "statx" | "faccessat" | "faccessat2" => {
            emit(path(1, Some(0))?, Operation::Metadata)
        }
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
        "mkdir" | "truncate" | "chmod" | "chown" | "lchown" | "utime" | "utimes" | "mknod" => {
            emit(path(0, None)?, Operation::Write)
        }
        "mkdirat" | "fchmodat" | "fchmodat2" | "fchownat" | "utimensat" | "mknodat" => {
            emit(path(1, Some(0))?, Operation::Write)
        }
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
        | "dup2" | "dup3" => {}
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

#[cfg(test)]
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
}
