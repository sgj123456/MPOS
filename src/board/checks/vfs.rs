//! `M6`/`M7`: the syscall gate, the ramfs root, a mounted tree, device files.

use alloc::sync::Arc;

use mpos::println;
use mpos::sched::{self, Priority};
use mpos::syscall::{self, err, linux::nr as lnr};
use mpos::vfs::{self, FileSystem, Inode, NodeKind};

use mpos::sched::kunit::drain;

/// The kernel's checks were written against a root filesystem of their own
/// making, where every name they wanted was free. Alpine's tree is not obliged
/// to leave those gaps: it has its own `/etc/hostname` and its own `/tmp`, and
/// both already hold something. The checks want a file to write through, not a
/// file that was not there before - so they pick names Alpine does not use.
fn ensure_child(dir: &Arc<dyn Inode>, name: &str, kind: NodeKind) -> Arc<dyn Inode> {
    match dir.lookup(name) {
        Ok(child) => child,
        Err(_) => dir.create(name, kind).expect("create the scratch file"),
    }
}

// ---- syscall + vfs: gate, ramfs, mounts, initramfs, devfs ---------------

/// Issue a Linux syscall through the one dispatch table both machines route
/// their gates through: one entry into [`syscall::linux_dispatch`], the path the
/// `syscall` gate drives for ring 3, called here from the kernel's own checks
/// with `from_user` false. It was `#[cfg(target_arch = "x86_64")]` while only the
/// first machine had a table to dispatch into; the table is one now, and the
/// gate was the stale kind.
pub(super) unsafe fn syscall(
    nr: u64,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
    unsafe { syscall::linux_dispatch(nr, [a1, a2, a3, a4, a5, a6], false, core::ptr::null_mut()) }
}

