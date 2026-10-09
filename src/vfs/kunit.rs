//! The `vfs` suite: the bootstrap ramfs, a mounted tree, and two tasks sharing one
//! file through the descriptor table.
//!
//! A child of `vfs` and compiled only into a test build. The syscall gate's cases
//! were written here and *are not here*: calling `crate::syscall` from a suite
//! inside `vfs` closes a `vfs <-> syscall` mutual pair the module graph does not
//! allow, which the ratchet reported the first time this suite moved - so they
//! belong to `mpos::syscall::kunit`, and that is where they are (`docs/NOTES.md`
//! §15.22).
//!
//! A task cannot return a value to the case that spawned it, so the task in
//! `mounted_tree_multi_task` stores its verdict in the shared vocabulary's slots
//! and the case reads it out (`crate::kunit::{record_verdict, take_verdict}`).

use alloc::string::String;
use alloc::sync::Arc;

use crate::case_check;
use crate::kunit::{Case, Phase, Suite, record_verdict, take_verdict};
use crate::sched::{self, Priority};
use crate::vfs::{self, FileSystem, Inode, NodeKind};

use crate::sched::kunit::drain;

/// The suite this module contributes to the boot's self-tests.
pub const SUITE: Suite = Suite {
    name: "vfs",
    phase: Phase::Vfs,
    cases: &[
        Case {
            name: "ramfs_file",
            run: ramfs_file,
        },
        Case {
            name: "mounted_tree_multi_task",
            run: mounted_tree_multi_task,
        },
        Case {
            name: "inode_flags_are_policed",
            run: inode_flags_are_policed,
        },
    ],
};

/// The verdict slots this suite's two tasks use.
///
/// Two of them because those tasks run concurrently: one slot would be whichever
/// of them stored last, and the point of that case is that both did their half.
const WRITER_SLOT: usize = 0;
const READER_SLOT: usize = 1;

/// Set by the writer task once its two chunks are in, which is the one thing the
/// reader is waiting for.
///
/// A flag rather than "wait until the task count is back to the permanent one":
/// what the reader needs is the *writer's* work, not the scheduler's arithmetic,
/// and a case that waits on what it actually needs cannot be broken by another
/// task existing (`docs/NOTES.md` §15.22).
static WRITER_DONE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Look a name up in a directory, creating it when the tree does not carry it.
///
/// The old checks were written against a root filesystem of their own making,
/// where every name they wanted was free. Alpine's tree is not obliged to leave
/// those gaps - it has its own `/etc/hostname` and its own `/tmp`, both already
/// occupied - so the cases want a file to write through rather than a file that
/// was not there before, and they pick names Alpine does not use.
fn ensure_child(dir: &Arc<dyn Inode>, name: &str, kind: NodeKind) -> Arc<dyn Inode> {
    match dir.lookup(name) {
        Ok(child) => child,
        Err(_) => dir.create(name, kind).expect("create the scratch file"),
    }
}

/// A file written and read back through the descriptor table on the bootstrap
/// root.
fn ramfs_file(_info: &crate::boot::BootInfo) -> Result<(), String> {
    // Make sure /etc exists before poking a scratch file into it: the initramfs
    // normally carries /etc, and a stripped archive must not turn into a failure
    // in the middle of a case.
    let etc = match vfs::resolve("/etc") {
        Ok(dir) => dir,
        Err(_) => vfs::resolve("/")
            .map_err(|e| alloc::format!("the bootstrap root does not resolve as `/`: {e:?}"))?
            .create("etc", NodeKind::Dir)
            .map_err(|e| alloc::format!("could not create /etc: {e:?}"))?,
    };
    ensure_child(&etc, "mpos-hostname", NodeKind::File);

    let fd = vfs::open("/etc/mpos-hostname")
        .map_err(|e| alloc::format!("open /etc/mpos-hostname: {e:?}"))?;
    case_check!(
        vfs::write(fd, b"mpos\n").map_err(|e| alloc::format!("write: {e:?}"))? == 5,
        "writing five bytes reported a different count"
    );
    vfs::close(fd).map_err(|e| alloc::format!("close: {e:?}"))?;

    let fd = vfs::open("/etc/mpos-hostname").map_err(|e| alloc::format!("reopen: {e:?}"))?;
    let mut buf = [0u8; 32];
    let n = vfs::read(fd, &mut buf).map_err(|e| alloc::format!("read: {e:?}"))?;
    vfs::close(fd).map_err(|e| alloc::format!("close: {e:?}"))?;
    case_check!(
        &buf[..n] == b"mpos\n",
        "the bytes read back were {:?}, not the five that were written",
        &buf[..n]
    );
    Ok(())
}

