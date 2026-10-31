//! Running a command in its own user and mount namespace over an overlay of
//! the workspace: what it writes there lands in an upper layer of its own,
//! which the sandbox reads when the task ends.

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// The layers of one slot of commands: `upper` and `work`, the overlay's, and
/// `real`, where the workspace is bound aside so that it can be the lower
/// layer.
pub fn layers(overlay: &crate::Overlay, slot: usize) -> [std::path::PathBuf; 3] {
    let command = overlay.directory.join(format!("command-{slot}"));
    ["upper", "work", "real"].map(|name| command.join(name))
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("a path holds a NUL byte")
}

/// What the child does between fork and exec, prepared beforehand: it may
/// make system calls only.
pub struct Setup {
    uid_map: CString,
    gid_map: CString,
    workspace: CString,
    real: CString,
    options: CString,
    /// The descriptor the workspace bound aside is held at while the
    /// overlay is mounted, as `real` may lie in the workspace, in qk's
    /// state, where the overlay hides it: one reserved here, so that no
    /// descriptor the child inherits has its number.
    aside: std::os::fd::OwnedFd,
    /// Each path bound through, as it lies in the workspace bound aside,
    /// reached through `aside`, and in the workspace.
    passthrough: Vec<(CString, CString)>,
    cwd: CString,
    /// The Landlock rule set's handled and file rights, the descriptors
    /// opened outside with their rights, the workspace's rights, and each
    /// path in the workspace, relative to it, with its rights and whether to
    /// reach it through no link.
    handled: u64,
    file_rights: u64,
    outside: Vec<(i32, u64)>,
    workspace_rights: u64,
    inside: Vec<(CString, u64, bool)>,
}

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

const RULE_PATH_BENEATH: i32 = 1;

