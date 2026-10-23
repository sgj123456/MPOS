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
//! were made), and the *opened* descriptor reporting the **namespace's own
//! number** as its `st_ino` (4026531832 for `mnt:[4026531832]`), which is the
//! number the text carries and the reason two names of one namespace agree on
//! it. The procfs *link* keeps an inode number of its own (the procfs counter),
//! which is what `lstat` reports.
//!
//! This module is the two facts both halves need: the pool, and the text. The
//! pool is one counter because the measurement is one pool; the text is written
//! here rather than at the renderer because three places have to agree on it -
//! the link's `readlink`, the `setns` validation that parses the kind back out,
//! and the tests that pin both. The third fact - what an *opened* descriptor is
//! - is [`NsFile`], the file the walk denotes for a followed name.
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
/// Six of Linux's eight kinds - eight of its ten *names*, since PID and TIME
/// each have a `_for_children` spelling ([`Entry`]) - and the six the ladder
/// has built (§6.3). The rest are absent, which is the honest surface: a kernel
/// built without a namespace has no entry for it either
/// (`/proc/self/ns/time` is `ENOENT` on a machine whose `CONFIG_TIME_NS` is
/// off), so a reader sees a machine that has these and is told nothing it cannot
/// check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `CLONE_NEWNS`: what is mounted where.
    Mount,
    /// `CLONE_NEWIPC`: the four objects `ipcs` lists - POSIX message queues,
    /// and SysV's shared-memory segments, semaphore sets and message queues.
    ///
    /// The one kind whose state lives in **four** modules and in three crates
    /// (`sched`'s three SysV tables, `vfs`'s POSIX queue table): each keys its
    /// own table by this namespace's number, exactly as `net` keys its stack, so
    /// nothing here has to name any of them. Measured on a current host
    /// (`.workbuddy/probe_ipcns.c`, as root inside a user namespace, which is the
    /// only way an unprivileged caller may make one): the number a queue is known
    /// by **restarts at 0** in a fresh namespace, an id from the namespace left
    /// behind answers `EINVAL`, all four families move together, `/proc/sysvipc/*`
    /// follows the reader, and a `fork` stays where its parent is (§6.16).
    Ipc,
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
    /// `CLONE_NEWUSER`: the table an id is translated through at the boundary.
    ///
    /// The one kind that is a *mapping* rather than a view of shared state, and
    /// the one whose creation is itself a grant: an unprivileged caller may make
    /// one, and the moment it does it holds the full capability set *inside*
    /// (measured, §6.3). It moves its caller like `NET`, so it has no
    /// `_for_children` spelling, and the object is held by `Arc` because nothing
    /// else keys state by it.
    User,
}