/// A filesystem mounted on the bootstrap root, and two tasks reaching one file
/// through it.
///
/// The mount and the two tasks are one case rather than two because the second
/// has no fixture without the first: the tasks write and read `/tmp/shared`,
/// which is a name inside the filesystem this case mounts. A case that needed
/// another case to have run first is not a case.
fn mounted_tree_multi_task(_info: &crate::boot::BootInfo) -> Result<(), String> {
    let tmp = vfs::ramfs::RamFs::new();
    ensure_child(
        &vfs::resolve("/").expect("the bootstrap root resolves"),
        "tmp",
        NodeKind::Dir,
    );
    vfs::mount("/tmp", tmp.clone()).map_err(|e| alloc::format!("mount /tmp: {e:?}"))?;
    let inner_root = tmp.root();
    case_check!(
        Arc::ptr_eq(
            &vfs::resolve("/tmp").map_err(|e| alloc::format!("resolve /tmp: {e:?}"))?,
            &inner_root
        ),
        "resolving /tmp did not cross the mount"
    );
    case_check!(
        vfs::mount("/tmp", tmp.clone()).err() == Some(vfs::VfsError::Busy),
        "mounting a second filesystem over /tmp was not refused Busy"
    );
    // Inside the freshly mounted filesystem, so nothing of Alpine's is here to
    // collide with: it is a new root, not the tree behind the mount.
    vfs::resolve("/tmp")
        .map_err(|e| alloc::format!("resolve /tmp: {e:?}"))?
        .create("shared", NodeKind::File)
        .map_err(|e| alloc::format!("create /tmp/shared: {e:?}"))?;

    sched::spawn(
        || {
            let verdict = (|| -> Result<(), String> {
                let fd =
                    vfs::open("/tmp/shared").map_err(|e| alloc::format!("writer: open: {e:?}"))?;
                for chunk in [&b"one"[..], &b"two"[..]] {
                    case_check!(
                        vfs::write(fd, chunk)
                            .map_err(|e| alloc::format!("writer: write: {e:?}"))?
                            == 3,
                        "writer: a three-byte write reported another count"
                    );
                }
                vfs::close(fd).map_err(|e| alloc::format!("writer: close: {e:?}"))?;
                Ok(())
            })();
            WRITER_DONE.store(true, core::sync::atomic::Ordering::Relaxed);
            record_verdict(WRITER_SLOT, verdict);
            sched::exit(0);
        },
        Priority::NORMAL,
    );
    sched::spawn(
        || {
            while !WRITER_DONE.load(core::sync::atomic::Ordering::Relaxed) {
                sched::sleep_ticks(1);
            }
            let verdict = (|| -> Result<(), String> {
                let fd =
                    vfs::open("/tmp/shared").map_err(|e| alloc::format!("reader: open: {e:?}"))?;
                let mut buf = [0u8; 6];
                let n =
                    vfs::read(fd, &mut buf).map_err(|e| alloc::format!("reader: read: {e:?}"))?;
                vfs::close(fd).map_err(|e| alloc::format!("reader: close: {e:?}"))?;
                case_check!(n == 6, "reader: the file was {n} bytes, not 6");
                case_check!(
                    &buf == b"onetwo",
                    "reader: the bytes were {:?}, not onetwo",
                    &buf
                );
                Ok(())
            })();
            record_verdict(READER_SLOT, verdict);
            sched::exit(0);
        },
        Priority::NORMAL,
    );
    drain();

    take_verdict(WRITER_SLOT, "the writer task")?;
    take_verdict(READER_SLOT, "the reader task")?;
    Ok(())
}

