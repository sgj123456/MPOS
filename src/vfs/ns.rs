//! Namespace identity: the number a namespace is known by, and the text
//! `/proc/<pid>/ns/<kind>` reads back as.
//!
//! A namespace is an object a task holds (see `crates/uts`'s header for why),
//! and Linux gives each *instance* a number so that userland can tell two of
//! them apart without holding either: `/proc/<pid>/ns/mnt` is a link whose
//! target reads `mnt:[4026531832]`, `lsns` groups processes by that number,
//! and a caller that opens the link gets a descriptor naming the namespace the
//! number names. Measured on a current host (7.2.9): the ten names in the
//! directory, every one of them a symlink (`lstat` mode 777), each target
//! `kind:[<n>]`, the numbers drawn from **one pool shared by every kind** (that
//! host's `net`, `uts`, `ipc`, `pid`, `user`, `mnt`, `cgroup` and `time` hold
//! 4026531833 down to 4026531832 - consecutive, in the order the namespaces
//! were made), and the *opened* descriptor reporting a different `st_ino`
//! (148898) than the link's text, so the text is the identity a reader uses.
//!
//! This module is the two facts both halves need: the pool, and the text. The
//! pool is one counter because the measurement is one pool; the text is written
//! here rather than at the renderer because three places have to agree on it -
//! the link's `readlink`, the `setns` validation that parses the kind back out,
//! and the tests that pin both.
//!
//! **What is not here**: the namespace *objects*. A [`Namespace`] names one of
//! the two this kernel has (the mount table and the names) as a handle to the
//! value itself, because `setns` joins the object rather than copying it -
//! Linux's `mntns_install`/`utsns_install` point the caller at the namespace
//! the descriptor holds, which is what makes a `setns` different from an
//! `unshare`. The kinds this kernel does not implement have no entry at all in
//! `/proc/<pid>/ns` - a machine configured without them, which is what a real
//! kernel without `CONFIG_TIME_NS` answers - so nothing here has to invent a
//! number for a namespace that does not exist.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

/// The kinds `/proc/<pid>/ns` names here.
///
/// Two of Linux's ten, and the two the ladder has built (§6.3). The rest are
/// absent, which is the honest surface: a kernel built without a namespace has
/// no entry for it either (`/proc/self/ns/time` is `ENOENT` on a machine whose
/// `CONFIG_TIME_NS` is off), so a reader sees a machine that has these two and
/// is told nothing it cannot check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `CLONE_NEWNS`: what is mounted where.
    Mount,
    /// `CLONE_NEWUTS`: the names `uname(2)` reports.
    Uts,
}

impl Kind {
    /// The word the link's target starts with.
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Mount => "mnt",
            Kind::Uts => "uts",
        }
    }

    /// The single `CLONE_*` bit `setns` must be given for this kind, and the
    /// bit the link's own `readlink` validation accepts. Zero is not a kind:
    /// `setns(fd, 0)` means "whatever this descriptor names", which the caller
    /// answers from the descriptor rather than from a bit.
    pub const fn flag(self) -> usize {
        match self {
            Kind::Mount => crate::abi::CLONE_NEWNS,
            Kind::Uts => crate::abi::CLONE_NEWUTS,
        }
    }

    /// The kind a `CLONE_*` bit names, if it names one of these.
    pub fn of_flag(bit: usize) -> Option<Kind> {
        match bit {
            b if b == crate::abi::CLONE_NEWNS => Some(Kind::Mount),
            b if b == crate::abi::CLONE_NEWUTS => Some(Kind::Uts),
            _ => None,
        }
    }

    /// The kind a link's target text names, for the reader that got it from
    /// `readlink`.
    pub fn of_name(name: &str) -> Option<Kind> {
        match name {
            "mnt" => Some(Kind::Mount),
            "uts" => Some(Kind::Uts),
            _ => None,
        }
    }

    /// Every kind, in the order the directory lists them (Linux lists its ten
    /// in name order; these two are `mnt` before `uts` for the same reason).
    pub const ALL: &'static [Kind] = &[Kind::Mount, Kind::Uts];
}

