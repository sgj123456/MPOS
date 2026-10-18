//! Linux-ABI file handlers: the path-carrying calls in Linux argument shapes.
//!
//! Linux passes paths as NUL-terminated strings - one pointer, no length
//! word - and many calls lead with a dirfd the path is relative to. That is
//! a different register layout from the mpos ABI's `(ptr, len)` pairs, so
//! these handlers decode the Linux shapes and speak to the VFS directly;
//! the mpos handlers in [`crate::syscall::fs`] keep their own callers.

use super::sync_nonblocking;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::syscall::linux::abi;
// The flag words and helpers the handlers below spell by name; the values
// and their tests live in `abi`.
use crate::syscall::linux::abi::{
    AT_EMPTY_PATH, AT_FDCWD, AT_REMOVEDIR, AT_SYMLINK_NOFOLLOW, EFD_NONBLOCK, EFD_SEMAPHORE,
    F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_GETFL, F_GETLK, F_OFD_GETLK, F_OFD_SETLK, F_OFD_SETLKW,
    F_SETFD, F_SETFL, F_SETLK, F_SETLKW, FD_CLOEXEC, FD_SETSIZE, O_ACCMODE, O_CLOEXEC, O_CREAT,
    O_DIRECTORY, O_EXCL, O_NOFOLLOW, O_RDWR, O_TMPFILE, O_TRUNC, O_WRONLY, POLLERR, POLLFD_EVENTS,
    POLLFD_REVENTS, POLLFD_SIZE, POLLHUP, POLLIN, POLLNVAL, POLLOUT, StatTimes, normalize_path,
    write_stat,
};
use crate::syscall::{ARG_COUNT, MAX_IO, buffer_ok, cstr_arg, err, err_of};
use crate::vfs::{FileSystem, Inode, NodeKind, R_OK, VfsResult, W_OK, X_OK};
// The concrete node types these handlers drive: the devices, and the socket
// families whose syscalls live here. Naming one is the *syscall* layer's job -
// it is the layer that knows which request belongs to which device - and it is
// exactly what the VFS stopped doing: `Inode` carries no method per device type
// any more, so a caller asks the node for its concrete type through
// `dyn Inode::as_any` instead (`NOTES.md` §11.2 Step B1, V-1). Gated with the
// modules they come from, which the host build does not compile.
// `drivers` is a machine module, and its node types are named here so the
// syscall layer can hand a device its own side effects (an evdev client's read
// position, a seat grab released on close, a uinput device unmade with its
// description). The hardware those types reach is on both machines now - the
// second one's input registry and GPU came up with its port - so the gate is
// `target_os = "none"` alone: the host build still cannot compile `drivers`.
#[cfg(target_os = "none")]
use crate::drivers::drm::DrmCard;
#[cfg(target_os = "none")]
use crate::drivers::uinput::Uinput;
#[cfg(target_os = "none")]
use crate::drivers::virtio::input::EvdevNode;

/// The owner an inode reports as, translated through the *reading* task's user
/// namespace: an id is stored once, in the parent namespace's numbering, and
/// every observation of it is mapped at the boundary (§6.3, USER - a mapping is
/// a translation, not a rename).
///
/// The translation belongs here rather than in `vfs` because it is the *reader*
/// that is being rendered: `Inode::owner()` is the stored id and stays the
/// stored id, since every permission check compares it against the caller's
/// stored identity (`vfs::may_as`, the sticky-bit rule in `may_remove`), and a
/// `vfs` that translated its own answers would refuse the wrong callers. The
/// boundary the map applies at is the ABI write, and `stat`, `fstat`,
/// `fstatat` and `statx` all render the stored id through here. A task that
/// never unshares reads its own ids back unmoved, because the machine's map is
/// the identity.
fn observed_owner(owner: (u32, u32)) -> (u32, u32) {
    let ns = crate::sched::current_user_ns();
    (ns.uid_to_inner(owner.0), ns.gid_to_inner(owner.1))
}

/// The stored permission word to report for `inode`, or `None` when the
/// filesystem has nothing to say and the kind's default is the honest answer.
///
/// The two cases a bare `u32` collapses are different facts, and only the inode
/// can tell them apart: a filesystem that *keeps* modes reports what it holds,
/// **including `0000`** (a file `chmod`ed to no bits, or one `mknod` was asked
/// for mode 0), where a filesystem that keeps none answers its kind's default
/// so directories stay traversable and devices usable. This asks
/// [`Inode::keeps_modes`](crate::vfs::Inode::keeps_modes) - the same question
/// the permission checks ask, which is why the *enforcement* was right while
/// the *report* was not: measured on a current host, a `mknodat(S_IFREG, 0)`
/// node answers `st_mode` `0o100000` and a `chmod 000` file answers the same,
/// where this kernel answered `0o100644` for both, because the stored word went
/// to `abi::mode_of` as a bare `u32` and `0` read as "nobody said". A nonzero
/// stored word is passed through as before, so a filesystem whose `mode()`
/// answers something without claiming to keep modes is unaffected.
fn reported_mode(inode: &Arc<dyn Inode>) -> Option<u32> {
    let stored = inode.mode();
    (stored != 0 || inode.keeps_modes()).then_some(stored)
}

/// The file a `linkat` source name leads to, with `AT_SYMLINK_FOLLOW`.
///
/// That is now the same walk every other following door uses: the resolver
/// denotes a *descriptor's* second name - `/proc/<pid>/fd/N` and the
/// `/dev/std*` links into it - as the file the descriptor holds, so following
/// the source is resolving it (see `crate::vfs::file::descriptor_inode`). The
/// flag still *means* something here, because the no-follow resolution beside
/// it stops at the link, and the two answers differ exactly as they do for any
/// other link.
fn followed_link(path: &str, base: Arc<dyn Inode>) -> VfsResult<Arc<dyn Inode>> {
    crate::vfs::resolve_at(path, base)
}

/// The inode a dirfd-led name resolves from: the descriptor's directory, or the
/// caller's cwd for `AT_FDCWD` - and the cwd *without looking at the descriptor
/// at all* when the name is absolute.
///
/// Linux asks the name's first character before it touches the dirfd. Measured
/// on a current host (`.workbuddy/probe_at_dirfd.c`, Linux 7.2.9), every member
/// of the family - `openat`, `openat2`, `newfstatat`, `statx`, `faccessat`,
/// `faccessat2`, `readlinkat`, `utimensat`, `futimesat`, `mkdirat`, `unlinkat`,
/// `symlinkat`, `renameat`, `renameat2`, `linkat` (both of its dirfds),
/// `fchmodat`, `fchmodat2`, `fchownat` and `mknodat` - serves an absolute name
/// through a descriptor nothing is open on,
/// and the descriptor's *kind* is not a question either: one open on a regular
/// file serves an absolute name too, where a relative name through that same
/// descriptor is `ENOTDIR` and through a closed one `EBADF`. Resolving the dirfd
/// first, whatever the name looked like, answered `EBADF` to a caller holding a
/// stale descriptor with an absolute path - a fallback Linux never makes such a
/// caller take - and the same difference was reachable through every one of
/// those calls, because they all came through one `base_of`.
///
/// Reading the name first is the rule and not an implementation detail, because
/// it is where the other half of Linux's order comes from: the name is copied in
/// `getname()` before `path_init()` looks at the descriptor, so a name the
/// kernel cannot read at all - a bad pointer, or a null one - is `EFAULT` rather
/// than the descriptor's `EBADF` (measured for every member above). Callers
/// therefore read their name *before* they ask this question.
///
/// The cwd this answers for an absolute name is never dereferenced: an absolute
/// name begins at the caller's root and the resolver ignores the start it is
/// handed, which is what makes passing the cwd here enough.
///
/// **An empty name is refused here too**, and for the same reason: it is a
/// question the *name* answers, so it comes before the descriptor is asked
/// anything - neither whether it is open nor what kind it is. Measured for all
/// nineteen doors (`.workbuddy/probe_at_dirfd.c`), an empty name answers
/// `ENOENT` through a closed dirfd, one open on a regular file *and* one open on
/// a directory, where the relative name beside it answers `EBADF`, `ENOTDIR` and
/// the walk's `ENOENT` respectively. This kernel reached the descriptor first,
/// so an empty name was `EBADF` or `ENOTDIR` - answers about the descriptor
/// where Linux's is about the name. `readlinkat` is the one caller that must not
/// come through here with an empty name and says so in its own comment: Linux
/// consults *its* descriptor first and only then refuses the name.
fn base_for(dirfd: i32, path: &str, cwd: &Arc<dyn Inode>) -> Result<Arc<dyn Inode>, isize> {
    if path.is_empty() {
        return Err(err::ENOENT);
    }
    if path.starts_with('/') || dirfd == abi::AT_FDCWD {
        return Ok(cwd.clone());
    }
    let file = crate::sched::current_fds()
        .clone_at(dirfd)
        .map_err(err_of)?;
    if file.metadata().kind != NodeKind::Dir {
        return Err(err::ENOTDIR);
    }
    Ok(file.inode())
}

/// The mode a newly created node gets: what the caller asked for, less the
/// process's file-creation mask. Every create masks - that is the whole of
/// what `umask` does - and the twelve bits are all a mode has.
fn masked_mode(mode: u32) -> u32 {
    mode & !crate::sched::current_umask() & 0o7777
}

/// `open`/`openat` shared tail: open the path, and on a miss with `O_CREAT`
/// make the file first, then open it.
fn open_linux(path: &str, flags: usize, mode: u32, base: Arc<dyn Inode>) -> isize {
    // O_TMPFILE - the bit over O_DIRECTORY - asks for an inode no name holds:
    // the caller writes it whole and `linkat`s it into the tree when it is
    // done, so a file is never seen half-written. `apk` refreshes a repository
    // index this way against its cache directory, and without it every `apk
    // add` fell back to a named temporary - the `APKINDEX.*.tmp.N` left behind
    // in `/var/cache/apk` was that fallback's fingerprint, and an index that
    // never landed under its real name is one that gets fetched again.
    //
    // The pairing is read the way Linux reads it: the bit without the flag is
    // not a spelling anyone meant. A filesystem that cannot keep an inode out
    // of the tree answers EOPNOTSUPP, which is what a caller with a fallback
    // is listening for.
    if flags & O_TMPFILE != 0 {
        if flags & O_DIRECTORY == 0 {
            return err::EINVAL;
        }
        // The directory the file is born into is what the name resolves to -
        // `openat(dirfd, ".")` names the directory itself.
        let dir = match crate::vfs::resolve_opt(path, Some(base)) {
            Ok(dir) => dir,
            Err(e) => return err_of(e),
        };
        // An unnamed inode is still an entry being made in this directory, so
        // the directory's bits decide - and they are asked before it exists,
        // so a refused create leaves nothing behind.
        if let Err(e) = crate::vfs::may_create_in(&dir) {
            return err_of(e);
        }
        let file = match dir.create_tmpfile() {
            Ok(file) => file,
            Err(e) => return err_of(e),
        };
        // The mode is the file's, as it is for any create - an unnamed inode
        // is made the same way and linked in later, and a caller with no name
        // to `chmod` afterwards has no second chance to say 04755. Package
        // extraction is that caller: apk unpacks a file through `O_TMPFILE`
        // and links it in, so a mode dropped here is a setuid helper that
        // arrives as 0644 and refuses to run.
        if mode & 0o7777 != 0 {
            file.set_mode(masked_mode(mode));
        }
        let fd = match crate::vfs::open_inode(file) {
            Ok(fd) => fd,
            Err(e) => return err_of(e),
        };
        // Nothing to truncate and no name to label: only `O_CLOEXEC` applies,
        // and it says the same thing here it says on any other open.
        if flags & O_CLOEXEC != 0 {
            let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
        }
        return fd as isize;
    }
    // Resolved before the base moves into `open_opt`, and used only if what
    // comes back is a directory.
    let label = recoverable_path(path, &base);
    // The access the flags ask for, in the bit set the triads use. `O_RDONLY`
    // is zero, so the access bits are a three-way choice rather than a flag:
    // reading, writing, or both. `O_TRUNC` is a write whatever the access bits
    // say - a file emptied by a caller who asked only to read it is the
    // combination Linux answers EACCES for.
    let mut want = match flags & O_ACCMODE {
        O_WRONLY => W_OK,
        O_RDWR => R_OK | W_OK,
        _ => R_OK,
    };
    if flags & O_TRUNC != 0 {
        want |= W_OK;
    }
    // Whether this open is what brought the name into being. Linux does not
    // check a mode against the caller who just asked for it: that is what
    // makes `open(name, O_CREAT | O_WRONLY, 0)` succeed and a second open of
    // the same name fail.
    let mut created = false;
    // O_EXCL refuses to open a name that is already there, whatever it names.
    if flags & O_CREAT != 0
        && flags & O_EXCL != 0
        && crate::vfs::resolve_opt(path, Some(base.clone())).is_ok()
    {
        return err::EEXIST;
    }
    // O_NOFOLLOW: the caller asked that the open not traverse a symbolic link
    // at the final component. Resolve the path without following its last name
    // - a *walk*, so a mount covering that name is crossed exactly as it is
    // without the flag, which is Linux's answer for `lstat` and for this door
    // alike (measured, `.workbuddy/probe_mountpoint_doors.c`: `open(mount
    // point, O_NOFOLLOW)` succeeds and reads the mounted root) - and if what it
    // lands on is a link, ELOOP (measured on the host:
    // open(symlink, O_NOFOLLOW) is ELOOP, open(regular, O_NOFOLLOW) succeeds,
    // and intermediate links are still followed). The old handler defined no
    // O_NOFOLLOW at all, so the bit was silently dropped and the open followed
    // the very link the caller said to stop at - the classic `/tmp` defence,
    // and a §4.2 accept-and-drop with security meaning.
    if flags & O_NOFOLLOW != 0
        && let Ok(inode) = crate::vfs::resolve_no_follow_at(path, base.clone())
        && inode.metadata().kind == NodeKind::Symlink
    {
        return err::ELOOP;
    }
    let opened = match crate::vfs::open_opt(path, Some(base.clone()), flags) {
        Ok(fd) => Ok(fd),
        Err(crate::vfs::VfsError::NoEntry) if flags & O_CREAT != 0 => {
            // O_EXCL names an existing file EEXIST; a missing directory, no
            // space: the create error is the answer, not a retry.
            //
            // A new name is a write to the directory and finding where it goes
            // is a search, so the parent's bits are asked first - and a refusal
            // here is a create that leaves nothing behind.
            if let Ok((parent, _)) = crate::vfs::split_parent(path)
                && let Ok(dir) = crate::vfs::resolve_opt(parent, Some(base.clone()))
                && let Err(e) = crate::vfs::may_create_in(&dir)
            {
                return err_of(e);
            }
            let parent_for_news = match crate::vfs::split_parent(path) {
                Ok((parent, name)) => crate::vfs::resolve_opt(parent, Some(base.clone()))
                    .ok()
                    .map(|dir| (dir, alloc::string::String::from(name))),
                Err(_) => None,
            };
            if let Err(e) = crate::vfs::create_file_opt(path, Some(base.clone())) {
                return err_of(e);
            }
            created = true;
            if let Some((dir, name)) = parent_for_news {
                // `create_file_opt` makes a file, so this name's create event
                // carries no `IN_ISDIR` - and the bit has to come from the
                // child rather than from the directory the name landed in.
                crate::vfs::inotify::emit_child(&dir, crate::vfs::inotify::IN_CREATE, &name, false);
            }
            // A file born through `open(O_CREAT, mode)` carries that mode, and
            // this is how a package is unpacked: `apk` writes every extracted
            // file's mode here - setuid helpers and executable scripts
            // included - and a kernel that drops the argument hands back a
            // tree of 0644 files. Only a *creation* sets it: on a name that
            // already exists the argument is ignored, as the man page says.
            if mode & 0o7777 != 0
                && let Ok(created) = crate::vfs::resolve_opt(path, Some(base.clone()))
            {
                created.set_mode(masked_mode(mode));
            }
            crate::vfs::open_opt(path, Some(base), flags)
        }
        Err(e) => Err(e),
    };
    match opened {
        Ok(fd) => {
            // The refusals the name earns once it resolves, the ones Linux's
            // may_open makes: a directory opened for writing - the access
            // bits, since O_RDONLY is zero - or with O_TRUNC, a write with
            // the serial number filed off, is EISDIR; and O_DIRECTORY over
            // anything but a directory is ENOTDIR. O_CREAT is *not* refused:
            // on a directory that already exists it is the ordinary
            // "ensure this is here" idiom, and Linux opens it. Saying yes
            // hands back a descriptor whose first real use fails with an
            // errno the caller has no way to connect to the open that
            // succeeded. The refused descriptor is closed on the way out: a
            // refusal that leaks the fd it refused would be a second bug
            // answering the first.
            if let Ok(file) = crate::sched::current_fds().clone_at(fd) {
                let dir = file.metadata().kind == NodeKind::Dir;
                if dir && (flags & O_ACCMODE != 0 || flags & O_TRUNC != 0) {
                    let _ = crate::sched::current_fds().close(fd);
                    return err::EISDIR;
                }
                if flags & O_DIRECTORY != 0 && !dir {
                    let _ = crate::sched::current_fds().close(fd);
                    return err::ENOTDIR;
                }
                // The permission check is last of the refusals, so the shape
                // errors above keep their errno, and it is skipped for a name
                // this very open created.
                if !created && want != 0 {
                    let inode = file.inode();
                    if let Err(e) = crate::vfs::may_access(&inode, want) {
                        let _ = crate::sched::current_fds().close(fd);
                        return err_of(e);
                    }
                    // Then the inode's own flags, and only for an open that
                    // means to write. `chattr(1)` is precise about both: an
                    // immutable file "cannot be opened in write mode", and an
                    // append-only one "can only be opened in append mode for
                    // writing". That is why the refusal is *here*, at the open,
                    // rather than left to the write - a caller that opens a file
                    // it may not write to should hear so before it starts.
                    //
                    // The write path asks the same questions again, and that is
                    // not redundant: a flag can be set on a file somebody is
                    // already holding open, and the flag is a fact about the
                    // file rather than about the description Linux's
                    // `rw_verify_area` exists for the same reason.
                    if want & W_OK != 0 {
                        if crate::vfs::is_immutable(&inode) {
                            let _ = crate::sched::current_fds().close(fd);
                            return err::EPERM;
                        }
                        if crate::vfs::is_append_only(&inode)
                            && flags & crate::vfs::file::O_APPEND as usize == 0
                        {
                            let _ = crate::sched::current_fds().close(fd);
                            return err::EPERM;
                        }
                    }
                }
            }
            // Where this description lives, recorded for the `st_dev` both
            // doors report. The label is the path it was opened by, and the
            // filesystem that serves a path is decided by the mount table, so
            // this is the one moment the answer is free. Without it `fstat`
            // had to guess from the inode, and an inode does not name its
            // filesystem - see `File::dev`.
            if let Some(path) = label.as_deref()
                && let Ok(file) = crate::sched::current_fds().clone_at(fd)
            {
                file.set_dev(crate::vfs::mount::dev_of_path(&absolute(path)));
            }
            // A directory description remembers the path it was opened by, so
            // a later `fchdir` can give the cwd a name `getcwd` reads back. A
            // character device remembers too: libc's `ttyname` names the tty
            // by reading `/proc/self/fd/<n>`, and a nameless descriptor reads
            // back as nothing at all.
            if let Some(label) = label
                && let Ok(file) = crate::sched::current_fds().clone_at(fd)
                && matches!(
                    file.metadata().kind,
                    crate::vfs::NodeKind::Dir | crate::vfs::NodeKind::CharDev
                )
            {
                file.set_label(label);
            }
            // The close events need the access mode from the moment it is
            // still only the open's flags: the description keeps it, and the
            // `IN_OPEN` report rides the same clone.
            if let Ok(file) = crate::sched::current_fds().clone_at(fd) {
                file.mark_opened_for(flags & O_ACCMODE != 0);
                // Record the `O_ACCMODE` itself, not just "wrote-ish": `File::read`
                // and `File::write` refuse a mismatch with `EBADF` from here on,
                // which is Linux's answer for `write()` on an `O_RDONLY` fd and the
                // reverse. This field is the one owner of the fact - `F_GETFL`
                // composes its answer from it, and `F_SETFL` cannot reach it, which
                // is what stops a caller that changes the status flags from
                // reopening a direction that was refused.
                file.set_access((flags & O_ACCMODE) as u32);
                crate::vfs::inotify::emit(&file.inode(), crate::vfs::inotify::IN_OPEN, "");
                // An evdev client's read position starts here, at the
                // description that will do the reading: Linux hands a client
                // its own event buffer at `open`, so an event that arrives
                // before the first read is still that client's to read.
                #[cfg(target_os = "none")]
                if let Some(ev) = file.inode().as_any().downcast_ref::<EvdevNode>() {
                    ev.opened(file.ofd());
                }
            }
            // `O_CLOEXEC` is part of the open flags, not a property of the file:
            // it says the descriptor must not survive the next `execve`. Left
            // unrecorded, an inherited descriptor keeps a pipe or a lock open
            // past the exec that was meant to end it.
            if flags & O_CLOEXEC != 0 {
                let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
            }
            // The half of the open's flag word that outlives the open and is
            // the caller's to change later: `O_APPEND` and `O_NONBLOCK`.
            // Storing it here is what makes `O_APPEND` bite at all - a
            // description that never learned it writes at its cursor, which
            // for a fresh open is the start of the file, so a log writer
            // overwrites the beginning of its own log instead of appending.
            // `set_status` keeps only those bits, and the access mode is
            // already recorded in the field `F_GETFL` composes its word from.
            if let Ok(file) = crate::sched::current_fds().clone_at(fd) {
                file.set_status(flags as u32);
                // And the other half of the word: everything the caller asked
                // for that the open answered without spending it -
                // `O_DIRECTORY`, `O_NOFOLLOW`, `O_PATH`, `O_SYNC` and the rest
                // of `O_*`, which Linux's `F_GETFL` reports back because its
                // `filp->f_flags` kept them. Without this a caller that
                // compares the word against what it opened with sees a
                // disagreement on Linux's answer for every one of them
                // (`docs/uapi/` §5.2 carried it as a gap; item 49 closes it).
                file.set_born(flags as u32);
                // What landed is the masked word, and the bit with teeth is
                // `O_NONBLOCK`: it has to reach the node that does the blocking.
                sync_nonblocking(&file, file.status());
            }
            // `O_TRUNC` empties an existing file as part of the open, which is
            // what a writer that reopens a name is promising: what it writes
            // now is the whole file. Without it, `wget -O page.html` over a
            // longer page leaves the new page followed by the old one's tail.
            //
            // A truncate that the file's own flags refuse follows the *open*
            // out of here rather than being swallowed: `O_TRUNC` on an
            // immutable file is `EPERM` in Linux, and a kernel that opened the
            // file and quietly left it whole would be answering the wrong
            // question in the only way a caller cannot see.
            if flags & O_TRUNC != 0
                && let Ok(file) = crate::sched::current_fds().clone_at(fd)
            {
                let inode = file.inode();
                if inode.metadata().kind == crate::vfs::NodeKind::File {
                    if let Err(e) = crate::vfs::may_alter(&inode) {
                        let _ = crate::vfs::close(fd);
                        return err_of(e);
                    }
                    // `O_TRUNC` is a resize, and Linux's `do_open` reaches the
                    // same `do_truncate` for it - measured, opening a `4755`
                    // file with it stores `0755`.
                    let _ = inode.truncate(0);
                    crate::vfs::remove_privs(&inode);
                }
            }
            fd as isize
        }
        Err(e) => err_of(e),
    }
}

/// The absolute path `path` names, when this is an open whose spelling can be
/// recovered: an absolute path, or a relative one taken against the cwd. A
/// relative path resolved against some other directory descriptor names a file
/// the walk from `/` would have to search for, so it is left unnamed and
/// `fchdir` falls back to searching.
fn recoverable_path(path: &str, base: &Arc<dyn Inode>) -> Option<alloc::string::String> {
    if path.starts_with('/') {
        Some(normalize_path(path, "/"))
    } else if Arc::ptr_eq(base, &crate::sched::current_cwd()) {
        Some(normalize_path(path, &crate::sched::current_cwd_path()))
    } else {
        None
    }
}

/// `open(path, flags, mode)` - the mode is ignored, as for every open here:
/// opens are read/write-capable and nothing is inherited across an exec that
/// does not happen.
pub fn open(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    open_linux(&path, args[1], args[2] as u32, cwd.clone())
}

/// `openat(dirfd, path, flags, mode)`.
pub fn openat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    // The name is read before the descriptor is looked at, because `base_for`
    // needs it and because that is Linux's own order - see its comment.
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let base = match base_for(args[0] as i32, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    open_linux(&path, args[2], args[3] as u32, base)
}

/// `stat(path, buf)` and `lstat(path, buf)`: the pre-`*at` spellings, which are
/// `fstatat` at the caller's cwd. `lstat` is the one that names the link rather
/// than what it points at.
pub fn stat_legacy(nr: u64, args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let flags = if nr == crate::syscall::linux::nr::LSTAT {
        AT_SYMLINK_NOFOLLOW
    } else {
        0
    };
    newfstatat([AT_FDCWD as usize, args[0], args[1], flags, 0, 0], cwd)
}

/// `fstat(fd, buf)` - the fd's description, not a path, is what is statted.
pub fn fstat(args: [usize; ARG_COUNT]) -> isize {
    let (fd, buf) = (args[0] as i32, args[1]);
    let (meta, dev, rdev, stored, nlink, owner, times) =
        match crate::sched::current_fds().clone_at(fd) {
            Ok(file) => {
                let inode = file.inode();
                // The filesystem a descriptor belongs to is the one the path it
                // was opened through is served by - the same answer `stat` by
                // path gives, which is what libc's `ttyname` compares the two
                // sides of, and what a lock-and-verify caller needs to agree. A
                // descriptor with no path of its own falls back to the
                // mount-table lookup, and a nameless one to the root.
                let dev = match file.dev() {
                    Some(dev) => dev,
                    None => match file.label() {
                        Some(path) => crate::vfs::mount::dev_of_path(&path),
                        None => crate::vfs::mount::dev_of_inode(&inode),
                    },
                };
                (
                    file.metadata(),
                    dev,
                    inode.rdev(),
                    reported_mode(&inode),
                    inode.nlink(),
                    inode.owner(),
                    StatTimes {
                        atime: inode.atime(),
                        mtime: inode.mtime(),
                        ctime: inode.ctime(),
                    },
                )
            }
            Err(e) => return err_of(e),
        };
    write_stat(
        buf,
        &meta,
        dev,
        rdev,
        stored,
        nlink,
        observed_owner(owner),
        times,
    )
}

/// `flock(fd, operation)` - the advisory whole-file lock `apk` takes on its
/// database before it will touch it.
///
/// Owned by the **open file description**, not the process, which is Linux's
/// rule and the one that makes `flock` usable for the `apk` database lock: two
/// independent `open()`s of one file in *one* process contend with each other,
/// while a `dup`ed descriptor shares the lock its original was given. So the
/// owner below is the description's identity, and only `release_description`
/// knows which locks die with it.
pub fn flock(args: [usize; ARG_COUNT]) -> isize {
    let file = match crate::sched::current_fds().clone_at(args[0] as i32) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    let op = args[1] as i32;
    const LOCK_SH: i32 = 1;
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const LOCK_UN: i32 = 8;
    if op & !(LOCK_SH | LOCK_EX | LOCK_NB | LOCK_UN) != 0 {
        return err::EINVAL;
    }
    // Exactly one of SH/EX/UN selects the operation; a request for both lock
    // kinds, or for none, is the `EINVAL` the man page spells.
    let kind = match op & (LOCK_SH | LOCK_EX | LOCK_UN) {
        LOCK_SH => Some(crate::vfs::locks::LockKind::Read),
        LOCK_EX => Some(crate::vfs::locks::LockKind::Write),
        LOCK_UN => None,
        _ => return err::EINVAL,
    };
    #[cfg(target_os = "none")]
    {
        let inode = file.inode();
        let dev = crate::vfs::mount::fs_id_of_inode(&inode);
        let ino = file.metadata().ino;
        // The description, not the caller: `dup` shares an `Arc<File>`, so the
        // same pointer *is* the same open file description, and two `open()`s
        // of one file are two pointers however few processes are involved.
        let ofd = Arc::as_ptr(&file) as u64;
        let pid = crate::sched::current_tgid().0 as u32;
        // A whole-file lock covers `[0, infinity)`, in the `Whole` class that
        // never interacts with an `fcntl` record lock, and it blocks unless
        // `LOCK_NB` was asked for.
        lock_take(
            dev,
            ino,
            ofd,
            pid,
            crate::vfs::locks::LockClass::Whole,
            kind,
            0,
            crate::vfs::locks::INF,
            op & LOCK_NB == 0,
            false,
        )
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = kind;
        0
    }
}

/// `ftruncate(fd, length)` - the resize `wget` asks of its output before it
/// writes and `apk`'s fetcher asks of its temp file after it does.
///
/// The resize is real, and has to be: a shorter file keeps no tail. A writer
/// that only overwrites from the front - and `O_TRUNC` that only forgets to
/// matter - leaves the new bytes followed by whatever the old file had that
/// the new one does not, which is a corrupt file nothing complains about.
pub fn ftruncate(args: [usize; ARG_COUNT]) -> isize {
    let length = args[1] as isize;
    if length < 0 {
        return err::EINVAL;
    }
    let file = match crate::sched::current_fds().clone_at(args[0] as i32) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    // Linux refuses to resize anything but a regular file, and `EINVAL` is
    // what a caller reads as "this descriptor is not that": a pipe, a
    // terminal, a socket. A pipe's inode reports [`NodeKind::File`] - the
    // vocabulary's only word for an unnamed channel, and the word `epoll`'s
    // pollable test reads it by - so the pipe is asked about separately, or the
    // refusal comes from the node's default `truncate` as `EOPNOTSUPP`.
    // Measured on the host (`.workbuddy/probe_fsize.c`): `ftruncate` answers
    // `EINVAL` on *both* ends of a pipe - the read end through the write-mode
    // check below, and the write end because a pipe's inode has no `setattr`
    // for the resize to reach.
    if file.metadata().kind != NodeKind::File || file.inode().as_pipe().is_some() {
        return err::EINVAL;
    }
    // `ftruncate` is a write, so a descriptor opened read-only is refused - and
    // the file is left whole. Measured on the host: `ftruncate` on an
    // `O_RDONLY` fd answers `EINVAL` and the size is unchanged, while an
    // `O_WRONLY` fd truncates happily (`0`, size follows) - so the rule is
    // "may it write?", which is `File::may_write`, not `check_writable`: this
    // call's own errno is `EINVAL` where the `write(2)` door owes `EBADF`. A
    // descriptor that never carried a path `open` (a socket, a pipe, an
    // anonymous buffer) holds the permissive default and is unaffected - and it
    // fails the file check above anyway.
    if !file.may_write() {
        return err::EINVAL;
    }
    // The inode's own flags, after the descriptor's: `chattr +i` on a file
    // somebody is already holding open still refuses the truncate, and
    // `chattr +a` refuses it too - which is the one place the two flags do not
    // differ, because cutting a file back is the operation `a` exists to
    // prevent.
    if let Err(e) = crate::vfs::may_alter(&file.inode()) {
        return err_of(e);
    }
    match file.inode().truncate(length as u64) {
        Ok(()) => {
            // A resize is a change to the bytes, whichever way it goes - down
            // or up - so the set-id bits go with it (`.workbuddy/probe_suid_write.c`:
            // both directions of `ftruncate` on a `4755` file store `0755`).
            crate::vfs::remove_privs(&file.inode());
            crate::vfs::inotify::emit(&file.inode(), crate::vfs::inotify::IN_MODIFY, "");
            0
        }
        Err(e) => err_of(e),
    }
}

