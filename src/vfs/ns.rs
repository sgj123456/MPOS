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
//! the four this kernel has (the mount table, the numbers, the names and the
//! network stack) as a handle to the value itself, because `setns` joins the
//! object rather than copying it -
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
/// Five of Linux's eight kinds - seven of its ten *names*, since PID and TIME
/// each have a `_for_children` spelling ([`Entry`]) - and the five the ladder
/// has built (§6.3). The rest are absent, which is the honest surface: a kernel
/// built without a namespace has no entry for it either
/// (`/proc/self/ns/time` is `ENOENT` on a machine whose `CONFIG_TIME_NS` is
/// off), so a reader sees a machine that has these and is told nothing it cannot
/// check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `CLONE_NEWNS`: what is mounted where.
    Mount,
    /// `CLONE_NEWPID`: the numbers a task is known by.
    ///
    /// The one kind whose *entry* is a namespace of its own rather than a view
    /// of shared state: a task's number is a fact about the namespace it is in,
    /// so two namespaces number one task differently and neither is "the"
    /// answer.
    Pid,
    /// `CLONE_NEWUTS`: the names `uname(2)` reports.
    Uts,
    /// `CLONE_NEWNET`: the network stack a task resolves and binds through.
    ///
    /// The one kind whose handle is a *number* rather than an object here: the
    /// stacks live in `net`'s own registry, keyed by this number, because a
    /// `sched` holding an `Arc<NetNs>` would have to name `net` - and `net`
    /// already names `sched` (it wakes tasks), so that is a mutual dependency the
    /// design does not have (`ARCHITECTURE.md` §2/§3; the ratchet in
    /// `cargo xtask depgraph` counts those pairs). The number is the identity
    /// either way, and this is where the identity layer keeps it.
    Net,
    /// `CLONE_NEWTIME`: the offsets a task's monotonic clocks are read through.
    ///
    /// The one kind whose *only* way in is `unshare`: `clone(CLONE_NEWTIME)` is
    /// `EINVAL` on a real kernel (measured, §6.3), so there is no flag path
    /// through child creation and the namespace is always made by the caller
    /// that then hands it to its children.
    Time,
}

impl Kind {
    /// The word the link's target starts with.
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Mount => "mnt",
            Kind::Net => "net",
            Kind::Pid => "pid",
            Kind::Uts => "uts",
            Kind::Time => "time",
        }
    }

    /// The single `CLONE_*` bit `setns` must be given for this kind, and the
    /// bit the link's own `readlink` validation accepts. Zero is not a kind:
    /// `setns(fd, 0)` means "whatever this descriptor names", which the caller
    /// answers from the descriptor rather than from a bit.
    pub const fn flag(self) -> usize {
        match self {
            Kind::Mount => crate::abi::CLONE_NEWNS,
            Kind::Net => crate::abi::CLONE_NEWNET,
            Kind::Pid => crate::abi::CLONE_NEWPID,
            Kind::Uts => crate::abi::CLONE_NEWUTS,
            Kind::Time => crate::abi::CLONE_NEWTIME,
        }
    }

    /// Whether this kind has a *second* spelling in `/proc/<pid>/ns`, naming the
    /// namespace the task's **children** are created in rather than the one the
    /// task itself is in.
    ///
    /// True for exactly the two kinds whose entry is a children's slot:
    /// `unshare(CLONE_NEWPID)` and `unshare(CLONE_NEWTIME)` both replace what
    /// `nsproxy` keeps *for the next child* and leave the caller where it was,
    /// so a reader that wants to see either has to be given a name for that
    /// slot. `pid_for_children` and `time_for_children` are those names
    /// (measured: a real kernel's `/proc/self/ns` lists all ten, the two of
    /// these among them).
    pub const fn has_children_slot(self) -> bool {
        matches!(self, Kind::Pid | Kind::Time)
    }

    /// The kind a `CLONE_*` bit names, if it names one of these.
    pub fn of_flag(bit: usize) -> Option<Kind> {
        match bit {
            b if b == crate::abi::CLONE_NEWNS => Some(Kind::Mount),
            b if b == crate::abi::CLONE_NEWNET => Some(Kind::Net),
            b if b == crate::abi::CLONE_NEWPID => Some(Kind::Pid),
            b if b == crate::abi::CLONE_NEWUTS => Some(Kind::Uts),
            b if b == crate::abi::CLONE_NEWTIME => Some(Kind::Time),
            _ => None,
        }
    }

    /// The kind a link's target text names, for the reader that got it from
    /// `readlink`. A `_for_children` word names the same kind - the flag and the
    /// kind are one question, which slot is another (see [`Entry`]).
    pub fn of_name(name: &str) -> Option<Kind> {
        match name {
            "mnt" => Some(Kind::Mount),
            "net" => Some(Kind::Net),
            "pid" | "pid_for_children" => Some(Kind::Pid),
            "uts" => Some(Kind::Uts),
            "time" | "time_for_children" => Some(Kind::Time),
            _ => None,
        }
    }

    /// Every kind, in name order.
    ///
    /// Note that this is *not* what the directory is built from: the names
    /// `/proc/<pid>/ns` lists are [`Entry`]s, because PID and TIME have two
    /// names each. This is the kinds alone, which is what the flag vocabulary
    /// and a reader that only wants "which namespaces does this machine have"
    /// are asking for.
    pub const ALL: &'static [Kind] = &[Kind::Mount, Kind::Net, Kind::Pid, Kind::Time, Kind::Uts];
}