/// The flags a node carries, and the refusals they owe.
///
/// `t-fsflags` pins the same rules through the syscall door; this case is the
/// *filesystem* half of the pair - the checks every path that changes a file
/// must ask, whether or not a program reaches them through `ioctl(2)`. Two
/// facts it adds that no payload program can: an append-only directory refuses
/// the removal of what lives in it (Linux's `may_delete`, asked of the parent),
/// and a symlink - a node with no attribute word of its own - answers
/// `ENOTTY`-shaped refusal to the flag question while still being policed by
/// its *parent's* flag like any other name. Both are kernel-side behaviour the
/// sweep never walks: the payload has no reason to `chattr +a` a directory, and
/// `FS_IOC_SETFLAGS` on a link goes through the fd table, which only a task
/// holding the descriptor can reach.
fn inode_flags_are_policed(_info: &crate::boot::BootInfo) -> Result<(), String> {
    use crate::vfs::{
        FS_APPEND_FL, FS_IMMUTABLE_FL, may_alter, may_create_in, may_modify, may_remove, may_write,
    };

    let root = vfs::resolve("/")
        .map_err(|e| alloc::format!("the bootstrap root does not resolve as `/`: {e:?}"))?;
    let dir = ensure_child(&root, "tmp", NodeKind::Dir);
    let file = ensure_child(&dir, "kunit-flags.bin", NodeKind::File);

    // No flags: every question answers yes.
    case_check!(
        file.set_flags(0).is_ok() && file.flags() == Ok(0),
        "a fresh file stores and reads the empty flag word"
    );
    case_check!(
        may_modify(&file).is_ok(),
        "an unflagged file may be modified"
    );
    case_check!(may_write(&file).is_ok(), "and written");
    case_check!(may_alter(&file).is_ok(), "and cut back or removed");
    case_check!(
        may_create_in(&dir).is_ok(),
        "and names may be made in an unflagged directory"
    );

    // IMMUTABLE: the metadata question and the content question both refuse,
    // and the extent question too.
    file.set_flags(FS_IMMUTABLE_FL)
        .map_err(|e| alloc::format!("set IMMUTABLE: {e:?}"))?;
    case_check!(
        file.flags() == Ok(FS_IMMUTABLE_FL),
        "IMMUTABLE round-trips through the node"
    );
    case_check!(
        may_modify(&file).err() == Some(vfs::VfsError::NotPermitted),
        "an immutable file refuses modification"
    );
    case_check!(
        may_write(&file).err() == Some(vfs::VfsError::NotPermitted),
        "and writing"
    );
    case_check!(
        may_alter(&file).err() == Some(vfs::VfsError::NotPermitted),
        "and cutting back or removing"
    );
    case_check!(
        may_remove(&dir, &file).err() == Some(vfs::VfsError::NotPermitted),
        "and removal through its parent - whatever the directory says"
    );

    // APPEND: the one place the two flags differ. Content may still change at
    // the end (`may_modify`/`may_write` answer yes - that is what `a` means),
    // but the extent is frozen, and so is the *directory*'s when the flag sits
    // on it: an append-only directory is one that may only be added to, so
    // `may_create_in` passes and `may_remove` of anything inside it refuses.
    file.set_flags(FS_APPEND_FL)
        .map_err(|e| alloc::format!("set APPEND: {e:?}"))?;
    case_check!(
        may_modify(&file).is_ok() && may_write(&file).is_ok(),
        "an append-only file still takes writes at its end"
    );
    case_check!(
        may_alter(&file).err() == Some(vfs::VfsError::NotPermitted),
        "but cannot be cut back or removed"
    );
    case_check!(
        may_remove(&dir, &file).err() == Some(vfs::VfsError::NotPermitted),
        "and its name cannot be taken away"
    );
    dir.set_flags(FS_APPEND_FL)
        .map_err(|e| alloc::format!("set APPEND on the directory: {e:?}"))?;
    case_check!(
        may_create_in(&dir).is_ok(),
        "an append-only directory still accepts new names"
    );
    case_check!(
        may_remove(&dir, &file).err() == Some(vfs::VfsError::NotPermitted),
        "and refuses the removal of what lives in it"
    );
    dir.set_flags(0)
        .map_err(|e| alloc::format!("clear APPEND: {e:?}"))?;
    file.set_flags(0)
        .map_err(|e| alloc::format!("clear flags: {e:?}"))?;

    // A symlink carries no attribute word: Linux answers a filesystem with no
    // `fileattr` half, and ramfs's link node is exactly that. Its *name* is
    // still policed by the directory's flag, because the check is about the
    // operation, not the kind.
    let link = dir
        .symlink("kunit-flags.lnk", "kunit-flags.bin")
        .map_err(|e| alloc::format!("create the link: {e:?}"))?;
    case_check!(
        link.flags().is_err(),
        "a symlink has no attribute word to read"
    );
    case_check!(
        link.set_flags(FS_IMMUTABLE_FL).is_err(),
        "and none to write"
    );
    dir.set_flags(FS_APPEND_FL)
        .map_err(|e| alloc::format!("set APPEND again: {e:?}"))?;
    case_check!(
        may_remove(&dir, &link).err() == Some(vfs::VfsError::NotPermitted),
        "yet an append-only directory refuses its removal like any other name"
    );
    dir.set_flags(0)
        .map_err(|e| alloc::format!("clear APPEND: {e:?}"))?;

    // The write path asks the flag through a description, which is where the
    // open-time check and the mid-life change meet: a file that becomes
    // immutable under a live writer stops it at the next `write`, and the
    // cursor moves nothing.
    let fd =
        vfs::open_inode(file.clone()).map_err(|e| alloc::format!("open the fixture: {e:?}"))?;
    case_check!(
        vfs::write(fd, b"before").map_err(|e| alloc::format!("write: {e:?}"))? == 6,
        "the fixture takes bytes while unflagged"
    );
    file.set_flags(FS_IMMUTABLE_FL)
        .map_err(|e| alloc::format!("set IMMUTABLE under the live writer: {e:?}"))?;
    case_check!(
        vfs::write(fd, b"after").err() == Some(vfs::VfsError::NotPermitted),
        "and the live descriptor's next write is refused - the flag is a fact about the file"
    );
    file.set_flags(0)
        .map_err(|e| alloc::format!("clear IMMUTABLE: {e:?}"))?;
    case_check!(
        vfs::write(fd, b"after").map_err(|e| alloc::format!("write after clearing: {e:?}"))? == 5,
        "and clearing the flag lets the same descriptor write again"
    );
    vfs::close(fd).map_err(|e| alloc::format!("close: {e:?}"))?;

    // Leave the tree as found: the boot's own device checks (§13.14's rule)
    // run on every session, and a case that changes shared state puts it back.
    dir.unlink("kunit-flags.lnk")
        .map_err(|e| alloc::format!("remove the link: {e:?}"))?;
    dir.unlink("kunit-flags.bin")
        .map_err(|e| alloc::format!("remove the file: {e:?}"))?;
    Ok(())
}
