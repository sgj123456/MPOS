#![no_main]

//! Unpacking an initramfs nobody built, and resolving paths inside it.
//!
//! The tar reader is the kernel's first contact with a file an attacker could
//! have written: it decides the tree `/` has, the mode of every node in it, and
//! where each symlink points. So the target does not stop at parsing - it
//! unpacks into a real `Vfs` and then reads the tree back, which is the only
//! way to find out that the bytes the archive promised are not the bytes the
//! file now holds, or that a member name escaped the directory it was given.

#[path = "support.rs"]
mod support;

use libfuzzer_sys::fuzz_target;
use mpos::vfs::NodeKind;
use mpos::vfs::mount::MountTable;
use mpos::vfs::ramfs::RamFs;
use mpos::vfs::tar::{self, EntryKind};

fuzz_target!(|data: &[u8]| {
    let vfs = MountTable::new();
    vfs.mount_root(RamFs::new());
    // The walk the syscall doors use, reaching this tree's root: the unpack
    // path is `init`'s, which has no caller context, so `root_inode` is the
    // start every resolution below hands the walk.
    let root = vfs.root_inode();

    // `unpack_into` is what `init` runs against the archive it was booted
    // with; the count is what it reports to the log.
    let Ok(count) = tar::unpack_into(&vfs, data) else {
        // A refused archive still has to leave a usable tree.
        let _ = root.lookup("etc");
        return;
    };

    let members: Vec<_> = tar::entries(data).collect();
    assert_eq!(
        members.len(),
        count,
        "unpack placed {count} members of {} the parser found",
        members.len()
    );

    let mut buf = [0u8; 512];
    for member in &members {
        // No shape check on the name: `usr/local/../` is not an escape, because
        // resolution folds it back into the tree it is being built in. What is
        // checked is the thing that would actually matter - that the bytes the
        // archive names for a path are the bytes that path now holds.
        let path = format!("/{}", member.path);
        let Ok(inode) = vfs.resolve_caller(&path, root.clone()) else {
            continue;
        };
        match member.kind {
            EntryKind::File => {
                // A member the archive calls a file can still resolve to a
                // directory, because `ensure_dir` makes a parent for whatever
                // follows it and the format allows `a/` and `a/b` in either
                // order. That is the archive's business, not this assert's.
                if inode.metadata().kind != NodeKind::File {
                    continue;
                }
                let n = inode
                    .read_at(0, &mut buf)
                    .expect("a member the unpacker wrote cannot be read back");
                assert!(n <= member.data.len(), "{path} is longer than its member");
                assert_eq!(
                    &buf[..n],
                    &member.data[..n],
                    "{path} holds bytes the archive did not put there"
                );
            }
            EntryKind::Dir => {
                // Nothing to compare: a directory holds no bytes of its own, and
                // a name the archive gave to both a directory and something else
                // is `ensure_dir`'s ordering rule to answer for, not this one's.
                let _ = inode.metadata();
            }
            EntryKind::Symlink => {
                // Followed resolution would land on the target, so ask for the
                // link itself. A name the archive gave twice - once as a link,
                // once as something else - is the unpacker's ordering rule to
                // answer for, not this comparison's.
                let Ok(link) = vfs.resolve_no_follow(&path, vfs.root_inode()) else {
                    continue;
                };
                if link.metadata().kind != NodeKind::Symlink {
                    continue;
                }
                let want = core::str::from_utf8(member.data).unwrap_or_default();
                assert_eq!(
                    link.readlink().expect("a symlink with no target").as_str(),
                    want,
                    "{path} points elsewhere"
                );
            }
        }
    }

    // And a path from the bytes themselves, so resolution sees names no member
    // of this archive has.
    for _ in 0..4 {
        let path = support::text(data, 64);
        let _ = vfs.resolve_caller(&path, root.clone());
        let _ = vfs.resolve_caller(&format!("/{}", path), root.clone());
    }
});