/// Which namespace *slot* of a task an entry of `/proc/<pid>/ns` reads.
///
/// The distinction exists because two kinds have two names each: a task has a
/// pid namespace it is *in* and a (possibly different) one its children will be
/// created in, and the same split Linux makes for time. The task's own slot is
/// the one that answers `getpid` and the one its own clocks come from; the
/// children's slot is what `unshare` and `setns` replace. Naming them apart is
/// the whole reason the directory has ten entries where it has eight kinds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Own,
    ForChildren,
}

/// One name in `/proc/<pid>/ns`: a kind, and which of its slots the name reads.
///
/// The listing is built from these rather than from [`Kind::ALL`] because the
/// directory's names are not one per kind: `pid` and `pid_for_children` are two
/// names for one kind's two slots, and so are `time` and `time_for_children`.
/// Keeping both in the vocabulary means the reader parses a name straight to the
/// entry it names, and a `setns` against either spelling asks the same kind
/// question it always did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub kind: Kind,
    pub slot: Slot,
}

impl Entry {
    /// Every name the directory lists, in name order - which is the order a real
    /// kernel lists them in, measured.
    pub const ALL: &'static [Entry] = &[
        Entry {
            kind: Kind::Mount,
            slot: Slot::Own,
        },
        Entry {
            kind: Kind::Net,
            slot: Slot::Own,
        },
        Entry {
            kind: Kind::Pid,
            slot: Slot::Own,
        },
        Entry {
            kind: Kind::Pid,
            slot: Slot::ForChildren,
        },
        Entry {
            kind: Kind::Time,
            slot: Slot::Own,
        },
        Entry {
            kind: Kind::Time,
            slot: Slot::ForChildren,
        },
        Entry {
            kind: Kind::Uts,
            slot: Slot::Own,
        },
    ];

    /// The word this entry is listed and read back as.
    pub const fn name(self) -> &'static str {
        match (self.kind, self.slot) {
            (Kind::Pid, Slot::ForChildren) => "pid_for_children",
            (Kind::Time, Slot::ForChildren) => "time_for_children",
            (kind, Slot::Own) => kind.name(),
            // A kind with no children's slot has no such name, and
            // [`Entry::of_name`] cannot produce one: this arm is unreachable and
            // says so by answering the kind's own name rather than panicking in
            // a `const fn`.
            (kind, Slot::ForChildren) => kind.name(),
        }
    }

    /// The entry a directory name names, for the reader that got it from the
    /// listing or from a `readlink`.
    pub fn of_name(name: &str) -> Option<Entry> {
        let kind = Kind::of_name(name)?;
        // The `_for_children` suffix is the slot, and only a kind that has that
        // slot has the name: `mnt_for_children` is not a namespace this or any
        // other kernel has, so it stays `None` rather than resolving to MNT.
        let slot = if name.ends_with("_for_children") {
            if !kind.has_children_slot() {
                return None;
            }
            Slot::ForChildren
        } else {
            Slot::Own
        };
        Some(Entry { kind, slot })
    }
}

