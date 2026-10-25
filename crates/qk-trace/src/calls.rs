//! The system calls the recorder records: their numbers on each architecture,
//! which of their arguments are paths, and how strace prints them, which is
//! how the recorder writes them.

/// How strace prints an argument, as far as qk's decoder reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arg {
    /// A path, quoted, or `NULL`.
    Path,
    /// A descriptor, with its path as `-yy` adds it.
    Fd,
    /// A descriptor a path is relative to: `AT_FDCWD`, or one as `Fd`.
    AtFd,
    /// `open`'s flags, by name.
    OpenFlags,
    /// `AT_*` flags, by name.
    AtFlags,
    /// A number, or a pointer to memory the decoder does not read.
    Value,
}

/// What a call does to a process's descriptors or working directory, beside
/// what the decoder makes of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Returns a descriptor for the path its arguments name.
    Opens,
    /// Closes its first argument.
    Closes,
    /// Closes the descriptors from its first argument to its second, unless
    /// its flags only mark them close-on-exec.
    ClosesRange,
    /// Returns a copy of its first argument.
    Dups,
    /// Copies its first argument to its second.
    DupsTo,
    /// Changes the working directory to its path.
    Chdir,
    /// Changes the working directory to its descriptor.
    Fchdir,
}

#[derive(Clone, Copy, Debug)]
pub struct Call {
    pub name: &'static str,
    /// Its number on x86-64 and on arm64; `None` where it does not exist.
    pub x86: Option<u16>,
    pub arm64: Option<u16>,
    pub args: &'static [Arg],
    pub effect: Effect,
    /// Whether the decoder reads it; a call kept only for its effect on
    /// descriptors is not written.
    pub written: bool,
    /// The argument pointing to `open_how`, whose flags are read in its
    /// place.
    pub flags_at: Option<u8>,
    /// Only `F_DUPFD` and `F_DUPFD_CLOEXEC`, of fcntl's commands.
    pub dup_commands_only: bool,
}

const fn call(
    name: &'static str,
    x86: Option<u16>,
    arm64: Option<u16>,
    args: &'static [Arg],
) -> Call {
    Call {
        name,
        x86,
        arm64,
        args,
        effect: Effect::None,
        written: true,
        flags_at: None,
        dup_commands_only: false,
    }
}

const fn with(mut call: Call, effect: Effect) -> Call {
    call.effect = effect;
    call
}

const fn unwritten(mut call: Call) -> Call {
    call.written = false;
    call
}

use Arg::{AtFd, AtFlags, Fd, OpenFlags, Path, Value};