impl Setup {
    pub fn new(overlay: &crate::Overlay, slot: usize, cwd: &Path) -> Result<Self> {
        let [upper, work, real] = layers(overlay, slot);
        for directory in [&upper, &work, &real] {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("cannot create {}", directory.display()))?;
        }
        let text = |path: &Path| -> Result<String> {
            let text = path.to_str().context("an overlay's paths must be UTF-8")?;
            // The mount options separate with `,` and lower layers with `:`.
            if text.contains([',', ':', '\\']) {
                bail!("an overlay cannot be mounted at {text}");
            }
            Ok(text.to_owned())
        };
        let options = format!(
            "lowerdir={},upperdir={},workdir={},userxattr",
            text(&real)?,
            text(&upper)?,
            text(&work)?
        );
        // The same user and group inside as outside, so files keep their
        // owner.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let aside: std::os::fd::OwnedFd = std::fs::File::open("/dev/null")
            .context("cannot reserve a descriptor")?
            .into();
        let mut passthrough = Vec::new();
        for path in &overlay.passthrough {
            let relative = path
                .strip_prefix(&overlay.workspace)
                .context("a path bound through lies outside the workspace")?;
            let source = Path::new("/proc/self/fd")
                .join(aside.as_raw_fd().to_string())
                .join(relative);
            passthrough.push((c_path(&source)?, c_path(path)?));
        }
        let rules = &overlay.rules;
        let mut inside = Vec::new();
        for (path, rights, unlinked) in &rules.inside {
            let Ok(relative) = path.strip_prefix(&overlay.workspace) else {
                continue;
            };
            let relative = if relative.as_os_str().is_empty() {
                Path::new(".")
            } else {
                relative
            };
            inside.push((c_path(relative)?, *rights, *unlinked));
        }
        Ok(Self {
            aside,
            handled: rules.handled,
            file_rights: rules.file_rights,
            outside: rules
                .outside
                .iter()
                .map(|(fd, rights)| (fd.as_raw_fd(), *rights))
                .collect(),
            workspace_rights: rules.workspace,
            inside,
            uid_map: CString::new(format!("{uid} {uid} 1"))?,
            gid_map: CString::new(format!("{gid} {gid} 1"))?,
            workspace: c_path(&overlay.workspace)?,
            real: c_path(&real)?,
            options: CString::new(options)?,
            passthrough,
            cwd: c_path(cwd)?,
        })
    }

    /// Enters the namespaces and mounts the overlay. Between fork and exec:
    /// system calls only, no allocation.
    pub fn enter(&self) -> std::io::Result<()> {
        let fail = || Err(std::io::Error::last_os_error());
        unsafe {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                return fail();
            }
            for (file, text) in [
                (c"/proc/self/setgroups", c"deny"),
                (c"/proc/self/uid_map", self.uid_map.as_c_str()),
                (c"/proc/self/gid_map", self.gid_map.as_c_str()),
            ] {
                let fd = libc::open(file.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if fd < 0 {
                    return fail();
                }
                let bytes = text.to_bytes();
                let written = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
                libc::close(fd);
                if written < 0 {
                    return fail();
                }
            }
            let none = std::ptr::null();
            if libc::mount(
                none,
                c"/".as_ptr(),
                none,
                libc::MS_REC | libc::MS_PRIVATE,
                none.cast(),
            ) != 0
                || libc::mount(
                    self.workspace.as_ptr(),
                    self.real.as_ptr(),
                    none,
                    libc::MS_BIND | libc::MS_REC,
                    none.cast(),
                ) != 0
            {
                return fail();
            }
            let aside = libc::open(
                self.real.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            );
            // In place of the reservation, which `aside` cannot be.
            let held = self.aside.as_raw_fd();
            if aside < 0 || libc::dup3(aside, held, libc::O_CLOEXEC) < 0 {
                return fail();
            }
            libc::close(aside);
            if libc::mount(
                c"overlay".as_ptr(),
                self.workspace.as_ptr(),
                c"overlay".as_ptr(),
                0,
                self.options.as_ptr().cast(),
            ) != 0
            {
                return fail();
            }
            for (source, target) in &self.passthrough {
                if libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    none,
                    libc::MS_BIND | libc::MS_REC,
                    none.cast(),
                ) != 0
                {
                    return fail();
                }
            }
            libc::close(held);
            // The directory it was in is now beneath the overlay.
            if libc::chdir(self.cwd.as_ptr()) != 0 {
                return fail();
            }
            self.restrict()?;
        }
        Ok(())
    }

    /// Builds the Landlock rule set over the mounted overlay and restricts
    /// the command to it. Between fork and exec: system calls only.
    unsafe fn restrict(&self) -> std::io::Result<()> {
        let fail = || Err(std::io::Error::last_os_error());
        unsafe {
            let attr = RulesetAttr {
                handled_access_fs: self.handled,
            };
            let ruleset = libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            ) as i32;
            if ruleset < 0 {
                return fail();
            }
            let add = |fd: i32, rights: u64| -> std::io::Result<()> {
                let mut stat: libc::stat = std::mem::zeroed();
                let directory =
                    libc::fstat(fd, &mut stat) == 0 && stat.st_mode & libc::S_IFMT == libc::S_IFDIR;
                let mut rights = rights & self.handled;
                if !directory {
                    rights &= self.file_rights;
                }
                if rights == 0 {
                    return Ok(());
                }
                let attr = PathBeneathAttr {
                    allowed_access: rights,
                    parent_fd: fd,
                };
                if libc::syscall(
                    libc::SYS_landlock_add_rule,
                    ruleset,
                    RULE_PATH_BENEATH,
                    &attr as *const PathBeneathAttr,
                    0u32,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            };
            for (fd, rights) in &self.outside {
                add(*fd, *rights)?;
            }
            let root = libc::open(
                self.workspace.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            );
            if root < 0 {
                return fail();
            }
            add(root, self.workspace_rights)?;
            for (relative, rights, unlinked) in &self.inside {
                let fd = if *unlinked {
                    let mut how: libc::open_how = std::mem::zeroed();
                    how.flags = (libc::O_PATH | libc::O_CLOEXEC) as u64;
                    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
                    libc::syscall(
                        libc::SYS_openat2,
                        root,
                        relative.as_ptr(),
                        &how as *const libc::open_how,
                        std::mem::size_of::<libc::open_how>(),
                    ) as i32
                } else {
                    libc::openat(root, relative.as_ptr(), libc::O_PATH | libc::O_CLOEXEC)
                };
                if fd < 0 {
                    // One that does not exist, or that a link now stands in,
                    // is not allowed, as outside the overlay.
                    match *libc::__errno_location() {
                        libc::ENOENT | libc::ELOOP | libc::EXDEV | libc::ENOTDIR => continue,
                        _ => return fail(),
                    }
                }
                let added = add(fd, *rights);
                libc::close(fd);
                added?;
            }
            libc::close(root);
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) != 0
            {
                return fail();
            }
            libc::close(ruleset);
        }
        Ok(())
    }
}

