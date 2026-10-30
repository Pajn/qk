---
status: proposed
---

# Write isolation for the Linux sandbox

On Linux, `--sandbox=enforce` holds a task to its plan with Landlock. Landlock
is available without privileges, but it restricts the real workspace rather
than giving the task a view of its own, and that shapes what enforce can
promise:

- **Rules attach to paths that exist.** qk creates an output before the task
  runs so that Landlock can grant it, and has to guess what kind of path it
  is. A missing output without an extension is taken for a directory, so a
  task declaring a file such as `target/release/app` cannot write it, and a
  missing file output with one, such as `dist/sprite.svg`, cannot be granted
  at all: the task is refused when it creates it.
- **A grant covers everything below it.** Allowing a file to be created in a
  directory means allowing every file there; nothing narrower can be said.
- **Writes are judged as they happen.** A tool that builds a directory beside
  its destination and renames it into place, as cargo makes `target`, is
  refused for the temporary directory, though nothing of it survives.
- **A refusal fails the task.** Enforce cannot let a task finish and then judge
  what it left behind; an undeclared write is an error at the system call.
- **Tasks share the workspace while they run.** A task can see, and change,
  what a task running beside it writes before that task finishes.

Audit on Linux records the task with eBPF or strace instead, so it reports what
enforce would refuse, but with the same event-by-event view of writes.

## How Nix and Bazel do it

Both build the action's view of the filesystem from its declarations rather
than restricting the real one.

- **Nix** runs a build in new user, mount, PID and network namespaces, in a
  chroot holding only its declared inputs, read-only, and a fresh output path.
  Nothing undeclared exists to be read, and only `$out` survives the build.
- **Bazel** gives each action an execution root of symlinks or hardlinks to its
  declared inputs, in a mount namespace where the rest of the system is
  read-only or hidden. Outputs are declared as files or as directories, so
  their parents can be made exactly; after the action, only declared outputs
  are collected.

In both, writes outside the declared outputs do not need to be refused: they
are never kept.

## Decision

We will try an overlay backend for Linux enforce that isolates writes, and
adopt it if the prototype below meets its criteria:

1. **Each task runs in its own mount namespace**, inside an unprivileged user
   namespace, with the workspace mounted as overlayfs: the workspace as the
   lower layer, and an upper layer in qk's state for the worktree.
2. **Some paths pass through**, bind-mounted from the workspace so that writes
   reach it directly, as enforce allows them today: `.git`, qk's state, the
   task's scratch directories and warm paths, and `node_modules`.
3. **When the task ends, qk reads the upper layer.** What lies within the
   task's declared outputs, written, created or removed, is applied to the
   workspace, by renaming on the same filesystem where it can. Everything else
   is a finding and is discarded.
4. **Reads stay as they are.** Landlock still restricts reads inside the
   namespace, and audit still records the task. Building a view of declared
   inputs alone, as Bazel does, costs too much per task for inputs that are
   globs over a whole project, and is left for later.

Writes are then judged by what the task leaves behind, not by each system
call:

- An output need not exist, and need not be guessed at, before the task runs.
- A directory renamed into place is judged where it ends up. A temporary
  directory that is gone by the end is not a finding.
- What a task leaves outside its outputs is listed with the rest of its
  findings, instead of failing the task at the first write.
- A task does not see what another running task writes until that task ends.

Audit can use the same backend: it applies what lies outside the outputs too,
after listing it, so the workspace ends as it would without the sandbox.

## What the prototype must answer

- **Changes to the lower layer.** overlayfs does not support changes to its
  lower layer while it is mounted, and qk applies a finished task's outputs to
  the workspace while other tasks run. Concurrent tasks never share outputs,
  and a task's dependencies finish before it starts, so a running task should
  not look up a path another task is changing. The prototype must show that
  this holds in practice, with concurrent tasks, before anything relies on
  it. If it does not, options include giving a task a read-only bind of the
  workspace with writable mounts only at its outputs' nearest existing
  directories, or applying outputs only between tasks.
- **When outputs are applied.** Today a failed task leaves what it wrote. The
  backend can apply a failed task's outputs too, which keeps that behaviour,
  or none of them, which keeps the workspace as it was. The prototype applies
  them.