/// `truncate(path, length)` - `ftruncate` by name instead of descriptor.
pub fn truncate(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let length = args[1] as isize;
    if length < 0 {
        return err::EINVAL;
    }
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let inode = match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(inode) => inode,
        Err(e) => return err_of(e),
    };
    // Only a regular file has a size to set, and Linux's two answers for what
    // is not one are the ones `setattr_prepare` gives: a directory is `EISDIR`,
    // and anything else that is not a regular file is `EINVAL` - measured
    // through `syscall(2)` on the host (`.workbuddy/probe_fsize.c`), where
    // `/tmp` answers `EISDIR` and `/dev/null` answers `EINVAL`. The question is
    // asked here rather than left to the node, because the node's default
    // `truncate` answers `EOPNOTSUPP` - the refusing shape `src/vfs/ext4/`'s own
    // inode already gets right for its tree (`Dir` is `IsDir`, everything else
    // that is not a file is `Invalid`), and this door is the one that reaches
    // every other filesystem's nodes.
    // A pipe reports `NodeKind::File` too - the vocabulary's only word for an
    // unnamed channel - so it is asked beside the kind, exactly as `ftruncate`
    // asks it (§5.1 item 22). The path spelling answers Linux's `EINVAL` for
    // *both* of a pipe's ends, measured through `syscall(2)` on a current host
    // (`.workbuddy/probe_fdlink.c`, where the name is a pipe's own
    // `/proc/self/fd/<N>` as well as a `mknod`'d FIFO in `.workbuddy/probe_fsize.c`),
    // and this door is the one that reaches it now that the walk denotes a
    // descriptor's second name as the file behind it.
    match inode.metadata().kind {
        NodeKind::File if inode.as_pipe().is_none() => {}
        NodeKind::Dir => return err::EISDIR,
        _ => return err::EINVAL,
    }
    // Emptying a file is a write to it, whatever the caller's descriptor
    // situation: Linux asks for write permission here, and `truncate` on a
    // file the caller may only read is EACCES rather than a silent success.
    if let Err(e) = crate::vfs::may_access(&inode, crate::vfs::W_OK) {
        return err_of(e);
    }
    if let Err(e) = crate::vfs::may_alter(&inode) {
        return err_of(e);
    }
    match inode.truncate(length as u64) {
        Ok(()) => {
            // The same door as `ftruncate`'s, reached by name rather than by
            // descriptor: the resize costs the file its set-id bits.
            crate::vfs::remove_privs(&inode);
            crate::vfs::inotify::emit(&inode, crate::vfs::inotify::IN_MODIFY, "");
            0
        }
        Err(e) => err_of(e),
    }
}

/// `fstatfs(fd, buf)` - filesystem facts for whatever `fd` is open on. The
/// descriptor names a file; the file names a filesystem. A file that *is* a
/// mountpoint reports that mount; anything else reports the root, which is
/// the honest answer this kernel can give without tracking a path per
/// descriptor.
pub fn fstatfs(args: [usize; ARG_COUNT]) -> isize {
    let inode = match crate::sched::current_fds()
        .clone_at(args[0] as i32)
        .map(|f| f.inode().clone())
    {
        Ok(i) => i,
        Err(e) => return err_of(e),
    };
    let fstype = crate::vfs::mount::fstype_of_inode(&inode);
    let stats = crate::vfs::mount::fs_stats_of_inode(&inode);
    write_statfs(args[1], &fstype, stats)
}

/// `statfs(path, buf)` - the path spelling of `fstatfs`, and the one that
/// carries real information: the deepest mountpoint prefixing the path is
/// the filesystem that serves it.
pub fn statfs(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(_) => {
            let abs = absolute(&path);
            let stats = crate::vfs::mount::fs_stats_of_path(&abs);
            let (_id, fstype) = crate::vfs::mount::fs_of_path(&abs);
            write_statfs(args[1], &fstype, stats)
        }
        Err(e) => err_of(e),
    }
}

/// The absolute, normalized spelling of a path a syscall was handed, for the
/// filesystem lookup: `st_dev` and `statfs`'s type both key on the path, and
/// `.` names whatever the caller's cwd is.
fn absolute(path: &str) -> alloc::string::String {
    if path.starts_with('/') {
        crate::vfs::path::normalize(path)
    } else {
        let cwd = crate::sched::current_cwd_path();
        crate::vfs::path::normalize(&alloc::format!("{}/{}", cwd.trim_end_matches('/'), path))
    }
}

/// `newfstatat(dirfd, path, buf, flags)` - stat by path, relative to `dirfd`.
pub fn newfstatat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let (dirfd, path_ptr, buf, flags) = (args[0] as i32, args[1], args[2], args[3]);
    let path = match cstr_arg(path_ptr, true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // AT_EMPTY_PATH is the fstatat spelling of "stat the descriptor".
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return err::ENOENT;
        }
        let (meta, dev, rdev, stored, nlink, owner, times) =
            match crate::sched::current_fds().clone_at(dirfd) {
                Ok(file) => {
                    let inode = file.inode();
                    let dev = match file.dev() {
                        Some(dev) => dev,
                        None => match file.label() {
                            Some(path) => crate::vfs::mount::dev_of_path(&path),
                            None => crate::vfs::mount::dev_of_inode(&inode),
                        },
                    };
                    (
                        file.metadata(),
                        dev,
                        inode.rdev(),
                        reported_mode(&inode),
                        inode.nlink(),
                        inode.owner(),
                        StatTimes {
                            atime: inode.atime(),
                            mtime: inode.mtime(),
                            ctime: inode.ctime(),
                        },
                    )
                }
                Err(e) => return err_of(e),
            };
        return write_stat(
            buf,
            &meta,
            dev,
            rdev,
            stored,
            nlink,
            observed_owner(owner),
            times,
        );
    }
    let base = match base_for(dirfd, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    // `lstat` is the one call that wants the link and not its target, so it
    // walks to the parent and looks the name up there - a resolver that
    // followed links would hand back the very thing it was told not to.
    let (meta, rdev, stored, nlink, owner, times) = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        match crate::vfs::resolve_no_follow_at(&path, base) {
            Ok(inode) => (
                inode.metadata(),
                inode.rdev(),
                reported_mode(&inode),
                inode.nlink(),
                inode.owner(),
                StatTimes {
                    atime: inode.atime(),
                    mtime: inode.mtime(),
                    ctime: inode.ctime(),
                },
            ),
            Err(e) => return err_of(e),
        }
    } else {
        match crate::vfs::resolve_opt(&path, Some(base)) {
            Ok(inode) => (
                inode.metadata(),
                inode.rdev(),
                reported_mode(&inode),
                inode.nlink(),
                inode.owner(),
                StatTimes {
                    atime: inode.atime(),
                    mtime: inode.mtime(),
                    ctime: inode.ctime(),
                },
            ),
            Err(e) => return err_of(e),
        }
    };
    let dev = crate::vfs::mount::dev_of_path(&absolute(&path));
    write_stat(
        buf,
        &meta,
        dev,
        rdev,
        stored,
        nlink,
        observed_owner(owner),
        times,
    )
}

/// `statx(dirfd, path, flags, mask, buf)` - the extended stat uutils calls.
///
/// Every `STATX_BASIC_STATS` bit the caller asked for is filled; timestamps
/// are not, and the mask says so. The struct is zeroed through `stx_mnt_id`
/// (152 bytes) first, so fields the caller did not ask about read as absent.
pub fn statx(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let (dirfd, path_ptr, flags, mask, buf) =
        (args[0] as i32, args[1], args[2], args[3] as u32, args[4]);
    // A flag word outside Linux's statx vocabulary is EINVAL, and
    // `AT_SYMLINK_NOFOLLOW` is honoured, not ignored: the old handler discarded
    // it (`_flags`) and always resolved the link, so an `lstat`-style statx
    // (which `ls -l`, `stat` and uutils reach for) reported the *target's*
    // metadata while claiming success - a success with data describing the wrong
    // node, the §4.2 wrong-shape answer. `fstatat` already split here; statx now
    // does the same.
    if !abi::statx_flags_are_known(flags) {
        return err::EINVAL;
    }
    let path = match cstr_arg(path_ptr, true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // An empty path names the descriptor, but only when the caller said so:
    // Linux requires AT_EMPTY_PATH and answers ENOENT for the unannounced form.
    let (meta, dev, rdev, stored, nlink, owner, times) = if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return err::ENOENT;
        }
        match crate::sched::current_fds().clone_at(dirfd) {
            Ok(file) => {
                let inode = file.inode();
                // The same ladder `fstat`/`fstatat` climb for `st_dev`: a
                // descriptor that knows its device says so, one carrying a path
                // is looked up by it, and one with neither is the root's.
                let dev = match file.dev() {
                    Some(dev) => dev,
                    None => match file.label() {
                        Some(path) => crate::vfs::mount::dev_of_path(&path),
                        None => crate::vfs::mount::dev_of_inode(&inode),
                    },
                };
                (
                    file.metadata(),
                    dev,
                    inode.rdev(),
                    reported_mode(&inode),
                    inode.nlink(),
                    inode.owner(),
                    StatTimes {
                        atime: inode.atime(),
                        mtime: inode.mtime(),
                        ctime: inode.ctime(),
                    },
                )
            }
            Err(e) => return err_of(e),
        }
    } else {
        let base = match base_for(dirfd, &path, cwd) {
            Ok(base) => base,
            Err(e) => return e,
        };
        // `AT_SYMLINK_NOFOLLOW` asks for the link, not its target: take the same
        // parent-and-name no-follow walk `fstatat`'s `lstat` uses, rather than
        // the following resolver.
        let resolved = if flags & AT_SYMLINK_NOFOLLOW != 0 {
            crate::vfs::resolve_no_follow_at(&path, base)
        } else {
            crate::vfs::resolve_opt(&path, Some(base))
        };
        // `stx_dev_major`/`minor` is the same `st_dev` `stat` answers with, so
        // it is looked up the same way - by the path, against the table of
        // mounts - and `statx` therefore agrees with `newfstatat` about a file
        // reached by a `dirfd` exactly as far as `newfstatat` agrees with
        // `stat`. Sharing that limit is the point: two answers to one question
        // that disagree are worse than two that are both approximate.
        let dev = crate::vfs::mount::dev_of_path(&absolute(&path));
        match resolved {
            Ok(inode) => (
                inode.metadata(),
                dev,
                inode.rdev(),
                reported_mode(&inode),
                inode.nlink(),
                inode.owner(),
                StatTimes {
                    atime: inode.atime(),
                    mtime: inode.mtime(),
                    ctime: inode.ctime(),
                },
            ),
            Err(e) => return err_of(e),
        }
    };

    // The byte layout - the zeroed struct, its field offsets, the
    // `STATX_BASIC_STATS` mask answer - is the ABI, and lives in `abi` where the
    // host tests can walk it field by field. This side only resolves what the
    // struct says.
    abi::write_statx(
        buf,
        mask,
        &meta,
        dev,
        stored,
        rdev,
        nlink,
        observed_owner(owner),
        times,
    )
}

/// The `access` question, for all three spellings: may the process whose ids
/// are `me` do `mode` to `inode`?
///
/// `mode` is the `R_OK`/`W_OK`/`X_OK` bit set, and `F_OK` - zero - asks only
/// that the name is there, which resolving it has already answered. `me` is
/// the identity to ask about: the **real** ids for `access(2)`, and either
/// pair for the `faccessat` family, which is what `AT_EACCESS` selects.
fn access_check(inode: &Arc<dyn Inode>, mode: usize, me: (u32, u32)) -> isize {
    if mode & !(R_OK as usize | W_OK as usize | X_OK as usize) != 0 {
        return err::EINVAL;
    }
    if mode == 0 {
        return 0;
    }
    // The ids are the door's (`access(2)`'s real pair, or the effective one
    // under `AT_EACCESS`), and the capabilities are the caller's current ones
    // either way - Linux's `capable()` reads `current_cred()` whatever ids the
    // door chose, which is [`crate::vfs::caller_as`]'s whole reason.
    match crate::vfs::may_as(crate::vfs::caller_as(me.0, me.1), inode, mode as u32) {
        Ok(()) => 0,
        Err(e) => err_of(e),
    }
}

/// The identity an `access` call asks about: the effective pair when
/// `AT_EACCESS` is set, the real one otherwise.
fn access_ids(flags: usize) -> (u32, u32) {
    if flags & abi::AT_EACCESS != 0 {
        crate::sched::current_owner()
    } else {
        crate::sched::current_real_owner()
    }
}

/// `access(path, mode)` - may the caller reach `path` for `mode`?
///
/// The ids asked about are the **real** ones, not the effective ones: this is
/// the call a setuid program makes to find out what the user who invoked it
/// could do, and answering with its own raised privileges would make the
/// question meaningless.
pub fn access(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(inode) => access_check(&inode, args[1], crate::sched::current_real_owner()),
        Err(e) => err_of(e),
    }
}

/// `faccessat(dirfd, path, mode, flags)` - the dirfd form of access.
pub fn faccessat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let base = match base_for(args[0] as i32, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    match crate::vfs::resolve_opt(&path, Some(base)) {
        Ok(inode) => access_check(&inode, args[2], access_ids(args[3])),
        Err(e) => err_of(e),
    }
}

// ---------------------------------------------------------------------------
// extended attributes: validated, then reported as unsupported
// ---------------------------------------------------------------------------

/// The inode a descriptor names, for the `f*` xattr spellings. The descriptor
/// only has to be open; the attribute's target is then the file it holds.
fn xattr_inode_of_fd(fd: i32) -> Result<Arc<dyn Inode>, isize> {
    crate::sched::current_fds()
        .clone_at(fd)
        .map(|file| file.inode())
        .map_err(err_of)
}

/// The filesystem a name resolves to, checked the way the xattr layer would
/// check it before reaching a handler: the path (and the attribute name, when
/// the operation takes one) must be readable strings, and the target must
/// exist. `follow` tells the `l`-forms, which act on a final symlink rather
/// than its target, to resolve without following.
///
/// This kernel stores no extended attributes on any of its filesystems, so once
/// the target is confirmed the answer is `EOPNOTSUPP` - the "this filesystem
/// does not do xattrs" report Linux gives a filesystem with no xattr support,
/// and the reason the call is wired to a handler at all. Before it was, the
/// syscall number fell through to `ENOSYS`, which a caller reads as "this
/// kernel has no such call" and may handle far more bluntly than a single
/// unsupported filesystem.
fn xattr_target(path: &str, cwd: &Arc<dyn Inode>, follow: bool) -> isize {
    let resolved = if follow {
        crate::vfs::resolve_opt(path, Some(cwd.clone()))
    } else {
        crate::vfs::resolve_no_follow_at(path, cwd.clone())
    };
    match resolved {
        Ok(_) => err::EOPNOTSUPP,
        Err(e) => err_of(e),
    }
}

/// Which of the three shapes an `*xattr` call is, for the arguments that only
/// some of them carry.
///
/// The family shares one handler because it shares one verdict: nothing here
/// stores an attribute, so every well-formed request ends as `EOPNOTSUPP`. What
/// the shapes do *not* share is their argument list - `setxattr` is the one with
/// a value and a flags word - and the checks that have to run before the verdict
/// are the ones those arguments exist for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Xattr {
    /// `getxattr` and its `l`/`f` forms: a name, then a buffer the store would
    /// have filled.
    Get,
    /// `setxattr` and its `l`/`f` forms: a value of `size` bytes, and the flags
    /// word saying how it may land.
    Set,
    /// `removexattr` and its `l`/`f` forms: the name, and nothing else.
    Remove,
}

impl Xattr {
    /// The shape a syscall number is. The three forms of one call differ only in
    /// how the target is reached - by path, by path without following a final
    /// link, or by descriptor - so they answer the same shape.
    fn of(nr: u64) -> Xattr {
        use crate::syscall::linux::nr;
        match nr {
            nr::SETXATTR | nr::LSETXATTR | nr::FSETXATTR => Xattr::Set,
            nr::REMOVEXATTR | nr::LREMOVEXATTR | nr::FREMOVEXATTR => Xattr::Remove,
            _ => Xattr::Get,
        }
    }
}

/// The checks on the two arguments only a `setxattr` carries, in Linux's order:
/// the flags word, then the value the call names.
///
/// `XATTR_CREATE` and `XATTR_REPLACE` are the two bits Linux defines - "make it,
/// or fail if it is already there" and its converse - so any other bit is
/// `EINVAL`, and both together are `EINVAL` too, because they contradict each
/// other rather than combine. The value is then *copied before the filesystem is
/// asked*, which is why a set whose value this kernel cannot read is `EFAULT`
/// even though the answer would have been `EOPNOTSUPP`: the refusal is about the
/// bytes, and a caller that got "unsupported" for a bad pointer would go looking
/// for the wrong bug.
fn setxattr_args(value: usize, size: usize, flags: usize) -> Option<isize> {
    const XATTR_CREATE: usize = 0x1;
    const XATTR_REPLACE: usize = 0x2;
    if flags & !(XATTR_CREATE | XATTR_REPLACE) != 0 || flags == (XATTR_CREATE | XATTR_REPLACE) {
        return Some(err::EINVAL);
    }
    if size != 0 && !buffer_ok(value, size, false, true) {
        return Some(err::EFAULT);
    }
    None
}

/// `getxattr`/`setxattr`/`removexattr` and their `l`-prefixed forms. All three
/// take a path then an attribute name in the first two slots, and all three
/// reach the same verdict here, so they share it - the call's shape deciding
/// only what else is read before it.
pub fn xattr_path(nr: u64, args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>, follow: bool) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    if let Err(e) = cstr_arg(args[1], true) {
        return e;
    }
    if Xattr::of(nr) == Xattr::Set
        && let Some(e) = setxattr_args(args[2], args[3], args[4])
    {
        return e;
    }
    xattr_target(&path, cwd, follow)
}

/// `fgetxattr`/`fsetxattr`/`fremovexattr`: the descriptor form. A bad descriptor
/// is `EBADF` before the name is even looked at, as the descriptor table check
/// happens first.
pub fn xattr_fd(nr: u64, args: [usize; ARG_COUNT]) -> isize {
    if let Err(e) = xattr_inode_of_fd(args[0] as i32) {
        return e;
    }
    if let Err(e) = cstr_arg(args[1], true) {
        return e;
    }
    if Xattr::of(nr) == Xattr::Set
        && let Some(e) = setxattr_args(args[2], args[3], args[4])
    {
        return e;
    }
    err::EOPNOTSUPP
}

/// `listxattr`/`llistxattr`: path, then a buffer to fill - no name argument.
pub fn xattr_list_path(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>, follow: bool) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    xattr_target(&path, cwd, follow)
}

/// `flistxattr`: the descriptor form of the list call.
pub fn xattr_list_fd(args: [usize; ARG_COUNT]) -> isize {
    match xattr_inode_of_fd(args[0] as i32) {
        Ok(_) => err::EOPNOTSUPP,
        Err(e) => e,
    }
}

/// Wall-clock seconds, the value `UTIME_NOW` resolves to - the same sum
/// `gettimeofday` reports (the boot epoch plus elapsed ticks).
fn wall_now() -> u64 {
    crate::hal::boot_epoch() + crate::hal::ticks() / crate::hal::TICK_HZ as u64
}

/// `struct timespec` as `utimensat` reads it: seconds, nanoseconds.
#[repr(C)]
struct Utimespec {
    sec: i64,
    nsec: i64,
}

/// The two `tv_nsec` sentinels that are not a clock value at all: use the
/// current time, or leave that clock as it is. Their values are the ABI's.
const UTIME_NOW: i64 = (1 << 30) - 1;
const UTIME_OMIT: i64 = (1 << 30) - 2;

/// Which of Linux's timestamps questions a call is asking.
///
/// The three handlers below take their clocks in three different structs, and
/// what the permission check turns on is not the struct but the *shape* of the
/// request: a null pointer means both clocks to `now`, and Linux spells that
/// `ATTR_TOUCH`; a pointer means at least one clock named, which is
/// `ATTR_TIMES_SET` - and the two are asked of different questions, so a
/// caller that may write a file somebody else owns may still not name a time
/// for it.
///
/// Measured whole on a current host (`.workbuddy/probe_capable_scope.sh own`,
/// as a root caller with its capabilities dropped one at a time, against files
/// that belong to uid 1000):
///
/// ```text
///                                       whole set  -CAP_FOWNER  -FOWNER,-DAC_OVERRIDE
/// utimensat(NULL) on another's 0600          0          0            EACCES
/// utimensat(NULL) on another's 0666          0          0               0
/// utimensat({NOW, NOW}) on 0600/0666         0          0         EACCES / 0
/// utimensat({NOW, OMIT}) on either           0        EPERM           EPERM
/// utimensat({explicit, explicit}) on either  0        EPERM           EPERM
/// utimensat({OMIT, OMIT}) on either          0          0               0
/// ```
///
/// So three things: the `now` form accepts **write permission** (the third
/// column's `0666` row is `0` with no capability at all, and its refusal is
/// `EACCES` - the bits' answer), the named form does not (both its rows are
/// `EPERM` whatever the bits say), and a call that names nothing but
/// `UTIME_OMIT` changes nothing and is asked nothing. One clock named and one
/// omitted takes the named form's answer, so it is the pointer that decides
/// rather than the values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TimesShape {
    /// A null pointer: both clocks to now - Linux's `ATTR_TOUCH`.
    BothToNow,
    /// A pointer, and something to set - Linux's `ATTR_TIMES_SET`.
    Setting,
    /// A pointer naming `UTIME_OMIT` for both clocks: nothing to ask.
    Nothing,
}

/// The permission half of the three timestamp handlers' shared tail, asked in
/// whichever of its two shapes the call is: the caller's standing on the node.
///
/// The node's own flags are [`crate::vfs::may_modify`]'s question, asked beside
/// this one rather than inside it - an immutable inode refuses the clocks with
/// `EPERM` whatever the caller's standing is, which is the answer `chattr(1)`
/// shows, and the one this kernel gave before either question existed.
fn may_set_times(inode: &Arc<dyn Inode>, shape: TimesShape) -> Result<(), isize> {
    let caller = crate::vfs::current_caller();
    match shape {
        TimesShape::Nothing => Ok(()),
        TimesShape::BothToNow => crate::vfs::may_touch(caller, inode),
        TimesShape::Setting => crate::vfs::may_setattr(caller, inode),
    }
    .map_err(err_of)
}

/// The tail all three timestamps handlers share: the node's flags, the
/// caller's standing on it, and the store.
///
/// It really is one tail now - `utimensat`, `futimesat` and `utime` differ
/// only in which struct they read the clocks out of and in which sentinels
/// that struct can carry, so the two questions every one of them asks live here
/// rather than in three copies that can drift apart.
fn store_times(inode: &Arc<dyn Inode>, atime: u64, mtime: u64, shape: TimesShape) -> isize {
    // The clocks are metadata, so an immutable inode refuses them.
    if let Err(e) = crate::vfs::may_modify(inode) {
        return err_of(e);
    }
    if let Err(e) = may_set_times(inode, shape) {
        return e;
    }
    match inode.set_times(atime, mtime) {
        Ok(()) => {
            crate::vfs::inotify::emit(inode, crate::vfs::inotify::IN_ATTRIB, "");
            0
        }
        Err(e) => err_of(e),
    }
}

/// One clock's new value from a `timespec`, resolving the sentinels: `keep` is
/// what the node holds now (`UTIME_OMIT`), and a plain value is its seconds.
/// A nanosecond field that is neither in range nor a sentinel is refused with
/// `None`, which the caller turns into `EINVAL`.
fn resolve_time(ts: &Utimespec, keep: u64, now: u64) -> Option<u64> {
    match ts.nsec {
        UTIME_NOW => Some(now),
        UTIME_OMIT => Some(keep),
        n if (0..1_000_000_000).contains(&n) => Some(ts.sec as u64),
        _ => None,
    }
}

/// `utimensat(dirfd, pathname, times, flags)`.
///
/// The timestamps are now kept, on a filesystem that has somewhere to put them
/// (ext2 is; the default `set_times` refuses). `touch` is the everyday user. The
/// branch that matters is the one `futimens` speaks: `utimensat(fd, NULL, times,
/// 0)` names no path at all - the descriptor *is* the file - and busybox `touch`
/// opens its target and then futimens it, so a NULL path with a plain-file fd is
/// the everyday shape, not a corner. A null `times` sets both clocks to now; the
/// `UTIME_NOW` / `UTIME_OMIT` sentinels resolve per clock.
///
/// Linux names that same request in a *second* spelling, an empty name together
/// with `AT_EMPTY_PATH`, and this door serves both (measured on a current host,
/// `.workbuddy/probe_at_dirfd.c`: `utimensat(fd, "", NULL, AT_EMPTY_PATH)`
/// answers `0` for a descriptor the caller owns and `EBADF` for one nothing is
/// open on, exactly as the NULL form does). The flag beside a name that is
/// really there is inert - the name answers it - and an empty name *without* the
/// flag is the name's own refusal, which `base_for` gives.
///
/// Who may is *two* questions rather than one, and which applies is the shape
/// of the request rather than the values in it ([`TimesShape`]): write
/// permission buys the null-pointer form and not a pair of named times, and a
/// pair of `UTIME_OMIT`s asks nothing at all.
pub fn utimensat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let (dirfd, path_ptr, times_ptr, flags) = (args[0] as i32, args[1], args[2], args[3]);
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return err::EINVAL;
    }
    // Read the name once, and let its *absence* be a word about the route rather
    // than a second reading of the pointer: the descriptor is the target when
    // there is no name (`futimens`'s spelling) and when the caller gave an empty
    // one and said `AT_EMPTY_PATH`. Everything else resolves by name - the empty
    // name it did *not* mark included, whose answer is `base_for`'s.
    let path = if path_ptr == 0 {
        None
    } else {
        match cstr_arg(path_ptr, true) {
            Ok(path) => Some(path),
            Err(e) => return e,
        }
    };
    let inode: Arc<dyn Inode> = match path {
        Some(name) if !name.is_empty() || flags & AT_EMPTY_PATH == 0 => {
            let base = match base_for(dirfd, &name, cwd) {
                Ok(base) => base,
                Err(e) => return e,
            };
            let resolved = if flags & AT_SYMLINK_NOFOLLOW != 0 {
                crate::vfs::resolve_no_follow_at(&name, base)
            } else {
                crate::vfs::resolve_opt(&name, Some(base))
            };
            match resolved {
                Ok(inode) => inode,
                Err(e) => return err_of(e),
            }
        }
        _ => match crate::sched::current_fds().clone_at(dirfd) {
            Ok(file) => file.inode(),
            Err(e) => return err_of(e),
        },
    };

    // The two clocks. A null `times` means "now, both"; otherwise read the
    // pair, refusing an unreadable pointer and an out-of-range nanosecond. The
    // *shape* of the request is read off the same words, because the permission
    // question turns on it ([`TimesShape`]): the pointer is what separates
    // "now" from "this value", and a pair of `UTIME_OMIT`s is neither.
    let (atime, mtime, shape) = if times_ptr == 0 {
        let now = wall_now();
        (now, now, TimesShape::BothToNow)
    } else {
        if !buffer_ok(times_ptr, 32, false, true) {
            return err::EFAULT;
        }
        let pair = unsafe { &*(times_ptr as *const [Utimespec; 2]) };
        let nothing = pair[0].nsec == UTIME_OMIT && pair[1].nsec == UTIME_OMIT;
        let now = wall_now();
        match (
            resolve_time(&pair[0], inode.atime(), now),
            resolve_time(&pair[1], inode.mtime(), now),
        ) {
            (Some(a), Some(m)) => (
                a,
                m,
                if nothing {
                    TimesShape::Nothing
                } else {
                    TimesShape::Setting
                },
            ),
            _ => return err::EINVAL,
        }
    };

    store_times(&inode, atime, mtime, shape)
}

/// `chdir(path)`.
///
/// Relative components are normalized against the task's cwd *path* and then
/// resolved absolutely: `..` above the cwd needs the way the cwd was named,
/// which the inode walk alone cannot answer (it has no parent edges).
pub fn chdir(args: [usize; ARG_COUNT]) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // An empty path names nothing. It has to be refused here, before the
    // normalization: joining `""` onto the cwd yields the cwd itself, so
    // `chdir("")` used to *succeed* on the directory it was already in. Linux
    // answers ENOENT for a path that names no file, and a tracer's startup
    // probe is written around that - it calls `chdir("")` and expects -ENOENT,
    // and the `0` this kernel handed back told it that syscall results could
    // not be trusted here at all.
    if path.is_empty() {
        return err::ENOENT;
    }
    let canonical = normalize_path(&path, &crate::sched::current_cwd_path());
    match crate::vfs::resolve(&canonical) {
        Ok(dir) if dir.metadata().kind == NodeKind::Dir => {
            // Entering a directory is the execute bit's job, the same as
            // walking through one: Linux refuses `chdir` without it, and a
            // kernel that allowed it would let a caller sit in a directory it
            // cannot then read a single name out of.
            if let Err(e) = crate::vfs::may_search(&dir) {
                return err_of(e);
            }
            adopt_cwd(dir, canonical);
            0
        }
        Ok(_) => err::ENOTDIR,
        Err(e) => err_of(e),
    }
}

/// `unlinkat(dirfd, path, flags)` - `AT_REMOVEDIR` picks which operation this
/// is: with the flag it is `rmdir`, without it `unlink`. The body below routes
/// the two separately; an earlier note here called the flag "accepted and
/// ignored", which was untrue the moment the rmdir/unlink split landed.
pub fn unlinkat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    // Linux rejects a flag bit that is not `AT_REMOVEDIR` with `EINVAL` before
    // it resolves anything (measured: `unlinkat(fd, path, 0x1000)` is `EINVAL`
    // and leaves the file in place). Routing only on the `AT_REMOVEDIR` bit
    // silently accepted every other flag and removed the name anyway - a
    // promise-shaped answer a caller could not detect, and the wrong *shape* of
    // one (it deletes where Linux declines). The check is first, the way Linux
    // words it.
    if args[2] & !AT_REMOVEDIR != 0 {
        return err::EINVAL;
    }
    // `AT_REMOVEDIR` is `unlinkat` spelling `rmdir`: the flag is not an
    // optimisation, it is how the caller says which of the two operations it
    // means, and getting it wrong either removes a directory that still holds
    // something or refuses a plain file.
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let base = match base_for(args[0] as i32, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    // The news wants the inode while it still has a name: resolve the
    // target and its parent before the removal (a failed lookup simply
    // means no news), remove, then tell the directory `IN_DELETE` and the
    // dying inode's watchers their loss.
    let news = match crate::vfs::split_parent(&path) {
        Ok((parent_path, name)) => {
            let child = crate::vfs::resolve_opt(&path, Some(base.clone())).ok();
            let parent = crate::vfs::resolve_opt(parent_path, Some(base.clone())).ok();
            child
                .zip(parent)
                .map(|(child, parent)| (parent, alloc::string::String::from(name), child))
        }
        Err(_) => None,
    };
    let outcome = if args[2] & AT_REMOVEDIR != 0 {
        crate::vfs::rmdir_opt(&path, Some(base))
    } else {
        crate::vfs::unlink_opt(&path, Some(base))
    };
    match outcome {
        Ok(()) => {
            if let Some((parent, name, child)) = news {
                // A socket's name going is also its binding's last day:
                // no connect may reach the listener by this path again
                // (the listener itself lives on by its descriptors, and a
                // still-open fd may even re-... - no: it cannot rebind;
                // only the *name* retires. `unix::unbind_node` is a
                // no-op for every other kind.
                //
                // Not machine-gated: this file is bare-metal only and `net` is
                // compiled wherever it is, so the question is whether the name's
                // kind is a socket, not which machine is running.
                if child.metadata().kind == crate::vfs::NodeKind::Socket {
                    crate::net::unix::unbind_node(&child);
                }
                // The victim's own kind decides `IN_ISDIR`, not the
                // directory's: the name that left is the event's subject.
                crate::vfs::inotify::emit_child(
                    &parent,
                    crate::vfs::inotify::IN_DELETE,
                    &name,
                    child.metadata().kind == crate::vfs::NodeKind::Dir,
                );
                crate::vfs::inotify::notify_deleted(&child);
            }
            0
        }
        Err(e) => err_of(e),
    }
}

/// `rmdir(path)` - remove an empty directory by name.
///
/// A separate call from `unlink` because a directory is a different thing to
/// take apart: it must be empty, and the kernel has to say so rather than
/// quietly taking what was underneath with it.
pub fn rmdir(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // Same news shape as `unlinkat`: the dying directory and its parent
    // are looked up while the name still exists, and reported after the
    // removal succeeds.
    let news = match crate::vfs::split_parent(&path) {
        Ok((parent_path, name)) => {
            let child = crate::vfs::resolve_opt(&path, Some(cwd.clone())).ok();
            let parent = crate::vfs::resolve_opt(parent_path, Some(cwd.clone())).ok();
            child
                .zip(parent)
                .map(|(child, parent)| (parent, alloc::string::String::from(name), child))
        }
        Err(_) => None,
    };
    match crate::vfs::rmdir_opt(&path, Some(cwd.clone())) {
        Ok(()) => {
            if let Some((parent, name, child)) = news {
                // The removed directory is the subject, so `IN_ISDIR` comes
                // from it - which for `rmdir` is always set, but the question
                // is asked of the child rather than assumed.
                crate::vfs::inotify::emit_child(
                    &parent,
                    crate::vfs::inotify::IN_DELETE,
                    &name,
                    child.metadata().kind == crate::vfs::NodeKind::Dir,
                );
                crate::vfs::inotify::notify_deleted(&child);
            }
            0
        }
        Err(e) => err_of(e),
    }
}