impl Kind {
    /// The word the link's target starts with.
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Mount => "mnt",
            Kind::Ipc => "ipc",
            Kind::Net => "net",
            Kind::Pid => "pid",
            Kind::Uts => "uts",
            Kind::Time => "time",
            Kind::User => "user",
        }
    }

    /// The single `CLONE_*` bit `setns` must be given for this kind, and the
    /// bit the link's own `readlink` validation accepts. Zero is not a kind:
    /// `setns(fd, 0)` means "whatever this descriptor names", which the caller
    /// answers from the descriptor rather than from a bit.
    pub const fn flag(self) -> usize {
        match self {
            Kind::Mount => crate::abi::CLONE_NEWNS,
            Kind::Ipc => crate::abi::CLONE_NEWIPC,
            Kind::Net => crate::abi::CLONE_NEWNET,
            Kind::Pid => crate::abi::CLONE_NEWPID,
            Kind::Uts => crate::abi::CLONE_NEWUTS,
            Kind::Time => crate::abi::CLONE_NEWTIME,
            Kind::User => crate::abi::CLONE_NEWUSER,
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
            b if b == crate::abi::CLONE_NEWIPC => Some(Kind::Ipc),
            b if b == crate::abi::CLONE_NEWNET => Some(Kind::Net),
            b if b == crate::abi::CLONE_NEWPID => Some(Kind::Pid),
            b if b == crate::abi::CLONE_NEWUTS => Some(Kind::Uts),
            b if b == crate::abi::CLONE_NEWTIME => Some(Kind::Time),
            b if b == crate::abi::CLONE_NEWUSER => Some(Kind::User),
            _ => None,
        }
    }

    /// The kind a link's target text names, for the reader that got it from
    /// `readlink`. A `_for_children` word names the same kind - the flag and the
    /// kind are one question, which slot is another (see [`Entry`]).
    pub fn of_name(name: &str) -> Option<Kind> {
        match name {
            "ipc" => Some(Kind::Ipc),
            "mnt" => Some(Kind::Mount),
            "net" => Some(Kind::Net),
            "pid" | "pid_for_children" => Some(Kind::Pid),
            "uts" => Some(Kind::Uts),
            "time" | "time_for_children" => Some(Kind::Time),
            "user" => Some(Kind::User),
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
    pub const ALL: &'static [Kind] = &[
        Kind::Ipc,
        Kind::Mount,
        Kind::Net,
        Kind::Pid,
        Kind::Time,
        Kind::User,
        Kind::Uts,
    ];
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
            kind: Kind::Ipc,
            slot: Slot::Own,
        },
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
            kind: Kind::User,
            slot: Slot::Own,
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
    /// An IPC namespace, held as the **object** the four tables hang their state
    /// off; `None` is the machine's own.
    ///
    /// The object and not a number, unlike `Net`: those four tables need to know
    /// when their state may go, and a number cannot say that. The four families
    /// key their state by [`IpcNs::id`], while the `Arc` count is what says when
    /// to free it - a task that is in the namespace, or a
    /// `/proc/<pid>/ns/ipc` descriptor that names it, holds one (Linux's
    /// `ns_common` refcount).
    Ipc(Option<Arc<IpcNs>>),
    /// A network namespace, by the number `net` knows its stack by.
    Net(u64),
    Pid(Arc<crate::pidns::PidNs>),
    Uts(Arc<crate::uts::Uts>),
    /// A time namespace, as the object rather than a number: nothing else keys
    /// state by it (the offsets *are* the whole namespace), so there is no
    /// registry for a number to index and the `Arc` is the identity.
    Time(Arc<crate::timens::TimeNs>),
    /// A user namespace, as the object for the same reason TIME's is: the maps
    /// *are* the whole namespace, so there is nothing for a number to key and
    /// the `Arc` is the identity.
    User(Arc<crate::userns::UserNs>),
}

impl Namespace {
    pub fn kind(&self) -> Kind {
        match self {
            Namespace::Mount(_) => Kind::Mount,
            Namespace::Ipc(_) => Kind::Ipc,
            Namespace::Net(_) => Kind::Net,
            Namespace::Pid(_) => Kind::Pid,
            Namespace::Uts(_) => Kind::Uts,
            Namespace::Time(_) => Kind::Time,
            Namespace::User(_) => Kind::User,
        }
    }

    /// The number this namespace is known by, as its own object reports it.
    pub fn id(&self) -> u64 {
        match self {
            Namespace::Mount(table) => table.ns_id(),
            Namespace::Ipc(ns) => ns.as_ref().map_or_else(machine_ipc_id, |ns| ns.id()),
            Namespace::Net(ns_id) => *ns_id,
            Namespace::Pid(pidns) => pidns.ns_id(),
            Namespace::Uts(uts) => uts.ns_id(),
            Namespace::Time(timens) => timens.id(),
            Namespace::User(userns) => userns.id(),
        }
    }

    /// The text `/proc/<pid>/ns/<kind>` reads back as.
    pub fn link_text(&self) -> String {
        link_text(self.kind(), self.id())
    }