/// A namespace, as the object a descriptor holds and `setns` joins.
///
/// The handle is the value's own `Arc` rather than a copy of what it holds:
/// joining a namespace means *sharing* it (a mount made after a `setns` is seen
/// by everyone in that namespace, which is the point of the call), and the arms
/// are the kinds this kernel has - minus `net`, whose handle is the number its
/// stack is registered under rather than the stack itself (the `Net` arm says
/// why). A descriptor keeps the `Arc` alive, so a namespace outlives the process
/// that opened it - which is why `/proc/<pid>/ns/mnt` stays openable after that
/// process exits.
#[derive(Clone)]
pub enum Namespace {
    Mount(Arc<crate::vfs::mount::MountTable>),
    /// A network namespace, by the number `net` knows its stack by.
    Net(u64),
    Pid(Arc<crate::pidns::PidNs>),
    Uts(Arc<crate::uts::Uts>),
    /// A time namespace, as the object rather than a number: nothing else keys
    /// state by it (the offsets *are* the whole namespace), so there is no
    /// registry for a number to index and the `Arc` is the identity.
    Time(Arc<crate::timens::TimeNs>),
}

impl Namespace {
    pub fn kind(&self) -> Kind {
        match self {
            Namespace::Mount(_) => Kind::Mount,
            Namespace::Net(_) => Kind::Net,
            Namespace::Pid(_) => Kind::Pid,
            Namespace::Uts(_) => Kind::Uts,
            Namespace::Time(_) => Kind::Time,
        }
    }