/// `mkdirat(dirfd, path, mode)` - mode is ignored.
pub fn mkdirat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let base = match base_for(args[0] as i32, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    // A name that is already taken is `EEXIST`, whatever kind of thing holds
    // it - and the names that are always taken are exactly the ones a
    // parent/name split cannot express: `/`, `.` and `..`. Checking here is
    // what makes `mkdir -p` work, and its absence was not a small thing:
    // `-p` creates a path by walking *up* it and stopping at the first
    // ancestor that is already there, so an `EINVAL` where an `EEXIST`
    // belonged sent it all the way to `mkdir("/")` and aborted the whole
    // command with "can't create directory '/'". The question is about the
    // *name*, so it is asked of the directory that holds it rather than by
    // walking it - a name a mount covers is still a taken name, and Linux
    // answers `EEXIST` for it just the same (measured,
    // `.workbuddy/probe_mountpoint_doors.c`); [`resolve_entry_at`] is that
    // question and [`resolve_no_follow_at`] is the other one.
    if crate::vfs::resolve_entry_at(&path, base.clone()).is_ok() {
        return err::EEXIST;
    }
    let (parent, name) = match crate::vfs::split_parent(&path) {
        Ok(p) => p,
        Err(e) => return err_of(e),
    };
    let parent = match crate::vfs::resolve_opt(parent, Some(base)) {
        Ok(d) => d,
        Err(e) => return err_of(e),
    };
    // A new name is a write to the directory and finding where it goes is a
    // search, so the parent's bits are asked before the directory exists.
    if let Err(e) = crate::vfs::may_create_in(&parent) {
        return err_of(e);
    }
    match parent.create(name, NodeKind::Dir) {
        Ok(created) => {
            created.set_mode(masked_mode(args[2] as u32));
            let is_dir = created.metadata().kind == crate::vfs::NodeKind::Dir;
            crate::vfs::inotify::emit_child(&parent, crate::vfs::inotify::IN_CREATE, name, is_dir);
            let _ = created;
            0
        }
        Err(e) => err_of(e),
    }
}