    /// The user namespace this namespace belongs to: the one its maker stood in
    /// when it was made.
    ///
    /// A capability question about a namespace *object* is asked of that
    /// object's owner, not of the caller's own - Linux's
    /// `ns_capable(ns->user_ns, cap)`, whose walk and measurements are
    /// [`crate::userns::Caller::capable_in`]. Measured on a current host
    /// (`.workbuddy/probe_ns_owner.c`): a caller inside a bare user namespace is
    /// `EPERM` joining the mount namespace it is standing in, and served joining
    /// one made inside its own.
    ///
    /// The owner is kept **here**, beside the pool that numbers the namespace,
    /// rather than on the six objects: a mount table, a pid namespace, a uts
    /// namespace, a time namespace, a stack's number and a user namespace live
    /// in five crates, and the alternative is six fields and five dependencies
    /// for one fact that is only ever read through the number. A namespace that
    /// is not in the table was made before anybody unshared, so its owner is the
    /// machine's.
    ///
    /// The one kind that is **its own** owner is `USER`: Linux's
    /// `struct ns_common` carries `user_ns`, and a user namespace's own points
    /// at itself - `setns` into one asks the namespace itself rather than the
    /// one it was made from, which is why joining a *sibling* from inside one
    /// is `EPERM` while joining one made *inside* it is served (the walk's
    /// third clause, [`crate::userns::Caller::capable_in`]).
    pub fn owner(&self) -> Arc<crate::userns::UserNs> {
        match self {
            crate::vfs::ns::Namespace::User(ns) => Arc::clone(ns),
            _ => owner_of(self.id()),
        }
    }
}

/// The file an opened namespace descriptor is: the namespace it named, pinned.
///
/// `/proc/<pid>/ns/<kind>` is a procfs *link*, and what opening it means is the
/// object it names - Linux resolves that at the open and hands back an `nsfs`
/// inode whose private data is the namespace itself, so the descriptor names
/// that namespace for as long as it is held. That is what makes `setns(fd)` a
/// join rather than a question about where the maker stands *now*, and what
/// lets a namespace outlive the process that opened it.
///
/// This inode is the shape that was missing (measured on a current host before
/// any of it was written, `.workbuddy/probe_ns_fd_stat.c`, Linux 7.2.9, as root
/// and as an unprivileged caller):
///
/// | the door | Linux | this kernel, before |
/// |---|---|---|
/// | `lstat` of the name | symlink `0777`, the procfs inode's own number | the same |
/// | `stat` of the name (followed) | regular `0444`, size 0 | the link again |
/// | `fstat` of the opened descriptor | regular `0444`, `st_ino` the **namespace's own number** | the link again |
/// | `read` | `EINVAL` | `EINVAL` |
/// | `lseek` | `ESPIPE` | `ESPIPE` |
/// | `open` with `O_WRONLY`, `O_RDWR` or `O_TRUNC` | `EPERM`, root and unprivileged alike | served |
/// | `open` with `O_APPEND` | served | served |
/// | open, then `unshare(CLONE_NEWNS)`, then `setns(fd)` | back in the namespace the descriptor named | left in the copy |
///
/// The inode number is the namespace's own, which is Linux's arrangement rather
/// than a coincidence: two names of one namespace (`/proc/self/ns/mnt` and
/// `/proc/1/ns/mnt`) report the same `st_ino` there, and it is the number the
/// link's text carries. `readlink` is untouched by any of this - it answers by
/// asking the live process, which is the *link's* own question.
pub struct NsFile {
    /// Resolved when the name was opened, and held from then on.
    ns: Namespace,
}

/// `0444`: the word Linux's `nsfs` inode carries. This kernel only *reports*
/// it - procfs keeps no modes to police (`docs/uapi/` §5.2) - which is why the
/// write-ish refusal below is the inode's own rule and not the bits'.
const NS_FILE_MODE: u32 = 0o444;