/// Whether a command can run over an overlay here: in a user and mount
/// namespace of its own, with an overlay mounted on a directory in
/// `directory`, which lies on the workspace's file system. The reason it
/// cannot, if not.
pub fn probe(directory: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let [lower, upper, work, merged] =
        ["lower", "upper", "work", "merged"].map(|name| directory.join(name));
    for path in [&lower, &upper, &work, &merged] {
        std::fs::create_dir_all(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
    }
    let options = CString::new(format!(
        "lowerdir={},upperdir={},workdir={},userxattr",
        lower.display(),
        upper.display(),
        work.display()
    ))?;
    let merged_path = c_path(&merged)?;
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let uid_map = CString::new(format!("{uid} {uid} 1"))?;
    let gid_map = CString::new(format!("{gid} {gid} 1"))?;
    // Which step failed, as the child can only return an error code: written
    // to a pipe the parent reads.
    let mut pipe = [0i32; 2];
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot make a pipe");
    }
    let (reader, writer) = (pipe[0], pipe[1]);
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args(["-c", "exit 0"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        command.pre_exec(move || {
            let fail = |step: u8| {
                let error = std::io::Error::last_os_error();
                libc::write(writer, (&step as *const u8).cast(), 1);
                Err(error)
            };
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                return fail(0);
            }
            for (step, (file, text)) in [
                (c"/proc/self/setgroups", c"deny"),
                (c"/proc/self/uid_map", uid_map.as_c_str()),
                (c"/proc/self/gid_map", gid_map.as_c_str()),
            ]
            .into_iter()
            .enumerate()
            {
                let fd = libc::open(file.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if fd < 0 {
                    return fail(1 + step as u8);
                }
                let bytes = text.to_bytes();
                let written = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
                libc::close(fd);
                if written < 0 {
                    return fail(1 + step as u8);
                }
            }
            if libc::mount(
                c"overlay".as_ptr(),
                merged_path.as_ptr(),
                c"overlay".as_ptr(),
                0,
                options.as_ptr().cast(),
            ) != 0
            {
                return fail(4);
            }
            Ok(())
        });
    }
    let output = command.output();
    unsafe { libc::close(writer) };
    let mut step = [u8::MAX];
    let read = unsafe { libc::read(reader, step.as_mut_ptr().cast(), 1) };
    unsafe { libc::close(reader) };
    clear(directory);
    let what = match (read, step[0]) {
        (1, 0) => "making a user and mount namespace",
        (1, 1) => "denying setgroups",
        (1, 2) => "mapping the user",
        (1, 3) => "mapping the group",
        (1, 4) => "mounting an overlay",
        _ => "running a command over an overlay",
    };
    let where_ = directory.display();
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => bail!("{what} in {where_} failed: {}", output.status),
        Err(error) => Err(error).with_context(|| format!("{what} in {where_} failed")),
    }
}

/// Removes an overlay's directory, whose work directory overlayfs leaves
/// unreadable.
pub fn clear(directory: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut stack = vec![directory.to_owned()];
    while let Some(path) = stack.pop() {
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
            if let Ok(entries) = std::fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|entry| entry.path()));
            }
        }
    }
    let _ = std::fs::remove_dir_all(directory);
}