- **What it costs.** Mounting is cheap, but applying a large output tree is a
  walk of the upper layer and a rename per entry. The prototype is measured on
  a workspace's checks, against Landlock enforce.
- **What tools notice.** Files in an overlay have different device numbers, a
  hard link across layers copies the file up, and file watchers see the
  overlay rather than the workspace. Tools that build in place, such as
  incremental compilers keeping state in their outputs, should behave as on
  the workspace.

## What the prototype found

A prototype behind `QK_SANDBOX_OVERLAY=1` runs each command in its own user
and mount namespace over an overlay of the workspace, with `.git`, qk's
state, every `node_modules` and the task's scratch directories bound through,
and applies its upper layer when the task ends. Reads are not restricted in
it. Measured on Linux 7.2, unprivileged, in a container:

- **The mechanism works without privileges.** overlayfs mounts in a user
  namespace with `userxattr`. The upper layer records writes, removals as
  whiteouts, and only where a renamed directory ends up. The layers can live
  in qk's state inside the workspace: the workspace bound aside is reached
  through a descriptor held while the overlay is mounted over it.
- **Changes to the lower layer.** New files and directories, and files
  changed in place, are seen by a task already running. A directory the task
  has looked up and that is then replaced whole, by renaming another into its
  place, is not: the task keeps seeing the old one. Outputs are therefore
  applied entry by entry, never by swapping a directory in. qk's cache
  restores swap directories, so a running task that read a restored
  directory before the restore keeps the old one; the task graph keeps
  declared inputs out of that, as it already must.
- **Applying outputs.** A failed task's outputs are applied too, as today.
- **Cost.** Entering the namespaces and mounting the overlay costs about 4ms
  per command. Applying 10,000 output files takes 60ms. On a workspace's
  checks, 69 tasks of tests, type checks and linters across ten projects ran
  in 13.4–13.6s unsandboxed and 13.9–14.7s over the overlay, all passing,
  with nothing discarded. Landlock enforce failed one of them, a build
  creating a declared file output that did not exist yet, and skipped the 19
  tasks depending on it.
- **What tools notice.** cargo, fresh and incremental, git through the bound
  `.git`, Node tools, `mv` and hard links work. `rename(2)` of a directory the
  lower layer holds fails with `EXDEV`, as unprivileged overlays cannot
  redirect directories; `mv` falls back to copying, but a tool calling
  `rename` on an existing directory must too.

What remains for adoption: restricting reads inside the namespace, which
needs Landlock's rules built there, after the overlay is mounted, as rules
built outside name the lower layer's files; and a decision on what
continuous tasks, which may never end, apply and when.

## Requirements

- Linux 5.11 or later, for overlayfs inside a user namespace.
- Unprivileged user namespaces. Some distributions restrict them, such as
  Ubuntu from 24.04 through AppArmor. Where they are restricted, qk falls back
  to Landlock enforce as today, and says so in the run's sandbox summary.
- No change on macOS, where Seatbelt stays as it is. The guarantee this backend
  gives is a Linux one, as CI's usually is.

## Considered options

- **Let Landlock grant an output's parent instead of creating the output.**
  Simple, and it fixes outputs without extensions, but it allows writing any
  file beside the output, which for an output in a project's root is the whole
  project.
- **Declare whether an output is a file or a directory.** It would fix the
  guessing, as Bazel's `declare_file` and `declare_directory` do, but Nx's
  `outputs` cannot say it, and writes would still be judged as they happen.
- **A Nix-style view of declared inputs alone.** It would also enforce reads,
  but building it for inputs that select whole projects costs too much per
  task. It stays possible on top of this backend.
- **Intercepting system calls**, with ptrace or seccomp notification. It sees
  every write as it happens, at a large cost per call, and still judges writes
  one by one.
- **fanotify permission events.** They need `CAP_SYS_ADMIN`, and judge writes
  one by one too.

## Consequences

- Under enforce, an undeclared write no longer fails the task, and no longer
  persists. Caches a tool keeps in the workspace are lost from run to run
  unless they are declared as warm paths or scratch directories, as they
  should be.
- Enforce's guarantee becomes stronger on Linux than on macOS.
- qk keeps two Linux enforce backends while the overlay one cannot be used
  everywhere, and has to say which one held each run.
- The upper layer needs room in qk's state for what a task writes before it is
  applied.