/// The open flags that ask for write access to a file that has none: an access
/// mode that is not `O_RDONLY` (the low two bits: `O_WRONLY` is 1, `O_RDWR` is
/// 2) and `O_TRUNC`, which Linux's `may_open` treats as a write whatever the
/// mode word says. The numbers are POSIX's and are written here rather than
/// imported from the ABI for the reason `ramfs`'s FIFO open gives: a filesystem
/// has no business naming the syscall layer.
const WRITE_ACCESS: usize = 0o3 | 0o1000;

impl NsFile {
    /// The inode a name under `/proc/<pid>/ns` denotes once it is known.
    pub fn opened(ns: Namespace) -> Arc<dyn crate::vfs::Inode> {
        Arc::new(NsFile { ns })
    }
}

impl crate::vfs::Inode for NsFile {
    fn metadata(&self) -> crate::vfs::Metadata {
        crate::vfs::Metadata {
            ino: self.ns.id(),
            kind: crate::vfs::NodeKind::File,
            size: 0,
        }
    }

    fn mode(&self) -> u32 {
        NS_FILE_MODE
    }

    fn as_namespace(&self) -> Option<Namespace> {
        Some(self.ns.clone())
    }

    fn open_replacement(
        &self,
        flags: usize,
    ) -> crate::vfs::VfsResult<Option<Arc<dyn crate::vfs::Inode>>> {
        // A namespace is not writable through, and Linux refuses a write-ish
        // open with `EPERM` whatever the caller's standing is (measured: the
        // same answer for root and for an unprivileged caller, which is what
        // says it is not the mode bits' `EACCES`). `O_APPEND` alone asks for
        // no write and is served, also measured.
        if flags & WRITE_ACCESS != 0 {
            return Err(crate::vfs::VfsError::NotPermitted);
        }
        Ok(None)
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> crate::vfs::VfsResult<usize> {
        // A namespace is not bytes, and `EINVAL` is Linux's answer rather than
        // a zero-length read.
        Err(crate::vfs::VfsError::Invalid)
    }

    fn seekable(&self) -> bool {
        false
    }
}

/// One entry per namespace ever minted: the number, and whose it is. Its own
/// alias because the type is three deep and the two readers below are clearer
/// for it.
type OwnerTable = critical_section::Mutex<
    core::cell::RefCell<alloc::vec::Vec<(u64, Arc<crate::userns::UserNs>)>>,
>;

/// Whose each namespace is, by the number it was given.
///
/// Written once, where a namespace is *made* - [`mint_owned`] is the only
/// writer - and read by [`Namespace::owner`]. One entry per namespace ever
/// minted, which is the same lifetime the number has: a namespace outlives the
/// process that made it, because a descriptor can hold it open.
static OWNERS: OwnerTable =
    critical_section::Mutex::new(core::cell::RefCell::new(alloc::vec::Vec::new()));

/// Mint a number for a namespace made by a caller standing in `owner`.
pub fn mint_owned(owner: Arc<crate::userns::UserNs>) -> u64 {
    let id = mint();
    critical_section::with(|cs| OWNERS.borrow(cs).borrow_mut().push((id, owner)));
    id
}

/// The user namespace that owns the namespace numbered `id`.
///
/// The machine's own for one that was never registered, which is every
/// namespace that exists before the first `unshare`: those are the machine's
/// tables, and the machine's user namespace is the only owner they can have.
pub fn owner_of(id: u64) -> Arc<crate::userns::UserNs> {
    let found = critical_section::with(|cs| {
        OWNERS
            .borrow(cs)
            .borrow()
            .iter()
            .find(|(held, _)| *held == id)
            .map(|(_, owner)| Arc::clone(owner))
    });
    found.unwrap_or_else(machine_user_ns)
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

/// The machine's own IPC namespace, minted on first use from the same pool.
///
/// The same shape and the same reason as [`machine_net_id`]: a task that has
/// never unshared holds no number of its own, so `/proc/<pid>/ns/ipc` and a
/// `setns` into the machine's namespace both need one, and the four tables key
/// their state by it. The machine's own is therefore a number like any other -
/// which is also what keeps the table keys unambiguous (§6.16).
static MACHINE_IPC_ID: AtomicU64 = AtomicU64::new(0);

pub fn machine_ipc_id() -> u64 {
    let id = MACHINE_IPC_ID.load(Ordering::Relaxed);
    if id != 0 {
        return id;
    }
    let fresh = mint();
    MACHINE_IPC_ID.store(fresh, Ordering::Relaxed);
    fresh
}

/// One IPC namespace, as the object the four IPC tables hang their state off.
///
/// Its `id` is the number every lookup takes; **this** is what says when the
/// state behind that number may go. A namespace is alive while a task is in it
/// or a descriptor names it - Linux's `ns_common` refcount - so the last `Arc`
/// drop is the moment to free the four tables' slot spaces. That matters
/// because the leak it closes is reachable from a program: `unshare(CLONE_NEWIPC)`
/// in a loop with one queue created each time grows the kernel without bound
/// (`MAX_TASKS` caps tasks, not namespaces), which is exactly the kind of
/// growth §8.0's panicking allocator makes fatal (§6.16).
///
/// The machine's own namespace is **not** one of these: it is `None` in
/// [`Namespace::Ipc`], numbered by [`machine_ipc_id`], and never freed.
pub struct IpcNs {
    id: u64,
}

impl IpcNs {
    /// A fresh namespace, owned by `owner` - the user namespace its maker stood
    /// in, which is what a `setns` into it asks for `CAP_SYS_ADMIN`.
    pub fn fresh(owner: Arc<crate::userns::UserNs>) -> Arc<IpcNs> {
        Arc::new(IpcNs {
            id: mint_owned(owner),
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for IpcNs {
    fn drop(&mut self) {
        // The last holder is gone. Each family drops the slot space it kept for
        // this number; anything a task that *left* still holds stays alive by
        // its own `Arc` - a segment mapped into an address space, a POSIX queue
        // an open descriptor names - which is Linux's shape too: a namespace's
        // exit takes the *ids*, not the objects somebody is still using.
        crate::sched::sysv_msg::forget(Some(self.id));
        crate::sched::sysv_sem::forget(Some(self.id));
        crate::mm::sysv_shm::forget(Some(self.id));
        crate::vfs::mqueue::forget(Some(self.id));
    }
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

/// The machine's own user namespace, made on first use from the same pool.
///
/// The same shape as [`machine_time_ns`] and for the same reason - a task in the
/// machine's namespace holds no namespace of its own, so `/proc/<pid>/ns/user`
/// and the three map files all have to answer with the machine's - and the
/// answer is an `Arc` because the maps are the whole of it.
///
/// It is the **identity map** (`"0 0 4294967295"`, which is what a real kernel
/// reports here) and it is **already written**, so every id translates to itself
/// and a write to its map is `EPERM`: the machine's ids are not a namespace's to
/// rename.
static MACHINE_USER: critical_section::Mutex<
    core::cell::RefCell<Option<Arc<crate::userns::UserNs>>>,
> = critical_section::Mutex::new(core::cell::RefCell::new(None));

pub fn machine_user_ns() -> Arc<crate::userns::UserNs> {
    critical_section::with(|cs| {
        let mut slot = MACHINE_USER.borrow(cs).borrow_mut();
        slot.get_or_insert_with(|| crate::userns::UserNs::machine(mint()))
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
        assert_eq!(link_text(Kind::Ipc, 4026531839), "ipc:[4026531839]");
        assert_eq!(Kind::of_name("ipc"), Some(Kind::Ipc));
        // A kind this kernel does not have is not recoverable from a word: the
        // entry does not exist, which is the whole refusal.
        assert_eq!(Kind::of_name("cgroup"), None);
        assert_eq!(Kind::of_name("user"), Some(Kind::User));
    }

    /// A namespace's state lives exactly as long as the namespace does: the last
    /// `Arc` drop frees the four tables' slot spaces for its number.
    ///
    /// The counterfactual is the leak this closes, and it is reachable from a
    /// program: `unshare(CLONE_NEWIPC)` in a loop with one queue made each time
    /// grows the kernel without bound (`MAX_TASKS` caps tasks, not namespaces),
    /// which §8.0's panicking allocator turns into a dead machine (§6.16).
    #[test]
    fn the_last_reference_frees_the_ipc_tables() {
        let ns = IpcNs::fresh(machine_user_ns());
        let id = ns.id();
        let key = Some(id);
        // One object in each of the four families, each in this namespace.
        let queue = crate::sched::sysv_msg::get_or_create(
            key,
            crate::sched::sysv_msg::New {
                key: 0,
                mode: 0o600,
                uid: 0,
                gid: 0,
                now: 1,
            },
            true,
            false,
        )
        .expect("a queue")
        .0;
        let set = crate::sched::sysv_sem::get_or_create(
            key,
            crate::sched::sysv_sem::New {
                key: 0,
                nsems: 1,
                mode: 0o600,
                uid: 0,
                gid: 0,
                now: 1,
            },
            true,
            false,
        )
        .expect("a set")
        .0;
        let segment = crate::mm::sysv_shm::get_or_create(
            key,
            crate::mm::sysv_shm::New {
                key: 0,
                size: 4096,
                mode: 0o600,
                uid: 0,
                gid: 0,
                cpid: 0,
                now: 1,
            },
            true,
            false,
        )
        .expect("a segment")
        .0;
        let name = "/mpos-ns-lifetime";
        let mq = crate::vfs::mqueue::create(key, String::from(name), 10, 8192, 0, u64::MAX)
            .expect("a POSIX queue");

        // All four are live in the namespace, and none of them is in the
        // machine's own space.
        assert!(crate::sched::sysv_msg::lookup(key, queue.id() as i64).is_some());
        assert!(crate::sched::sysv_sem::lookup(key, set.id() as i64).is_some());
        assert!(crate::mm::sysv_shm::lookup(key, segment.id() as i64).is_some());
        assert!(crate::vfs::mqueue::lookup(key, name).is_some());
        assert!(
            !crate::sched::sysv_msg::all(None)
                .iter()
                .any(|q| Arc::ptr_eq(q, &queue))
        );

        drop(ns);

        // The namespace is gone, so its state is: every table has let the space
        // go, and the numbers it had resolve to nothing.
        assert!(crate::sched::sysv_msg::all(key).is_empty());
        assert!(crate::sched::sysv_sem::all(key).is_empty());
        assert!(crate::mm::sysv_shm::all(key).is_empty());
        assert!(crate::vfs::mqueue::lookup(key, name).is_none());
        assert!(crate::sched::sysv_msg::lookup(key, queue.id() as i64).is_none());
        assert!(crate::sched::sysv_sem::lookup(key, set.id() as i64).is_none());
        assert!(crate::mm::sysv_shm::lookup(key, segment.id() as i64).is_none());

        // The objects themselves are still alive - a task that *left* the
        // namespace holds its own reference - which is why dropping the space
        // cannot free memory somebody is still using.
        assert_eq!(queue.stats().1, 0);
        assert_eq!(set.nsems(), 1);
        // The segment is *marked* destroyed rather than freed: a mapping that
        // outlived the namespace still holds it, and its frames come back when
        // that last reference goes (`Segment`'s `Drop`).
        assert!(segment.is_reapable());
        assert_eq!(mq.name(), name);
    }

    /// The directory's names, pinned as the *complete* list against a measured
    /// one: a real kernel lists ten (`cgroup, ipc, mnt, net, pid,
    /// pid_for_children, time, time_for_children, user, uts`) and this one lists
    /// the nine of those whose kinds are built, in the same order - `cgroup`
    /// alone is the one with nothing behind it.
    #[test]
    fn the_directory_names_are_the_ones_a_real_kernel_lists() {
        let names: alloc::vec::Vec<&str> = Entry::ALL.iter().map(|entry| entry.name()).collect();
        assert_eq!(
            names,
            [
                "ipc",
                "mnt",
                "net",
                "pid",
                "pid_for_children",
                "time",
                "time_for_children",
                "user",
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

    /// The pool hands out a new number every time.
    #[test]
    fn the_pool_hands_out_a_new_number_every_time() {
        let a = mint();
        let b = mint();
        assert_ne!(a, b);
        assert!(b > a);
    }

    /// The opened descriptor's *shape*, walked row by row against the host
    /// measurement in [`NsFile`]'s own table: a regular file of size zero whose
    /// inode number is the namespace's, `0444`, readable as nothing (`EINVAL`),
    /// unseekable - and a write-ish open refused `EPERM` while `O_APPEND`, which
    /// asks for no write, is left to the opener.
    ///
    /// The kind is `Net` because a *number* is the whole of that namespace: the
    /// shape does not depend on which kind it is, and this is the one a host
    /// build can make without a mount table or a task table behind it.
    #[test]
    fn an_opened_namespace_descriptor_has_the_shape_a_host_measured() {
        use crate::vfs::{Inode, NodeKind, VfsError};
        let id = 4026531833;
        let file = NsFile::opened(Namespace::Net(id));

        let meta = file.metadata();
        assert_eq!(
            meta.ino, id,
            "the descriptor's inode number is the namespace's"
        );
        assert_eq!(
            meta.kind,
            NodeKind::File,
            "`stat` reports a regular file, not the link"
        );
        assert_eq!(meta.size, 0, "and no size, as Linux reports");
        assert_eq!(file.mode(), 0o444, "the word Linux's nsfs inode carries");
        assert!(!file.seekable(), "`lseek` is ESPIPE");

        let mut buf = [0u8; 4];
        assert_eq!(
            file.read_at(0, &mut buf),
            Err(VfsError::Invalid),
            "`read` is EINVAL"
        );

        // The pin: what the descriptor answers is the namespace it was made
        // with, whatever the process has done since.
        match file.as_namespace() {
            Some(Namespace::Net(held)) => assert_eq!(held, id),
            Some(other) => panic!("the descriptor names the wrong kind: {:?}", other.kind()),
            None => panic!("the descriptor must name a namespace at all"),
        }

        // The write-ish open, measured as `EPERM` for root and for an
        // unprivileged caller alike - so it is the inode's rule and not the
        // mode bits' `EACCES` - with the two controls that must *not* refuse.
        for flags in [1, 2, 0o1000, 0o1001, 0o1002, 0o4001] {
            assert_eq!(
                file.open_replacement(flags).err(),
                Some(VfsError::NotPermitted),
                "open flags {flags:#o} ask for write access and must be EPERM"
            );
        }
        // `O_APPEND` alone asks for none - and `O_NONBLOCK` with it changes
        // nothing, because a namespace descriptor never parks.
        for flags in [0, 0o4000, 0o2000, 0o6000] {
            assert!(
                matches!(file.open_replacement(flags), Ok(None)),
                "open flags {flags:#o} ask for no write and must be served"
            );
        }
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
        assert_eq!(Kind::of_flag(crate::abi::CLONE_NEWCGROUP), None);
        assert_eq!(Kind::of_flag(crate::abi::CLONE_NEWUSER), Some(Kind::User));
        assert_eq!(Kind::User.flag(), crate::abi::CLONE_NEWUSER);
        assert_eq!(Kind::of_flag(0), None);
        assert_eq!(Kind::of_flag(0x1000), None);
        // `CLONE_NEWTIME` is a kind now, and its bit is the one in the signal
        // byte - which is why the value is pinned in `abi` as well.
        assert_eq!(Kind::of_flag(crate::abi::CLONE_NEWTIME), Some(Kind::Time));
        assert_eq!(Kind::Time.flag(), crate::abi::CLONE_NEWTIME);
    }
}