    /// The number this namespace is known by, as its own object reports it.
    pub fn id(&self) -> u64 {
        match self {
            Namespace::Mount(table) => table.ns_id(),
            Namespace::Net(ns_id) => *ns_id,
            Namespace::Pid(pidns) => pidns.ns_id(),
            Namespace::Uts(uts) => uts.ns_id(),
            Namespace::Time(timens) => timens.id(),
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

/// The machine's own network namespace, minted on first use from the same pool
/// as every other kind's.
///
/// It lives *here* rather than in `net` because two layers need it and neither
/// may name the other: `/proc/<pid>/ns/net` is `sched`'s to answer (for a task in
/// the machine's namespace, which holds no number of its own), and the stack the
/// number keys is `net`'s to keep. `sched` names `vfs` and `net` names `vfs`, so
/// the identity belongs with the pool - which is where every other namespace's
/// number is minted anyway.
static MACHINE_NET_ID: AtomicU64 = AtomicU64::new(0);

pub fn machine_net_id() -> u64 {
    let id = MACHINE_NET_ID.load(Ordering::Relaxed);
    if id != 0 {
        return id;
    }
    let fresh = mint();
    MACHINE_NET_ID.store(fresh, Ordering::Relaxed);
    fresh
}

/// The machine's own time namespace, made on first use from the same pool.
///
/// The same shape as [`machine_net_id`] and for the same reason - a task in the
/// machine's namespace holds no namespace of its own, so `/proc/<pid>/ns/time`
/// and `/proc/<pid>/timens_offsets` both have to answer with the machine's - but
/// the answer is an `Arc` rather than a number, because nothing else keys state
/// by a time namespace: the offsets are the whole of it, so the object *is* the
/// identity and a registry would have nothing to hold.
///
/// It is **frozen** from the instant it exists, which is the measured rule for a
/// namespace `init` is already in: writing `/proc/self/timens_offsets` on a
/// machine that has never unshared answers `EACCES`, and the file reads two zero
/// lines (§6.3).
static MACHINE_TIME: critical_section::Mutex<
    core::cell::RefCell<Option<Arc<crate::timens::TimeNs>>>,
> = critical_section::Mutex::new(core::cell::RefCell::new(None));

pub fn machine_time_ns() -> Arc<crate::timens::TimeNs> {
    critical_section::with(|cs| {
        let mut slot = MACHINE_TIME.borrow(cs).borrow_mut();
        slot.get_or_insert_with(|| crate::timens::TimeNs::machine(mint()))
            .clone()
    })
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
        assert_eq!(link_text(Kind::Pid, 4026531836), "pid:[4026531836]");
        assert_eq!(Kind::of_name("pid"), Some(Kind::Pid));
        assert_eq!(link_text(Kind::Net, 4026531833), "net:[4026531833]");
        assert_eq!(Kind::of_name("net"), Some(Kind::Net));
        assert_eq!(link_text(Kind::Time, 4026531834), "time:[4026531834]");
        assert_eq!(Kind::of_name("time"), Some(Kind::Time));
        // The two `_for_children` words name the same kinds as the plain ones:
        // which *slot* they read is the entry's business, not the kind's.
        assert_eq!(Kind::of_name("time_for_children"), Some(Kind::Time));
        assert_eq!(Kind::of_name("pid_for_children"), Some(Kind::Pid));
        // A kind this kernel does not have is not recoverable from a word: the
        // entry does not exist, which is the whole refusal.
        assert_eq!(Kind::of_name("ipc"), None);
        assert_eq!(Kind::of_name("user"), None);
    }

    /// The directory's names, pinned as the *complete* list against a measured
    /// one: a real kernel lists ten (`cgroup, ipc, mnt, net, pid,
    /// pid_for_children, time, time_for_children, user, uts`) and this one lists
    /// the seven of those whose kinds are built, in the same order.
    #[test]
    fn the_directory_names_are_the_ones_a_real_kernel_lists() {
        let names: alloc::vec::Vec<&str> = Entry::ALL.iter().map(|entry| entry.name()).collect();
        assert_eq!(
            names,
            [
                "mnt",
                "net",
                "pid",
                "pid_for_children",
                "time",
                "time_for_children",
                "uts"
            ]
        );
        // Every listed name parses back to the entry that produced it, so the
        // listing and the reader cannot drift - and a `_for_children` word for a
        // kind that has no such slot is not a name at all.
        for entry in Entry::ALL {
            assert_eq!(Entry::of_name(entry.name()), Some(*entry));
        }
        assert_eq!(Entry::of_name("mnt_for_children"), None);
        assert_eq!(Entry::of_name("net_for_children"), None);
        assert_eq!(Entry::of_name("uts_for_children"), None);
        assert_eq!(Entry::of_name("cgroup"), None);
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
        // caller hears their own word back rather than a wrong namespace. The
        // vocabulary is down to the kinds this kernel still has no copy of.
        assert_eq!(Kind::of_flag(crate::abi::CLONE_NEWIPC), None);
        assert_eq!(Kind::of_flag(crate::abi::CLONE_NEWUSER), None);
        assert_eq!(Kind::of_flag(0), None);
        assert_eq!(Kind::of_flag(0x1000), None);
        // `CLONE_NEWTIME` is a kind now, and its bit is the one in the signal
        // byte - which is why the value is pinned in `abi` as well.
        assert_eq!(Kind::of_flag(crate::abi::CLONE_NEWTIME), Some(Kind::Time));
        assert_eq!(Kind::Time.flag(), crate::abi::CLONE_NEWTIME);
    }
}