/// Applies to the workspace what each command's upper layer holds within
/// the overlay's `keep`, entry by entry. Entry by entry, since an overlay
/// another task has mounted would keep seeing a directory replaced whole as
/// it was.
pub fn settle(overlay: &crate::Overlay) -> crate::Settled {
    let mut settled = crate::Settled::default();
    for slot in 0.. {
        let [upper, ..] = layers(overlay, slot);
        if !upper.parent().is_some_and(Path::exists) {
            break;
        }
        if upper.is_dir() {
            let layer = Layer {
                upper: &upper,
                root: &overlay.workspace,
                keep: &overlay.keep,
                passthrough: &overlay.passthrough,
            };
            layer.apply(Path::new(""), false, &mut settled);
        }
    }
    settled
}

/// One command's upper layer, applied to the workspace at `root`.
struct Layer<'a> {
    upper: &'a Path,
    root: &'a Path,
    /// Where the task may write.
    keep: &'a [std::path::PathBuf],
    /// Bound through the overlay: what lies there the layer never holds.
    passthrough: &'a [std::path::PathBuf],
}

impl Layer<'_> {
    fn allowed(&self, target: &Path) -> bool {
        self.keep.iter().any(|keep| target.starts_with(keep))
    }

    /// Whether something bound through the overlay lies beneath `target`.
    fn holds_bound(&self, target: &Path) -> bool {
        self.passthrough
            .iter()
            .any(|through| through.starts_with(target))
    }

    /// Whether something the task may write lies beneath `target`.
    fn holds_kept(&self, target: &Path) -> bool {
        self.keep.iter().any(|keep| keep.starts_with(target))
    }

    /// Applies what the layer holds under `relative`, adding what it may
    /// not write to `settled.discarded` and what could not be applied to
    /// `settled.failed`. A whiteout removes; an opaque directory, or one
    /// beneath it, which hides what the workspace holds there as `hidden`
    /// says, replaces what was there; anything else is moved into place.
    /// Nothing is reached through a symbolic link in the workspace: a command
    /// could leave one in an output pointing out of it, which a later
    /// command's directory would be applied through. A link where the layer
    /// holds a directory is replaced by it, as the command did, or, outside
    /// what it may write, keeps the directory's entries out. `relative`
    /// itself holds no link: each directory is checked before what it holds
    /// is applied. Returns whether it applied anything.
    fn apply(&self, relative: &Path, hidden: bool, settled: &mut crate::Settled) -> bool {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let mut applied = hidden && self.prune(relative, settled);
        let entries = match std::fs::read_dir(self.upper.join(relative)) {
            Ok(entries) => entries,
            Err(error) => {
                settled.fail(&self.upper.join(relative), &error);
                return applied;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    settled.fail(&self.upper.join(relative), &error);
                    continue;
                }
            };
            let relative = relative.join(entry.file_name());
            let source = self.upper.join(&relative);
            let target = self.root.join(&relative);
            let metadata = match std::fs::symlink_metadata(&source) {
                Ok(metadata) => metadata,
                Err(error) => {
                    settled.fail(&source, &error);
                    continue;
                }
            };
            let name = relative.to_string_lossy().into_owned();
            let kind = metadata.file_type();
            let existing = std::fs::symlink_metadata(&target).ok();
            let is_dir = existing.as_ref().is_some_and(|existing| existing.is_dir());
            let is_link = existing
                .as_ref()
                .is_some_and(|existing| existing.is_symlink());
            if kind.is_char_device() && metadata.rdev() == 0 {
                // A whiteout.
                if self.allowed(&target) {
                    settled.removed(&target);
                    applied = true;
                } else if is_dir && self.holds_kept(&target) {
                    // A directory above outputs is gone, and so is what it
                    // held of them.
                    applied |= self.prune(&relative, settled);
                } else {
                    settled.discarded.insert(name);
                }
            } else if kind.is_dir() {
                let replaces = hidden || opaque(&source);
                if self.allowed(&target) {
                    // A file or link in its place goes, as the command
                    // removed it; a directory only when the layer's replaces
                    // it whole. What is bound through beneath it stays.
                    let prunes = is_dir && replaces && self.holds_bound(&target);
                    if existing.is_some() && (!is_dir || replaces) && !prunes {
                        settled.removed(&target);
                    }
                    // What lies above holds no link, so neither does what
                    // this makes.
                    let made = std::fs::create_dir_all(&target).and_then(|()| {
                        match std::fs::symlink_metadata(&target)?.is_dir() {
                            true => Ok(()),
                            false => Err(std::io::Error::other("not a directory")),
                        }
                    });
                    match made {
                        Ok(()) => {
                            self.apply(&relative, prunes, settled);
                            // After its entries, which a read-only mode
                            // would keep out.
                            let _ = std::fs::set_permissions(&target, metadata.permissions());
                            applied = true;
                        }
                        Err(error) => settled.fail(&target, &error),
                    }
                } else if is_link {
                    settled.discarded.insert(name);
                } else if self.apply(&relative, replaces && is_dir, settled) {
                    applied = true;
                } else if !is_dir {
                    // A directory it made outside its outputs, holding
                    // nothing of them.
                    settled.discarded.insert(name);
                }
            } else if self.allowed(&target) {
                if let Some(parent) = target.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if is_dir {
                    settled.removed(&target);
                }
                // A rename replaces a link at the target rather than follow
                // it.
                if std::fs::rename(&source, &target).is_err() {
                    let _ = remove(&target);
                    // Copied as it is: a link the command made is never
                    // followed by qk, which Landlock does not hold, to a
                    // file the command could not read.
                    let copied = if kind.is_symlink() {
                        std::fs::read_link(&source)
                            .and_then(|link| std::os::unix::fs::symlink(link, &target))
                    } else if kind.is_file() {
                        std::fs::copy(&source, &target).map(|_| ())
                    } else {
                        settled.discarded.insert(name);
                        continue;
                    };
                    if let Err(error) = copied {
                        settled.fail(&target, &error);
                        continue;
                    }
                }
                applied = true;
            } else {
                settled.discarded.insert(name);
            }
        }
        applied
    }

    /// Beneath a directory the layer replaces whole or removes: removes what
    /// the workspace holds under `relative` and the layer does not, where the
    /// task may write, and adds the rest to `settled.discarded`. What is
    /// bound through the overlay stays: the layer never holds it.
    fn prune(&self, relative: &Path, settled: &mut crate::Settled) -> bool {
        let entries = match std::fs::read_dir(self.root.join(relative)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(error) => {
                settled.fail(&self.root.join(relative), &error);
                return false;
            }
        };
        let mut removed = false;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    settled.fail(&self.root.join(relative), &error);
                    continue;
                }
            };
            let relative = relative.join(entry.file_name());
            if std::fs::symlink_metadata(self.upper.join(&relative)).is_ok() {
                continue;
            }
            let target = self.root.join(&relative);
            if self
                .passthrough
                .iter()
                .any(|through| target.starts_with(through))
            {
                continue;
            }
            let real_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if real_dir
                && (self.holds_bound(&target)
                    || (!self.allowed(&target) && self.holds_kept(&target)))
            {
                removed |= self.prune(&relative, settled);
            } else if self.allowed(&target) {
                settled.removed(&target);
                removed = true;
            } else {
                settled
                    .discarded
                    .insert(relative.to_string_lossy().into_owned());
            }
        }
        removed
    }
}

impl crate::Settled {
    fn fail(&mut self, path: &Path, error: &std::io::Error) {
        self.failed.push(format!("{}: {error}", path.display()));
    }

    /// Removes `target`, noting a failure unless it is already gone.
    fn removed(&mut self, target: &Path) {
        if let Err(error) = remove(target)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            self.fail(target, &error);
        }
    }
}

/// Removes a file, a link itself, or a directory with what it holds,
/// following no link.
fn remove(target: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(target)? {
        metadata if metadata.is_dir() => std::fs::remove_dir_all(target),
        _ => std::fs::remove_file(target),
    }
}

/// Whether overlayfs marked a directory of its upper layer opaque: it hides
/// what the lower layer holds there.
fn opaque(directory: &Path) -> bool {
    let Ok(path) = c_path(directory) else {
        return false;
    };
    let mut value = [0u8; 2];
    let length = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            c"user.overlay.opaque".as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    length == 1 && value[0] == b'y'
}
