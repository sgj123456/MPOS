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