/// `renameat2(olddirfd, oldpath, newdirfd, newpath, flags)`.
///
/// `RENAME_NOREPLACE` refuses to take a name that is already taken; the
/// exchange and whiteout forms have no implementation underneath and answer
/// `EINVAL`, as Linux does for a flag combination it cannot serve.
pub fn renameat2(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    const RENAME_NOREPLACE: usize = 0x1;
    const RENAME_EXCHANGE: usize = 0x2;
    const RENAME_WHITEOUT: usize = 0x4;

    let flags = args[4];
    if flags & !(RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT) != 0
        || flags & (RENAME_EXCHANGE | RENAME_WHITEOUT) != 0
    {
        return err::EINVAL;
    }
    // Both names are read before either *descriptor* is looked at, which is this
    // call's shape rather than `linkat`'s: measured on a current host, a missing
    // old name beside a bad new dirfd is that descriptor's EBADF. The two
    // *pointers* come first of all, and that is the half that makes a name the
    // kernel cannot read EFAULT even with a bad dirfd on either side.
    let old_path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let new_path = match cstr_arg(args[3], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let old_base = match base_for(args[0] as i32, &old_path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let new_base = match base_for(args[2] as i32, &new_path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    // Renaming a name onto itself is a no-op - and it has to be said so
    // *before* the copy path runs, because that path unlinks the taken name
    // on the way to recreating it, and with one shared dentry that unlink
    // takes the file's only name with it.
    if alloc::sync::Arc::ptr_eq(&old_base, &new_base) && old_path == new_path {
        return 0;
    }
    // No atomic rename primitive underneath: the old name's body is copied
    // to the new one and the old name dropped. A crash in between leaves
    // both names, never none.
    //
    // The old name is looked up *without following it*. A rename moves the
    // name, whatever kind of node holds - and apk's commits rename symlinks
    // wholesale: a package's `.so` link is laid down under a temp name and
    // renamed to its real one, with the link's target typically still under
    // its own temp name at that moment. Resolving the old path would follow
    // the link and go looking for the target by its real name - ENOENT for
    // every `apk add` that ships a symlink - and where the target did exist,
    // the copy would drag its whole body over what should have stayed a
    // link, which is how the tree got corrupted the first time this was
    // driven by apk.
    let (old_dir, old_name) = match old_path.rsplit_once('/') {
        Some((parent, name)) => {
            let parent = if parent.is_empty() { "/" } else { parent };
            match crate::vfs::resolve_opt(parent, Some(old_base.clone())) {
                Ok(d) => (d, alloc::string::String::from(name)),
                Err(e) => return err_of(e),
            }
        }
        None => (old_base.clone(), old_path.clone()),
    };
    let old_inode = match old_dir.lookup(&old_name) {
        Ok(ino) => ino,
        Err(e) => return err_of(e),
    };

    // Where the new name goes. A path with a slash names its own parent; a
    // bare name has none, and the directory it is created in is the one the
    // caller pointed at - `newdirfd`, or the cwd when that is `AT_FDCWD`.
    // Treating a bare name as rooted at `/` is what put apk's fetched index in
    // the root directory: `renameat(dfd, "INDEX.part", dfd, "INDEX")` created
    // `/INDEX` instead of `/var/cache/apk/INDEX`, and the next `apk update`
    // could not find the index it had already downloaded.
    let (dir, name) = match new_path.rsplit_once('/') {
        Some((parent, name)) => {
            let parent = if parent.is_empty() { "/" } else { parent };
            match crate::vfs::resolve_opt(parent, Some(new_base.clone())) {
                Ok(d) => (d, alloc::string::String::from(name)),
                Err(e) => return err_of(e),
            }
        }
        None => (new_base.clone(), new_path.clone()),
    };
    // What already holds the new name, if anything - the entry this rename
    // would replace. Asked of the directory that holds the name rather than by
    // walking it, which is the same question every make-or-remove asks.
    let existing = dir.lookup(&name).ok();

    // Both names are resolved before either rule is asked, because the *shapes*
    // come first and they need both of them: renaming a file onto a directory
    // is `EISDIR` and a directory onto a file is `ENOTDIR`, and each of those
    // beats the mount refusal and the permission check. Measured on a current
    // host (`.workbuddy/probe_mountpoint_doors.c` §5): `rename(file, mounted
    // dir)` answers `EISDIR` where `rename(dir, mounted dir)` answers `EBUSY`,
    // and `rename(mounted dir, file)` answers `ENOTDIR` where
    // `rename(mounted dir, dir)` answers `EBUSY` - so the type is asked first
    // and the mount point second.
    if let Some(target) = &existing {
        let old_is_dir = old_inode.metadata().kind == NodeKind::Dir;
        let new_is_dir = target.metadata().kind == NodeKind::Dir;
        if !old_is_dir && new_is_dir {
            return err::EISDIR;
        }
        if old_is_dir && !new_is_dir {
            return err::ENOTDIR;
        }
    }

    // Renaming takes the old name out of its directory, so the rule that
    // governs `unlink` governs this too: without it, a rename is a way around
    // the permission check - and a rename is what `mv` is. It is also where the
    // old end's mount point is refused (`may_remove` asks the same question an
    // `unlink` does, for the same reason).
    if let Err(e) = crate::vfs::may_remove(&old_dir, &old_inode) {
        return err_of(e);
    }

    // The new name is an entry being made in `dir`, so the directory's bits
    // decide - the same rule a create follows. Without it a rename is a way to
    // put a name somewhere the caller could not have created one.
    if let Err(e) = crate::vfs::may_create_in(&dir) {
        return err_of(e);
    }
    // And the new end's mount point, refused for the reason Linux refuses it:
    // the filesystem mounted on that name is not this call's to lift. It is
    // asked *after* the two rules above, which is Linux's order and the one an
    // unprivileged caller can see - `rename(mounted, x)` in a directory it may
    // not write is `EACCES` and not `EBUSY` (measured, same probe §9). Without
    // this the name went, the mounted filesystem was orphaned, and the table
    // kept an entry whose mount point no longer had a name.
    if let Some(target) = &existing
        && crate::vfs::is_mount_point(target)
    {
        return err::EBUSY;
    }

    // What `create` owes the caller when the name is taken: the name is
    // replaced, whatever kind of node holds - unless NOREPLACE protected it.
    // Replacing by unlink-and-create is what keeps a link from being written
    // in place of the link it points at.
    macro_rules! claim_name {
        ($make:expr) => {{
            match $make {
                Ok(inode) => inode,
                Err(crate::vfs::VfsError::Exists) => {
                    if flags & RENAME_NOREPLACE != 0 {
                        return err::EEXIST;
                    }
                    match dir.unlink(&name) {
                        Ok(()) => {}
                        Err(e) => return err_of(e),
                    }
                    match $make {
                        Ok(inode) => inode,
                        Err(e) => return err_of(e),
                    }
                }
                Err(e) => return err_of(e),
            }
        }};
    }

    match old_inode.metadata().kind {
        // The old name is a link: the new name becomes the same link. Its
        // target text travels verbatim, relative targets and all.
        NodeKind::Symlink => {
            let target = match old_inode.readlink() {
                Ok(t) => t,
                Err(e) => return err_of(e),
            };
            let link = claim_name!(dir.symlink(&name, &target));
            // The same rule a file's copy follows: the name moves and the node
            // keeps its owner. A link's mode is consulted by nothing, so only
            // the owner has to be carried across.
            let (uid, gid) = old_inode.owner();
            link.set_owner(uid, gid);
        }
        // A regular file: copy the bytes, then drop the old name.
        NodeKind::File => {
            let inode = claim_name!(dir.create(&name, NodeKind::File));
            // The copy is the kernel moving bytes between two names of the
            // same file, not a write anyone made: `write_at` stands outside
            // the `File::write` IN_MODIFY chokepoint on purpose. Copy in
            // bounded chunks - there is no atomic rename under this
            // filesystem, so a rename is a copy, but buffering the whole body
            // would OOM the kernel on a large file, and `apk` renames every
            // package member into place (a 128 MiB `Vec` from a big one is
            // what took the machine down).
            let mut chunk = alloc::vec![0u8; 65536];
            let mut off: u64 = 0;
            loop {
                let n = match old_inode.read_at(off, &mut chunk) {
                    Ok(n) => n,
                    Err(e) => return err_of(e),
                };
                if n == 0 {
                    break;
                }
                if inode.write_at(off, &chunk[..n]).is_err() {
                    return err::ENOSPC;
                }
                off += n as u64;
            }
            // A rename moves the *name*; the node keeps everything that was its
            // own. There is no atomic rename underneath this filesystem, so the
            // body lands in a freshly created inode, and that inode has to be
            // handed back the mode and the owner the old one carried. Without
            // this, every rename of a regular file resets it to the
            // filesystem's default - 0644, and owned by whoever did the
            // renaming - which is exactly how a package install turns every
            // extracted binary into a plain readable file: `apk` writes each
            // member through `O_TMPFILE`, gives it a temporary name, and
            // renames it into place, so surviving the rename is the whole of
            // how a mode, a setuid bit or an owner reaches its final name.
            if old_inode.keeps_modes() {
                inode.set_mode(old_inode.mode());
            }
            let (uid, gid) = old_inode.owner();
            inode.set_owner(uid, gid);
        }
        // A directory rename would have to move the whole subtree's parent
        // back-pointer; nothing here needs it yet, and pretending otherwise
        // would half-move it. Say so.
        _ => return err::EISDIR,
    }

    let _ = old_dir.unlink(&old_name);
    // The move's news: `IN_MOVED_FROM` on the old directory and
    // `IN_MOVED_TO` on the new share one cookie so a reader joins the
    // pair, and the inode that travelled says `IN_MOVE_SELF` to its own
    // watchers. The old name is gone and the new one stands for a fresh
    // inode (this kernel's rename copies), so the moved report names both
    // sides as it should.
    crate::vfs::inotify::emit_move(
        &old_dir,
        &old_name,
        &dir,
        &name,
        &inode_for_news(&old_inode, &dir, &name),
    );
    crate::vfs::inotify::notify_deleted(&old_inode);
    0
}

/// The inode a finished rename stands for: the body copy's fresh node for
/// a regular file (looked up again under the new name), the old node's
/// own identity for the kinds renamed in place. The identity is what the
/// `IN_MOVE_SELF` watcher compares, and reporting the old number keeps
/// that event tied to the watch that asked.
fn inode_for_news(old_inode: &Arc<dyn Inode>, dir: &Arc<dyn Inode>, name: &str) -> Arc<dyn Inode> {
    if old_inode.metadata().kind == crate::vfs::NodeKind::File {
        dir.lookup(name).unwrap_or_else(|_| old_inode.clone())
    } else {
        old_inode.clone()
    }
}

/// `readlink(path, buf, size)` - what a symlink points at, and on mpos that
/// is exactly one name: `/proc/self/exe`, the image the process came from.
pub fn readlink(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let (buf, size) = (args[1], args[2]);
    if size == 0 {
        return err::EINVAL;
    }
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    readlink_named(&path, buf, size, cwd)
}

/// The tail both `readlink` and `readlinkat` end at: the name is already read,
/// and `readlinkat` has already answered the two questions its descriptor and
/// its name can be asked on their own.
///
/// The buffer is checked here rather than in either door, which is also where
/// Linux checks it: `do_readlinkat` refuses a non-positive `bufsiz` (the
/// `EINVAL` above) before it looks at anything else, and the buffer itself is
/// only touched once a link has been found. Both spellings answer `EFAULT` for
/// an unreadable buffer, so the move is invisible to a caller - but the name's
/// and the descriptor's answers come first, which is the order `base_for`'s
/// comment is about.
fn readlink_named(path: &str, buf: usize, size: usize, base: &Arc<dyn Inode>) -> isize {
    if !buffer_ok(buf, size, true, true) {
        return err::EFAULT;
    }
    let target: String = if path == "/proc/self/exe" {
        crate::sched::current_exe()
    } else if let Some(rest) = path.strip_prefix("/proc/self/fd/") {
        // The descriptor's own path: what libc's `ttyname` reads to name the
        // tty a login session is on. The fd must be one of the caller's, and
        // the answer is the name it was opened by - or the anonymous spelling
        // (`pipe:[ino]`) for a descriptor that has no path anywhere.
        let fd: i32 = match rest.parse() {
            Ok(n) => n,
            Err(_) => return err::ENOENT,
        };
        match crate::sched::current_fds().clone_at(fd) {
            Ok(file) => file.display_name(),
            Err(e) => return err_of(e),
        }
    } else {
        // The link, not what it points at: a resolver that followed it would
        // answer EINVAL for every link and the target for nothing.
        match crate::vfs::resolve_no_follow_at(path, base.clone()) {
            Ok(node) => match node.readlink() {
                Ok(target) => target,
                Err(e) => return err_of(e),
            },
            Err(e) => return err_of(e),
        }
    };
    let n = target.len().min(size);
    unsafe {
        core::ptr::copy_nonoverlapping(target.as_ptr(), buf as *mut u8, n);
    }
    n as isize
}

/// `readlinkat(dirfd, path, buf, bufsiz)` - the dirfd form of `readlink`.
///
/// The arguments shift by one: the dirfd sits in `args[0]` and the path in
/// `args[1]`, so the plain `readlink` layout would read the dirfd number as
/// the path and answer ENOENT for every caller.
///
/// `bufsiz` and the name are read here rather than in `readlink_named` so that
/// the descriptor is the *third* question asked, which is where Linux asks it:
/// an empty name with a dirfd nothing is open on is this call's one member whose
/// answer is `EBADF` rather than `ENOENT`, because `do_readlinkat` has no
/// empty-name check to place before the walk.
///
/// That is also why the empty name is answered here instead of in `base_for`,
/// which refuses it for every other door: Linux consults *this* door's
/// descriptor first (`EBADF` when it is not open) and then refuses the name, so
/// the answer is `ENOENT` once the descriptor is open *whatever* it is - a
/// regular file included, where `base_for`'s kind check would answer `ENOTDIR`
/// and Linux does not (measured; `.workbuddy/probe_at_dirfd.c`). Each of the
/// three shapes is pinned in `t-at-empty-name`.
pub fn readlinkat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let (buf, size) = (args[2], args[3]);
    if size == 0 {
        return err::EINVAL;
    }
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // The name first, then this door's descriptor - only *whether* it is open,
    // not what it names - and then the empty name's own answer.
    if path.is_empty() {
        if args[0] as i32 != abi::AT_FDCWD {
            if let Err(e) = crate::sched::current_fds().clone_at(args[0] as i32) {
                return err_of(e);
            }
        }
        return err::ENOENT;
    }
    let base = match base_for(args[0] as i32, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    readlink_named(&path, buf, size, &base)
}

/// `symlink(target, linkpath)` — name a link that stands for `target`.
///
/// The target is stored, not checked: a link may point at a path that does not
/// exist yet, and POSIX says making one is still allowed. What is an error is
/// the name already being taken, or the filesystem refusing to hold a link.
pub fn symlink(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let target = match cstr_arg(args[0], true) {
        Ok(target) => target,
        Err(e) => return e,
    };
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    symlink_named(&target, &path, cwd)
}

/// The tail `symlink` and `symlinkat` share: the target and the link's name are
/// both already read, and the caller has already asked which directory the name
/// lands in. Nothing below depends on which door it came through.
fn symlink_named(target: &str, path: &str, base: &Arc<dyn Inode>) -> isize {
    // The parent, not the name: the name is what this call is about to make,
    // so it must *not* be there, and asking the tree for it would only
    // discover that. Splitting it off is also what keeps a link from being
    // written in place of the link it points at.
    let (parent_path, name) = match crate::vfs::split_parent(path) {
        Ok(p) => p,
        Err(e) => return err_of(e),
    };
    let parent = match crate::vfs::resolve_opt(parent_path, Some(base.clone())) {
        Ok(parent) => parent,
        Err(e) => return err_of(e),
    };
    // A link is a new name in this directory, so the directory's bits decide
    // whether it may be made - the same rule a create follows.
    if let Err(e) = crate::vfs::may_create_in(&parent) {
        return err_of(e);
    }
    match parent.symlink(name, target) {
        Ok(_) => {
            // The new name is a symlink, so its create event is never an
            // `IN_ISDIR` one.
            crate::vfs::inotify::emit_child(&parent, crate::vfs::inotify::IN_CREATE, name, false);
            0
        }
        Err(e) => err_of(e),
    }
}

/// `symlinkat(target, newdirfd, linkpath)` - the dirfd form of `symlink`.
///
/// The dirfd names where the link lands: `AT_FDCWD` means the caller's
/// directory, anything else is a directory descriptor apk keeps open across
/// the whole extraction of a package.
///
/// Both strings are read before the descriptor is looked at - the *target*
/// first, which is Linux's order for this call (`getname(oldname)` before
/// `getname(newname)`) and the reason an unreadable target pointer is `EFAULT`
/// rather than a bad dirfd's `EBADF`.
pub fn symlinkat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let target = match cstr_arg(args[0], true) {
        Ok(target) => target,
        Err(e) => return e,
    };
    let path = match cstr_arg(args[2], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let base = match base_for(args[1] as i32, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    symlink_named(&target, &path, &base)
}

/// `mount(source, target, fstype, flags, data)`.
///
/// The filesystem types are the ones this kernel has. A block-backed type
/// resolves its source through the tree - `/dev/vda3` is a node, and the node
/// knows which storage it is - which is the mechanism that lets userland
/// decide what gets mounted where, instead of the kernel's boot code deciding
/// for it.
///
/// Not implemented, and said so rather than silently accepted: read-only
/// mounts. `MS_RDONLY` names a property of the filesystem underneath, and
/// this kernel's writers have no read-only mode to consult; a caller asking
/// for one gets `EOPNOTSUPP` rather than a writable mount that claims to be
/// read-only.
pub fn handle_mount(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    const MS_RDONLY: usize = 0x1;

    let source = match cstr_arg(args[0], true) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let target = match cstr_arg(args[1], true) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let fstype = cstr_arg(args[2], true).unwrap_or_default();

    // Read-only is honoured, not recorded and ignored: the filesystem is told,
    // and refuses writes from then on. This is what an initramfs relies on
    // while it checks a root it has not mounted writable yet.
    let read_only = args[3] & MS_RDONLY != 0;

    // `mount -o remount,...`: no new filesystem, just new flags on one that is
    // already mounted. `rw` is spelled by the *absence* of MS_RDONLY, which is
    // why this is a replacement rather than a change.
    const MS_REMOUNT: usize = 0x20;
    if args[3] & MS_REMOUNT != 0 {
        return match crate::vfs::mount::remount(&target, Some(cwd.clone()), read_only) {
            Ok(()) => 0,
            Err(e) => err_of(e),
        };
    }

    // `mount --bind`: the source is a tree already mounted somewhere, and the
    // target gets it as a second name. Checked before the type dispatch, as
    // Linux checks it: busybox spells the fstype `none` for a bind, and a
    // bind never builds a filesystem - the type word is not consulted at all.
    const MS_BIND: usize = 0x1000;
    const MS_REC: usize = 0x4000;
    if args[3] & MS_BIND != 0 {
        // MS_REC promises the subtree's *other* mounts come along too; this
        // kernel's bind aliases one directory tree, so answering success to a
        // recursive bind would silently mount less than was asked. Refused
        // rather than pretended - nothing in this image's boot uses it.
        if args[3] & MS_REC != 0 {
            return err::EOPNOTSUPP;
        }
        return match crate::vfs::mount::bind_mount(&source, &target, Some(cwd.clone()), read_only) {
            Ok(()) => 0,
            Err(e) => err_of(e),
        };
    }

    // `mount(2)`'s MS_MOVE: the filesystem mounted at the source moves to the
    // target, and a target of `/` is what `switch_root` asks for - the new
    // root becomes the filesystem, and the initramfs it came from goes away.
    // No new filesystem is built for a move, so this comes before the type
    // dispatch: `switch_root` passes no type at all.
    const MS_MOVE: usize = 0x2000;
    if args[3] & MS_MOVE != 0 {
        let src = match crate::vfs::resolve_opt(&source, Some(cwd.clone())) {
            Ok(s) => s,
            Err(e) => return err_of(e),
        };
        // A move onto `/` is not a re-homing - nothing can be "the directory
        // under /" when / is the tree the move puts there. That spelling is
        // what `switch_root` uses to make the tree it stands in the root: the
        // mount carrying it is promoted, the table it leaves behind is
        // dropped, and the caller's cwd follows, because the names it was
        // using resolved through a tree that is no longer mounted anywhere.
        if target.trim_end_matches('/').is_empty() {
            return match crate::vfs::mount::move_root(&src) {
                Ok(()) => {
                    if let Ok(root) = crate::vfs::resolve("/") {
                        crate::sched::set_cwd(root);
                        crate::sched::set_cwd_path(alloc::string::String::from("/"));
                    }
                    0
                }
                Err(e) => err_of(e),
            };
        }
        // The ordinary move: one mount, re-homed, everything else untouched.
        let at = match crate::vfs::resolve_opt(&target, Some(cwd.clone())) {
            Ok(at) => at,
            Err(e) => return err_of(e),
        };
        match crate::vfs::mount::move_mount(&src, &at) {
            Ok(()) => 0,
            Err(e) => err_of(e),
        }
    } else {
        mount_new(&source, &target, &fstype, read_only, cwd)
    }
}

/// The mount half of `mount(2)`: build the filesystem the type names and
/// hang it on the target.
fn mount_new(
    source: &str,
    target: &str,
    fstype: &str,
    read_only: bool,
    cwd: &Arc<dyn Inode>,
) -> isize {
    // MS_NOSUID, MS_NODEV, MS_NOEXEC and MS_NOATIME are accepted and mean
    // nothing here: there is no setuid bit to honour, no device to refuse and
    // no attribute to leave unstamped. Accepting a flag that has no meaning is
    // not the same as lying about one that does.

    // The mountpoint must already be there and must be a directory, as Linux
    // requires: mounting does not invent directories.
    let at = match crate::vfs::resolve_opt(target, Some(cwd.clone())) {
        Ok(at) => at,
        Err(e) => return err_of(e),
    };
    if at.metadata().kind != NodeKind::Dir {
        return err::ENOTDIR;
    }

    // The device the source named, when it named one: this is the number
    // `stat` will report as `st_dev` for everything under the new mount, and
    // the reason it is decided *here* rather than in the VFS is that this is
    // where the source stopped being a string and became a device - Linux's
    // own `lookup_bdev`, in the same place. A type whose source is a name
    // rather than a path ("proc", "tmpfs", a 9p tag) leaves it `None` and gets
    // an anonymous number, which is what Linux gives a pseudo-filesystem.
    let mut source_dev: Option<u64> = None;

    let fs: alloc::sync::Arc<dyn crate::vfs::FileSystem> = match fstype {
        // The block types. The source names a device node; the node knows
        // which storage it stands for.
        "ext2" | "ext3" | "ext4" => {
            if source.is_empty() {
                return err::ENOENT;
            }
            let dev_node = match crate::vfs::resolve_opt(source, Some(cwd.clone())) {
                Ok(node) => node,
                Err(e) => return err_of(e),
            };
            let Some(dev) = dev_node.as_block() else {
                return err::ENOTBLK;
            };
            // A block node's own `st_rdev`, which is its identity in the
            // numbering `/sys/class/block`, `/dev` and a `add` event all agree
            // on. Taking it from the node rather than re-deriving it from the
            // source string means a caller that reached the same device by
            // another name - a bind, a different `/dev` entry - still mounts
            // it under the device's own number.
            source_dev = dev_node
                .rdev()
                .map(|(major, minor)| crate::uevent::makedev(major, minor));
            match crate::vfs::ext4::Ext4Fs::mount(dev) {
                Ok(fs) => alloc::sync::Arc::new(fs) as alloc::sync::Arc<dyn crate::vfs::FileSystem>,
                Err(_) => return err::EINVAL,
            }
        }
        // The kernel's own trees, by name. Both machines have these now: a real
        // init mounts `/proc` and `/sys` on either one (Alpine's openrc does it
        // on every boot), and the filesystems themselves build for both, with
        // their machine-specific leaves behind capability flags. The gate that
        // used to be here named the architecture for a *deferral*, and the
        // deferral is over - an arm that answered `ENODEV` for a filesystem the
        // kernel has is the "wrong shape" answer these are supposed to avoid.
        "proc" => crate::vfs::procfs::ProcFs::new(),
        "sysfs" => crate::vfs::sysfs::SysFs::new(),
        // A host share, served over 9p: the *source* is the mount tag the
        // host exported the share under - `mount -t 9p hostshare /mnt`
        // opens the device QEMU was told `-virtfs ...,mount_tag=hostshare`
        // for. An empty source takes the only share the bus has, which is
        // the same call the boot's own mount makes. Both machines have the
        // device now (`NOTES.md` §13.29), so the arm is the filesystem's
        // rather than one machine's.
        "9p" => match crate::vfs::p9::mount(source) {
            Ok(fs) => fs,
            Err(e) => return err_of(e),
        },
        "devtmpfs" | "devfs" => crate::drivers::devices::standard_devfs(),
        "devpts" => crate::vfs::devpts::DevPts::new(),
        "tmpfs" | "ramfs" => {
            let fs = crate::vfs::ramfs::RamFs::new();
            // A tmpfs root starts out `S_IRWXUGO | S_ISVTX` - 01777 - and that
            // default is the whole of what `/tmp` is: writable by everyone,
            // with each name removable only by its own owner. It is not
            // decoration. A root that kept no mode at all is one whose
            // permissions are never checked, sticky bit included, so without
            // this the rule the bit exists for simply does not run - which is
            // how any user could delete any other user's files out of `/tmp`.
            if fstype == "tmpfs" {
                fs.root().set_mode(0o1777);
            }
            fs
        }
        // A type this kernel does not speak. ENODEV is what Linux answers, and
        // it is the answer a caller walks a list of types on.
        _ => return err::ENODEV,
    };

    match crate::vfs::mount::mount_at_opt(
        target,
        Some(cwd.clone()),
        fs,
        fstype,
        source,
        read_only,
        source_dev,
    ) {
        Ok(()) => 0,
        Err(e) => err_of(e),
    }
}

/// `umount2(target, flags)`.
///
/// `MNT_FORCE` and `MNT_DETACH` both land on the same immediate detach, which
/// is what this kernel's unmount is: there is no in-flight I/O to force and no
/// lazy expiry to schedule, so the flags name distinctions that do not exist
/// here and are accepted as no-ops.
pub fn handle_umount2(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    const MNT_FORCE: usize = 0x1;
    const MNT_DETACH: usize = 0x2;
    let _ = (MNT_FORCE, MNT_DETACH);

    let target = match cstr_arg(args[0], true) {
        Ok(t) => t,
        Err(e) => return e,
    };
    match crate::vfs::mount::unmount_opt(&target, Some(cwd.clone())) {
        Ok(()) => 0,
        Err(e) => err_of(e),
    }
}

/// `pivot_root(new_root, put_old)` - replace the root, parking the old one.
///
/// This is the call an initramfs exists to make: it mounts the real root
/// somewhere, moves the root to it, and from then on the machine lives on the
/// filesystem it was built to run. The old root is parked at `put_old`, which
/// is what lets the caller detach it and give its memory back.
pub fn handle_pivot_root(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let _ = cwd;
    let new_root = match cstr_arg(args[0], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let put_old = match cstr_arg(args[1], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::vfs::pivot_root(&new_root, &put_old) {
        Ok(()) => {
            // The caller's cwd named a directory in the tree that just stopped
            // being the root. Everything it reaches for from here on - `exec`,
            // relative paths, `getcwd` - has to answer in the new one.
            if let Ok(root) = crate::vfs::resolve("/") {
                crate::sched::set_cwd(root);
                crate::sched::set_cwd_path(alloc::string::String::from("/"));
            }
            0
        }
        Err(e) => err_of(e),
    }
}

/// `eventfd(initval, flags)` - a counter a process can wait on.
///
/// One thread writes a number to it, another's `poll(2)` wakes: that is the
/// entire contract, and it is what a program with a worker uses instead of a
/// pipe.
pub fn handle_eventfd(args: [usize; ARG_COUNT]) -> isize {
    let flags = args[1];
    // `eventfd2`'s three flags are `EFD_SEMAPHORE`, `EFD_CLOEXEC` (`== O_CLOEXEC`)
    // and `EFD_NONBLOCK` (`== O_NONBLOCK`). Any other bit is `EINVAL`, refused
    // before anything else - the way Linux checks the flag word first. Silently
    // accepting an unknown bit and then ignoring it would tell the caller it got
    // the descriptor it asked for when it did not (the same §4.2 shape `signalfd`
    // had before its flag word was validated).
    if flags & !(EFD_SEMAPHORE | EFD_NONBLOCK | O_CLOEXEC) != 0 {
        return err::EINVAL;
    }
    if flags & EFD_SEMAPHORE != 0 {
        // The counter is taken whole here; a caller that asked for one-per-read
        // asked for something else, and saying so beats doing something close.
        return err::EOPNOTSUPP;
    }
    let nonblocking = flags & EFD_NONBLOCK != 0;
    let fd = match crate::vfs::open_inode(crate::vfs::eventfd::EventFd::new(
        next_anon_ino(),
        args[0] as u32 as u64,
        nonblocking,
    )) {
        Ok(fd) => fd,
        Err(e) => return err_of(e),
    };
    if flags & O_CLOEXEC != 0 {
        // `EFD_CLOEXEC` is not cosmetic: without it the descriptor leaks across
        // `exec`, the same leak `SFD_CLOEXEC` closed.
        let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
    }
    if nonblocking && let Ok(file) = crate::sched::current_fds().clone_at(fd) {
        file.set_status(crate::vfs::file::O_NONBLOCK);
    }
    fd as isize
}

/// `memfd_create(name, flags)` - an anonymous shared buffer, as a descriptor.
///
/// The buffer is a `Memfd` inode that owns its pages, so a `wl_shm` pool can
/// be built here, mapped, drawn into, and handed to a compositor over
/// `SCM_RIGHTS` with the mapping still shared. The name is cosmetic (it only
/// ever reaches `/proc`); a slash in it is `EINVAL`, as Linux's `mfd_validate_name`
/// answers, and `MFD_CLOEXEC` marks the descriptor close-on-exec like
/// `open(O_CLOEXEC)`.
#[cfg(target_os = "none")]
pub fn handle_memfd_create(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const NAME_MAX: usize = 255;
    // The `MFD_*` vocabulary, measured on a current host: `MFD_CLOEXEC`/
    // `MFD_ALLOW_SEALING` are honoured here, `MFD_HUGETLB` and its size bits
    // (`MFD_HUGE_MASK << MFD_HUGE_SHIFT`) are real Linux flags, and a bit
    // outside that set - or a size bit without `MFD_HUGETLB` beside it - is
    // `EINVAL`. This kernel has no hugepage backing, so `MFD_HUGETLB` is
    // refused with `EOPNOTSUPP` rather than quietly returning an ordinary memfd
    // the caller believes is huge-backed: a success that promises pages this
    // machine cannot hand over is exactly the §4.2 lie the flag audit removes.
    const MFD_HUGETLB: usize = 0o004;
    const MFD_HUGE_SHIFT: usize = 26;
    const MFD_HUGE_MASK: usize = 0x3f;
    const MFD_HUGE_BITS: usize = MFD_HUGE_MASK << MFD_HUGE_SHIFT;
    const MFD_KNOWN: usize = crate::vfs::memfd::MFD_CLOEXEC
        | crate::vfs::memfd::MFD_ALLOW_SEALING
        | MFD_HUGETLB
        | MFD_HUGE_BITS;
    let flags = args[1];
    if flags & !MFD_KNOWN != 0 || (flags & MFD_HUGE_BITS != 0 && flags & MFD_HUGETLB == 0) {
        return err::EINVAL;
    }
    if flags & MFD_HUGETLB != 0 {
        return err::EOPNOTSUPP;
    }
    let name = match crate::syscall::cstr_arg(args[0], from_user) {
        Ok(name) => name,
        Err(e) => return e,
    };
    if name.len() >= NAME_MAX || name.as_bytes().contains(&b'/') {
        return err::EINVAL;
    }
    let fd = match crate::vfs::open_inode(crate::vfs::memfd::Memfd::new(next_anon_ino(), flags)) {
        Ok(fd) => fd,
        Err(e) => return err_of(e),
    };
    // The name is not only validated, it is kept - as Linux keeps it, in the
    // one spelling both of the readers of it expect: `readlink
    // /proc/self/fd/<n>` and the last column of `/proc/<pid>/maps` each answer
    // `/memfd:<name> (deleted)` on Linux, the `(deleted)` because a memfd has
    // no name in any directory to be found under. Measured on Linux 7.2.7,
    // including the empty name (`memfd_create("", 0)` reports `/memfd:
    // (deleted)`); this used to discard the name, which left a `wl_shm` pool's
    // mapping with a blank column where a compositor's log would say which
    // buffer it was.
    if let Ok(file) = crate::sched::current_fds().clone_at(fd) {
        file.set_label(alloc::format!("/memfd:{name} (deleted)"));
    }
    if flags & crate::vfs::memfd::MFD_CLOEXEC != 0 {
        let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
    }
    fd as isize
}

/// `signalfd(fd, mask, flags)` - signals as a descriptor.
///
/// The mask is read, converted from the wire's `sigset_t` (signal *n* in bit
/// *n-1*) into the kernel's own spelling, and kept: the descriptor reports the
/// signals that are both blocked by the reading task and named there, which is
/// `signalfd(2)`'s contract and the whole reason a compositor's `SIGTERM` can
/// be noticed at all.
///
/// `flags` sits in the fourth slot of the `signalfd4` ABI musl makes; the
/// plain number routes a zero here, because the original spelling has no
/// flags at all - reading `args[2]` for it would take the mask *size* for a
/// flag word and drop the caller's nonblock silently.
pub fn handle_signalfd(args: [usize; ARG_COUNT], flags: usize, from_user: bool) -> isize {
    const SFD_NONBLOCK: usize = 0x800;
    const SFD_CLOEXEC: usize = 0x80000;
    if flags & !(SFD_NONBLOCK | SFD_CLOEXEC) != 0 {
        // `signalfd4` has only these two bits. A word with anything else set is
        // refused rather than silently accepted - the same shape `timerfd` and
        // the other `*_create(2)` calls give, and the honest answer for a caller
        // that asked for a flag this descriptor has no meaning for.
        return err::EINVAL;
    }
    if !crate::syscall::buffer_ok(args[1], 8, false, from_user) {
        return err::EFAULT;
    }
    let wire = unsafe { (args[1] as *const u64).read_unaligned() };
    let mask = crate::sched::signal::from_wire(wire);
    let nonblock = flags & SFD_NONBLOCK != 0;
    let fd = match crate::vfs::open_inode(crate::vfs::signalfd::SignalFd::new(
        next_anon_ino(),
        mask,
        nonblock,
    )) {
        Ok(fd) => fd,
        Err(e) => return err_of(e),
    };
    if flags & SFD_CLOEXEC != 0 {
        // `SFD_CLOEXEC` is not cosmetic: without it the descriptor leaks across
        // `exec`, which is exactly what a compositor that installs a signalfd
        // and then runs a helper would not want.
        let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
    }
    if nonblock && let Ok(file) = crate::sched::current_fds().clone_at(fd) {
        file.set_status(crate::vfs::file::O_NONBLOCK);
    }
    fd as isize
}

/// A number for a descriptor that has no name to be numbered by.
/// The next inode number for a node with no filesystem behind it: an eventfd, a
/// memfd, and the unnamed object a `MAP_SHARED|MAP_ANONYMOUS` mapping is. One
/// counter for all three, because the number's only job is to be distinct from
/// every other anonymous node's - `fstat` reports it and the DRM identity table
/// keys on it.
pub fn next_anon_ino() -> crate::vfs::Ino {
    use core::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// `chroot(path)` - make `path` the calling task's root, not the machine's.
///
/// Linux's `chroot` re-homes the process, not the namespace: the root it moves is
/// the caller's own view, and a `..` at that root stays there. `switch_root` uses
/// this and still works unchanged because it is pid 1 - every process that comes
/// after inherits the new root on `fork`, so the whole userspace tree sees it
/// without the machine root ever moving. The kernel keeps the caller's cwd inside
/// the new root as a convenience (Linux leaves `cwd` alone; a process with a stale
/// cwd outside the root can still walk back to it - `t-chroot` pins that this
/// implementation is the confined case, and a named divergence that it does not
/// mimic Linux's unconfined one).
pub fn handle_chroot(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    // `CAP_SYS_CHROOT` first, as Linux asks it - before the path is even
    // looked at (a root-less caller answers `EPERM` whatever the path names).
    if !crate::sched::current_credentials().has_effective(crate::cred::CAP_SYS_CHROOT) {
        return err::EPERM;
    }
    let path = match cstr_arg(args[0], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::vfs::mount::chroot_target(&path, cwd) {
        Ok(inode) => {
            crate::sched::set_root(inode.clone());
            crate::sched::set_cwd(inode);
            crate::sched::set_cwd_path(alloc::string::String::from("/"));
            0
        }
        Err(e) => err_of(e),
    }
}

/// `fcntl(fd, cmd, arg)` - descriptor flags are stored, the rest is a stub.
///
/// `F_GETFD`/`F_SETFD` round-trip through the fd table: startup code probes
/// them and reads back what it wrote. `F_GETFL` answers the description's open
/// word - the access mode it was made under, OR'd with the flags it carries -
/// and `F_SETFL` replaces only the bits Linux lets a caller change, so the
/// mode survives it. A command Linux has no meaning for answers `EINVAL`, not
/// success.
///
/// `F_GETLK`/`F_SETLK`/`F_SETLKW` are permissive stubs: this kernel has one
/// user and, in practice, one locker at a time, so every lock is granted and
/// every unlock is a no-op. `apk` and other tools that use `fcntl` record
/// locking need these to succeed rather than return `EINVAL`.
pub fn fcntl(args: [usize; ARG_COUNT]) -> isize {
    let (fd, cmd, arg) = (args[0] as i32, args[1], args[2]);
    match cmd {
        // `dup` with a floor rather than a free-for-all: the new descriptor is
        // the lowest free one at or above `arg`. A shell's redirection is built
        // on this - busybox moves stdout out of the way with
        // `fcntl(1, F_DUPFD, 10)` before pointing fd 1 at a file, so that the
        // descriptors it is about to reuse cannot collide with the saved copy.
        F_DUPFD => {
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(file) => file,
                Err(e) => return err_of(e),
            };
            let fds = crate::sched::current_fds();
            match fds.install_at_least(file, arg as i32) {
                Ok(new) => new as isize,
                Err(e) => err_of(e),
            }
        }
        // The CLOEXEC-marked twin of `F_DUPFD`: the same copy with the new
        // descriptor flagged for the exec, which is what a modern libc asks
        // for by default (`musl` reaches for it on every pipe-protected
        // spawn). EINVAL for a negative floor, the same rule F_DUPFD has.
        F_DUPFD_CLOEXEC => {
            if (arg as isize) < 0 {
                return err::EINVAL;
            }
            let file = match crate::sched::current_fds().clone_at(fd) {
                Ok(file) => file,
                Err(e) => return err_of(e),
            };
            let fds = crate::sched::current_fds();
            match fds.install_at_least(file, arg as i32) {
                Ok(new) => {
                    let _ = fds.fd_flags_set(new, FD_CLOEXEC);
                    new as isize
                }
                Err(e) => err_of(e),
            }
        }
        F_GETFD => match crate::sched::current_fds().fd_flags_get(fd) {
            Ok(flags) => flags as isize,
            Err(e) => err_of(e),
        },
        // POSIX stores the descriptor's flag word; Linux's `set_close_on_exec`
        // reads `arg & FD_CLOEXEC`, so a word with other bits in it is *masked*
        // to the one bit it names and succeeds. Measured on a current host
        // through the raw syscall: `F_SETFD(1, 0xFFFFFFFF)` answers 0 and
        // `F_GETFD` then reads 1, while `F_SETFD(1, 0xFFFFFFFE)` answers 0 and
        // clears it. This arm answered `EINVAL` for such a word until migration
        // batch 11, which is a shape a caller that builds the word from a
        // variable cannot recover from the way the ABI's answer lets it -
        // `t-fcntl` pins both halves now.
        F_SETFD => match crate::sched::current_fds().fd_flags_set(fd, (arg as u32) & FD_CLOEXEC) {
            Ok(()) => 0,
            Err(e) => err_of(e),
        },
        // `F_GETFL` reports the description's open word: the access mode it was
        // made under, OR'd with the status flags it carries. The flags are
        // where `O_NONBLOCK` lives, which a DNS resolver reads back to know how
        // its sockets will behave, and the mode is the half glibc's streams ask
        // for (`F_GETFL & O_ACCMODE`) to learn which way one is open. Measured
        // on a current host: an `O_WRONLY` descriptor answers `1`, a pipe's
        // write end answers `1` where its read end answers `0`, and a socket
        // answers `2` - the word belongs to the description, not to the file.
        F_GETFL => match crate::sched::current_fds().clone_at(fd) {
            Ok(file) => file.open_flags() as isize,
            Err(e) => err_of(e),
        },
        F_SETFL => match crate::sched::current_fds().clone_at(fd) {
            Ok(file) => {
                // Only the bits Linux lets a caller change are replaced - the
                // access mode is not one of them, which is why a caller that
                // asks for `O_NONBLOCK` alone does not lose it (`File::set_status`
                // carries the measurement).
                file.set_status(arg as u32);
                // The one stored flag with teeth is O_NONBLOCK, and on a pipe
                // the behaviour lives in the inode: sync the bit, so a
                // `fcntl(F_SETFL, O_NONBLOCK)` on a plain pipe() reads and
                // writes exactly as one born through `pipe2(O_NONBLOCK)`
                // would. An eventfd keeps its own copy for the same reason -
                // its reader blocks on an empty counter, and a caller that
                // flips the flag after the fact expects the flip to bite.
                sync_nonblocking(&file, arg as u32);
                0
            }
            Err(e) => err_of(e),
        },
        // POSIX record locking, and its open-file-description twin. The range
        // math, the conflict rule and the split-on-relock all live in
        // `vfs::locks`, host-tested; this resolves the caller's `struct flock`
        // against the descriptor's file and applies it. `F_SETLK` refuses a
        // blocked request with `EAGAIN`; `F_SETLKW` would block, and - since
        // there is no wake-on-unlock yet - it records the grant instead of
        // spinning, so a blocking caller never hangs. The `F_OFD_*` commands
        // are the same call with the open file description as the owner.
        F_GETLK | F_SETLK | F_SETLKW | F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW => {
            record_lock(fd, cmd, arg)
        }
        // memfd sealing: only a memfd has a seal state, and only under
        // `MFD_ALLOW_SEALING` can it be added to. The commands are the
        // `F_ADD_SEALS`/`F_GET_SEALS` a wl_shm pool uses to freeze its size.
        #[cfg(target_os = "none")]
        crate::vfs::memfd::F_ADD_SEALS => match crate::sched::current_fds().clone_at(fd) {
            Ok(file) => match file.inode().as_memfd() {
                Some(m) => match m.add_seals(arg as u32) {
                    Ok(()) => 0,
                    Err(e) => err_of(e),
                },
                None => err::EINVAL,
            },
            Err(e) => err_of(e),
        },
        #[cfg(target_os = "none")]
        crate::vfs::memfd::F_GET_SEALS => match crate::sched::current_fds().clone_at(fd) {
            Ok(file) => match file.inode().as_memfd() {
                Some(m) => m.seals() as isize,
                None => err::EINVAL,
            },
            Err(e) => err_of(e),
        },
        _ => err::EINVAL,
    }
}

/// The `struct flock` byte offsets on x86_64: `l_type` (i16) and `l_whence`
/// (i16) share the first word, then `l_start`, `l_len` (i64 each) and `l_pid`
/// (i32) follow at the ABI's padding. The struct is 32 bytes.
mod flock_off {
    pub const TYPE: usize = 0;
    pub const WHENCE: usize = 2;
    pub const START: usize = 8;
    pub const LEN: usize = 16;
    pub const PID: usize = 24;
    pub const BYTES: usize = 32;
}

/// Take or drop a lock in `class` over `[lo, hi)`, the one path `F_SETLK`,
/// `F_SETLKW` and `flock(2)` share. `block` false refuses a conflict with
/// `EAGAIN`; `block` true parks until the range frees, the caller acquires it,
/// or a signal interrupts with `EINTR` (`set_or_arm` takes the lock or registers
/// a waiter atomically, and `block_until_deadline` re-checks the condition under
/// the scheduler's own lock before parking, so a release is never missed). A
/// `kind` of `None` is an unlock.
///
/// The arguments are the lock request itself - device, inode, owner, pid,
/// class, kind and the byte range - which the two callers below already hold
/// as separate values, plus `block`, which is the one thing `F_SETLKW` adds
/// to `F_SETLK`, and `ofd`, which records whether an `F_OFD_*` request is what
/// is being taken.
#[allow(clippy::too_many_arguments)]
#[cfg(target_os = "none")]
fn lock_take(
    dev: u64,
    ino: u64,
    owner: u64,
    pid: u32,
    class: crate::vfs::locks::LockClass,
    kind: Option<crate::vfs::locks::LockKind>,
    lo: i64,
    hi: i64,
    block: bool,
    ofd: bool,
) -> isize {
    use crate::vfs::locks;
    if !block {
        return match locks::set(dev, ino, owner, pid, class, kind, lo, hi, ofd) {
            Ok(()) => 0,
            Err(_holder) => err::EAGAIN,
        };
    }
    let me = crate::sched::current().0;
    loop {
        if locks::set_or_arm(dev, ino, owner, pid, class, me, kind, lo, hi, ofd) {
            locks::disarm(me);
            return 0;
        }
        match crate::sched::block_until_deadline(
            || locks::is_free(dev, ino, owner, class, kind, lo, hi),
            u64::MAX,
        ) {
            crate::sched::WaitEnd::Condition => continue,
            crate::sched::WaitEnd::Interrupted => {
                locks::disarm(me);
                return err::EINTR;
            }
            crate::sched::WaitEnd::Deadline => unreachable!("no timeout"),
        }
    }
}

/// What `F_GETLK` reports as `l_pid` for a lock held by task `owner`: the
/// holder's number **where the caller stands**, or 0 when it has none there.
///
/// The rule every pid-crossing call in this kernel follows, and Linux's
/// `locks_translate_pid` asking `pid_nr_ns(holder, reader's namespace)` for
/// exactly it. All three cases measured on the host
/// (`.workbuddy/probe_getlk.c`; `docs/notes/` §6.3):
///
/// * a reader in the holder's namespace is handed the holder's number there - a
///   task inside a namespace reading the init's lock sees `1`, not the init's
///   machine slot;
/// * a reader *outside* it is handed the holder's number out there;
/// * a reader whose namespace has no number for the holder at all is handed
///   **0**, which is what `pid_nr_ns` answers for a task it cannot see - the
///   holder is outside the reader's namespace, so there is no number to give.
///
/// The lock table stores a task id as the owner, and this is the one place that
/// becomes a pid the caller can act on - which is what the field is for: a
/// program gets `l_pid` to know *which* process is in its way.
fn holder_pid_in(owner: u64) -> i32 {
    crate::sched::pid_number_of(crate::sched::TaskId(owner as usize)).map_or(0, |n| n as i32)
}

/// One `fcntl` locking command against `fd`: `F_GETLK` reports the first
/// conflicting lock, `F_SETLK`/`F_SETLKW` take or drop one, and the `F_OFD_*`
/// trio is the same with the open file description as the owner rather than the
/// thread group. `arg` is a user `struct flock *`; for the `SET` commands the
/// fields are read, and a conflict is the holder's pid (returned as `EAGAIN`).
#[cfg(target_os = "none")]
fn record_lock(fd: i32, cmd: usize, arg: usize) -> isize {
    use crate::vfs::locks::{self, F_RDLCK, F_WRLCK, LockClass};
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    if arg == 0
        || !crate::syscall::buffer_ok(
            arg,
            flock_off::BYTES,
            matches!(cmd, abi::F_GETLK | abi::F_OFD_GETLK),
            true,
        )
    {
        return err::EFAULT;
    }
    let read_i16 = |off: usize| unsafe { ((arg + off) as *const i16).read_unaligned() };
    let read_i64 = |off: usize| unsafe { ((arg + off) as *const i64).read_unaligned() };
    let l_type = read_i16(flock_off::TYPE);
    let l_whence = read_i16(flock_off::WHENCE);
    let l_start = read_i64(flock_off::START);
    let l_len = read_i64(flock_off::LEN);

    let meta = file.metadata();
    let inode = file.inode();
    let dev = crate::vfs::mount::fs_id_of_inode(&inode);
    let ino = meta.ino;
    let size = meta.size as i64;
    // A POSIX record lock is the *process's*: one thread group's second request
    // on a range it already holds replaces the first rather than deadlocking on
    // it, and `pid` (the same value here) is what `F_GETLK` reports as `l_pid`.
    // An `F_OFD_*` request is the third ownership model: the same record class
    // and conflict domain, but owned by the open file description like a
    // `flock`, so its `owner` is the description's identity and no pid can name
    // it - `F_OFD_GETLK` hands back `-1` for exactly that reason.
    let ofd = matches!(cmd, abi::F_OFD_GETLK | abi::F_OFD_SETLK | abi::F_OFD_SETLKW);
    let owner = if ofd {
        file.ofd()
    } else {
        crate::sched::current_tgid().0 as u64
    };
    let pid = crate::sched::current_tgid().0 as u32;

    let (lo, hi) = match locks::resolve_range(l_whence, l_start, l_len, size, file.pos() as i64) {
        Some(r) => r,
        None => return err::EINVAL,
    };

    match cmd {
        abi::F_GETLK | abi::F_OFD_GETLK => {
            // Ask as the requested kind; a lock the caller would take is
            // reported back with the holder's kind, range and pid, or cleared
            // to `F_UNLCK` when nothing is in the way.
            let kind = if l_type == F_WRLCK {
                locks::LockKind::Write
            } else {
                locks::LockKind::Read
            };
            let write_i16 =
                |off: usize, v: i16| unsafe { ((arg + off) as *mut i16).write_unaligned(v) };
            let write_i64 =
                |off: usize, v: i64| unsafe { ((arg + off) as *mut i64).write_unaligned(v) };
            match locks::query(dev, ino, owner, LockClass::Record, kind, lo, hi) {
                Some(held) => {
                    write_i16(
                        flock_off::TYPE,
                        match held.kind {
                            locks::LockKind::Write => F_WRLCK,
                            locks::LockKind::Read => F_RDLCK,
                        },
                    );
                    write_i64(flock_off::START, held.start);
                    write_i64(
                        flock_off::LEN,
                        if held.end == locks::INF {
                            0
                        } else {
                            held.end - held.start
                        },
                    );
                    // An OFD holder has no process to name, so Linux answers
                    // `-1` before it ever translates a pid - measured on the
                    // host (`.workbuddy/probe_ofd.c`, case B).
                    let reported = if held.ofd {
                        -1
                    } else {
                        holder_pid_in(held.owner)
                    };
                    unsafe { ((arg + flock_off::PID) as *mut i32).write_unaligned(reported) };
                }
                None => write_i16(flock_off::TYPE, 2), // F_UNLCK
            }
            0
        }
        F_SETLK | F_SETLKW | abi::F_OFD_SETLK | abi::F_OFD_SETLKW => {
            // An `F_UNLCK` drops the range, the others take it; `F_SETLK` is
            // the non-blocking spelling and `F_SETLKW` waits for the range. The
            // `OFD` pair is the same call with the description as owner.
            let kind = match l_type {
                F_RDLCK => Some(locks::LockKind::Read),
                F_WRLCK => Some(locks::LockKind::Write),
                2 => None, // F_UNLCK
                _ => return err::EINVAL,
            };
            lock_take(
                dev,
                ino,
                owner,
                pid,
                LockClass::Record,
                kind,
                lo,
                hi,
                matches!(cmd, abi::F_SETLKW | abi::F_OFD_SETLKW),
                ofd,
            )
        }
        _ => err::EINVAL,
    }
}

#[cfg(not(target_os = "none"))]
fn record_lock(_fd: i32, _cmd: usize, _arg: usize) -> isize {
    err::EINVAL
}

// The `struct pollfd` layout (its size, where `events` and `revents` sit)
// and the event bits live in `abi`, where the host tests pin the byte
// offsets against the x86_64 ABI.

/// Wait for any of `nfds` descriptors to be ready.
///
/// This is the call a real shell is built on: `ash` polls stdin with an
/// infinite timeout and reads what the poll says arrived. A `poll` that always
/// answers "nothing, and stop asking" is not a slow `poll`, it is a shell that
/// decides its terminal hung up and exits at the first prompt - which is
/// exactly what happened before this existed.
///
/// Readiness comes from the node itself, since only it knows: a pipe is
/// readable while it holds data, and the loss of its writer is the hang-up
/// beside that rather than a way of *being* readable - the distinction Linux's
/// `pipe_poll` draws, and the one a waiter that "ends on hang-up or nothing"
/// depends on. Waiting is done through the node's own wakeup list, so the
/// arrival that ends the wait is the same arrival that would have ended a
/// blocking read.
///
/// `poll` and `ppoll` are the same wait in different clocks, and the difference
/// is not cosmetic: `poll`'s timeout is a millisecond count and a negative one
/// means "forever", while `ppoll`'s is a `struct timespec *` and a null one
/// means the same. Read a pointer as a count and the wait is measured in
/// centuries - which is exactly the hang this split fixes, because `ppoll` is
/// the call rustix makes and therefore the call every Rust program here makes.
pub fn poll(args: [usize; ARG_COUNT]) -> isize {
    let timeout_ms = args[2] as i64;
    let deadline = (timeout_ms >= 0).then(|| deadline_in_ticks(timeout_ms as u64));
    poll_wait(args, deadline)
}

/// `ppoll(fds, nfds, timeout, sigmask, sigsetsize)`.
///
/// The signal mask is ignored: this kernel delivers signals on the way out of
/// a syscall rather than by interrupting one, so there is nothing for a
/// temporary mask to hold back.
pub fn ppoll(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let deadline = match ppoll_deadline(args[2], from_user) {
        Ok(deadline) => deadline,
        Err(e) => return e,
    };
    poll_wait(args, deadline)
}

/// The instant a `ppoll` timeout names. `Ok(None)` is "wait forever", which is
/// what a null pointer asks for.
fn ppoll_deadline(tsp: usize, from_user: bool) -> Result<Option<u64>, isize> {
    if tsp == 0 {
        return Ok(None);
    }
    if !buffer_ok(tsp, 16, false, from_user) {
        return Err(err::EFAULT);
    }
    let (sec, nsec) = unsafe {
        let at = |offset: usize| (tsp + offset) as *const u64;
        (at(0).read_unaligned(), at(8).read_unaligned())
    };
    // The `tv_nsec < 1s` check and the round-*up* to milliseconds are the
    // man page's contract, spelled in `abi`: a caller that asked for one
    // nanosecond must not get a zero-millisecond wait. The deadline further
    // out than the tick count can name saturates rather than wrapping into
    // the past - Linux clamps the same way.
    let ms = abi::ppoll_ms(sec, nsec)?;
    Ok(Some(deadline_in_ticks(ms)))
}

/// `ms` from now, in ticks.
fn deadline_in_ticks(ms: u64) -> u64 {
    let ticks = ms.saturating_mul(crate::hal::TICK_HZ as u64) / 1000;
    crate::hal::ticks().saturating_add(ticks)
}

/// The wait [`poll`] and [`ppoll`] are both made of, with the timeout already
/// turned into the instant it names.
fn poll_wait(args: [usize; ARG_COUNT], deadline: Option<u64>) -> isize {
    let (fds, nfds) = (args[0], args[1]);
    let len = match nfds.checked_mul(POLLFD_SIZE) {
        Some(len) => len,
        None => return err::EINVAL,
    };
    // An `nfds` of zero is a *wait*, not a no-op: Linux's `poll(NULL, 0, ms)`
    // sleeps for the timeout (measured on a current host,
    // `.workbuddy/probe_poll0.c`: `100.1 ms` for a 100 ms timeout) and
    // `ppoll(NULL, 0, NULL)` never returns without a signal. That second shape
    // is not academic - **`pause()` is that call on arm64**, where musl builds it
    // as `ppoll(NULL, 0, NULL, NULL, 0)` (measured: `llvm-objdump` of musl's
    // `pause.lo` for `aarch64-unknown-linux-musl` shows syscall 73 with every
    // argument zero) - so answering zero at once is a `pause()` that returns
    // immediately and a program idling that way spinning.
    //
    // The array is validated only when there is one: a zero-length range needs
    // no page, and the null `fds` the spelling above uses names none.
    if len > 0 && !buffer_ok(fds, len, true, true) {
        return err::EFAULT;
    }

    let me = crate::sched::current();
    // Ask every node to wake us before the first check: one that becomes ready
    // between the check and the sleep must not be missed. A descriptor the
    // table does not name has no node, and is reported POLLNVAL - counting it
    // is what stops a caller waiting on a closed fd from sleeping forever. A
    // negative fd is not a closed one but a placeholder, and is skipped rather
    // than reported: the check below zero-answers it, so it never counts.
    let mut nodes: Vec<Option<PollWatch>> = Vec::with_capacity(nfds);
    for index in 0..nfds {
        let at = fds + index * POLLFD_SIZE;
        let fd = unsafe { (at as *const i32).read_unaligned() };
        let watch = crate::sched::current_fds()
            .clone_at(fd)
            .ok()
            .map(|file| PollWatch {
                ofd: file.ofd(),
                node: file.inode(),
            });
        if let Some(watch) = &watch {
            watch.node.poll_subscribe_client(watch.ofd, me);
        }
        nodes.push(watch);
    }

    let ready: isize = loop {
        let count = check_ready(fds, &nodes);
        if count > 0 {
            break count as isize;
        }
        let expired = deadline.is_some_and(|end| crate::hal::ticks() >= end);
        if expired {
            break 0;
        }
        // A pending signal ends the wait: this is an *interruptible* wait, and
        // the gate delivers the signal on the way out, where a register file
        // exists to build a handler's frame on. Without this the signal is
        // never seen here at all - the wait runs to its own deadline, and a
        // wait that has none never ends. `ping` is the shape it matters for: a
        // reply ends the wait, and the next request is sent when the wait
        // times out or a signal arrives.
        if crate::sched::has_deliverable_signal() {
            break -1;
        }
        // One *interruptible* park instead of a plain tick sleep. The nodes
        // were asked to wake us above, and the ones that can - pipes, ttys,
        // unix and now network sockets - do; a wake re-runs the readiness
        // query below and ends the park the moment any watched descriptor
        // has something. The clock only bounds the wait for the nodes that
        // have no waker behind them - a netlink uevent socket, a signalfd,
        // an eventfd - whose readiness nobody can announce, so one tick at
        // a time is what keeps such a wait from sleeping forever
        // (`nlplug-findfs` parks exactly that way, with no timeout). The
        // condition is read-only: it reports, and the loop's own
        // `check_ready` above fills the caller's `revents` once.
        let next = crate::hal::ticks().saturating_add(1);
        let floor = deadline.map_or(next, |end| end.min(next));
        let any_ready = || {
            (0..nfds).any(|index| {
                let at = fds + index * POLLFD_SIZE;
                // The same skip `check_ready` makes, and for the same reason:
                // a placeholder entry answers "not ready", so a wait whose
                // only other descriptors are quiet has to keep waiting.
                // Reading it as a dead one instead answers *ready* forever -
                // `check_ready` reports nothing, the park returns at once, and
                // the loop never sleeps: `apk add` parks on
                // `poll({9},{11},{-1}, -1)` for its trigger's pipes and the
                // whole machine spins at that one call.
                let fd = unsafe { (at as *const i32).read_unaligned() };
                if fd < 0 {
                    return false;
                }
                let events = unsafe { ((at + POLLFD_EVENTS) as *const i16).read_unaligned() };
                let Some(watch) = &nodes[index] else {
                    // A dead fd has POLLNVAL waiting for the caller: the
                    // wait ends so the report can be made.
                    return true;
                };
                let ready = watch.node.poll_ready_client(watch.ofd);
                // The hang-up and error conditions count as an arrival even
                // when the caller asked for neither: Linux reports them to
                // every waiter, and a wait that slept through one would sleep
                // past the only report there will ever be.
                let node = watch.node.as_ref();
                let forced = node.poll_hup() || node.poll_err();
                forced
                    || (events & POLLIN != 0 && ready.read)
                    || (events & POLLOUT != 0 && ready.write)
            })
        };
        if matches!(
            crate::sched::block_until_deadline(any_ready, floor),
            crate::sched::WaitEnd::Interrupted
        ) {
            break -1;
        }
    };

    for watch in nodes.iter().flatten() {
        watch.node.poll_unsubscribe(me);
    }
    if ready < 0 {
        return err::EINTR;
    }
    ready
}

/// One descriptor a wait is watching: the node, and the open file description
/// that named it.
///
/// The description travels beside the node because a node whose readiness
/// depends on *which* client is asking - an evdev node with a grab on it - cannot
/// answer from the node alone. `poll` and `select` are the two paths that ask
/// this way; `epoll` keeps the same pairing in its own interest entries.
struct PollWatch {
    node: Arc<dyn Inode>,
    ofd: u64,
}

/// Fill in \`revents\` for every entry and answer how many have something.
fn check_ready(fds: usize, nodes: &[Option<PollWatch>]) -> usize {
    let mut count = 0;
    for (index, watch) in nodes.iter().enumerate() {
        let at = fds + index * POLLFD_SIZE;
        let fd = unsafe { (at as *const i32).read_unaligned() };
        let events = unsafe { ((at + POLLFD_EVENTS) as *const i16).read_unaligned() };
        // A negative `fd` names no descriptor *on purpose*: poll(2) skips
        // the entry whole - `events` ignored, `revents` zeroed, the entry
        // never counted as ready. Answering POLLNVAL instead would report
        // the slot as ready, and callers keep such slots beside real ones:
        // musl's resolver polls the TCP-fallback slots it holds at -1 next
        // to its UDP socket, so a "ready" answer turns its wait into a spin
        // that only the timeout ends, with the name it asked for unanswered
        // - which busybox prints as `bad address`.
        let revents = if fd < 0 {
            0
        } else {
            match watch {
                None => POLLNVAL,
                Some(watch) => {
                    let node = watch.node.as_ref();
                    let ready = node.poll_ready_client(watch.ofd);
                    let mut bits = 0;
                    if events & POLLIN != 0 && ready.read {
                        bits |= POLLIN;
                    }
                    if events & POLLOUT != 0 && ready.write {
                        bits |= POLLOUT;
                    }
                    // Outside the caller's interest on purpose - `poll` in the
                    // kernel reports them whether or not `events` asked, which
                    // is what tells a waiter that its peer has gone.
                    if node.poll_hup() {
                        bits |= POLLHUP;
                    }
                    if node.poll_err() {
                        bits |= POLLERR;
                    }
                    bits
                }
            }
        };
        unsafe { ((at + POLLFD_REVENTS) as *mut i16).write_unaligned(revents) };
        if revents != 0 {
            count += 1;
        }
    }
    count
}

// ---------------------------------------------------------------------------
// select: the fd_set-bitmap spelling of poll
// ---------------------------------------------------------------------------

// The ABI's ceiling on a descriptor number in an `fd_set` - `FD_SETSIZE`,
// 1024 - is pinned in `abi` with the rest of the flag words. Linux answers
// `EINVAL` for anything past it, and so does the check below.

/// One descriptor a `select` caller asked about: which fd, what it is, and
/// which of the caller's sets it appeared in.
struct SelectWatch {
    fd: i32,
    node: Option<Arc<dyn Inode>>,
    /// The description that named it, for the nodes whose readiness depends on
    /// which client is asking - the same pairing [`PollWatch`] carries.
    ofd: u64,
    want_read: bool,
    want_write: bool,
}

/// What one watched descriptor can do right now, mapped through the caller's
/// interest. `poll_ready` is the same source `poll` consults.
///
/// The hang-up and error conditions are not part of that interest and are not
/// filtered by it: Linux's `select` counts a descriptor as ready when the
/// condition mask carries `EPOLLHUP` or `EPOLLERR`, in the read set and (for
/// the error alone) in the write set. A `select` that skipped them would leave
/// a caller parked on a peer that has already gone - the one case where no
/// further event is coming to wake it.
fn watch_ready(watch: &SelectWatch) -> (bool, bool) {
    let Some(node) = &watch.node else {
        return (false, false);
    };
    let ready = node.poll_ready_client(watch.ofd);
    let hup = node.poll_hup();
    let err = node.poll_err();
    (
        watch.want_read && (ready.read || hup || err),
        watch.want_write && (ready.write || err),
    )
}

/// `select(nfds, readfds, writefds, exceptfds, timeout)`.
///
/// musl calls straight into this on x86_64 - there is no separate "new"
/// select number as there was on i386 - and `apk`'s downloader parks every
/// fetch in it, which is what made this worth having. The fd_set bitmaps are
/// read into a watch list, the wait is poll's loop with different plumbing,
/// and the caller's sets are rewritten with the ready bits before the count
/// comes back.
///
/// The except set is validated and cleared but never answered: it reports
/// out-of-band data, and nothing here generates any.
/// `pselect6(2)` — `select` with a nanosecond timeout and a signal mask.
///
/// The sixth argument is a signal mask Linux swaps in atomically for the
/// duration of the wait. Nothing here runs a handler during a wait, and a
/// single-threaded caller cannot have one delivered either, so the mask is
/// read for validity and not acted on: the wait is the same wait.
pub fn pselect6(args: [usize; ARG_COUNT]) -> isize {
    // A mask pointer that is not null names a `sigset_t`; the size word
    // musl passes alongside is not in this call's arguments.
    select_inner(args, true)
}

/// `select(2)` — the microsecond-timeout original.
pub fn select(args: [usize; ARG_COUNT]) -> isize {
    select_inner(args, false)
}

/// The whole of `select`, with `timespec` selecting which shape `args[4]`
/// points at: `pselect6` hands a `timespec` (nanoseconds), `select` a
/// `timeval` (microseconds). Everything else - the sets, the wait, the
/// write-back - is one behaviour, so they share it rather than repeat it.
fn select_inner(args: [usize; ARG_COUNT], timespec: bool) -> isize {
    let (nfds_arg, readfds, writefds, exceptfds, tvp) =
        (args[0], args[1], args[2], args[3], args[4]);
    if (nfds_arg as i64) < 0 || nfds_arg > FD_SETSIZE {
        return err::EINVAL;
    }
    let nfds = nfds_arg;

    // One word per 64 descriptors, rounded up; a set shorter than that is
    // the caller's business, since only the bits below `nfds` are read.
    let set_bytes = nfds.div_ceil(64) * 8;
    for set in [readfds, writefds, exceptfds] {
        if set != 0 && !buffer_ok(set, set_bytes, true, true) {
            return err::EFAULT;
        }
    }
    if tvp != 0 && !buffer_ok(tvp, 16, true, true) {
        return err::EFAULT;
    }

    /// Read bit `fd` of a caller's set. `None` (no set) reads as clear.
    fn bit(set: usize, fd: usize) -> bool {
        if set == 0 {
            return false;
        }
        unsafe {
            let word = ((set + (fd / 64) * 8) as *const u64).read_unaligned();
            word >> (fd % 64) & 1 == 1
        }
    }

    let mut watches: Vec<SelectWatch> = Vec::new();
    for fd in 0..nfds {
        let (r, w, e) = (bit(readfds, fd), bit(writefds, fd), bit(exceptfds, fd));
        if !r && !w && !e {
            continue;
        }
        // Unlike poll, which reports a dead fd as POLLNVAL, select answers
        // EBADF for the whole call - the shape every caller's error path
        // actually expects.
        let Ok(file) = crate::sched::current_fds().clone_at(fd as i32) else {
            return err::EBADF;
        };
        watches.push(SelectWatch {
            fd: fd as i32,
            node: Some(file.inode()),
            ofd: file.ofd(),
            want_read: r,
            want_write: w,
        });
    }

    // Ask every node to wake us before the first check, as poll does: a
    // descriptor that turns ready between the check and the sleep must not
    // be missed.
    let me = crate::sched::current();
    for watch in &watches {
        if let Some(node) = &watch.node {
            node.poll_subscribe_client(watch.ofd, me);
        }
    }

    let deadline = if tvp == 0 {
        None
    } else {
        // Both shapes lead with seconds and follow with a sub-second field:
        // microseconds for `select`, nanoseconds for `pselect6`. The unit
        // check and the fold into milliseconds are the man page's contract,
        // spelled once in `abi` - the resolution the wait below works in is
        // what the milliseconds are for.
        let (sec, frac) = unsafe {
            let at = |offset: usize| (tvp + offset) as *const u64;
            (at(0).read_unaligned(), at(8).read_unaligned())
        };
        let ms = match abi::select_timeout_ms(sec, frac, timespec) {
            Ok(ms) => ms,
            Err(e) => return e,
        };
        let ticks = ms * crate::hal::TICK_HZ as u64 / 1000;
        Some(crate::hal::ticks() + ticks)
    };

    // Readiness as of the last check, kept beside the watches so the
    // write-back does not have to ask the nodes twice.
    let mut ready: Vec<(bool, bool)> = watches.iter().map(watch_ready).collect();
    let count = loop {
        let count: isize = ready.iter().filter(|(r, w)| *r || *w).count() as isize;
        if count > 0 {
            break count;
        }
        if deadline.is_some_and(|end| crate::hal::ticks() >= end) {
            break 0;
        }
        // The same interruptible-wait rule as `poll`: a pending signal ends
        // this wait, and the gate delivers it on the way out.
        if crate::sched::has_deliverable_signal() {
            break -1;
        }
        // Nothing could wake a wait with no descriptors and no clock: answer
        // zero rather than park the task forever.
        if deadline.is_none() && watches.is_empty() {
            break 0;
        }
        // An interruptible park, bounded by the next tick - the same shape
        // as poll's wait. A wake from a subscribed node (pipes, ttys, unix
        // and, since the stack grew its wake list, network sockets) re-runs
        // the readiness query and ends the park the moment anything watched
        // has something; the tick is the bound for the descriptors whose
        // readiness nobody can announce - netlink, signalfd, eventfd.
        // `watch_ready` only reads; the recorded readiness is refreshed from
        // it once the park ends, for the write-back below.
        let next = crate::hal::ticks().saturating_add(1);
        let floor = deadline.map_or(next, |end| end.min(next));
        let any_ready = || {
            watches
                .iter()
                .any(|watch| matches!(watch_ready(watch), (true, _) | (_, true)))
        };
        if let crate::sched::WaitEnd::Interrupted =
            crate::sched::block_until_deadline(any_ready, floor)
        {
            break -1;
        }
        ready = watches.iter().map(watch_ready).collect();
    };

    for watch in &watches {
        if let Some(node) = &watch.node {
            node.poll_unsubscribe(me);
        }
    }
    if count < 0 {
        return err::EINTR;
    }

    // Rewrite the caller's sets with only the ready bits, as select's
    // contract demands - the sets arrive as interest and leave as result.
    for set in [readfds, writefds, exceptfds] {
        if set != 0 {
            unsafe {
                core::ptr::write_bytes(set as *mut u8, 0, set_bytes);
            }
        }
    }
    if count > 0 {
        for (watch, (r, w)) in watches.iter().zip(ready.iter()) {
            let mark = |set: usize, fd: usize| {
                if set != 0 {
                    unsafe {
                        let at = (set + (fd / 64) * 8) as *mut u64;
                        at.write_unaligned(at.read_unaligned() | 1u64 << (fd % 64));
                    }
                }
            };
            if *r {
                mark(readfds, watch.fd as usize);
            }
            if *w {
                mark(writefds, watch.fd as usize);
            }
            // exceptfds never gets bits: nothing here generates OOB data.
        }
    }

    // Linux reports the time that was left, and callers that loop on a
    // timeout compare it with zero to see whether they timed out. Both read
    // it the same way after a zero write.
    if tvp != 0 {
        let remaining_ms = match deadline {
            None => 0,
            Some(end) => {
                let now = crate::hal::ticks();
                if now >= end {
                    0
                } else {
                    (end - now) * 1000 / crate::hal::TICK_HZ as u64
                }
            }
        };
        unsafe {
            let at = |offset: usize| (tvp + offset) as *mut u64;
            at(0).write_unaligned(remaining_ms / 1000);
            at(8).write_unaligned(remaining_ms % 1000 * 1000);
        }
    }
    count
}

// The `struct stat` writer lives in `abi`: the field offsets *and the length*
// are the ABI's - 144 bytes on x86_64, 128 on aarch64 - and both are pinned
// where they live (host tests for the offsets, `t-stat-buffer` for the length
// on each machine). An off-by-one word is invisible to every caller that only
// reads `st_mode`, and silently answers "empty file" to the ones that ask for
// `st_size`; a length too long is worse, because the bytes past the struct
// belong to whoever handed the buffer over.

/// Write the 120-byte Linux x86_64 `struct statfs` for the filesystem `name`
/// spells. Same rule as `stat`: the offsets are the ABI, and every
/// field is a word except the two-`u32` `f_fsid`. The magic comes from
/// `abi::fstype_magic` - `switch_root` reads `f_type` to decide
/// whether the root it stands in is an initramfs, and answering "ext2" for a
/// ramfs is what made it refuse to switch.
///
/// The block counts are real for a filesystem that reports them (ext2, from
/// its superblock and group descriptors) and "memory-sized" for the kernel's
/// own trees. A filesystem with neither - and one that reports zero total
/// blocks - is what `df` filters out, exactly as Linux's own does. The byte
/// writing itself is `abi::write_statfs`, tested on the host; the PMM fallback
/// below is the only part that has to stay machine-side.
fn write_statfs(buf: usize, name: &str, stats: Option<crate::vfs::FsStats>) -> isize {
    let magic = abi::fstype_magic(name);
    // Memory-backed trees are as big as the frame allocator; a filesystem
    // with its own accounting answers with those numbers instead.
    let (bsize, blocks, bfree, files, ffree) = if let Some(s) = stats {
        (s.bsize, s.blocks, s.bfree, s.files, s.ffree)
    } else {
        let memory_backed = matches!(name, "ramfs" | "tmpfs" | "devfs" | "devtmpfs");
        if memory_backed {
            critical_section::with(|cs| {
                let pmm = crate::mm::PMM.borrow(cs).borrow();
                (
                    4096u64,
                    (pmm.total_frames() * 4) as u64,
                    (pmm.free_frames() * 4) as u64,
                    0u64,
                    0u64,
                )
            })
        } else {
            (4096, 0, 0, 0, 0)
        }
    };
    abi::write_statfs(buf, magic, bsize, blocks, bfree, files, ffree)
}

// ---------------------------------------------------------------------------
// File-descriptor calls: the syscalls whose argument is an fd, not a path.
// These are the same shape every libc hands its wrappers: (fd, ...) and
// nothing more.
// ---------------------------------------------------------------------------

/// The socket leg of `read`/`readv`: `recvfrom` with empty source slots is
/// exactly `read`'s contract on a socket. Gated on the `mpos_net` capability
/// rather than on a machine: a build that declares no network stack has no
/// descriptor that can report a socket handle, so this helper's body is never
/// reached there and `ENOTTY` is the honest answer - while on a machine that
/// *does* declare one the routing is the same as every other socket call's.
#[cfg(mpos_net)]
fn socket_read(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    crate::net::socket::handle_recvfrom(args, from_user)
}
#[cfg(not(mpos_net))]
fn socket_read(_args: [usize; ARG_COUNT], _from_user: bool) -> isize {
    err::ENOTTY
}

/// The socket leg of `write`/`writev`: `sendto` with no destination is exactly
/// `write`'s contract on a connected socket. Same capability gate and same
/// honest `ENOTTY` for the build that has no stack as [`socket_read`].
#[cfg(mpos_net)]
fn socket_write(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    crate::net::socket::handle_sendto(args, from_user)
}
#[cfg(not(mpos_net))]
fn socket_write(_args: [usize; ARG_COUNT], _from_user: bool) -> isize {
    err::ENOTTY
}

/// Report `IN_ACCESS` for a descriptor whose read moved bytes.
///
/// Linux calls `fsnotify_access` once per successful read-family or copy
/// call, so this sits at the doors and not inside `vfs::read` - a two-iovec
/// `readv` is one read of the file and reports one record, and the kernel's
/// own readers (`read_file`, the `execve` ELF load) reach `File::read`
/// without reporting at all. The file description is looked up rather than
/// threaded in because the watchers key on the inode's `(dev, ino)` and the
/// doors carry a number.
pub(crate) fn notice_access(fd: i32) {
    if let Ok(file) = crate::sched::current_fds().clone_at(fd) {
        crate::vfs::inotify::emit_access(&file.inode());
    }
}

/// Report `IN_MODIFY` for a descriptor whose write moved bytes - the write-side
/// twin of [`notice_access`], and the same shape for the same reason: Linux's
/// `fsnotify_modify` is once per successful write-family or copy call, so a
/// door that writes chunk by chunk (a vectored write, a copy) writes through
/// [`crate::vfs::write_quiet`] and says the one event here when it is done.
/// The scalar `write`/`pwrite` doors reach `File::write`, which reports on its
/// own, so they do not call this.
pub(crate) fn notice_modify(fd: i32) {
    if let Ok(file) = crate::sched::current_fds().clone_at(fd) {
        crate::vfs::inotify::emit_write(&file.inode());
    }
}

/// `read(2)` — read up to `buf.len()` bytes from `fd` into `buf`.
pub fn handle_read(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, len) = (args[0], args[1], args[2]);
    if !buffer_ok(buf, len, true, from_user) {
        return if len > MAX_IO {
            err::EINVAL
        } else {
            err::EFAULT
        };
    }
    // A socket descriptor reads like a file to `read(2)` - the same call,
    // routed to the stack. `recvfrom` with null source slots is exactly the
    // semantics `read` has on a socket.
    let is_socket = crate::sched::current_fds()
        .clone_at(fd as i32)
        .map(|file| file.inode().socket_handle() != 0)
        .unwrap_or(false);
    if is_socket {
        return socket_read([fd, buf, len, 0, 0, 0], from_user);
    }
    if len == 0 {
        return match crate::vfs::read(fd as i32, &mut []) {
            Ok(_) => 0,
            Err(e) => err_of(e),
        };
    }
    let bytes = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
    match crate::vfs::read(fd as i32, bytes) {
        Ok(n) => {
            if n > 0 {
                notice_access(fd as i32);
            }
            n as isize
        }
        Err(e) => err_of(e),
    }
}

/// `write(2)` — write `buf` to `fd`.
pub fn handle_write(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, len) = (args[0], args[1], args[2]);
    if !buffer_ok(buf, len, false, from_user) {
        return if len > MAX_IO {
            err::EINVAL
        } else {
            err::EFAULT
        };
    }
    // The write half of the socket split: `sendto` with no destination,
    // which for a connected socket is the whole of `write`'s contract.
    let is_socket = crate::sched::current_fds()
        .clone_at(fd as i32)
        .map(|file| file.inode().socket_handle() != 0)
        .unwrap_or(false);
    if is_socket {
        return socket_write([fd, buf, len, 0, 0, 0], from_user);
    }
    if len == 0 {
        return match crate::vfs::write(fd as i32, &[]) {
            Ok(_) => 0,
            Err(e) => err_of(e),
        };
    }
    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
    match crate::vfs::write(fd as i32, bytes) {
        Ok(n) => n as isize,
        Err(e) => err_of(e),
    }
}

/// `close(2)` — close `fd`.
/// `close(2)` — close `fd`.
///
/// A socket whose last descriptor this was comes down with it: the
/// description is lifted out of the table first, and only a sole surviving
/// reference - this close being the last one - tears the channel down.
pub fn handle_close(args: [usize; ARG_COUNT]) -> isize {
    let fd = args[0] as i32;
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    // POSIX drops every one of the process's record locks on a file the moment
    // it closes any descriptor to it - the behaviour that surprises people, and
    // the reason the lock is keyed by the file and the owner, not the fd.
    #[cfg(target_os = "none")]
    {
        // An evdev seat grab dies with its descriptor, the way a `flock` does -
        // which is what lets a compositor that was killed (or a test that closed
        // the device) leave the keyboard claimable instead of grabbing it from
        // beyond its own grave.
        #[cfg(target_os = "none")]
        if let Some(ev) = file.inode().as_any().downcast_ref::<EvdevNode>() {
            ev.set_grab(false, file.ofd());
        }
        let inode = file.inode();
        crate::vfs::locks::release_on_close(
            crate::vfs::mount::fs_id_of_inode(&inode),
            file.metadata().ino,
            crate::sched::current_tgid().0 as u32,
        );
    }
    match crate::vfs::close(fd) {
        Ok(()) => {}
        Err(e) => return err_of(e),
    }
    // A socket whose last descriptor this was comes down with it - and that is
    // not a tidiness question on any machine: a socket left in the stack's set
    // keeps its local port, so the *next* program to ask for that port is
    // refused. Measured on the second machine when this was still gated to
    // x86_64: `t-socket` bound 34567 and exited, `t-tcp`'s bind of the same port
    // answered EADDRINUSE on a machine that had just booted.
    crate::net::socket::close_last_socket(&file);
    crate::vfs::inotify::close_last(&file);
    // The evdev client's read position dies with the description too - and it
    // is asked *after* the close, where "last" is knowable: a `dup` is the same
    // reader and has to keep its place, so only the descriptor that took the
    // final reference forgets it.
    #[cfg(target_os = "none")]
    if Arc::strong_count(&file) == 1 {
        if let Some(ev) = file.inode().as_any().downcast_ref::<EvdevNode>() {
            ev.forget(file.ofd());
        }
        // And the device a `/dev/uinput` description made goes with it - the
        // node out of `/dev/input`, its readers told the stream has ended.
        if let Some(ui) = file.inode().as_any().downcast_ref::<Uinput>() {
            ui.forget(file.ofd());
        }
        // The DRM master belongs to the open file description, so it dies with
        // the last reference to it: a compositor that is killed - or a test that
        // simply closes the card - must not leave the machine holding a screen
        // nobody can drive. Asked after the close, for the same reason as the
        // read position above.
        if file.inode().as_any().downcast_ref::<DrmCard>().is_some() {
            crate::drivers::drm::master_closed(file.ofd());
        }
        // The keyboard's mode goes back with the session that wrote it, so a
        // `K_OFF` left behind by a process that is gone cannot strand a respawned
        // getty with no keys to read.
        //
        // The console's `VT_PROCESS` promise deliberately does *not* go back here,
        // which is the opposite of what this hook used to do. A promise belongs to
        // the process that wrote `VT_SETMODE`, and the process that writes it
        // closes the descriptor on purpose - seatd's `vt_open` sets the mode and
        // `close(ttyfd)`s before the compositor it serves has the screen - so
        // clearing a promise at a close unmakes the claim in the same breath it is
        // made, and the text console is back to racing the compositor for one
        // scanout. Linux notices a dead keeper where the signal is actually owed
        // (`kill_pid` failing, then `reset_vc`); so does this kernel - see
        // `crate::vfs::vcmux::abandon` for the whole of the reasoning.
        if let Some(tty) = file.inode().as_tty()
            && let Some(vc) = tty.vc_number()
        {
            crate::vfs::vcmux::abandon(vc);
        }
        // The description's *own* locks - its `flock` and its `F_OFD_*` record
        // locks - die here and not in the `release_on_close` above, because
        // they belong to the description rather than to the process: Linux
        // releases them when the last descriptor referring to the description
        // closes, so closing one of several `dup`s leaves them in force
        // (measured on a current host: `flock(a, LOCK_EX)`, `close(dup_of_a)`,
        // then a fresh `open()` still answers `EWOULDBLOCK` - `docs/notes/`
        // §15.16, `user/tests/src/bin/t-filelock.rs`). Asked after the close,
        // where "last" is knowable, exactly like the evdev read position and
        // the DRM master above.
        let inode = file.inode();
        crate::vfs::locks::release_description(
            crate::vfs::mount::fs_id_of_inode(&inode),
            file.metadata().ino,
            file.ofd(),
        );
    }
    0
}

/// `lseek(fd, offset, whence)` — move the file position.
///
/// `whence` is `SEEK_SET` (0), `SEEK_CUR` (1), `SEEK_END` (2). Anything
/// else, or a position that would land before byte 0, is `EINVAL` rather
/// than wrapping — the same refusal the man page spells out.
pub fn handle_lseek(args: [usize; ARG_COUNT]) -> isize {
    let (fd, offset, whence) = (args[0] as i32, args[1] as i64, args[2] as i32);
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    // A pipe, a socket or a terminal has no position to move, and Linux says
    // so with ESPIPE. Answering with a number instead is worse than useless:
    // a caller that pushes back bytes it did not want - which is how a
    // buffered reader un-reads the tail of a chunk - is told the push-back
    // worked and then drops them.
    if !file.inode().seekable() {
        return err::ESPIPE;
    }
    let cur = file.pos() as i64;
    let size = file.metadata().size as i64;
    // The whence arithmetic and the "before byte 0 is EINVAL" rule are the
    // man page's contract, spelled in `abi`.
    match abi::lseek_target(cur, size, offset, whence) {
        Some(pos) => {
            file.set_pos(pos as u64);
            pos as isize
        }
        None => err::EINVAL,
    }
}

/// `getdents64(fd, buf, len)` — Linux directory listing format.
pub fn handle_getdents64(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, buf_len) = (args[0] as i32, args[1], args[2]);
    if !buffer_ok(buf, buf_len, true, from_user) {
        return err::EFAULT;
    }

    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    let entries = match file.inode().read_dir() {
        Ok(e) => e,
        Err(e) => return err_of(e),
    };
    let start = file.pos() as usize;
    if start >= entries.len() {
        return 0;
    }

    // Linux getdents64 format:
    //   u64 d_ino, u64 d_off, u16 d_reclen, u8 d_type, u8 d_name[NUL] (8-aligned).
    // The `d_reclen` rounding and the `DT_*` numbering are the format's
    // contract, spelled in `abi` and pinned there by host tests.
    let mut offset = 0usize;
    for (idx, (name, kind)) in entries.iter().enumerate().skip(start) {
        let reclen = abi::reclen_of(name.len());
        if offset + reclen > buf_len {
            if offset == 0 {
                return err::EINVAL;
            }
            break;
        }
        let d_type = abi::dt_type_of(*kind);
        unsafe {
            let base = (buf + offset) as *mut u8;
            (base as *mut u64).write_unaligned(idx as u64 + 1);
            ((base.add(8)) as *mut u64).write_unaligned((idx + 1) as u64);
            ((base.add(16)) as *mut u16).write_unaligned(reclen as u16);
            base.add(18).write_unaligned(d_type);
            let name_bytes = name.as_bytes();
            core::ptr::copy_nonoverlapping(name_bytes.as_ptr(), base.add(19), name_bytes.len());
            base.add(19 + name_bytes.len()).write_unaligned(0);
        }
        offset += reclen;
        file.set_pos((idx + 1) as u64);
    }
    offset as isize
}

/// `dup(oldfd)` — duplicate onto the lowest free fd.
pub fn handle_dup(args: [usize; ARG_COUNT]) -> isize {
    let old = args[0] as i32;
    let file = match crate::sched::current_fds().clone_at(old) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    match crate::sched::current_fds().install(file) {
        Ok(fd) => fd as isize,
        Err(e) => err_of(e),
    }
}

/// `dup2(oldfd, newfd)` — make `newfd` name what `oldfd` names.
pub fn handle_dup2(args: [usize; ARG_COUNT]) -> isize {
    let (old, new) = (args[0] as i32, args[1] as i32);
    if new < 0 {
        return err::EBADF;
    }
    match crate::vfs::dup2(old, new) {
        Ok(fd) => fd as isize,
        Err(e) => err_of(e),
    }
}

/// `dup3(oldfd, newfd, flags)` — `dup2` with `O_CLOEXEC`.
///
/// `old == new` is `EINVAL`, the only flag accepted is `O_CLOEXEC`, anything
/// else is `EINVAL`.
pub fn handle_dup3(args: [usize; ARG_COUNT]) -> isize {
    const O_CLOEXEC: i32 = 0o200_0000;
    let (old, new, flags) = (args[0] as i32, args[1] as i32, args[2] as i32);
    if old == new || flags & !O_CLOEXEC != 0 {
        return err::EINVAL;
    }
    let file = match crate::sched::current_fds().clone_at(old) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    match crate::sched::current_fds().install_at(file, new) {
        Ok(()) => new as isize,
        Err(e) => err_of(e),
    }
}

/// `pipe(fds)` — write `[read_end, write_end]` into `fds`.
pub fn handle_pipe(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const O_CLOEXEC: u32 = 0o200_0000_u32;
    const O_NONBLOCK: u32 = crate::vfs::file::O_NONBLOCK;
    // `pipe(2)` is `pipe2(2)` with a zero flag word; the two flags that exist
    // both belong to the ends rather than the numbers. `O_CLOEXEC`, because a
    // pipe that outlives the `execve` it was made for never reports
    // end-of-stream to whoever is reading it - which is exactly how
    // `posix_spawn` learns whether the exec worked. `O_NONBLOCK`, because it
    // is the birth state a self-pipe wants: a program with an event loop -
    // sudo's signal channel is the one that was dying here - reads its pipe
    // only when something was signalled, and a pipe born blocking parks it on
    // the first empty read instead. Anything else in the word is `EINVAL`,
    // the answer Linux gives a flag combination it cannot serve.
    let out = args[0];
    let flags = args[1] as u32;
    if flags & !(O_CLOEXEC | O_NONBLOCK) != 0 {
        return err::EINVAL;
    }
    if out == 0 || !buffer_ok(out, 2 * core::mem::size_of::<i32>(), true, from_user) {
        return err::EFAULT;
    }
    let (read_end, write_end) = crate::vfs::pipe::pair();
    let nonblocking = flags & O_NONBLOCK != 0;
    read_end.set_nonblocking(nonblocking);
    write_end.set_nonblocking(nonblocking);
    let fd0 = match crate::vfs::file::open_inode(read_end) {
        Ok(fd) => fd,
        Err(e) => return err_of(e),
    };
    let fd1 = match crate::vfs::file::open_inode(write_end) {
        Ok(fd) => fd,
        Err(e) => {
            // The read end landed but the write end could not: hand the first
            // back so a failed `pipe` leaves no descriptor behind.
            let _ = crate::vfs::close(fd0);
            return err_of(e);
        }
    };
    let fds = [fd0, fd1];
    // A pipe's two ends are the one description whose direction the kernel
    // mints rather than the caller's open: measured on a current host,
    // `fcntl(F_GETFL)` reports the read end as `O_RDONLY` and the write end as
    // `O_WRONLY`, and the same asymmetry is what makes `write` on the read end
    // and `read` on the write end answer `EBADF` there.
    {
        let table = crate::sched::current_fds();
        for (fd, mode) in [
            (fd0, crate::vfs::file::ACC_RDONLY),
            (fd1, crate::vfs::file::ACC_WRONLY),
        ] {
            if let Ok(file) = table.clone_at(fd) {
                file.set_access(mode);
            }
        }
    }
    if flags & O_CLOEXEC != 0 {
        let table = crate::sched::current_fds();
        for fd in fds {
            let _ = table.fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
        }
    }
    if nonblocking {
        // The status word is what `fcntl(F_GETFL)` reads back, and a caller
        // asking how the pipe will behave has to be told.
        let table = crate::sched::current_fds();
        for fd in fds {
            if let Ok(file) = table.clone_at(fd) {
                file.set_status(O_NONBLOCK);
            }
        }
    }
    unsafe {
        let slot = out as *mut i32;
        slot.write(fds[0]);
        slot.add(1).write(fds[1]);
    }
    0
}

/// `getcwd(buf, size)` — write the cwd's path into the buffer.
///
/// The path is the one [`adopt_cwd`] derived from the directory - Linux's
/// canonical answer - or the spelling `chdir`/`fchdir` was handed when the
/// tree could not name the cwd ([`MountTable::path_of`]'s `None`).
///
/// One case the derivation does not reach, measured on the host
/// (`.workbuddy/probe_getcwd.c`): a cwd removed underneath the caller answers
/// `ENOENT` there, because its dentry chain no longer reaches the root, where
/// this kernel - which keeps no parent edge to notice the disconnect from -
/// still answers the name the directory had. Named in `docs/uapi/` §5.2 rather
/// than faked.
///
/// [`MountTable::path_of`]: crate::vfs::mount::MountTable::path_of
pub fn handle_getcwd(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (buf, size) = (args[0], args[1]);
    if !buffer_ok(buf, size, true, from_user) || size < 2 {
        return err::EINVAL;
    }
    let path = crate::sched::current_cwd_path();
    if path.len() + 1 > size {
        return err::ERANGE;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(path.as_ptr(), buf as *mut u8, path.len());
        *((buf + path.len()) as *mut u8) = 0;
    }
    buf as isize
}

/// `fchdir(fd)` — change cwd to the directory the descriptor names.
pub fn handle_fchdir(args: [usize; ARG_COUNT]) -> isize {
    let fd = args[0] as i32;
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    if file.metadata().kind != crate::vfs::NodeKind::Dir {
        return err::ENOTDIR;
    }
    let inode = file.inode();
    // The directory's own name is what `adopt_cwd` derives; the name the
    // description was opened by is only the *fallback*, for a node the
    // caller's tree cannot name - an inherited descriptor, or one opened
    // before names were recorded at all.
    let fallback = file
        .label()
        .unwrap_or_else(|| alloc::string::String::from("/"));
    adopt_cwd(inode, fallback);
    0
}

/// `writev(fd, iov, iovcnt)` — scatter write.
pub fn handle_writev(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let fd = args[0] as i32;
    let n = writev_inner(args, from_user);
    // One `IN_MODIFY` for the whole call, not one per iovec: Linux reports a
    // write, and a vectored write is one however many iovecs carried it
    // (measured, `.workbuddy/probe_inotify_modify.c` - a two-iovec `writev`
    // answers one record). This is also the body `pwritev`/`pwritev2` run
    // through, so all three get it once, and the inner writes run through
    // `write_quiet` so they do not each report their own.
    if n > 0 {
        notice_modify(fd);
    }
    n
}

fn writev_inner(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, iov_ptr, iov_cnt) = (args[0] as i32, args[1], args[2]);
    if iov_cnt > 1024 {
        return err::EINVAL;
    }
    let iov_size = 16;
    if !buffer_ok(iov_ptr, iov_cnt * iov_size, false, from_user) {
        return err::EFAULT;
    }

    // A socket has no VFS write behind it, and musl's stdio hands every
    // buffered flush to `writev` - without this split a socket's stdio
    // answers ENOSYS and every libc program that talks through FILE* dies
    // on its first send. TCP is a byte stream, so sending the iovecs one
    // by one lands the same bytes in the same order one vectored send
    // would.
    let is_socket = crate::sched::current_fds()
        .clone_at(fd)
        .map(|file| file.inode().socket_handle() != 0)
        .unwrap_or(false);

    let mut total: isize = 0;
    for i in 0..iov_cnt {
        let base = iov_ptr + i * iov_size;
        let (ptr, len): (usize, usize) = unsafe {
            (
                (base as *const usize).read(),
                ((base + 8) as *const usize).read(),
            )
        };
        if len == 0 {
            continue;
        }
        if !buffer_ok(ptr, len, false, from_user) {
            return err::EFAULT;
        }
        if is_socket {
            let sent = socket_write([fd as usize, ptr, len, 0, 0, 0], from_user);
            if sent < 0 {
                if total > 0 {
                    return total;
                }
                return sent;
            }
            total += sent;
            if (sent as usize) < len {
                return total;
            }
            continue;
        }
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
        match crate::vfs::write_quiet(fd, bytes) {
            Ok(n) => total += n as isize,
            Err(e) => {
                if total > 0 {
                    return total;
                }
                return err_of(e);
            }
        }
    }
    total
}

/// `readv(fd, iov, iovcnt)` — scatter read.
pub fn handle_readv(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let fd = args[0] as i32;
    let n = readv_inner(args, from_user);
    // One `IN_ACCESS` for the whole call, not one per iovec: Linux reports a
    // read, and a vectored read is one (measured, `.workbuddy/probe_inotify_access.c`
    // scenario L - two full iovecs still answer one record). This is also the
    // body `preadv`/`preadv2` run through, so all three get it once.
    if n > 0 {
        notice_access(fd);
    }
    n
}

fn readv_inner(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, iov_ptr, iov_cnt) = (args[0] as i32, args[1], args[2]);
    if iov_cnt > 1024 {
        return err::EINVAL;
    }
    let iov_size = 16;
    if !buffer_ok(iov_ptr, iov_cnt * iov_size, false, from_user) {
        return err::EFAULT;
    }

    // The read twin of the writev split below it: musl's stdio issues every
    // fill as a readv - an internal buffer plus the caller's own - and a
    // socket answers through the stack, not the VFS. The stack hands out
    // one contiguous run per recv, so the first buffer with room takes it;
    // stopping early with a short count is also what a kernel readv does.
    let is_socket = crate::sched::current_fds()
        .clone_at(fd)
        .map(|file| file.inode().socket_handle() != 0)
        .unwrap_or(false);

    let mut total: isize = 0;
    for i in 0..iov_cnt {
        let base = iov_ptr + i * iov_size;
        let (ptr, len): (usize, usize) = unsafe {
            (
                (base as *const usize).read(),
                ((base + 8) as *const usize).read(),
            )
        };
        if len == 0 {
            continue;
        }
        if !buffer_ok(ptr, len, true, from_user) {
            return err::EFAULT;
        }
        if is_socket {
            let got = socket_read([fd as usize, ptr, len, 0, 0, 0], from_user);
            if got < 0 {
                if total > 0 {
                    return total;
                }
                return got;
            }
            total += got;
            if (got as usize) < len {
                break;
            }
            continue;
        }
        let bytes = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len) };
        match crate::vfs::read(fd, bytes) {
            Ok(n) => {
                total += n as isize;
                if n < len {
                    break;
                }
            }
            Err(e) => {
                if total > 0 {
                    return total;
                }
                return err_of(e);
            }
        }
    }
    total
}

