use std::ffi::OsStr;
use std::process::Command;

/// A command that cannot reach the repository running the tests. A Git hook,
/// or `git rebase --exec` in a linked worktree, sets `GIT_DIR`, and a
/// fixture's `git init` or commit would otherwise act on that repository.
/// These are Git's `rev-parse --local-env-vars`.
pub fn fixture_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    for name in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
    ] {
        command.env_remove(name);
    }
    command
}
