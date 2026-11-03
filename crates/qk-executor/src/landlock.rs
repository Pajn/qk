//! Landlock, through its system calls: what the kernel's `linux/landlock.h`
//! declares, and no more.
//!
//! A rule set allows rights beneath paths, and refuses every right it handles
//! elsewhere. The calls here make system calls only and allocate nothing, so
//! a command may make them between fork and exec. [`Rights`] is every
//! platform's, so what a task is granted can be worked out anywhere.

/// Rights over files beneath a path, as Landlock's access bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rights(u64);

impl Rights {
    pub const NONE: Self = Self(0);
    pub const EXECUTE: Self = Self(1 << 0);
    pub const WRITE_FILE: Self = Self(1 << 1);
    pub const READ_FILE: Self = Self(1 << 2);
    pub const READ_DIR: Self = Self(1 << 3);
    /// Every right of ABI 1, from executing to making symlinks.
    pub const ABI1: Self = Self((1 << 13) - 1);
    pub const REFER: Self = Self(1 << 13);
    pub const TRUNCATE: Self = Self(1 << 14);
    pub const IOCTL_DEV: Self = Self(1 << 15);
    /// Every right, those of later ABIs too.
    pub const ALL: Self = Self(u64::MAX);
    /// Reading files and running them.
    pub const READ: Self = Self(Self::READ_FILE.0 | Self::EXECUTE.0);
    /// Reading and running what lies beneath a directory, and listing it.
    pub const READ_TREE: Self = Self(Self::READ.0 | Self::READ_DIR.0);
    /// The rights that apply to a file rather than a directory.
    pub const FILE: Self = Self(
        Self::EXECUTE.0
            | Self::WRITE_FILE.0
            | Self::READ_FILE.0
            | Self::TRUNCATE.0
            | Self::IOCTL_DEV.0,
    );

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Every right the kernel's ABI `abi` handles.
    pub fn handled(abi: u32) -> Self {
        let mut rights = Self::ABI1;
        if abi >= 2 {
            rights |= Self::REFER;
        }
        if abi >= 3 {
            rights |= Self::TRUNCATE;
        }
        if abi >= 5 {
            rights |= Self::IOCTL_DEV;
        }
        rights
    }
}

impl std::ops::BitOr for Rights {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOrAssign for Rights {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

impl std::ops::BitAnd for Rights {
    type Output = Self;

    fn bitand(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

impl std::ops::BitAndAssign for Rights {
    fn bitand_assign(&mut self, other: Self) {
        self.0 &= other.0;
    }
}

impl std::ops::Not for Rights {
    type Output = Self;

    fn not(self) -> Self {
        Self(!self.0)
    }
}

#[cfg(target_os = "linux")]
pub use system::*;

#[cfg(target_os = "linux")]
mod system {
    use std::ffi::CStr;
    use std::io;
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};

    use super::Rights;

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
    pub fn abi() -> io::Result<u32> {
        let version = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        if version < 1 {
            return Err(io::Error::last_os_error());
        }
        Ok(version as u32)
    }

    /// A new rule set handling `handled`.
    pub fn create_ruleset(handled: Rights) -> io::Result<OwnedFd> {
        let attr = RulesetAttr {
            handled_access_fs: handled.bits(),
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
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call returned a new descriptor, owned here.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }

    /// Allows `rights` beneath the path opened as `fd`, or on it for a file,
    /// in `ruleset`, which handles `handled`. What the rule set does not
    /// handle, or a file cannot have, is left out; a rule left with no right
    /// is not added.
    pub fn add_rule(ruleset: RawFd, fd: RawFd, rights: Rights, handled: Rights) -> io::Result<()> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let directory = unsafe { libc::fstat(fd, &mut stat) } == 0
            && stat.st_mode & libc::S_IFMT == libc::S_IFDIR;
        let mut rights = rights & handled;
        if !directory {
            rights &= Rights::FILE;
        }
        if rights.is_empty() {
            return Ok(());
        }
        let attr = PathBeneathAttr {
            allowed_access: rights.bits(),
            parent_fd: fd,
        };
        let added = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset,
                RULE_PATH_BENEATH,
                &attr as *const PathBeneathAttr,
                0u32,
            )
        };
        if added != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Opens `name`, relative to the directory `directory`, as a path only,
    /// through no link and without leaving it. Where a link stands in the
    /// way, the error is `ELOOP` or `EXDEV`.
    pub fn open_beneath(directory: RawFd, name: &CStr) -> io::Result<OwnedFd> {
        // SAFETY: zeroed is a valid open_how, whose fields are integers.
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (libc::O_PATH | libc::O_CLOEXEC) as u64;
        how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                directory,
                name.as_ptr(),
                &how as *const libc::open_how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call returned a new descriptor, owned here.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }

    /// Restricts the calling thread, and what it runs, to `ruleset`, once it
    /// can gain no privileges.
    pub fn restrict_self(ruleset: RawFd) -> io::Result<()> {
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}