/// Record the cwd inode + the name `getcwd` reads back.
///
/// The name is derived from the *directory*, not kept from the argument the
/// caller spelled, because that is Linux's answer: `getcwd(2)` walks the dentry
/// chain and reports the canonical path, so `chdir("/x/link")` through a link to
/// `/y/real` reads back `/y/real` there - and a later `..` climbs from the
/// target's parent, not from the link's. Measured whole on a current host
/// (`.workbuddy/probe_getcwd.c`, Linux 7.2.9): a link at the end or in the
/// middle of the argument, a descriptor's `/proc/self/fd/N` name, and a
/// `fchdir` through a symlinked descriptor all canonicalise; a `..` taken after
/// a link that crosses to another parent lands on the *target's* parent; a name
/// with `./` and a trailing `..` folds; and a cwd removed underneath the caller
/// makes `getcwd` answer `ENOENT` (a case this kernel still approximates, see
/// the arm in `handle_getcwd`).
///
/// The search is [`MountTable::path_of`] - the bounded walk from the caller's
/// root that `/proc/mounts` already uses - and it can answer a *different* name
/// for a directory the tree reaches several ways, or `None` for a node the
/// caller's tree cannot name (a cwd deeper than its visit budget, a mount's root
/// reached from outside). `fallback` is the caller's own spelling for that case,
/// so the answer is never worse than the string the argument named. It is also
/// what `fchdir` used to search for itself; doing it here is the one place the
/// cwd is written, so the two doors can no longer disagree.
///
/// [`MountTable::path_of`]: crate::vfs::mount::MountTable::path_of
fn adopt_cwd(dir: Arc<dyn Inode>, fallback: alloc::string::String) {
    let path = crate::vfs::mount::caller_table()
        .path_of(&dir)
        .unwrap_or(fallback);
    crate::sched::set_cwd(dir);
    crate::sched::set_cwd_path(path);
}

