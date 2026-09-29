//! Landlock rule sets, through its system calls: what the kernel's
//! `linux/landlock.h` declares, and no more.
//!
//! A rule set allows rights beneath paths, and refuses every right it
//! handles elsewhere. Rights cannot be taken back beneath an allowed path,
//! so everything outside the workspace is allowed by allowing each sibling
//! along the way down to it.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const EXECUTE: u64 = 1 << 0;
const WRITE_FILE: u64 = 1 << 1;
const READ_FILE: u64 = 1 << 2;
const READ_DIR: u64 = 1 << 3;
/// Every right of ABI 1, from executing to making symlinks.
const ABI1: u64 = (1 << 13) - 1;
const REFER: u64 = 1 << 13;
const TRUNCATE: u64 = 1 << 14;
const IOCTL_DEV: u64 = 1 << 15;
/// The rights that apply to a file rather than a directory.
const FILE_RIGHTS: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: i32 = 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The Landlock ABI the kernel offers.
pub fn abi() -> Result<u32> {
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if version < 1 {
        return Err(std::io::Error::last_os_error()).context("Landlock is not available");
    }
    Ok(version as u32)
}

/// Every right the kernel's ABI handles.
fn handled(abi: u32) -> u64 {
    let mut rights = ABI1;
    if abi >= 2 {
        rights |= REFER;
    }
    if abi >= 3 {
        rights |= TRUNCATE;
    }
    if abi >= 5 {
        rights |= IOCTL_DEV;
    }
    rights
}

struct Rules {
    fd: OwnedFd,
    handled: u64,
}

impl Rules {
    fn new() -> Result<Self> {
        let handled = handled(abi()?);
        let attr = RulesetAttr {
            handled_access_fs: handled,
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot create a Landlock rule set");
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd as i32) },
            handled,
        })
    }

    /// Allows `rights` beneath `path`, or on it for a file; a path that does
    /// not exist is skipped.
    fn allow(&self, path: &Path, rights: u64) -> Result<()> {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("cannot open {}", path.display()));
            }
        };
        let directory = file.metadata().is_ok_and(|metadata| metadata.is_dir());
        let mut rights = rights & self.handled;
        if !directory {
            rights &= FILE_RIGHTS;
        }
        if rights == 0 {
            return Ok(());
        }
        let attr = PathBeneathAttr {
            allowed_access: rights,
            parent_fd: file.as_raw_fd(),
        };
        let added = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                self.fd.as_raw_fd(),
                RULE_PATH_BENEATH,
                &attr as *const PathBeneathAttr,
                0u32,
            )
        };
        if added != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot allow {} in the sandbox", path.display()));
        }
        Ok(())
    }
}

/// What a task may do, as paths.
pub struct Paths<'a, F, W> {
    pub root: &'a Path,
    pub read_directories: &'a [PathBuf],
    pub read_files: F,
    pub read_trees: &'a std::collections::BTreeSet<PathBuf>,
    pub write_trees: W,
}

/// A task's rule set: everything outside the workspace, listing any
/// directory, reading its inputs and dependencies' outputs, and everything
/// beneath the trees it writes.
pub fn rules<'a, F, W>(paths: &Paths<'a, F, W>) -> Result<OwnedFd>
where
    F: Iterator<Item = &'a PathBuf> + Clone,
    W: Iterator<Item = &'a PathBuf> + Clone,
{
    let rules = Rules::new()?;
    let all = u64::MAX;
    // Listing, anywhere.
    rules.allow(Path::new("/"), READ_DIR)?;
    // Outside the workspace: each sibling on the way down to it.
    let mut ancestor = PathBuf::from("/");
    for component in paths.root.components().skip(1) {
        let next = ancestor.join(component);
        let entries = std::fs::read_dir(&ancestor)
            .with_context(|| format!("cannot list {}", ancestor.display()))?;
        for entry in entries.flatten() {
            if entry.path() != next {
                rules.allow(&entry.path(), all)?;
            }
        }
        ancestor = next;
    }
    if ancestor != paths.root {
        bail!("cannot walk down to {}", paths.root.display());
    }
    for directory in paths.read_directories {
        rules.allow(directory, READ_FILE | READ_DIR | EXECUTE)?;
    }
    for file in paths.read_files.clone() {
        rules.allow(file, READ_FILE | EXECUTE)?;
    }
    for tree in paths.read_trees {
        rules.allow(tree, READ_FILE | READ_DIR | EXECUTE)?;
    }
    for tree in paths.write_trees.clone() {
        rules.allow(tree, all)?;
    }
    Ok(rules.fd)
}