/// Prove the syscall gate, ramfs, mounts, initramfs unpack, and device filesystem.
pub(crate) fn run() {
    // M6: syscall dispatch and the fd table.
    {
        // Probe a number beyond the table: 999 is now CLEANUP_AFTER_KTHREAD.
        assert_eq!(
            unsafe {
                syscall::linux_dispatch(1024, [0; syscall::ARG_COUNT], false, core::ptr::null_mut())
            },
            err::ENOSYS
        );
        assert_eq!(
            unsafe {
                syscall::linux_dispatch(lnr::READ, [0, 0, 0, 0, 0, 0], false, core::ptr::null_mut())
            },
            0,
            "a zero-length read is empty, not an error"
        );
        assert_eq!(
            unsafe {
                syscall::linux_dispatch(lnr::READ, [3, 0, 0, 0, 0, 0], false, core::ptr::null_mut())
            },
            err::EBADF
        );
        assert_eq!(
            unsafe {
                syscall::linux_dispatch(
                    lnr::WRITE,
                    [7, 0, 0, 0, 0, 0],
                    false,
                    core::ptr::null_mut(),
                )
            },
            err::EBADF
        );

        let msg = b"syscall: write through dispatch\n";
        let wrote = unsafe {
            syscall::linux_dispatch(
                lnr::WRITE,
                [1, msg.as_ptr() as usize, msg.len(), 0, 0, 0],
                false,
                core::ptr::null_mut(),
            )
        };
        assert_eq!(wrote as usize, msg.len());

        // The same dispatch, run inside a real task: getpid answers this
        // task's id, yield and sleep suspend and resume it, and exit through
        // dispatch takes the task down.
        sched::spawn(
            || {
                let pid = unsafe {
                    syscall::linux_dispatch(
                        lnr::GETPID,
                        [0; syscall::ARG_COUNT],
                        false,
                        core::ptr::null_mut(),
                    )
                };
                assert!(pid >= 2, "getpid returned {pid}");

                let msg = b"syscall: write through dispatch in a spawned task\n";
                let wrote = unsafe {
                    syscall::linux_dispatch(
                        lnr::WRITE,
                        [1, msg.as_ptr() as usize, msg.len(), 0, 0, 0],
                        false,
                        core::ptr::null_mut(),
                    )
                };
                assert_eq!(wrote as usize, msg.len(), "dispatch dropped bytes");

                assert_eq!(
                    unsafe {
                        syscall::linux_dispatch(
                            lnr::SCHED_YIELD,
                            [0; syscall::ARG_COUNT],
                            false,
                            core::ptr::null_mut(),
                        )
                    },
                    0
                );
                // 2 ms, 1 byte, 0 padding — a request for the handler to
                // exercise, not to time precisely.
                let req: [u64; 2] = [0, 2_000_000];
                assert_eq!(
                    unsafe {
                        syscall::linux_dispatch(
                            lnr::NANOSLEEP,
                            [req.as_ptr() as usize, 0, 0, 0, 0, 0],
                            false,
                            core::ptr::null_mut(),
                        )
                    },
                    0
                );
                unsafe {
                    syscall::linux_dispatch(
                        lnr::EXIT_GROUP,
                        [0, 0, 0, 0, 0, 0],
                        false,
                        core::ptr::null_mut(),
                    );
                }
            },
            Priority::NORMAL,
        );
        drain();
        assert_eq!(
            sched::task_count(),
            sched::permanent_tasks(),
            "exit through dispatch did not end the task"
        );
        println!("M6_OK");
    }

    // M7: ramfs, mounts, multi-task file I/O through the fd table.
    {
        // Ensure /etc exists before poking a scratch file into it. The
        // initramfs normally carries /etc, but a stripped archive must not
        // turn into a panic in the middle of a milestone check.
        let etc = match vfs::resolve("/etc") {
            Ok(dir) => dir,
            Err(_) => vfs::resolve("/")
                .unwrap()
                .create("etc", NodeKind::Dir)
                .expect("create /etc"),
        };
        ensure_child(&etc, "mpos-hostname", NodeKind::File);

        let fd = vfs::open("/etc/mpos-hostname").expect("open /etc/mpos-hostname");
        assert_eq!(vfs::write(fd, b"mpos\n").unwrap(), 5);
        vfs::close(fd).unwrap();

        let fd = vfs::open("/etc/mpos-hostname").unwrap();
        let mut buf = [0u8; 32];
        let n = vfs::read(fd, &mut buf).unwrap();
        vfs::close(fd).unwrap();
        assert_eq!(&buf[..n], b"mpos\n");

        let tmp = vfs::ramfs::RamFs::new();
        ensure_child(&vfs::resolve("/").unwrap(), "tmp", NodeKind::Dir);
        vfs::mount("/tmp", tmp.clone()).expect("mount /tmp");
        let inner_root = tmp.root();
        assert!(
            Arc::ptr_eq(&vfs::resolve("/tmp").unwrap(), &inner_root),
            "resolving /tmp did not cross the mount"
        );
        assert_eq!(
            vfs::mount("/tmp", tmp.clone()).err(),
            Some(vfs::VfsError::Busy)
        );
        // Inside the freshly mounted filesystem, so nothing of Alpine's is here
        // to collide with - it is a new root, not the tree behind the mount.
        vfs::resolve("/tmp")
            .unwrap()
            .create("shared", NodeKind::File)
            .unwrap();

        sched::spawn(
            || {
                let fd = vfs::open("/tmp/shared").expect("writer: open");
                for chunk in [&b"one"[..], &b"two"[..]] {
                    assert_eq!(vfs::write(fd, chunk).unwrap(), 3, "writer: write");
                }
                vfs::close(fd).unwrap();
                sched::exit(0);
            },
            Priority::NORMAL,
        );
        sched::spawn(
            || {
                while sched::task_count() > sched::permanent_tasks() + 1 {
                    sched::sleep_ticks(1);
                }
                let fd = vfs::open("/tmp/shared").expect("reader: open");
                let mut buf = [0u8; 6];
                let n = vfs::read(fd, &mut buf).expect("reader: read");
                vfs::close(fd).unwrap();
                assert_eq!(n, 6, "reader: the file was short");
                assert_eq!(&buf, b"onetwo", "reader: the wrong bytes came back");
                sched::exit(0);
            },
            Priority::NORMAL,
        );
        drain();
        assert_eq!(
            sched::task_count(),
            sched::permanent_tasks(),
            "a vfs task did not exit"
        );
        println!("M7_OK");
    }
}