// ---------------------------------------------------------------------------
// The calls an ordinary libc reaches for
// ---------------------------------------------------------------------------

/// Run `body` with `fd`'s position moved to `offset`, and put the position
/// back whatever happens.
///
/// The offset read/write pair is `read`/`write` at a position the caller
/// names instead of the one the descriptor carries - which is why it does
/// not disturb that position: a `pread` between two `read`s must leave the
/// second one where the first left off. Restoring on every path, error
/// included, is the whole of the promise.
fn at_position<R>(fd: i32, offset: i64, body: impl FnOnce() -> R) -> R {
    let file = crate::sched::current_fds().clone_at(fd);
    let saved = file.as_ref().map(|f| f.pos()).ok();
    if let (Ok(file), Ok(offset)) = (&file, u64::try_from(offset)) {
        file.set_pos(offset);
    }
    let result = body();
    if let (Ok(file), Some(pos)) = (&file, saved) {
        file.set_pos(pos);
    }
    result
}

/// `pread64(fd, buf, count, offset)` — read at a position without moving it.
pub fn handle_pread64(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, len, offset) = (args[0] as i32, args[1], args[2], args[3] as i64);
    // A negative offset is not a position; `EOVERFLOW`-shaped nonsense is
    // still just `EINVAL` to the caller that wrote it.
    if offset < 0 {
        return err::EINVAL;
    }
    if !buffer_ok(buf, len, true, from_user) {
        return if len > MAX_IO {
            err::EINVAL
        } else {
            err::EFAULT
        };
    }
    if !crate::sched::current_fds().clone_at(fd).is_ok() {
        return err::EBADF;
    }
    at_position(fd, offset, || {
        let bytes = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
        match crate::vfs::read(fd, bytes) {
            Ok(n) => {
                if n > 0 {
                    notice_access(fd);
                }
                n as isize
            }
            Err(e) => err_of(e),
        }
    })
}

/// `pwrite64(fd, buf, count, offset)` — write at a position without moving it.
pub fn handle_pwrite64(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, buf, len, offset) = (args[0] as i32, args[1], args[2], args[3] as i64);
    if offset < 0 {
        return err::EINVAL;
    }
    if !buffer_ok(buf, len, false, from_user) {
        return if len > MAX_IO {
            err::EINVAL
        } else {
            err::EFAULT
        };
    }
    if !crate::sched::current_fds().clone_at(fd).is_ok() {
        return err::EBADF;
    }
    at_position(fd, offset, || {
        let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
        match crate::vfs::write(fd, bytes) {
            Ok(n) => n as isize,
            Err(e) => err_of(e),
        }
    })
}

/// `preadv(fd, iov, iovcnt, offset)` / `pwritev`: the vectored pair at a
/// position. Same position discipline as the scalar pair, same loop as
/// `readv`/`writev` - the offset is the only thing added.
///
/// The offset arrives already read, because the two machines spell it
/// differently and the reading is [`position_offset`]'s job: passing the raw
/// arguments down to the closure is right (the loop needs `iov`/`iovcnt`),
/// but reading the *position* from them here would read one machine's shape
/// on both.
fn io_at_position(
    fd: i32,
    offset: i64,
    args: [usize; ARG_COUNT],
    from_user: bool,
    writing: bool,
) -> isize {
    if offset < 0 {
        return err::EINVAL;
    }
    if !crate::sched::current_fds().clone_at(fd).is_ok() {
        return err::EBADF;
    }
    at_position(fd, offset, || {
        if writing {
            handle_writev(args, from_user)
        } else {
            handle_readv(args, from_user)
        }
    })
}

/// `preadv(2)` — vectored read at a position.
pub fn handle_preadv(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    io_at_position(
        args[0] as i32,
        position_offset(args),
        args,
        from_user,
        false,
    )
}

/// `pwritev(2)` — vectored write at a position.
pub fn handle_pwritev(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    io_at_position(args[0] as i32, position_offset(args), args, from_user, true)
}

/// `fsync(fd)` / `fdatasync(fd)` — flush a descriptor to its storage.
///
/// Every write here already reached its storage, so there is nothing to
/// flush: ramfs and ext2 write through, and a socket has no storage at all.
/// What the call still owes the caller is the argument check - and Linux
/// answers `EINVAL` for a descriptor bound to something that cannot be
/// synchronized, which is a pipe, a terminal or a socket.
pub fn handle_fsync(args: [usize; ARG_COUNT]) -> isize {
    let fd = args[0] as i32;
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(f) => f,
        Err(e) => return err_of(e),
    };
    // A byte channel is asked about by *name* rather than by kind, because the
    // kind cannot answer for it: a pipe and a `socketpair` end report `File`,
    // the filesystem's vocabulary having no word for "anonymous byte channel"
    // (see `pipe.rs`). Reading the kind alone therefore answered `fsync` on a
    // pipe with 0 where Linux answers EINVAL - and the two families whose
    // `kind` lies are exactly the two `as_*` below.
    let inode = file.inode();
    if inode.as_pipe().is_some() || inode.as_duplex().is_some() {
        return err::EINVAL;
    }
    match file.metadata().kind {
        NodeKind::File | NodeKind::Dir | NodeKind::Symlink | NodeKind::BlockDev => 0,
        // Nothing to flush behind a character door, and nothing behind a
        // socket's either - Linux answers EINVAL for both. The whole
        // anonymous family - a pty, an eventfd, a timerfd, an epoll, an
        // inotify, a signalfd - is a character device here, so this one arm
        // answers for all of it.
        NodeKind::CharDev | NodeKind::Socket | NodeKind::Fifo => err::EINVAL,
    }
}

/// The mode a `chmod` is allowed to store, or the errno that refuses it.
///
/// Two rules, both Linux's, and neither of them is "is the caller root". Only
/// the owner of a node - or a caller holding `CAP_FOWNER` - may change its
/// mode: the bits are the owner's to set, and letting anyone rewrite them is
/// letting anyone hand themselves the file. And a caller changing a node whose
/// group is not one of theirs loses the set-group-id bit, *owner or not*:
/// that bit says "run me as this group", so a caller outside the group has no
/// standing to grant it, and what stands in for the group is `CAP_FSETID`
/// rather than an id - which is exactly the hole a `chmod g+s` on somebody
/// else's file would otherwise open.
///
/// Both rules were measured on a current host
/// (`.workbuddy/probe_capable_scope.sh own`, one capability dropped at a time
/// from a root caller against fixtures that belong to uid 1000): with
/// `CAP_FOWNER` gone the `chmod` of another user's file is `EPERM` and the
/// `chmod` of the caller's own is served, and with `CAP_FSETID` gone
/// `chmod 2755` on the caller's own file stores **`0755`** - the call succeeds
/// and the group is dropped, because uid 0 is not a standing to grant a group
/// the caller is not in.
fn chmod_mode(inode: &Arc<dyn Inode>, mode: u32) -> Result<u32, isize> {
    // The inode's own flags first, and before the caller's identity: the bits
    // *are* the node's metadata, and `chattr +i` protects them whatever the
    // caller's uid is - `chattr(1)`'s "most of the metadata cannot be
    // modified". `CAP_LINUX_IMMUTABLE` is what clears the flag, not a way
    // around it, so there is no carve-out here either. One check, in the one
    // function all four `chmod` handlers reach.
    if crate::vfs::is_immutable(inode) {
        return Err(err::EPERM);
    }
    let caller = crate::vfs::current_caller();
    if crate::vfs::may_setattr(caller, inode).is_err() {
        return Err(err::EPERM);
    }
    let mut mode = mode & 0o7777;
    if !crate::vfs::in_group_or_capable(caller, inode.owner().1) {
        mode &= !crate::vfs::S_ISGID;
    }
    Ok(mode)
}

/// `chmod(2)` / `fchmod(2)` — set permission bits.
///
/// Two rules decide who may, both Linux's, and they live in [`chmod_mode`]:
/// the owner of the node - or a caller holding `CAP_FOWNER` - is who may
/// rewrite its bits, and a caller changing a node whose group is not one of
/// theirs does not get to keep the set-group-id bit, `CAP_FSETID` being what
/// stands in for the group. The bits are *stored*,
/// not merely accepted: it is how a package install marks its setuid helpers
/// and its scripts, and a kernel that forgets them hands back a tree where
/// `doas` says "not installed setuid" and every service script fails
/// `test -x`.
pub fn handle_chmod(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let (path, mode) = (args[0], args[1] as u32);
    let path = match cstr_arg(path, true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(inode) => match chmod_mode(&inode, mode) {
            Ok(mode) => {
                inode.set_mode(mode);
                crate::vfs::inotify::emit(&inode, crate::vfs::inotify::IN_ATTRIB, "");
                0
            }
            Err(e) => e,
        },
        Err(e) => err_of(e),
    }
}

/// `fchmod(2)` — the same, by descriptor.
pub fn handle_fchmod(args: [usize; ARG_COUNT]) -> isize {
    match crate::sched::current_fds().clone_at(args[0] as i32) {
        Ok(file) => {
            let inode = file.inode();
            match chmod_mode(&inode, args[1] as u32) {
                Ok(mode) => {
                    inode.set_mode(mode);
                    crate::vfs::inotify::emit(&inode, crate::vfs::inotify::IN_ATTRIB, "");
                    0
                }
                Err(e) => e,
            }
        }
        Err(e) => err_of(e),
    }
}

/// `chown(2)` / `fchown(2)` — set owner and group. One user, no groups:
/// the call is accepted for anything that exists, as `chmod` is.
/// The ids a `chown`-family call names, applied to what the node has now.
///
/// `-1` is the family's "leave this one alone", which is why the caller's word
/// is read as signed before it is believed; a caller that means root passes a
/// plain zero. The ids are stored rather than noted and dropped: an unpacked
/// package's files belong to whoever the archive says, and a kernel that
/// answers every `ls -l` with root:root cannot tell one user's work from
/// another's.
/// The owner a `chown` is allowed to store, or the errno that refuses it.
///
/// Linux's rules, and the asymmetry is the point. Changing the **owner** needs
/// privilege: the uid is the identity a file's permissions are measured
/// against, so naming somebody else is handing them the file, and what holds
/// that privilege is `CAP_CHOWN` - *not* the uid `0` that used to stand in for
/// it. Changing the **group** needs less - the file's owner may do it, but only
/// to a group they are themselves in, because the group is what a set-group-id
/// file runs with and naming a group you are not in would be granting yourself
/// its authority; `CAP_CHOWN` may name any group.
///
/// The ids being *the ones the node already holds* is not a standing either,
/// and that is the half the id-shaped rule got wrong. Measured
/// (`.workbuddy/probe_capable_scope.sh own`): a caller that owns neither an
/// unchanging call - `chown(2)` naming the owner the file already has, or
/// `chgrp(2)` naming its existing group - is `EPERM`, because it is still
/// naming an owner it has no standing over, while the file's own owner naming
/// *itself* is served with every capability gone. Nothing named at all
/// (`chown(path, -1, -1)`) asks nothing: neither attribute is set, so Linux
/// runs neither arm of this rule and answers `0` even to a caller holding
/// nothing.
///
/// A `-1` means "leave this one alone", which is how a caller changes one id
/// without having to know the other.
fn chown_allowed(inode: &Arc<dyn Inode>, uid_arg: i32, gid_arg: i32) -> Result<(u32, u32), isize> {
    let current = inode.owner();
    let caller = crate::vfs::current_caller();
    let mine = caller.uid == current.0;

    let uid = if uid_arg == -1 {
        current.0
    } else {
        uid_arg as u32
    };
    let gid = if gid_arg == -1 {
        current.1
    } else {
        gid_arg as u32
    };

    // The uid arm: the node's own owner naming the id it already has, or
    // `CAP_CHOWN`. A caller that is not the owner is refused even when the id
    // it names is the one already there.
    if uid_arg != -1 && !(caller.overrides.chown || (mine && uid == current.0)) {
        return Err(err::EPERM);
    }
    // The gid arm: the same owner, naming one of their own groups or the group
    // the node already has, or `CAP_CHOWN`.
    if gid_arg != -1
        && !(caller.overrides.chown
            || (mine && (gid == current.1 || crate::sched::current_groups().contains(&gid))))
    {
        return Err(err::EPERM);
    }
    Ok((uid, gid))
}

/// Store a new owner for `inode`, applying the rules above and dropping the
/// set-uid and set-group-id bits when an unprivileged caller is the one
/// changing it.
///
/// Those two bits are the authority the file runs with. A caller who could
/// rewrite who a file belongs to *and* keep them would be choosing its own
/// privileges, which is the whole of what the rule prevents.
///
/// It is not, however, a rule about the *caller*, which is what this kernel
/// wrote down (`uid != 0`). Measured whole on a current host
/// (`.workbuddy/probe_chown_bits.c`, a root caller with one capability dropped
/// at a time against fixtures carrying both bits):
///
/// ```text
///                                            whole set  -FSETID  -CHOWN  -FOWNER
/// chown(0,-1)  6755 own group (no change)      0755      0755     0755    0755
/// chown(-1,0)  6755 own group (no change)      0755      0755     0755    0755
/// chown(-1,-1) 6755 (nothing named at all)     0755      0755     0755    0755
/// chown(0,-1)  6644 (sgid, no group-x)         2644      2644     2644    2644
/// chown(0,-1)  4644 (suid, no exec)            0644      0644     0644    0644
/// chown(0,-1)  6755 directory                  6755      6755     6755    6755
/// ```
///
/// Four things, none of them a capability's. Naming an id is not required -
/// `chown(-1,-1)` takes the bits away too - and neither is *changing* one, so
/// the test is not "did the owner move". The set-uid bit goes whatever else is
/// true, even on a file with no execute bit at all. The set-group-id bit goes
/// only where the group may execute, because without it the bit is a locking
/// mark rather than an authority. And a directory keeps both, since neither
/// means an authority there.
fn chown_apply(inode: &Arc<dyn Inode>, uid_arg: i32, gid_arg: i32) -> isize {
    // The same refusal as `chmod`, and for the same reason: the owner is
    // metadata, and an immutable inode's metadata is what the flag protects.
    // One check for all four `chown` handlers, which reach this through
    // `handle_chown`, `handle_fchown`, `handle_fchownat` and `handle_lchown`.
    if crate::vfs::is_immutable(inode) {
        return err::EPERM;
    }
    let (uid, gid) = match chown_allowed(inode, uid_arg, gid_arg) {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    inode.set_owner(uid, gid);
    crate::vfs::inotify::emit(inode, crate::vfs::inotify::IN_ATTRIB, "");
    // What the chown takes away - see the function's table. Nothing here asks
    // the caller's ids or its capabilities, because the measurement says no
    // caller's does: the two bits go for root holding the whole set as they do
    // for anybody else, and for a call that named nothing as for one that moved
    // an id. The one thing that spares the set-group-id bit is the absence of
    // group execute, and the one thing that spares both is being a directory.
    if inode.keeps_modes() && inode.metadata().kind != crate::vfs::NodeKind::Dir {
        // The two clauses are [`crate::vfs::without_set_id`]'s, which the
        // write path asks too: the set-uid bit always, the set-group-id bit
        // only where the group may execute.
        let mode = inode.mode();
        let next = crate::vfs::without_set_id(mode);
        if next != mode {
            inode.set_mode(next);
        }
    }
    0
}

pub fn handle_chown(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(inode) => chown_apply(&inode, args[1] as i32, args[2] as i32),
        Err(e) => err_of(e),
    }
}

/// `fchown(2)` — the same, by descriptor.
pub fn handle_fchown(args: [usize; ARG_COUNT]) -> isize {
    match crate::sched::current_fds().clone_at(args[0] as i32) {
        Ok(file) => {
            let inode = file.inode();
            chown_apply(&inode, args[1] as i32, args[2] as i32)
        }
        Err(e) => err_of(e),
    }
}

/// `fchownat(dirfd, path, uid, gid, flags)` — the path form, and the only one
/// this architecture has: there is no `lchown` syscall, so this is what musl's
/// `lchown` becomes.
///
/// The flag is the reason the call exists here at all. `AT_SYMLINK_NOFOLLOW` is
/// what makes it able to name a *link* rather than what it points at, which is
/// the whole difference between `chown` and `lchown` - and a file that is not
/// there is still an error, because the caller is naming something.
pub fn handle_fchownat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // `AT_EMPTY_PATH` with an empty name is the `fchown`-by-descriptor spelling:
    // set the owner of whatever the fd holds, not of a path. This is how `apk`
    // gives an `O_TMPFILE` file its owner before naming it, and without it the
    // empty name resolves to nothing and the preserve fails.
    //
    // An empty name is answered before the dirfd is looked at, which is Linux's
    // order for this call: under `AT_EMPTY_PATH` the descriptor *is* the target,
    // and without it the name is the whole request.
    if path.is_empty() {
        if args[4] & AT_EMPTY_PATH == 0 {
            return err::ENOENT;
        }
        return match crate::sched::current_fds().clone_at(args[0] as i32) {
            Ok(file) => chown_apply(&file.inode(), args[2] as i32, args[3] as i32),
            Err(e) => err_of(e),
        };
    }
    let base = match base_for(args[0] as i32, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let found = if args[4] & AT_SYMLINK_NOFOLLOW != 0 {
        crate::vfs::resolve_no_follow_at(&path, base)
    } else {
        crate::vfs::resolve_opt(&path, Some(base))
    };
    // The name is resolved first, because a caller that named something wrong
    // has to hear about it rather than be told the owner was set.
    match found {
        Ok(inode) => chown_apply(&inode, args[2] as i32, args[3] as i32),
        Err(e) => err_of(e),
    }
}

/// `lchown(path, uid, gid)` — the owner of the link itself.
///
/// This is `fchownat` with the one flag that makes it name the link rather than
/// what it points at, which is the whole of what `lchown` is. A tool unpacking
/// an archive sets the owner of every member, and a link is a member like any
/// other.
pub fn handle_lchown(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    handle_fchownat(
        [
            AT_FDCWD as usize,
            args[0],
            args[1],
            args[2],
            AT_SYMLINK_NOFOLLOW,
            0,
        ],
        cwd,
    )
}