/// A namespace, as the object a descriptor holds and `setns` joins.
///
/// The handle is the value's own `Arc` rather than a copy of what it holds:
/// joining a namespace means *sharing* it (a mount made after a `setns` is seen
/// by everyone in that namespace, which is the point of the call), and the two
/// arms are the two kinds this kernel has. A descriptor keeps the `Arc` alive,
/// so a namespace outlives the process that opened it - which is why
/// `/proc/<pid>/ns/mnt` stays openable after that process exits.
#[derive(Clone)]
pub enum Namespace {
    Mount(Arc<crate::vfs::mount::MountTable>),
    Uts(Arc<crate::uts::Uts>),
}

impl Namespace {
    pub fn kind(&self) -> Kind {
        match self {
            Namespace::Mount(_) => Kind::Mount,
            Namespace::Uts(_) => Kind::Uts,
        }
    }

    /// The number this namespace is known by, as its own object reports it.
    pub fn id(&self) -> u64 {
        match self {
            Namespace::Mount(table) => table.ns_id(),
            Namespace::Uts(uts) => uts.ns_id(),
        }
    }

    /// The text `/proc/<pid>/ns/<kind>` reads back as.
    pub fn link_text(&self) -> String {
        link_text(self.kind(), self.id())
    }
}

/// The link's target text: `kind:[<id>]`, which is what a real kernel's
/// `readlink` answers (measured above).
pub fn link_text(kind: Kind, id: u64) -> String {
    format!("{}:[{}]", kind.name(), id)
}

/// The pool every kind's numbers come from.
///
/// Linux's numbers are per-*type* inode numbers from one allocator, which is
/// why that host's are consecutive across kinds; one counter here is the same
/// arrangement, and the starting value is Linux's own (`0xf0000000` region) so
/// that a number printed by this kernel cannot be mistaken for a small file
/// inode by a reader that only eyeballs them.
static NEXT_ID: AtomicU64 = AtomicU64::new(0xf000_0000);

/// Mint the next namespace number. Called where a namespace is *made*: the
/// machine's tables at boot, a copy at `unshare`, and never at read time - an
/// identity that changed between two reads of one namespace would be no
/// identity at all.
pub fn mint() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text is the format a reader parses, and it is pinned against a real
    /// kernel's answer rather than against itself: measured on this host,
    /// `/proc/self/ns/mnt` reads `mnt:[4026531832]`.
    #[test]
    fn the_link_text_is_what_a_reader_parses() {
        assert_eq!(link_text(Kind::Mount, 4026531832), "mnt:[4026531832]");
        assert_eq!(link_text(Kind::Uts, 4026531838), "uts:[4026531838]");
        // And the kind is recoverable from the word, which is how `setns`
        // checks that a descriptor's kind matches the bit it was given.
        assert_eq!(Kind::of_name("mnt"), Some(Kind::Mount));
        assert_eq!(Kind::of_name("uts"), Some(Kind::Uts));
        assert_eq!(Kind::of_name("pid"), None);
    }

    #[test]
    fn the_pool_hands_out_a_new_number_every_time() {
        let a = mint();
        let b = mint();
        assert_ne!(a, b);
        assert!(b > a);
    }

    /// Every kind's name and bit, in one place, so the three readers of the
    /// vocabulary cannot drift.
    #[test]
    fn the_kinds_carry_their_flag_and_their_name() {
        for kind in Kind::ALL {
            assert_eq!(Kind::of_flag(kind.flag()), Some(*kind));
            assert_eq!(Kind::of_name(kind.name()), Some(*kind));
        }
        // A bit that is a namespace and not one of these is *not* a kind: the
        // caller hears their own word back rather than a wrong namespace.
        assert_eq!(Kind::of_flag(crate::abi::CLONE_NEWPID), None);
        assert_eq!(Kind::of_flag(0), None);
        assert_eq!(Kind::of_flag(0x1000), None);
    }
}