/// Every call recorded. The filter qk gives strace, `%file`, `%process`,
/// `getdents64`, `fchdir`, `close`, `dup`s and `getcwd`, less what the
/// decoder makes nothing of, and with `close_range` and fcntl's descriptor
/// copies, which strace's descriptor annotations stand in for.
pub const CALLS: &[Call] = &[
    with(
        call("open", Some(2), None, &[Path, OpenFlags, Value]),
        Effect::Opens,
    ),
    with(
        call(
            "openat",
            Some(257),
            Some(56),
            &[AtFd, Path, OpenFlags, Value],
        ),
        Effect::Opens,
    ),
    Call {
        flags_at: Some(2),
        ..with(
            call(
                "openat2",
                Some(437),
                Some(437),
                &[AtFd, Path, OpenFlags, Value],
            ),
            Effect::Opens,
        )
    },
    with(call("creat", Some(85), None, &[Path, Value]), Effect::Opens),
    unwritten(with(
        call("close", Some(3), Some(57), &[Fd]),
        Effect::Closes,
    )),
    unwritten(with(
        call("close_range", Some(436), Some(436), &[Value, Value, Value]),
        Effect::ClosesRange,
    )),
    unwritten(with(call("dup", Some(32), Some(23), &[Fd]), Effect::Dups)),
    unwritten(with(
        call("dup2", Some(33), None, &[Fd, Value]),
        Effect::DupsTo,
    )),
    unwritten(with(
        call("dup3", Some(292), Some(24), &[Fd, Value, Value]),
        Effect::DupsTo,
    )),
    Call {
        dup_commands_only: true,
        ..unwritten(with(
            call("fcntl", Some(72), Some(25), &[Fd, Value, Value]),
            Effect::Dups,
        ))
    },
    with(call("chdir", Some(80), Some(49), &[Path]), Effect::Chdir),
    with(call("fchdir", Some(81), Some(50), &[Fd]), Effect::Fchdir),
    call("getdents64", Some(217), Some(61), &[Fd, Value, Value]),
    call("readlink", Some(89), None, &[Path, Value, Value]),
    call(
        "readlinkat",
        Some(267),
        Some(78),
        &[AtFd, Path, Value, Value],
    ),
    call("stat", Some(4), None, &[Path, Value]),
    call("lstat", Some(6), None, &[Path, Value]),
    call("access", Some(21), None, &[Path, Value]),
    call("statfs", Some(137), Some(43), &[Path, Value]),
    call("getxattr", Some(191), Some(8), &[Path, Value, Value, Value]),
    call(
        "lgetxattr",
        Some(192),
        Some(9),
        &[Path, Value, Value, Value],
    ),
    call("listxattr", Some(194), Some(11), &[Path, Value, Value]),
    call("llistxattr", Some(195), Some(12), &[Path, Value, Value]),
    call(
        "newfstatat",
        Some(262),
        Some(79),
        &[AtFd, Path, Value, AtFlags],
    ),
    call(
        "statx",
        Some(332),
        Some(291),
        &[AtFd, Path, AtFlags, Value, Value],
    ),
    call("faccessat", Some(269), Some(48), &[AtFd, Path, Value]),
    call(
        "faccessat2",
        Some(439),
        Some(439),
        &[AtFd, Path, Value, AtFlags],
    ),
    call(
        "name_to_handle_at",
        Some(303),
        Some(264),
        &[AtFd, Path, Value, Value, AtFlags],
    ),
    call("inotify_add_watch", Some(254), Some(27), &[Fd, Path, Value]),
    call("unlink", Some(87), None, &[Path]),
    call("rmdir", Some(84), None, &[Path]),
    call("unlinkat", Some(263), Some(35), &[AtFd, Path, AtFlags]),
    call("rename", Some(82), None, &[Path, Path]),
    call("renameat", Some(264), Some(38), &[AtFd, Path, AtFd, Path]),
    call(
        "renameat2",
        Some(316),
        Some(276),
        &[AtFd, Path, AtFd, Path, Value],
    ),
    call("mkdir", Some(83), None, &[Path, Value]),
    call("truncate", Some(76), Some(45), &[Path, Value]),
    call("chmod", Some(90), None, &[Path, Value]),
    call("chown", Some(92), None, &[Path, Value, Value]),
    call("lchown", Some(94), None, &[Path, Value, Value]),
    call("utime", Some(132), None, &[Path, Value]),
    call("utimes", Some(235), None, &[Path, Value]),
    call("mknod", Some(133), None, &[Path, Value, Value]),
    call(
        "setxattr",
        Some(188),
        Some(5),
        &[Path, Value, Value, Value, Value],
    ),
    call(
        "lsetxattr",
        Some(189),
        Some(6),
        &[Path, Value, Value, Value, Value],
    ),
    call("removexattr", Some(197), Some(14), &[Path, Value]),
    call("lremovexattr", Some(198), Some(15), &[Path, Value]),
    call("mkdirat", Some(258), Some(34), &[AtFd, Path, Value]),
    call("fchmodat", Some(268), Some(53), &[AtFd, Path, Value]),
    call(
        "fchmodat2",
        Some(452),
        Some(452),
        &[AtFd, Path, Value, AtFlags],
    ),
    call(
        "fchownat",
        Some(260),
        Some(54),
        &[AtFd, Path, Value, Value, AtFlags],
    ),
    call(
        "utimensat",
        Some(280),
        Some(88),
        &[AtFd, Path, Value, AtFlags],
    ),
    call("mknodat", Some(259), Some(33), &[AtFd, Path, Value, Value]),
    call("futimesat", Some(261), None, &[AtFd, Path, Value]),
    call("symlink", Some(88), None, &[Path, Path]),
    call("symlinkat", Some(266), Some(36), &[Path, AtFd, Path]),
    call("link", Some(86), None, &[Path, Path]),
    call(
        "linkat",
        Some(265),
        Some(37),
        &[AtFd, Path, AtFd, Path, AtFlags],
    ),
];

/// The architecture the recorder runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86,
    Arm64,
}

impl Arch {
    pub fn current() -> Option<Self> {
        match std::env::consts::ARCH {
            "x86_64" => Some(Self::X86),
            "aarch64" => Some(Self::Arm64),
            _ => None,
        }
    }

    pub fn number(self, call: &Call) -> Option<u16> {
        match self {
            Self::X86 => call.x86,
            Self::Arm64 => call.arm64,
        }
    }

    /// prctl's number, which a process calls to join a recording.
    pub fn prctl(self) -> u32 {
        match self {
            Self::X86 => 157,
            Self::Arm64 => 167,
        }
    }

    /// `O_DIRECTORY`, the one open flag the recorder names that differs.
    pub fn o_directory(self) -> u64 {
        match self {
            Self::X86 => 0o200000,
            Self::Arm64 => 0o40000,
        }
    }

    /// The call with `number`, if it is one recorded.
    pub fn call(self, number: u16) -> Option<&'static Call> {
        CALLS.iter().find(|call| self.number(call) == Some(number))
    }
}

/// The highest call number the kernel program looks up.
pub const MAX_CALL: u16 = 511;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_call_has_a_number_the_program_can_look_up_and_no_two_share_one() {
        for arch in [Arch::X86, Arch::Arm64] {
            let mut seen = std::collections::BTreeMap::new();
            for call in CALLS {
                if let Some(number) = arch.number(call) {
                    assert!(number <= MAX_CALL, "{}", call.name);
                    assert!(
                        seen.insert(number, call.name).is_none(),
                        "{arch:?} {number} is {} and {}",
                        seen[&number],
                        call.name
                    );
                }
            }
            assert!(!seen.contains_key(&(arch.prctl() as u16)));
        }
    }

    #[test]
    fn the_paths_a_call_reads_are_within_its_arguments() {
        for call in CALLS {
            assert!(call.args.len() <= 6, "{}", call.name);
            assert!(
                call.args.iter().filter(|arg| **arg == Path).count() <= 2,
                "{}",
                call.name
            );
        }
    }
}