/// `linkat(olddirfd, oldpath, newdirfd, newpath, flags)` — a second name
/// for one inode.
///
/// The VFS has no link count: every node has one name, and `unlink` drops
/// the node with it. Linux answers `EPERM` when the filesystem does not
/// support hard links, which is exactly this, so a caller's fallback -
/// copy the file - is what it would do on a filesystem that cannot link.
pub fn handle_linkat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    // This is the family's one door that asks its two names *in sequence*: the
    // old name, its descriptor and its walk all come before the new name is read
    // at all. Measured on a current host, the four combinations say so - a
    // missing old name is ENOENT with *any* new-side answer (a bad new dirfd,
    // which alone would be EBADF, and an unreadable new pointer, which alone
    // would be EFAULT), while a resolvable old name beside a bad new dirfd is
    // that descriptor's EBADF and beside an unreadable new pointer is EFAULT.
    // Linux's sequence for this call is the same (`filename_lookup` of the old
    // name before `getname` of the new one); `renameat2` below is the family's
    // other shape, reading both names before it resolves either.
    let old_path = match cstr_arg(args[1], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let old_base = match base_for(args[0] as i32, &old_path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    // The old name has to exist as a path for the request to be a request:
    // a link to a missing file is ENOENT, not EPERM.
    let old = match crate::vfs::resolve_no_follow_at(&old_path, old_base.clone()) {
        Ok(inode) => inode,
        Err(e) => return err_of(e),
    };
    // `AT_SYMLINK_FOLLOW` says the source is the *file* the name leads to
    // rather than the name. Without it a link to a link is what was asked
    // for, which is the same reading `AT_SYMLINK_NOFOLLOW` gives every other
    // call. With it a stored link is resolved again following it, and a
    // descriptor's second name under `/proc` resolves to the file that
    // descriptor holds - the walk denotes it, exactly as it does for `stat`
    // and the rest (see `crate::vfs::file::descriptor_inode`). That is how a
    // file with no name of its own gets one, which is what `apk` does with the
    // repository index it wrote through `O_TMPFILE`.
    let followed = if args[4] & abi::AT_SYMLINK_FOLLOW != 0 {
        match followed_link(&old_path, old_base) {
            Ok(inode) => Some(inode),
            Err(e) => return err_of(e),
        }
    } else {
        None
    };
    let new_path = match cstr_arg(args[3], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let new_base = match base_for(args[2] as i32, &new_path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let via_descriptor = followed.is_some() && old.as_fd().is_some();
    let old = followed.unwrap_or(old);
    let (parent_path, name) = match crate::vfs::split_parent(&new_path) {
        Ok(p) => p,
        Err(e) => return err_of(e),
    };
    let parent = match crate::vfs::resolve_opt(parent_path, Some(new_base)) {
        Ok(parent) => parent,
        Err(e) => return err_of(e),
    };
    // A name edge belongs to the filesystem that keeps the directory. An old
    // inode served by a different filesystem cannot be reached by one of this
    // one's edges, which is `link(2)`'s EXDEV - the caller's cue to copy.
    //
    // The source is asked for its filesystem in whichever way it can answer.
    // While the source is still a *name*, the deepest mountpoint over each
    // path is the answer - the rule `st_dev` uses, and the only one that also
    // places a node *inside* a mount, which no inode of its own can
    // (`/proc/mounts` is procfs, and only its path says so). Followed through
    // a descriptor it is not a name any more: `/proc/self/fd/7` names procfs
    // while standing for a file on the root filesystem, so the file is asked
    // instead, by the same identity test `fstatfs` uses on a descriptor.
    //
    // The identity test is the weaker of the two for a node inside a mount
    // that is not the mount's own root - such an inode cannot say which
    // filesystem it belongs to, and the answer falls back to the root's - and
    // it can therefore refuse a link that would have worked. A refusal is the
    // safe direction: EXDEV's meaning is "copy instead", which is what the
    // caller does, and the alternative is a directory entry naming an inode
    // the filesystem does not own.
    //
    // Whose table it is asked of is not a detail: `fs_serving_inode_of` asks the
    // *caller's*, because a task with mounts of its own is resolving in a tree
    // the machine's table does not describe. Asked of the machine's, this check
    // compares two nodes of a tree the caller is not in and answers about them.
    let same_filesystem = if via_descriptor {
        match (
            crate::vfs::fs_serving_inode_of(&old),
            crate::vfs::fs_serving_inode_of(&parent),
        ) {
            (Some(a), Some(b)) => Arc::ptr_eq(&a, &b),
            _ => false,
        }
    } else {
        crate::vfs::mount::fs_of_path(&absolute(&old_path)).0
            == crate::vfs::mount::fs_of_path(&absolute(parent_path)).0
    };
    if !same_filesystem {
        return err::EXDEV;
    }
    // Linux's `may_linkat`, before the link is made: a second name for an
    // immutable or append-only inode is refused, and the caller hears it as
    // `EPERM`. It is *this* side of the check that matters - the destination
    // directory's own flags are `may_create_in`'s business, and it was asked
    // above.
    if let Err(e) = crate::vfs::may_alter(&old) {
        return err_of(e);
    }
    match parent.link(name, &old) {
        Ok(()) => {
            // A second name is a new entry in the destination directory -
            // the create event belongs to it, and `IN_ISDIR` describes the
            // inode the name now points at rather than the directory, with
            // the watched inode's own `IN_ATTRIB`-style news left to a later
            // hook family.
            crate::vfs::inotify::emit_child(
                &parent,
                crate::vfs::inotify::IN_CREATE,
                name,
                old.metadata().kind == crate::vfs::NodeKind::Dir,
            );
            0
        }
        Err(e) => err_of(e),
    }
}

/// `mknod(path, mode, dev)` — create a device or FIFO node.
///
/// The nodes an initramfs's init pre-seeds (`[ -c /dev/null ] || mknod
/// /dev/null c 1 3`) land on the temporary root, which is a ramfs: they are
/// placeholders the `devtmpfs` mount then covers with driver-backed devices.
/// The guard in that idiom is the *caller's*, and it is there because the call
/// itself is not idempotent: measured on a current host through `syscall(2)`
/// (`.workbuddy/probe_mknod.c`), a name that already exists is `EEXIST` — for
/// the same type, for a different one, and through this legacy spelling and
/// `mknodat` alike. This door answered `0` and called the caller's intent its
/// own ("the caller wanted the node, not the call"), which is a success where
/// Linux refuses: a script that checks whether a node exists *by making it*
/// read a yes for a name somebody else had already made.
pub fn handle_mknod(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    // The type is the first question, before the name is even copied - Linux's
    // `may_mknod`, whose two answers (`S_IFDIR` `EPERM`, an uncreatable type
    // `EINVAL`) come back over an *empty* name as readily as over an existing
    // one, measured through `syscall(2)` (`.workbuddy/probe_mknod.c`). Linux's
    // legacy `mknod` is `do_mknodat(AT_FDCWD, …)`, so every rule in
    // [`handle_mknodat`] is this call's too, in the same order.
    if let Err(e) = abi::mknod_type_is_creatable(args[1]) {
        return e;
    }
    let path = match cstr_arg(args[0], true) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let absolute = if path.starts_with('/') {
        crate::vfs::path::normalize(&path)
    } else {
        let cwd_path = crate::sched::current_cwd_path();
        crate::vfs::path::normalize(&alloc::format!(
            "{}/{}",
            cwd_path.trim_end_matches('/'),
            path
        ))
    };
    // A name that is already there is `EEXIST`, and the question is about the
    // *name* rather than about a path: asked of the directory that holds it, a
    // dangling link is a name that exists and answers the same way Linux does -
    // and so is a name a mount covers, which is why this is [`resolve_entry_at`]
    // and not the no-follow *walk* beside it.
    if crate::vfs::resolve_entry_at(&absolute, cwd.clone()).is_ok() {
        return err::EEXIST;
    }
    let (parent_path, name) = match absolute.rsplit_once('/') {
        Some(("", n)) => (String::from("/"), n),
        Some((p, n)) => (String::from(p), n),
        None => return err::ENOENT,
    };
    let parent = match crate::vfs::resolve_opt(&parent_path, Some(cwd.clone())) {
        Ok(p) => p,
        Err(e) => return err_of(e),
    };
    // The directory's own bits decide whether a name may be made in it, and the
    // question comes after `EEXIST` (Linux's `may_create` asks both, in that
    // order) - so a name that is already there is `EEXIST` even in a directory
    // the caller may not write.
    if let Err(e) = crate::vfs::may_create_in(&parent) {
        return err_of(e);
    }
    // The kind, once the name is settled - the position Linux asks for a device
    // node's `CAP_MKNOD` in, and where a FIFO's refusal belongs, so
    // `mknodat(existing, S_IFIFO)` above is the `EEXIST` Linux answers for a
    // type it serves.
    let kind = match abi::mknod_kind(args[1]) {
        Ok(kind) => kind,
        Err(e) => return e,
    };
    // Linux's `CAP_MKNOD`, and the last question this call has: only a *device*
    // node asks for it, and the caller it asks about is the one running -
    // its **effective set**, never its name. Measured as an unprivileged caller
    // on a current host through `syscall(2)` (`.workbuddy/probe_mknod_cap.c`,
    // Linux 7.2.9, and beside the type field: `.workbuddy/probe_mknod.c`): a
    // character or block node is `EPERM` there, a `S_IFREG`/`S_IFIFO`/`S_IFSOCK`
    // node is served to a caller holding nothing, and the refusals above
    // answer first - `EEXIST` over a name that exists, `EACCES` in a directory
    // the caller may not write, `ENOENT` for a path that is not there. The one
    // device node that needs none is the character node whose `dev` is `0`,
    // which is how overlayfs spells a whiteout; `abi::mknod_needs_capability`
    // holds that rule and its measurement - and the capability is asked of the
    // **initial** user namespace, which is Linux's `capable(CAP_MKNOD)` rather
    // than `ns_capable(current_user_ns(), ...)`: measured
    // (`.workbuddy/probe_capable_scope.sh`), a caller that has entered a user
    // namespace is uid 0 *in it*, reads `CapEff` `0x1ffffffffff` back, and is
    // refused a device node in a directory the *parent* namespace owns while an
    // ordinary file in that same directory is served.
    if abi::mknod_needs_capability(args[1], args[2])
        && !crate::sched::current_credentials().capable(crate::cred::CAP_MKNOD)
    {
        return err::EPERM;
    }
    match parent.create(name, kind) {
        Ok(created) => {
            created.set_mode(masked_mode(args[1] as u32));
            0
        }
        Err(e) => err_of(e),
    }
}

/// `fadvise64(fd, offset, len, advice)` — a hint about coming access.
///
/// Nothing here reads ahead or caches evictable pages, so every advice is
/// accepted and does nothing - which is what Linux does for the advices it
/// does not act on. The descriptor still has to be real.
pub fn handle_fadvise64(args: [usize; ARG_COUNT]) -> isize {
    // `posix_fadvise(2)` is a hint, but the hint's vocabulary is closed and the
    // descriptor's kind still matters. Measured on a current host: advice
    // `0..=5` (`NORMAL`, `RANDOM`, `SEQUENTIAL`, `WILLNEED`, `DONTNEED`,
    // `NOREUSE`) answers `0`, an advice past the set answers `EINVAL`, a number
    // that is not open answers `EBADF`, and a pipe answers `ESPIPE` - there is
    // no byte range to give advice about. Returning `0` for any advice on any
    // open descriptor was the accept-and-drop this removes: an unknown advice
    // got a success it was never owed, and a pipe was told it had a page cache.
    // This kernel prefetches nothing, so a *recognised* hint answered `0` is
    // truthful (best-effort, no observable promise); the refusals are what make
    // the vocabulary honest.
    let file = match crate::sched::current_fds().clone_at(args[0] as i32) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    if !matches!(args[3], 0..=5) {
        return err::EINVAL;
    }
    if file.inode().as_pipe().is_some() {
        return err::ESPIPE;
    }
    0
}

// ---------------------------------------------------------------------------
// epoll: the interest-list multiplexer (epoll_create(2), epoll_ctl(2),
// epoll_wait(2)). The interest list itself is `vfs::epoll`; this is the
// argument-shaping half - flag validation in the man page's order, the
// packed event record, and the three ways a wait can end.
// ---------------------------------------------------------------------------

/// `EPOLL_CLOEXEC`: the one flag `epoll_create1` takes. Its value is
/// `O_CLOEXEC` under the other name, as the header spells it.
const EPOLL_CLOEXEC: usize = 0o200_0000;

/// `epoll_create(size)` — the original spelling, whose size argument has
/// been ignored since Linux 2.6.8 but must still be positive: the man page
/// keeps the check even after losing the meaning.
pub fn handle_epoll_create(args: [usize; ARG_COUNT]) -> isize {
    if args[0] as i64 <= 0 {
        return err::EINVAL;
    }
    epoll_create(0)
}

/// `epoll_create1(flags)` — the same instance, the modern flag word.
pub fn handle_epoll_create1(args: [usize; ARG_COUNT]) -> isize {
    if args[0] & !EPOLL_CLOEXEC != 0 {
        return err::EINVAL;
    }
    epoll_create(args[0])
}

/// The shared half of both creations: the instance, and the descriptor
/// carrying the close-on-exec flag that belongs to the number, not to the
/// instance.
fn epoll_create(flags: usize) -> isize {
    let fd = match crate::vfs::open_inode(crate::vfs::epoll::Epoll::new(next_anon_ino())) {
        Ok(fd) => fd,
        Err(e) => return err_of(e),
    };
    if flags & EPOLL_CLOEXEC != 0 {
        let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
    }
    fd as isize
}

/// `epoll_ctl(epfd, op, fd, event)` — add, change or drop one entry of the
/// interest list.
///
/// The refusal order is the man page's: an operation outside the three
/// answers EINVAL before a descriptor is looked at; a number that is not
/// open answers EBADF; an open number that is not an epoll answers EINVAL -
/// the line between "the number is bad" and "the thing it names is the wrong
/// kind".
pub fn handle_epoll_ctl(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (epfd, op, fd, event) = (args[0] as i32, args[1], args[2] as i32, args[3]);
    if !matches!(
        op,
        crate::vfs::epoll::EPOLL_CTL_ADD
            | crate::vfs::epoll::EPOLL_CTL_MOD
            | crate::vfs::epoll::EPOLL_CTL_DEL
    ) {
        return err::EINVAL;
    }
    let file = match crate::sched::current_fds().clone_at(epfd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    let inode = file.inode();
    let Some(instance) = inode.as_epoll() else {
        return err::EINVAL;
    };
    // DEL ignores the event pointer altogether - it may be NULL. ADD and
    // MOD read it, and the record is packed: the caller's data word sits at
    // byte 4, not 8, which is the one thing a word-aligned read would get
    // silently wrong.
    let (events, data) = if op == crate::vfs::epoll::EPOLL_CTL_DEL {
        (0, 0)
    } else {
        if !buffer_ok(event, crate::vfs::epoll::EPOLL_EVENT_SIZE, false, from_user) {
            return err::EFAULT;
        }
        unsafe {
            (
                (event as *const u32).read_unaligned(),
                ((event + crate::vfs::epoll::EPOLL_EVENT_DATA) as *const u64).read_unaligned(),
            )
        }
    };
    match instance.ctl(&crate::sched::current_fds(), epfd, op, fd, events, data) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/// The wait `epoll_wait`, `epoll_pwait` and `epoll_pwait2` are all made
/// of: the interest list scanned, the ready entries written out in the
/// packed twelve-byte record, and the three ways out - events, the clock,
/// a signal - that `block_until_deadline` already spells. The table is
/// taken once, before the wait: the condition runs inside the scheduler's
/// own borrow of it, where taking it again is not a thing.
fn epoll_wait(args: [usize; ARG_COUNT], deadline: Option<u64>, from_user: bool) -> isize {
    let (epfd, events, maxevents) = (args[0] as i32, args[1], args[2] as i64);
    if maxevents <= 0 {
        return err::EINVAL;
    }
    let maxevents = maxevents as usize;
    let file = match crate::sched::current_fds().clone_at(epfd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    let inode = file.inode();
    let Some(instance) = inode.as_epoll() else {
        return err::EINVAL;
    };
    let len = match maxevents.checked_mul(crate::vfs::epoll::EPOLL_EVENT_SIZE) {
        Some(len) => len,
        None => return err::EINVAL,
    };
    if !buffer_ok(events, len, true, from_user) {
        return err::EFAULT;
    }

    let fds = crate::sched::current_fds();
    // The collected reports live in a cell: the wait's condition is called
    // by the scheduler as an `Fn`, and a cell is how a condition keeps
    // state without claiming to be anything but a read.
    let out_cell = core::cell::RefCell::new(alloc::vec::Vec::with_capacity(maxevents));
    let me = crate::sched::current();
    instance.subscribe_waits(me);
    let answer = crate::sched::block_until_deadline(
        || {
            let mut out = out_cell.borrow_mut();
            instance.collect(&fds, &mut out);
            !out.is_empty()
        },
        deadline.unwrap_or(u64::MAX),
    );
    instance.unsubscribe_waits(me);
    let out = out_cell.into_inner();

    match answer {
        crate::sched::WaitEnd::Interrupted => return err::EINTR,
        crate::sched::WaitEnd::Deadline => return 0,
        crate::sched::WaitEnd::Condition => {}
    }
    for (index, report) in out.iter().enumerate() {
        unsafe {
            let at = events + index * crate::vfs::epoll::EPOLL_EVENT_SIZE;
            (at as *mut u32).write_unaligned(report.events);
            ((at + crate::vfs::epoll::EPOLL_EVENT_DATA) as *mut u64).write_unaligned(report.data);
        }
    }
    out.len() as isize
}

/// `epoll_wait(epfd, events, maxevents, timeout)` — the timeout is a
/// millisecond count, and the two special values are a signed integer's
/// ends: -1 waits for ever, 0 answers what is ready right now.
pub fn handle_epoll_wait(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let timeout = args[3] as i64;
    let deadline = (timeout >= 0).then(|| deadline_in_ticks(timeout as u64));
    epoll_wait(args, deadline, from_user)
}

/// `epoll_pwait(epfd, events, maxevents, timeout, sigmask)` — the wait
/// with a signal mask swapped in for the duration. This kernel delivers
/// signals on the way out of a syscall rather than by interrupting one,
/// so the mask is read for validity and not acted on - the same treatment
/// `ppoll` gives it, and for the same reason.
pub fn handle_epoll_pwait(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    if args[4] != 0 && !buffer_ok(args[4], 8, false, from_user) {
        return err::EFAULT;
    }
    handle_epoll_wait(args, from_user)
}

/// `epoll_pwait2(epfd, events, maxevents, timeout, sigmask, sigsetsize)` -
/// the nanosecond spelling of the pwait pair. The timeout is a
/// `struct timespec *`, NULL naming the endless wait; the mask size is the
/// ABI's eight bytes, and anything else is EINVAL before a thing is read.
pub fn handle_epoll_pwait2(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    if args[4] != 0 {
        if args[5] != 8 {
            return err::EINVAL;
        }
        if !buffer_ok(args[4], 8, false, from_user) {
            return err::EFAULT;
        }
    }
    let deadline = match ppoll_deadline(args[3], from_user) {
        Ok(deadline) => deadline,
        Err(e) => return e,
    };
    epoll_wait(args, deadline, from_user)
}

// ---------------------------------------------------------------------------
// timerfd: the timer that reports through a descriptor (timerfd_create(2)).
// The state machine is `vfs::timerfd`; this is the argument-shaping half,
// including the timespec arithmetic the tick clock measures in.
// ---------------------------------------------------------------------------

/// `timerfd_create(clockid, flags)`.
pub fn handle_timerfd_create(args: [usize; ARG_COUNT]) -> isize {
    let (clockid, flags) = (args[0], args[1]);
    if !crate::vfs::timerfd::clock_ok(clockid) {
        return err::EINVAL;
    }
    if flags & !(crate::vfs::timerfd::TFD_CLOEXEC | crate::vfs::timerfd::TFD_NONBLOCK) != 0 {
        return err::EINVAL;
    }
    let fd = match crate::vfs::open_inode(crate::vfs::timerfd::TimerFd::new(
        next_anon_ino(),
        clockid,
        flags & crate::vfs::timerfd::TFD_NONBLOCK != 0,
    )) {
        Ok(fd) => fd,
        Err(e) => return err_of(e),
    };
    if flags & crate::vfs::timerfd::TFD_CLOEXEC != 0 {
        let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
    }
    if flags & crate::vfs::timerfd::TFD_NONBLOCK != 0
        && let Ok(file) = crate::sched::current_fds().clone_at(fd)
    {
        // The status word is set as well, so a `fcntl(F_GETFL)` caller
        // reads back the nonblocking it asked for - the same bookkeeping
        // `signalfd4` keeps.
        file.set_status(crate::vfs::file::O_NONBLOCK);
    }
    fd as isize
}

/// `inotify_init1(flags)` — a fresh event queue. The flags are the `O_`
/// pair under other names and the refusal of anything else is the man
/// page's; the queue itself lives in `src/vfs/inotify.rs`.
pub fn handle_inotify_init1(args: [usize; ARG_COUNT]) -> isize {
    let flags = args[0];
    if flags & !(crate::vfs::inotify::IN_CLOEXEC | crate::vfs::inotify::IN_NONBLOCK) != 0 {
        return err::EINVAL;
    }
    let Some(instance) = crate::vfs::inotify::InotifyInstance::try_new(
        next_anon_ino(),
        flags & crate::vfs::inotify::IN_NONBLOCK != 0,
    ) else {
        // The system-wide instance cap, answered with Linux's errno.
        return err::EMFILE;
    };
    let fd = match crate::vfs::open_inode(instance) {
        Ok(fd) => fd,
        Err(e) => return err_of(e),
    };
    // An inotify queue is readable and nothing else, and Linux says so in the
    // word a caller can read back: measured on a current host,
    // `fcntl(F_GETFL)` on this descriptor answers `O_RDONLY` (0) where a
    // `timerfd`, an `eventfd`, a `signalfd`, an `epoll` and a socket all answer
    // `O_RDWR` (2). The description is what carries the direction here.
    if let Ok(file) = crate::sched::current_fds().clone_at(fd) {
        file.set_access(crate::vfs::file::ACC_RDONLY);
    }
    if flags & crate::vfs::inotify::IN_CLOEXEC != 0 {
        let _ = crate::sched::current_fds().fd_flags_set(fd, crate::vfs::file::FD_CLOEXEC);
    }
    if flags & crate::vfs::inotify::IN_NONBLOCK != 0
        && let Ok(file) = crate::sched::current_fds().clone_at(fd)
    {
        file.set_status(crate::vfs::file::O_NONBLOCK);
    }
    fd as isize
}

/// `inotify_add_watch(fd, pathname, mask)` — watch one inode.
///
/// The mask's rules are the man page's, in order: unknown bits are
/// `EINVAL`, and the two merge flags that contradict each other
/// (`IN_MASK_ADD` with `IN_MASK_CREATE`) are `EINVAL` together. The path is
/// resolved from the caller's cwd - this call has no dirfd to ask - and
/// `IN_ONLYDIR` refuses anything that is not a directory with `EINVAL`,
/// which is also Linux's answer. The dedup, merge and cap decisions live
/// in [`crate::vfs::inotify::add_watch`]; this half only translates them
/// into errno numbers and a watch descriptor.
pub fn handle_inotify_add_watch(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    use crate::vfs::inotify::{self, AddOutcome};
    // Linux's argument order is (fd, pathname, mask) - the pointer is the
    // second word, the mask the third, and reading them the other way made
    // a path pointer look like an impossible mask.
    let (fd, path_ptr) = (args[0] as i32, args[1]);
    let in_mask = args[2] as u32;
    if in_mask & !inotify::IN_ON_ALL_EVENTS != 0
        || in_mask & (inotify::IN_MASK_ADD | inotify::IN_MASK_CREATE)
            == (inotify::IN_MASK_ADD | inotify::IN_MASK_CREATE)
    {
        return err::EINVAL;
    }
    let Ok(file) = crate::sched::current_fds().clone_at(fd) else {
        return err::EBADF;
    };
    let node = file.inode();
    if node.as_inotify().is_none() {
        return err::EINVAL;
    }
    let path = match cstr_arg(path_ptr, true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let watched = match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(node) => node,
        Err(e) => return err_of(e),
    };
    if in_mask & inotify::IN_ONLYDIR != 0 && watched.metadata().kind != crate::vfs::NodeKind::Dir {
        return err::EINVAL;
    }
    let owner = node.metadata().ino;
    match inotify::add_watch(&node, owner, inotify::key_of(&watched), in_mask) {
        AddOutcome::New(wd) | AddOutcome::Replaced(wd) | AddOutcome::Exists(wd) => wd as isize,
        AddOutcome::NoSpace => err::ENOSPC,
        AddOutcome::NotAnInstance => err::EINVAL,
    }
}

/// `inotify_rm_watch(fd, wd)` — drop one watch. The wd belongs to the
/// instance behind the descriptor; an unknown wd is the man page's
/// `EINVAL`, and the watch's death is reported to the queue as
/// `IN_IGNORED` - the record that lets `inotifywait -m` stop printing a
/// watch it no longer has.
pub fn handle_inotify_rm_watch(args: [usize; ARG_COUNT]) -> isize {
    let (fd, wd) = (args[0] as i32, args[1] as i32);
    let Ok(file) = crate::sched::current_fds().clone_at(fd) else {
        return err::EBADF;
    };
    let node = file.inode();
    let Some(instance) = node.as_inotify() else {
        return err::EINVAL;
    };
    if crate::vfs::inotify::remove_watch(instance, node.metadata().ino, wd) {
        0
    } else {
        err::EINVAL
    }
}

/// `timerfd_settime(fd, flags, new_value, old_value)` — arm, re-arm or
/// disarm the timer.
///
/// The flag vocabulary is small and its one rule is strict: the
/// cancel-on-set flag is only a word beside an absolute arm of the wall
/// clock, because only that arm can be invalidated by somebody setting the
/// clock underneath it.
pub fn handle_timerfd_settime(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, flags, new_value, old_value) = (args[0] as i32, args[1], args[2], args[3]);
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    let inode = file.inode();
    let Some(timer) = inode.as_timerfd() else {
        return err::EINVAL;
    };
    if flags
        & !(crate::vfs::timerfd::TFD_TIMER_ABSTIME | crate::vfs::timerfd::TFD_TIMER_CANCEL_ON_SET)
        != 0
    {
        return err::EINVAL;
    }
    let cancel_on_set = flags & crate::vfs::timerfd::TFD_TIMER_CANCEL_ON_SET != 0;
    let on_wall_clock = matches!(
        timer.clockid(),
        crate::vfs::timerfd::CLOCK_REALTIME | crate::vfs::timerfd::CLOCK_REALTIME_ALARM
    );
    if cancel_on_set && (flags & crate::vfs::timerfd::TFD_TIMER_ABSTIME == 0 || !on_wall_clock) {
        return err::EINVAL;
    }
    if new_value == 0
        || !buffer_ok(
            new_value,
            crate::vfs::timerfd::ITIMERSPEC_SIZE,
            false,
            from_user,
        )
    {
        return err::EFAULT;
    }
    // `struct itimerspec`: the interval first, the initial value second -
    // the reverse of `struct itimerval`, which is the thing a guess here
    // would hand back wrong. Each half is a `struct timespec`, seconds
    // then nanoseconds.
    let (interval, initial) = unsafe {
        let word = |offset: usize| (new_value + offset) as *const u64;
        (
            (word(0).read_unaligned(), word(8).read_unaligned()),
            (
                word(crate::vfs::timerfd::ITIMERSPEC_VALUE).read_unaligned(),
                word(crate::vfs::timerfd::ITIMERSPEC_VALUE + 8).read_unaligned(),
            ),
        )
    };
    let hz = crate::hal::TICK_HZ as u64;
    let interval_ticks = match crate::vfs::timerfd::ticks_of_timespec(interval.0, interval.1, hz) {
        Ok(ticks) => ticks,
        Err(e) => return e,
    };
    let now = crate::hal::ticks();
    let deadline = if initial.0 == 0 && initial.1 == 0 {
        // The man page's disarm: a zero `it_value` is the whole spelling.
        None
    } else {
        // The value as this clock reads it, in ticks. A relative arm
        // counts from now; an absolute one names a point the clock has
        // either reached (the deadline is due) or has not.
        let at = match crate::vfs::timerfd::ticks_of_timespec(initial.0, initial.1, hz) {
            Ok(ticks) => ticks,
            Err(e) => return e,
        };
        if flags & crate::vfs::timerfd::TFD_TIMER_ABSTIME == 0 {
            Some(now.saturating_add(at))
        } else if on_wall_clock {
            // The wall clock is the RTC's boot anchor plus the tick
            // count, so a point named on it is that many ticks past the
            // anchor - and one already past is a deadline of zero, which
            // the state machine counts the next time it looks.
            Some(at.saturating_sub(crate::hal::boot_epoch().saturating_mul(hz)))
        } else {
            // The monotonic clock *is* the tick count: an absolute point
            // on it is already the number this state machine keeps.
            Some(at)
        }
    };
    let old = timer.set(now, deadline, interval_ticks, cancel_on_set);
    if old_value != 0 {
        write_itimerspec(old_value, old, from_user);
    }
    0
}

/// `timerfd_gettime(fd, curr_value)` — what is left of the current span
/// and the interval between spans. A disarmed timer reports zeros for both.
pub fn handle_timerfd_gettime(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (fd, curr) = (args[0] as i32, args[1]);
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    let inode = file.inode();
    let Some(timer) = inode.as_timerfd() else {
        return err::EINVAL;
    };
    if !buffer_ok(curr, crate::vfs::timerfd::ITIMERSPEC_SIZE, true, from_user) {
        return err::EFAULT;
    }
    write_itimerspec(curr, timer.snapshot(crate::hal::ticks()), from_user);
    0
}

/// Write the `struct itimerspec` a timer reports: interval first, then the
/// time left, each a `struct timespec` in the tick clock's own resolution.
/// `None` is the disarmed spelling, zeros throughout.
fn write_itimerspec(buf: usize, snapshot: Option<(u64, u64)>, from_user: bool) {
    if !buffer_ok(buf, crate::vfs::timerfd::ITIMERSPEC_SIZE, true, from_user) {
        return;
    }
    let hz = crate::hal::TICK_HZ as u64;
    let (remaining, interval) = snapshot.unwrap_or((0, 0));
    let (rsec, rnsec) = crate::vfs::timerfd::timespec_of_ticks(remaining, hz);
    let (isec, insec) = crate::vfs::timerfd::timespec_of_ticks(interval, hz);
    unsafe {
        let word = |offset: usize| (buf + offset) as *mut u64;
        word(0).write_unaligned(isec);
        word(8).write_unaligned(insec);
        word(crate::vfs::timerfd::ITIMERSPEC_VALUE).write_unaligned(rsec);
        word(crate::vfs::timerfd::ITIMERSPEC_VALUE + 8).write_unaligned(rnsec);
    }
}

// ---------------------------------------------------------------------------
// fallocate(2): arrange a file's storage before writing it
// ---------------------------------------------------------------------------

/// `fallocate(fd, mode, offset, len)` — lay out the storage of a byte
/// range ahead of the writes that fill it.
///
/// The default operation is real here: extending the file to the range's
/// end is what "the space is allocated" means on this kernel, and the
/// zero-filled extension `truncate(2)` gives is the contract the man page
/// spells — the size grows, and the new range reads as zeros.
/// `FALLOC_FL_KEEP_SIZE` is accepted and remembered nowhere: the write
/// that later fills the range allocates as it goes, so the promise the
/// flag makes — that writes into the range succeed — holds without it.
/// The operations that reshape or free an *interior* range are refused
/// with the errno Linux gives for a filesystem without the feature,
/// which is the honest answer from one that cannot undo a byte in place.
pub fn handle_fallocate(args: [usize; ARG_COUNT]) -> isize {
    const FALLOC_FL_KEEP_SIZE: usize = 0x01;
    const FALLOC_FL_PUNCH_HOLE: usize = 0x02;
    const FALLOC_FL_NO_HIDE_STALE: usize = 0x04;
    const FALLOC_FL_COLLAPSE_RANGE: usize = 0x08;
    const FALLOC_FL_ZERO_RANGE: usize = 0x10;
    const FALLOC_FL_INSERT_RANGE: usize = 0x20;
    const FALLOC_FL_UNSHARE_RANGE: usize = 0x40;
    const FALLOC_FL_WRITE_ZEROES: usize = 0x80;
    const RESHAPERS: usize = FALLOC_FL_PUNCH_HOLE
        | FALLOC_FL_COLLAPSE_RANGE
        | FALLOC_FL_ZERO_RANGE
        | FALLOC_FL_INSERT_RANGE
        | FALLOC_FL_WRITE_ZEROES;

    let (fd, mode, offset, len) = (args[0] as i32, args[1], args[2] as i64, args[3] as i64);
    if offset < 0 || len <= 0 {
        return err::EINVAL;
    }
    if mode & !(FALLOC_FL_KEEP_SIZE | FALLOC_FL_NO_HIDE_STALE | FALLOC_FL_UNSHARE_RANGE | RESHAPERS)
        != 0
    {
        // Bits outside the operation vocabulary: EOPNOTSUPP, the way Linux
        // answers a mode word from a kernel newer than it.
        return err::EOPNOTSUPP;
    }
    if mode & RESHAPERS != 0 {
        // A punch without KEEP_SIZE beside it is EINVAL before the
        // refusal for the operation itself - the man page's own pair of
        // sentences, in its own order.
        if mode & FALLOC_FL_PUNCH_HOLE != 0 && mode & FALLOC_FL_KEEP_SIZE == 0 {
            return err::EINVAL;
        }
        return err::EOPNOTSUPP;
    }
    let file = match crate::sched::current_fds().clone_at(fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    // `fallocate` is a write to the file's block range, so a descriptor not
    // opened for writing is refused `EBADF` before the range is examined
    // (measured on the host: `fallocate(O_RDONLY)` is `EBADF` and nothing
    // changes) - the same write-mode demand `ftruncate` now makes and the
    // general "operations that write require a writable descriptor" rule.
    if !file.may_write() {
        return err::EBADF;
    }
    // Then the file's own immutable flag - the one `chattr` bit Linux gates
    // this door on: an `i` file refuses the allocation with `EPERM` and changes
    // nothing, where an `a` file is *served*, because a range that grows the
    // file is an append and Linux does not refuse the interior case either
    // (measured, `.workbuddy/probe_fallocate_mode.c` J, I and K, on a
    // descriptor opened *before* the flag was set, since `open(O_RDWR)` is
    // itself `EPERM` afterwards). That is [`crate::vfs::may_modify`] and not
    // [`crate::vfs::may_alter`]: the latter would refuse an append-only file,
    // and Linux does not.
    if let Err(e) = crate::vfs::may_modify(&file.inode()) {
        return err_of(e);
    }
    match file.metadata().kind {
        // A pipe's metadata answers `File` - the vocabulary's only word for an
        // unnamed channel - and it is told apart here by the node, because its
        // answer is *not* the `ENODEV` below: measured on the host
        // (`.workbuddy/probe_fsize.c`), a writable pipe end answers `ESPIPE`
        // where a socket, an anon-inode descriptor and a character device all
        // answer `ENODEV`. The unwritable end never reaches this match - the
        // write-mode check above answers `EBADF`, which is Linux's own order.
        NodeKind::File if file.inode().as_pipe().is_some() => return err::ESPIPE,
        NodeKind::File => {}
        NodeKind::Dir => return err::EISDIR,
        // A FIFO is a channel, not storage: Linux answers `ESPIPE` for a
        // `fallocate` on one (measured, `.workbuddy/probe_fifo.c`), the same
        // refusal a pipe's writable end gets one arm above.
        NodeKind::Fifo => return err::ESPIPE,
        // A socket, an anon-inode descriptor, a character device: no byte range
        // to lay out. The man page's ENODEV line about descriptors that are
        // neither files nor directories lands here.
        _ => return err::ENODEV,
    }
    // A range past the end of what an offset can name is EFBIG, the
    // "beyond the maximum file size" of the error list, rather than a
    // wrap into a small file.
    let end = match offset.checked_add(len) {
        Some(end) => end,
        None => return err::EFBIG,
    };
    if mode & FALLOC_FL_KEEP_SIZE == 0 && end as u64 > file.metadata().size {
        return match file.inode().truncate(end as u64) {
            Ok(()) => {
                fallocate_modified(&file.inode());
                0
            }
            Err(e) => err_of(e),
        };
    }
    // Nothing about the length changes here, and the call is still a write:
    // Linux's `vfs_fallocate` ends every successful path in `file_modified`,
    // so a `KEEP_SIZE` allocation and a range already inside the file cost the
    // set-id bits and stamp the clocks exactly as a growing one does
    // (`.workbuddy/probe_fallocate_mode.c` B, D and E - a `4755` file stores
    // `0755` in all three, with `mtime` moved off the value `utimensat` had
    // pinned).
    fallocate_modified(&file.inode());
    0
}

/// What a successful `fallocate` does to the file besides laying out blocks:
/// Linux's `file_modified`, in its order.
///
/// Three things, and each was measured rather than assumed
/// (`.workbuddy/probe_fallocate_mode.c`, Linux 7.2.9, ext4 and tmpfs agreeing):
///
/// * the set-id bits go, and `CAP_FSETID` is the only thing that spares them -
///   a root caller keeps `4755` where the same call as an ordinary user stores
///   `0755` (A against H). This is [`crate::vfs::remove_privs`], the same rule
///   a write and both `truncate` doors follow;
/// * `mtime` moves, on every successful call and on none of the refused ones
///   (J: an immutable file's `EPERM` leaves the pinned `mtime` where it was),
///   and `ctime` rides with it - which is `set_times`' own promise here;
/// * `IN_MODIFY` is reported, the event a write emits.
///
/// A caller that only preallocates still changed the file: the bits say who
/// may run it and the clock says when it last changed, so "nothing was
/// written" is not a defence either of them accepts.
fn fallocate_modified(inode: &Arc<dyn crate::vfs::Inode>) {
    crate::vfs::remove_privs(inode);
    // The atime is not this call's business - a read is - so it is carried
    // through as it stands, exactly as Linux's `file_update_time` does.
    let _ = inode.set_times(inode.atime(), wall_now());
    crate::vfs::inotify::emit(inode, crate::vfs::inotify::IN_MODIFY, "");
}

// ---------------------------------------------------------------------------
// copy_file_range(2): the kernel-made copy
// ---------------------------------------------------------------------------

/// `copy_file_range(in, off_in, out, off_out, len, flags)` — copy between
/// two files without the caller's buffers in the middle.
///
/// Both descriptors must name regular files, the one EINVAL the man page
/// is explicit about; a copy across a mount is EXDEV, the same refusal
/// `link(2)` gives a cross-filesystem name, because it is the same rule -
/// a byte range belongs to the storage that serves it. The offsets are
/// in-out `loff_t *`s; a NULL one means the descriptor's own cursor, and
/// that cursor is what the copy moves.
pub fn handle_copy_file_range(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    let (in_fd, off_in, out_fd, off_out, len, flags) = (
        args[0] as i32,
        args[1],
        args[2] as i32,
        args[3],
        args[4],
        args[5],
    );
    if flags != 0 {
        return err::EINVAL;
    }
    if len == 0 {
        return 0;
    }
    let fds = crate::sched::current_fds();
    let input = match fds.clone_at(in_fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    let output = match fds.clone_at(out_fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    for file in [&input, &output] {
        // A pipe is an anonymous channel, not a file: its read has no
        // position, and a read that would block must not be mistaken for
        // seekable storage. `copy_file_range` refuses it with EINVAL - and
        // the kind check alone cannot do that, because a pipe's metadata
        // answers `File`, the filesystem's only word for an unnamed channel.
        if file.inode().as_pipe().is_some() || file.metadata().kind != NodeKind::File {
            return err::EINVAL;
        }
    }
    // The positions: a caller's word when there is one, the descriptor's
    // own cursor when there is not. A negative word is not a position.
    let in_start = match splice_position(&input, off_in, from_user) {
        Ok(pos) => pos,
        Err(e) => return e,
    };
    let out_start = match splice_position(&output, off_out, from_user) {
        Ok(pos) => pos,
        Err(e) => return e,
    };
    // The man page's overlap refusal: one file, ranges that collide - the
    // one way a copy could corrupt what it is copying.
    if alloc::sync::Arc::ptr_eq(&input.inode(), &output.inode()) {
        let (a, b) = (in_start, in_start.saturating_add(len as u64));
        let (c, d) = (out_start, out_start.saturating_add(len as u64));
        if a < d && c < b {
            return err::EINVAL;
        }
    }
    // EXDEV across mounts, decided the way `st_dev` decides it: the
    // deepest mountpoint over each descriptor's label, and the labels
    // both descriptors were opened by.
    let (in_dev, _) = crate::vfs::mount::fs_of_path(
        &input
            .label()
            .unwrap_or_else(|| alloc::string::String::from("/")),
    );
    let (out_dev, _) = crate::vfs::mount::fs_of_path(
        &output
            .label()
            .unwrap_or_else(|| alloc::string::String::from("/")),
    );
    if in_dev != out_dev {
        return err::EXDEV;
    }

    let mut pos_in = in_start;
    let mut pos_out = out_start;
    let mut chunk = [0u8; 4096];
    let mut copied = 0usize;
    while copied < len {
        let want = (len - copied).min(chunk.len());
        let n = match input.inode().read_at(pos_in, &mut chunk[..want]) {
            Ok(n) => n,
            Err(e) => {
                if copied > 0 {
                    break;
                }
                return err_of(e);
            }
        };
        if n == 0 {
            break;
        }
        let w = match output.inode().write_at(pos_out, &chunk[..n]) {
            Ok(w) => w,
            Err(e) => {
                if copied > 0 {
                    break;
                }
                return err_of(e);
            }
        };
        if w == 0 {
            break;
        }
        pos_in += w as u64;
        pos_out += w as u64;
        copied += w;
    }
    // Each side's position goes home: through the caller's word when it
    // named one, into the descriptor's cursor when it did not.
    splice_finish(&input, off_in, pos_in, from_user);
    splice_finish(&output, off_out, pos_out, from_user);
    // The source was read, so a watcher on it hears `IN_ACCESS` - one record
    // per call, as Linux's `vfs_copy_file_range` reports it.
    if copied > 0 {
        notice_access(in_fd);
        // And the destination was written, so a watcher on it hears
        // `IN_MODIFY`, also one per call however many chunks the copy took -
        // measured, `.workbuddy/probe_inotify_modify.c` scenario N. Emitted
        // here because the copy writes through `Inode::write_at` and never
        // reaches `File::write`, the write doors' reporter.
        notice_modify(out_fd);
    }
    copied as isize
}

/// Where one side of a kernel copy starts: the caller's `loff_t` when
/// there is one, the descriptor's own cursor when there is not.
fn splice_position(
    file: &Arc<crate::vfs::File>,
    off: usize,
    from_user: bool,
) -> Result<u64, isize> {
    if off != 0 {
        if !buffer_ok(off, 8, false, from_user) {
            return Err(err::EFAULT);
        }
        let at = unsafe { (off as *const u64).read_unaligned() } as i64;
        if at < 0 {
            return Err(err::EINVAL);
        }
        Ok(at as u64)
    } else {
        Ok(file.pos())
    }
}

/// The tail of one side of a kernel copy: the new position written back
/// through the caller's word, or into the descriptor's cursor when the
/// position was the descriptor's own.
fn splice_finish(file: &Arc<crate::vfs::File>, off: usize, pos: u64, from_user: bool) {
    if off != 0 {
        if buffer_ok(off, 8, true, from_user) {
            unsafe { (off as *mut u64).write_unaligned(pos) };
        }
    } else {
        file.set_pos(pos);
    }
}

// ---------------------------------------------------------------------------
// preadv2(2) / pwritev2(2): the vectored pair with the RWF_* advice words
// ---------------------------------------------------------------------------

/// `preadv2(fd, iov, iovcnt, offset, flags)` — the vectored read at a
/// position, with the advice word. `pwritev2` is the same with the write's
/// verb.
///
/// The advice bits this kernel cannot act on are accepted — its writes
/// already reach storage, and its reads never wait for a device — and the
/// one bit it cannot honour at all, `RWF_ATOMIC`, is refused the way Linux
/// refuses it from a filesystem without atomic-write support. A flag
/// outside the vocabulary is EOPNOTSUPP, the man page's own spelling.
/// `offset` of -1 is the position-carrying form: the descriptor's own
/// cursor, moved by the call, exactly as the un-at `readv`/`writev` move
/// it. `RWF_APPEND` is a per-write O_APPEND: the end of the file,
/// wherever the offset named.
pub fn handle_preadv2(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    vectored_at_position(args, from_user, false)
}

/// `pwritev2(fd, iov, iovcnt, offset, flags)` — see [`handle_preadv2`].
pub fn handle_pwritev2(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    vectored_at_position(args, from_user, true)
}

/// The position and the advice word of a `preadv2`/`pwritev2` call, read the
/// way *this* machine's ABI spells them.
///
/// The two machines do not agree on the shape, and the difference is the
/// 32-bit `off_t` x86_64 carries for its own historical reasons: there the
/// position arrives as two halves (`pos_l`, `pos_h`) and the advice word is
/// the *sixth* argument, while asm-generic - aarch64 - passes the whole
/// 64-bit position as the fourth and the advice word fifth
/// (`preadv2(fd, iov, iovcnt, pos, flags)`, which is what musl emits on
/// arm64). Reading one shape on both machines is wrong on one of them: the
/// generic read on x86_64 takes the high half of the position for the flags
/// and truncates the position to its low 32 bits, so a negative position
/// becomes a huge positive one, the cursor spelling (`-1`) is not
/// recognised, and every advice bit in the sixth slot is invisible - the
/// seven claims `t-preadv2` makes about exactly those words.
#[cfg(target_arch = "x86_64")]
fn position_and_flags(args: [usize; ARG_COUNT]) -> (i64, usize) {
    (position_offset(args), args[5])
}

/// The asm-generic reading: see [`position_and_flags`]'s note on the two
/// machines.
#[cfg(not(target_arch = "x86_64"))]
fn position_and_flags(args: [usize; ARG_COUNT]) -> (i64, usize) {
    (position_offset(args), args[4])
}

/// The position a `preadv`/`pwritev`/`preadv2`/`pwritev2` call names, which
/// is the half of the argument shape the four calls share - only the advice
/// word on the end differs between the `*v2` pair and the plain one, so the
/// reading of the position lives here once.
///
/// x86_64 splits the 64-bit position across two 32-bit words (`pos_l` then
/// `pos_h`) and asm-generic passes it whole; `-1` arrives as `0xffff_ffff`
/// in each half there, and the all-ones pair *is* the offset the cursor
/// spelling means, so the halves have to be rejoined before anything asks
/// whether the position is negative.
#[cfg(target_arch = "x86_64")]
fn position_offset(args: [usize; ARG_COUNT]) -> i64 {
    (args[3] as u32 as u64 | (args[4] as u64) << 32) as i64
}

/// The asm-generic position: one 64-bit argument, with the fourth slot left
/// to `flags` where the `*v2` calls have one.
#[cfg(not(target_arch = "x86_64"))]
fn position_offset(args: [usize; ARG_COUNT]) -> i64 {
    args[3] as i64
}

fn vectored_at_position(args: [usize; ARG_COUNT], from_user: bool, writing: bool) -> isize {
    const RWF_HIPRI: usize = 0x1;
    const RWF_DSYNC: usize = 0x2;
    const RWF_SYNC: usize = 0x4;
    const RWF_NOWAIT: usize = 0x8;
    const RWF_APPEND: usize = 0x10;
    const RWF_NOAPPEND: usize = 0x20;
    const RWF_ATOMIC: usize = 0x40;
    const RWF_DONTCACHE: usize = 0x80;
    const KNOWN: usize = RWF_HIPRI
        | RWF_DSYNC
        | RWF_SYNC
        | RWF_NOWAIT
        | RWF_APPEND
        | RWF_NOAPPEND
        | RWF_ATOMIC
        | RWF_DONTCACHE;

    let (fd, (offset, flags)) = (args[0] as i32, position_and_flags(args));
    if flags & !KNOWN != 0 || flags & RWF_ATOMIC != 0 {
        // Unknown advice is the man page's EOPNOTSUPP; ATOMIC is known but
        // asks for a torn-write guarantee no storage here can make.
        return err::EOPNOTSUPP;
    }
    if !crate::sched::current_fds().clone_at(fd).is_ok() {
        return err::EBADF;
    }
    if writing && flags & RWF_APPEND != 0 {
        // The append the flag spells: the end of the file is the position,
        // whatever the offset argument said.
        let size = crate::sched::current_fds()
            .clone_at(fd)
            .map(|file| file.metadata().size as i64)
            .unwrap_or(0);
        return at_position(fd, size, || handle_writev(args, from_user));
    }
    if offset == -1 {
        // The cursor-carrying spelling: no position discipline at all, the
        // plain `readv`/`writev` behaviour.
        return if writing {
            handle_writev(args, from_user)
        } else {
            handle_readv(args, from_user)
        };
    }
    io_at_position(fd, offset, args, from_user, writing)
}

// ---------------------------------------------------------------------------
// openat2(2), faccessat2(2): the extensible forms
// ---------------------------------------------------------------------------

/// `openat2(dirfd, path, how, size)` — `openat` with the extensible
/// argument struct.
///
/// The struct is three words — flags, mode, resolve — and the size word
/// is how a kernel and a caller from different eras agree on which fields
/// exist: smaller than any known version is EINVAL, larger is E2BIG, the
/// man page's own split between "your struct is too old to be real" and
/// "your struct has fields this kernel has never heard of". The resolve
/// flags this kernel's resolver cannot keep are refused EOPNOTSUPP —
/// they are promises about the *walk*, and a walk that cannot keep one
/// says so rather than pretending. `RESOLVE_CACHED` is accepted: nothing
/// here caches, which makes every walk a cached one.
pub fn handle_openat2(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>, from_user: bool) -> isize {
    const OPEN_HOW_SIZE: usize = 24;
    const RESOLVE_NO_XDEV: usize = 0x01;
    const RESOLVE_NO_MAGICLINKS: usize = 0x02;
    const RESOLVE_NO_SYMLINKS: usize = 0x04;
    const RESOLVE_BENEATH: usize = 0x08;
    const RESOLVE_IN_ROOT: usize = 0x10;
    const RESOLVE_CACHED: usize = 0x20;

    let (dirfd, path_ptr, how, size) = (args[0] as i32, args[1], args[2], args[3]);
    if size < OPEN_HOW_SIZE {
        return err::EINVAL;
    }
    if size > OPEN_HOW_SIZE {
        return err::E2BIG;
    }
    if !buffer_ok(how, OPEN_HOW_SIZE, false, from_user) {
        return err::EFAULT;
    }
    // The three words are `__u64`s in the struct; the flag vocabulary
    // below them is the `usize` every other open call here speaks.
    let (flags, mode, resolve) = unsafe {
        (
            (how as *const u64).read_unaligned() as usize,
            ((how + 8) as *const u64).read_unaligned() as usize,
            ((how + 16) as *const u64).read_unaligned() as usize,
        )
    };
    if resolve
        & !(RESOLVE_NO_XDEV
            | RESOLVE_NO_MAGICLINKS
            | RESOLVE_NO_SYMLINKS
            | RESOLVE_BENEATH
            | RESOLVE_IN_ROOT
            | RESOLVE_CACHED)
        != 0
    {
        return err::EINVAL;
    }
    if resolve & !RESOLVE_CACHED != 0 {
        return err::EOPNOTSUPP;
    }
    // The mode is a create's business alone, the man page's own EINVAL.
    if mode != 0 && flags & (abi::O_CREAT | abi::O_TMPFILE) == 0 {
        return err::EINVAL;
    }
    let path = match cstr_arg(path_ptr, true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let base = match base_for(dirfd, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    open_linux(&path, flags, mode as u32, base)
}

/// `faccessat2(dirfd, path, mode, flags)` — the access check with the
/// flags the original could not take.
///
/// `AT_EACCESS` is accepted and indistinguishable: the real and effective
/// IDs are one ID here, so the two questions the flag distinguishes have
/// one answer. `AT_SYMLINK_NOFOLLOW` names the link itself and is honoured
/// the way `lstat`'s is. `AT_EMPTY_PATH` with an empty name asks about the
/// *descriptor* rather than a name (Linux 5.8 added it, and the measurement
/// agrees: `faccessat2(fd, "", F_OK, AT_EMPTY_PATH)` answers `0` for a
/// descriptor the caller owns and `EBADF` for one nothing is open on, while
/// the same flag beside a name that is really there is inert). The mode is
/// validated the way `access(2)` validates it — anything outside
/// F_OK/R_OK/W_OK/X_OK is EINVAL — and `X_OK` answers from the mode bits the
/// filesystem stores, the only bits this kernel keeps: a directory traverses,
/// a script with its bit clear does not, which is the question a shell's
/// `test -x` is asking.
pub fn handle_faccessat2(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let (dirfd, path_ptr, mode, flags) = (args[0] as i32, args[1], args[2], args[3]);
    if mode & !(R_OK as usize | W_OK as usize | X_OK as usize) != 0 {
        return err::EINVAL;
    }
    if flags & !(abi::AT_SYMLINK_NOFOLLOW | abi::AT_EACCESS | AT_EMPTY_PATH) != 0 {
        return err::EINVAL;
    }
    let path = match cstr_arg(path_ptr, true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // The descriptor *is* the target under `AT_EMPTY_PATH`, and only there: the
    // question is then the one the path route asks of the inode it resolved,
    // with the same identity the flags select. An empty name the caller did not
    // mark falls through to `base_for`, whose answer for it is the name's own.
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        return match crate::sched::current_fds().clone_at(dirfd) {
            Ok(file) => access_check(&file.inode(), mode, access_ids(flags)),
            Err(e) => err_of(e),
        };
    }
    let base = match base_for(dirfd, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let found = if flags & abi::AT_SYMLINK_NOFOLLOW != 0 {
        crate::vfs::resolve_no_follow_at(&path, base)
    } else {
        crate::vfs::resolve_opt(&path, Some(base))
    };
    match found {
        Ok(inode) => access_check(&inode, mode, access_ids(flags)),
        Err(e) => err_of(e),
    }
}

/// `fchmodat(dirfd, path, mode)` — the permission bits, relative to a dirfd.
///
/// The syscall takes three arguments and no flags word: what a caller wants
/// from one (`AT_SYMLINK_NOFOLLOW`, `AT_EMPTY_PATH`) goes through
/// [`handle_fchmodat2`] instead - musl and glibc both refuse those flags
/// before the kernel is ever reached, so the fourth register here holds
/// nothing a caller meant to say, and is not read.
pub fn handle_fchmodat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let base = match base_for(args[0] as i32, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    match crate::vfs::resolve_opt(&path, Some(base)) {
        Ok(inode) => match chmod_mode(&inode, args[2] as u32) {
            Ok(mode) => {
                inode.set_mode(mode);
                crate::vfs::inotify::emit(&inode, crate::vfs::inotify::IN_ATTRIB, "");
                0
            }
            Err(e) => e,
        },
        Err(e) => err_of(e),
    }
}

/// `fchmodat2(dirfd, path, mode, flags)` — the permission bits, relative to a
/// dirfd, with the flags word the old call never had (Linux 6.6 added the
/// call for exactly this).
///
/// `AT_SYMLINK_NOFOLLOW` names the link itself rather than what it points
/// at, and a link has no mode of its own, so one is answered `EOPNOTSUPP` -
/// while any other file is chmod'ed as asked. `AT_EMPTY_PATH` with an empty
/// name is `fchmod` by descriptor - how an installer marks a not-yet-linked
/// `O_TMPFILE` setuid before it has a name at all.
///
/// This is the call `apk` makes for every setuid or set-group-id file it
/// unpacks (`fs_fsys.c` reaches for it under `AT_SYMLINK_NOFOLLOW`), which
/// is why it exists here: without it, every package install warned
/// "failed to preserve <file>: permission" over exactly those files and
/// dropped their special bits.
pub fn handle_fchmodat2(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let flags = args[3];
    if flags & !(abi::AT_SYMLINK_NOFOLLOW | abi::AT_EMPTY_PATH) != 0 {
        return err::EINVAL;
    }
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // An empty name with `AT_EMPTY_PATH` is the descriptor, not the tree.
    if path.is_empty() {
        if flags & abi::AT_EMPTY_PATH == 0 {
            return err::ENOENT;
        }
        let inode = match crate::sched::current_fds().clone_at(args[0] as i32) {
            Ok(file) => file.inode(),
            Err(e) => return err_of(e),
        };
        return match chmod_mode(&inode, args[2] as u32) {
            Ok(mode) => {
                inode.set_mode(mode);
                crate::vfs::inotify::emit(&inode, crate::vfs::inotify::IN_ATTRIB, "");
                0
            }
            Err(e) => e,
        };
    }
    let base = match base_for(args[0] as i32, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let resolved = if flags & abi::AT_SYMLINK_NOFOLLOW != 0 {
        crate::vfs::resolve_no_follow_at(&path, base)
    } else {
        crate::vfs::resolve_opt(&path, Some(base))
    };
    let inode = match resolved {
        Ok(inode) => inode,
        Err(e) => return err_of(e),
    };
    // The flag can name a link; a link carries no permission bits of its
    // own, so there is nothing to change - the answer Linux gives.
    if inode.metadata().kind == NodeKind::Symlink {
        return err::EOPNOTSUPP;
    }
    match chmod_mode(&inode, args[2] as u32) {
        Ok(mode) => {
            inode.set_mode(mode);
            crate::vfs::inotify::emit(&inode, crate::vfs::inotify::IN_ATTRIB, "");
            0
        }
        Err(e) => e,
    }
}

// ---------------------------------------------------------------------------
// The *at relatives and the legacy spellings this table has numbers for
// ---------------------------------------------------------------------------

/// `mknodat(dirfd, path, mode, dev)` — create a node, relative to a
/// dirfd. `mknod` is this at the caller's cwd.
///
/// **The `dev` argument is not written on the node**, and that is a named
/// divergence rather than an oversight (`docs/uapi/` §5.2): `st_rdev` is the
/// field this argument fills, and it is what makes a node *that* device -
/// `/dev/null` reports `rdev 1:3`, the number its own creation passed
/// (measured on a current host). What this kernel can honestly do with it is
/// nothing yet, because it has no table from a device number to a driver: its
/// devices are objects a driver published under a name (`/dev/null` *is* the
/// null device; a node made here is not), so writing the number would make
/// `stat` true and the device still absent - and would put a fabricated node
/// in front of the rdev-keyed ioctl families, which dispatch on that word.
pub fn handle_mknodat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    // The type is the first question, before the name is copied and before the
    // dirfd is looked at - Linux's `may_mknod`, measured through `syscall(2)`
    // (`.workbuddy/probe_mknod.c`, Linux 7.2.9): `S_IFDIR` answers `EPERM` and
    // an uncreatable type `EINVAL` even with an *empty* name, which names
    // nothing and would be `ENOENT` had the type been asked second, and the
    // same two come back over a name that already exists. `t-at-empty-name`
    // pins the other side of that order with `S_IFIFO`: the type passes this
    // check and the empty name is the next question, so `ENOENT` still wins.
    if let Err(e) = abi::mknod_type_is_creatable(args[2]) {
        return e;
    }
    let path = match cstr_arg(args[1], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let base = match base_for(args[0] as i32, &path, cwd) {
        Ok(base) => base,
        Err(e) => return e,
    };
    let (parent, name) = match crate::vfs::split_parent(&path) {
        Ok(split) => split,
        Err(e) => return err_of(e),
    };
    let parent = match crate::vfs::resolve_opt(parent, Some(base)) {
        Ok(dir) => dir,
        Err(e) => return err_of(e),
    };
    // A name that is already there is `EEXIST` — measured on a current host,
    // through `syscall(2)` (`.workbuddy/probe_mknod.c`): over a node of the same
    // type, over one of a *different* type (including a type this kernel
    // refuses, which Linux serves), over a *dangling link* (the question is
    // about the name, so it is asked of the directory that holds it), and
    // through this spelling and the legacy `mknod` alike. This door answered `0`
    // and called the caller's intent its own ("the caller wanted the node, not
    // the call"), which is a success where Linux refuses.
    if crate::vfs::resolve_entry_at(name, parent.clone()).is_ok() {
        return err::EEXIST;
    }
    // The parent's bits decide whether a name may be made here, and the mode
    // arrives masked like every other create's. Linux asks this *after* the
    // existence check (`may_create` does both, in that order), and before the
    // device-node capability - measured: a device node in a directory the
    // caller may not write is `EACCES`, and one in a directory it may write is
    // `EPERM` for want of `CAP_MKNOD`.
    if let Err(e) = crate::vfs::may_create_in(&parent) {
        return err_of(e);
    }
    // The kind, once the name and its existence are settled: the position
    // `mknod_kind` answers `S_IFIFO` in (a FIFO, so the `EEXIST` above is what
    // a second `mknodat(S_IFIFO)` over its own node answers - Linux's answer
    // for a type it serves).
    let kind = match abi::mknod_kind(args[2]) {
        Ok(kind) => kind,
        Err(e) => return e,
    };
    // Linux's `CAP_MKNOD`, the position the comment above reserved for it and
    // the last question this call has: only a *device* node asks, and the
    // namespace that answers is the **initial** one - `capable()`, not the
    // caller's own. Measured as an unprivileged caller on a current host
    // (`.workbuddy/probe_mknod_cap.c`, Linux 7.2.9): a character or block node
    // is `EPERM`, a `S_IFREG`/`S_IFIFO`/`S_IFSOCK` node is served, the character
    // node whose `dev` is `0` (an overlayfs whiteout) is served too, and every
    // refusal above comes first - `EEXIST`, `EACCES` in a directory the caller
    // may not write, `ENOENT` for a path that is not there. Measured again from
    // inside a user namespace (`.workbuddy/probe_capable_scope.sh`): the
    // device-node refusal survives the namespace's capability grant, which is
    // what makes this the initial-namespace question and not the caller's.
    if abi::mknod_needs_capability(args[2], args[3])
        && !crate::sched::current_credentials().capable(crate::cred::CAP_MKNOD)
    {
        return err::EPERM;
    }
    match parent.create(name, kind) {
        Ok(created) => {
            created.set_mode(masked_mode(args[2] as u32));
            let is_dir = created.metadata().kind == crate::vfs::NodeKind::Dir;
            crate::vfs::inotify::emit_child(&parent, crate::vfs::inotify::IN_CREATE, name, is_dir);
            let _ = created;
            0
        }
        Err(e) => err_of(e),
    }
}

/// `futimesat(dirfd, path, tv)` — the timestamps call with the dirfd the
/// future took.
///
/// A NULL `path` names the descriptor itself — the `futimens` shape — and
/// requires a real one, not `AT_FDCWD`. Timestamps are not kept, so the
/// answer is the resolution's; the `timeval`s are still validated,
/// because a caller that names a usec past a second has made a request
/// nobody would recognise as the one it meant.
pub fn handle_futimesat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>, from_user: bool) -> isize {
    let (dirfd, path_ptr, tvp) = (args[0] as i32, args[1], args[2]);
    let inode: Arc<dyn Inode> = if path_ptr == 0 {
        if dirfd == abi::AT_FDCWD {
            return err::EINVAL;
        }
        match crate::sched::current_fds().clone_at(dirfd) {
            Ok(file) => file.inode(),
            Err(e) => return err_of(e),
        }
    } else {
        let path = match cstr_arg(path_ptr, true) {
            Ok(path) => path,
            Err(e) => return e,
        };
        let base = match base_for(dirfd, &path, cwd) {
            Ok(base) => base,
            Err(e) => return e,
        };
        match crate::vfs::resolve_opt(&path, Some(base)) {
            Ok(inode) => inode,
            Err(e) => return err_of(e),
        }
    };
    // `futimesat` carries `struct timeval times[2]`; the microseconds are
    // validated but dropped, since an inode clock is whole seconds. A null
    // `tvp` means "now, both" - and it is also the `ATTR_TOUCH` shape of the
    // permission question, where a non-null pair names times and is
    // `ATTR_TIMES_SET` ([`TimesShape`], and `utimes`' specs have no sentinel to
    // name nothing with).
    let (atime, mtime, shape) = if tvp == 0 {
        let now = wall_now();
        (now, now, TimesShape::BothToNow)
    } else {
        if !buffer_ok(tvp, 32, false, from_user) {
            return err::EFAULT;
        }
        let (a_usec, m_usec) = unsafe {
            (
                ((tvp + 8) as *const u64).read_unaligned(),
                ((tvp + 24) as *const u64).read_unaligned(),
            )
        };
        if a_usec >= 1_000_000 || m_usec >= 1_000_000 {
            return err::EINVAL;
        }
        (
            unsafe { (tvp as *const i64).read_unaligned() } as u64,
            unsafe { ((tvp + 16) as *const i64).read_unaligned() } as u64,
            TimesShape::Setting,
        )
    };
    store_times(&inode, atime, mtime, shape)
}

/// `creat(path, mode)` — open, spelled the way the 1970s spelled it:
/// create-or-truncate, for writing. The mode is advisory exactly as
/// `open`'s is, and the file it names is the one `open` would make.
pub fn handle_creat(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>) -> isize {
    let path = match cstr_arg(args[0], true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    // O_WRONLY (1) of the access bits: the write-only the name promises.
    open_linux(
        &path,
        abi::O_CREAT | abi::O_TRUNC | 1,
        args[1] as u32,
        cwd.clone(),
    )
}

/// `utime(path, times)` — the seconds-only spelling of the timestamps
/// call. NULL `times` means now; the `utimbuf` the pointer names is two
/// `time_t`s, access time then modification time. Both spellings land on
/// [`store_times`], which is where the clocks are kept and where the two
/// questions a timestamps call asks live.
pub fn handle_utime(args: [usize; ARG_COUNT], cwd: &Arc<dyn Inode>, from_user: bool) -> isize {
    let (path_ptr, times) = (args[0], args[1]);
    if times != 0 && !buffer_ok(times, 16, false, from_user) {
        return err::EFAULT;
    }
    let path = match cstr_arg(path_ptr, true) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let inode = match crate::vfs::resolve_opt(&path, Some(cwd.clone())) {
        Ok(inode) => inode,
        Err(e) => return err_of(e),
    };
    // `struct utimbuf { time_t actime; time_t modtime; }`; a null pointer sets
    // both clocks to now, exactly as `utime(path, NULL)` says - and it is the
    // two shapes of the permission question: "now" is `ATTR_TOUCH` and a pair
    // of values is `ATTR_TIMES_SET` ([`TimesShape`]). A `time_t` has no room
    // for `UTIME_OMIT`, so this spelling can never name nothing.
    let (atime, mtime, shape) = if times == 0 {
        let now = wall_now();
        (now, now, TimesShape::BothToNow)
    } else {
        (
            unsafe { (times as *const i64).read_unaligned() } as u64,
            unsafe { ((times + 8) as *const i64).read_unaligned() } as u64,
            TimesShape::Setting,
        )
    };
    store_times(&inode, atime, mtime, shape)
}

// ---------------------------------------------------------------------------
// splice(2), readahead(2), syncfs(2), syslog(2)
// ---------------------------------------------------------------------------

/// `splice(fd_in, off_in, fd_out, off_out, len, flags)` — move bytes
/// between a pipe and anything else, without a caller's buffer in the
/// middle.
///
/// One of the two descriptors must be a pipe, the man page's own rule,
/// and a pipe takes no offset: its data is a stream, and the cursor the
/// descriptor keeps is the only position it has — asking for one by
/// pointer is EINVAL. The move itself is a chunked copy through the
/// kernel; the flags are the advice bits the man page lists, accepted
/// for the same reason `poll` accepts its nonblocking one: the
/// descriptor's own flags are what govern.
pub fn handle_splice(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    const SPLICE_F_MOVE: usize = 0x01;
    const SPLICE_F_NONBLOCK: usize = 0x02;
    const SPLICE_F_MORE: usize = 0x04;
    const SPLICE_F_GIFT: usize = 0x08;

    let (in_fd, off_in, out_fd, off_out, len, flags) = (
        args[0] as i32,
        args[1],
        args[2] as i32,
        args[3],
        args[4],
        args[5],
    );
    if flags & !(SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE | SPLICE_F_GIFT) != 0 {
        return err::EINVAL;
    }
    if len == 0 {
        return 0;
    }
    let fds = crate::sched::current_fds();
    let input = match fds.clone_at(in_fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    let output = match fds.clone_at(out_fd) {
        Ok(file) => file,
        Err(e) => return err_of(e),
    };
    let in_is_pipe = input.inode().as_pipe().is_some();
    let out_is_pipe = output.inode().as_pipe().is_some();
    if !in_is_pipe && !out_is_pipe {
        return err::EINVAL;
    }
    if in_is_pipe && off_in != 0 || out_is_pipe && off_out != 0 {
        return err::EINVAL;
    }

    let mut pos_in = match splice_position(&input, off_in, from_user) {
        Ok(pos) => pos,
        Err(e) => return e,
    };
    let mut pos_out = match splice_position(&output, off_out, from_user) {
        Ok(pos) => pos,
        Err(e) => return e,
    };
    let mut chunk = [0u8; 4096];
    let mut moved = 0usize;
    while moved < len {
        let want = (len - moved).min(chunk.len());
        let n = match input.inode().read_at(pos_in, &mut chunk[..want]) {
            Ok(n) => n,
            Err(e) => {
                if moved > 0 {
                    break;
                }
                return err_of(e);
            }
        };
        if n == 0 {
            break;
        }
        let w = match output.inode().write_at(pos_out, &chunk[..n]) {
            Ok(w) => w,
            Err(e) => {
                if moved > 0 {
                    break;
                }
                return err_of(e);
            }
        };
        if w == 0 {
            break;
        }
        pos_in += w as u64;
        pos_out += w as u64;
        moved += w;
    }
    splice_finish(&input, off_in, pos_in, from_user);
    splice_finish(&output, off_out, pos_out, from_user);
    // A file on the *input* side was read, so a watcher on it hears
    // `IN_ACCESS`; a file on the *output* side was written, so a watcher on it
    // hears `IN_MODIFY` - one record per call either way. At most one side is
    // a file, because a splice needs a pipe on one end (two pipes is the
    // pipe-to-pipe move, which reports nothing). Measured both directions in
    // `.workbuddy/probe_inotify_access.c` scenarios K and M, and the write
    // side again in `.workbuddy/probe_inotify_modify.c` scenario P. The write
    // goes through `Inode::write_at`, below `File::write`'s reporter, so the
    // one `IN_MODIFY` is said here.
    if moved > 0 {
        if !in_is_pipe {
            notice_access(in_fd);
        }
        if !out_is_pipe {
            notice_modify(out_fd);
        }
    }
    moved as isize
}

/// `readahead(fd, offset, count)` — a promise that the range is coming.
/// Nothing here reads lazily, so there is nothing to pull in early; the
/// call is the descriptor's validity and the success.
pub fn handle_readahead(args: [usize; ARG_COUNT]) -> isize {
    match crate::sched::current_fds().clone_at(args[0] as i32) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}

/// `syncfs(fd)` — flush the filesystem one descriptor belongs to. Every
/// write here already reached its storage — ramfs and ext2 write through —
/// so the flush is empty; the call still owes the descriptor check.
pub fn handle_syncfs(args: [usize; ARG_COUNT]) -> isize {
    match crate::sched::current_fds().clone_at(args[0] as i32) {
        Ok(_) => 0,
        Err(e) => err_of(e),
    }
}

/// `syslog(type, buf, len)` — the kernel log, `klogctl`'s door.
///
/// The types are numbers; the man page says so itself. The reads are the
/// heart of it: type 2 takes what is there and it is gone — the one a
/// `klogd` parks in, waiting for the kernel to speak; type 3 reads the
/// tail without consuming, the one `dmesg` makes; type 4 reads and then
/// empties. Clearing (5) empties; the counters (9, 10) are the numbers a
/// reader sizes itself by. The console-loglevel games (0, 1, 6, 7, 8)
/// are accepted: this kernel has one loglevel — everything — and one
/// user.
pub fn handle_syslog(args: [usize; ARG_COUNT], from_user: bool) -> isize {
    // The capability gate Linux puts *before everything*: a caller without
    // `CAP_SYSLOG` answers `EPERM` even for a type it would otherwise refuse
    // (host-measured: the container's unknown type 99 came back `EPERM`, not
    // `EINVAL`, `NOTES.md` §15.11 batch 29). The check is first for exactly
    // that ordering, and the namespace it asks is the **initial** one - measured
    // (`.workbuddy/probe_capable_scope.sh`): a user-namespace root, holding the
    // whole set in its own namespace, is still refused the ring's size.
    if !crate::sched::current_credentials().capable(crate::cred::CAP_SYSLOG) {
        return err::EPERM;
    }
    const READ: usize = 2;
    const READ_ALL: usize = 3;
    const READ_CLEAR: usize = 4;
    const CLEAR: usize = 5;
    const CONSOLE_LEVEL: usize = 8;
    const SIZE_UNREAD: usize = 9;
    const SIZE_BUFFER: usize = 10;

    let (kind, bufp, len) = (args[0], args[1], args[2] as i64);
    match kind {
        // Open, close, console off and on: bookkeeping for a loglevel
        // this kernel does not keep.
        0 | 1 | 6 | 7 => 0,
        CONSOLE_LEVEL => {
            if !(1..=8).contains(&len) {
                return err::EINVAL;
            }
            0
        }
        READ => {
            if bufp == 0 || len < 0 {
                return err::EINVAL;
            }
            if !buffer_ok(bufp, len as usize, true, from_user) {
                return err::EFAULT;
            }
            // The destructive read waits until there is something to
            // take, and a signal that would run a handler is a way out of
            // the wait the man page spells EINTR. The cell is how a
            // condition hands a whole read back out of an `Fn`.
            let taken_cell = core::cell::RefCell::new(alloc::vec::Vec::new());
            let answer = crate::sched::block_until_deadline(
                || {
                    *taken_cell.borrow_mut() = crate::console::dmesg_take(len as usize);
                    !taken_cell.borrow().is_empty()
                },
                u64::MAX,
            );
            match answer {
                crate::sched::WaitEnd::Interrupted => return err::EINTR,
                crate::sched::WaitEnd::Deadline => return 0,
                crate::sched::WaitEnd::Condition => {}
            }
            let taken = taken_cell.into_inner();
            unsafe {
                core::ptr::copy_nonoverlapping(taken.as_ptr(), bufp as *mut u8, taken.len());
            }
            taken.len() as isize
        }
        READ_ALL | READ_CLEAR => {
            if bufp == 0 || len < 0 {
                return err::EINVAL;
            }
            if !buffer_ok(bufp, len as usize, true, from_user) {
                return err::EFAULT;
            }
            let log = crate::console::dmesg_snapshot();
            let n = log.len().min(len as usize);
            let start = log.len() - n;
            unsafe { core::ptr::copy_nonoverlapping(log[start..].as_ptr(), bufp as *mut u8, n) };
            if kind == READ_CLEAR {
                crate::console::dmesg_clear();
            }
            n as isize
        }
        CLEAR => {
            crate::console::dmesg_clear();
            0
        }
        SIZE_UNREAD => crate::console::dmesg_snapshot().len() as isize,
        SIZE_BUFFER => crate::console::dmesg_capacity() as isize,
        _ => err::EINVAL,
    }
}
