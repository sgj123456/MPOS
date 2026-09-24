//! `epoll(7)`: the interest-list I/O multiplexer.
//!
//! A poll(2) caller names every descriptor on every call; an epoll caller
//! names them once, with `epoll_ctl`, and every later `epoll_wait` reports
//! only what changed. That split is the whole of the interface, and it is
//! why musl's default multiplexer is epoll: a server with a thousand
//! descriptors pays for the thousand once, not per wakeup.
//!
//! The instance is an anonymous inode holding one *interest list* - one
//! entry per `epoll_ctl` ADD, each remembering the descriptor, the caller's
//! `data` word, and the events asked for. Readiness is not cached: a wait
//! asks each watched node what it can do right now, through the same
//! `poll_ready` poll(2) consults, so the arrival that ends a wait is the
//! same arrival a blocking read would have seen. An entry whose descriptor
//! the caller has since closed is dropped on the next look - Linux removes
//! one at close, and without a close hook in the descriptor table the next
//! look is the earliest place the fact can be acted on.
//!
//! The interest list is keyed the way Linux keys it: by the descriptor
//! *and* the open file description it named at ADD time. `dup2` a new file
//! onto the number and ADD again, and the second registration is a second
//! entry - which is also the answer Linux gives.
//!
//! Edge-triggered (`EPOLLET`) and one-shot (`EPOLLONESHOT`) entries are
//! honest here, with one documented limit: the kernel observes a watched
//! node when a wait scans it, not at the instant data arrives, so an edge
//! that arrives and leaves entirely *between* two scans is invisible - to
//! this epoll and to any poll(2) caller alike. An edge that is still there
//! at scan time is reported, and reported only once, exactly as the man
//! page words it.
//!
//! The whole module compiles on the host: the interest list is plain data
//! over the descriptor table, and every rule above is pinned there by tests
//! that drive a real `FdTable` over a real pipe.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use super::file::{FdTable, File, RawFd};
use super::{Ino, Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

// ---------------------------------------------------------------------------
// The ABI: event bits, operations, and the packed epoll_event layout
// ---------------------------------------------------------------------------

/// EPOLLIN: a read would not block - data is waiting, or the end of the
/// stream has arrived and a read answers 0.
pub const EPOLLIN: u32 = 0x001;
/// EPOLLPRI: exceptional (out-of-band) data. Nothing here generates any, so
/// an entry asking only for this never fires - the honest answer for a
/// condition no driver produces.
pub const EPOLLPRI: u32 = 0x002;
/// EPOLLOUT: a write would not block.
pub const EPOLLOUT: u32 = 0x004;
/// EPOLLERR: an error condition on the descriptor. Linux reports this one
/// whether it was asked for or not; no node here reports it at all.
pub const EPOLLERR: u32 = 0x008;
/// EPOLLHUP: a hang-up on the descriptor. As with EPOLLERR: watched for
/// free on Linux, never produced here.
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLRDNORM: u32 = 0x040;
pub const EPOLLRDBAND: u32 = 0x080;
pub const EPOLLWRNORM: u32 = 0x100;
pub const EPOLLWRBAND: u32 = 0x200;
pub const EPOLLMSG: u32 = 0x400;
/// EPOLLRDHUP: the peer shut down its writing half. A stream node here
/// cannot say so - the read end's answer is "readable", and the 0 the read
/// returns is the news - so the bit is watched and never fires.
pub const EPOLLRDHUP: u32 = 0x2000;
pub const EPOLLEXCLUSIVE: u32 = 1 << 28;
pub const EPOLLWAKEUP: u32 = 1 << 29;
pub const EPOLLONESHOT: u32 = 1 << 30;
pub const EPOLLET: u32 = 1 << 31;

/// `epoll_ctl(2)`'s three operations.
pub const EPOLL_CTL_ADD: usize = 1;
pub const EPOLL_CTL_DEL: usize = 2;
pub const EPOLL_CTL_MOD: usize = 3;

/// One `struct epoll_event` of the x86_64 ABI: twelve bytes, and *packed* -
/// the `data` word sits at byte 4, not 8, because the struct carries a u32
/// and a u64 with no alignment padding between them. A wait that wrote
/// `data` at a word-aligned offset would hand the caller garbage for the
/// very field it uses to tell its descriptors apart.
pub const EPOLL_EVENT_SIZE: usize = 12;
/// Where `data` sits inside a `struct epoll_event`.
pub const EPOLL_EVENT_DATA: usize = 4;

/// How many descriptors one interest list may hold. Linux bounds this with
/// `/proc/sys/fs/epoll/max_user_watches` and answers ENOSPC when it is hit;
/// a list that cannot grow without bound is the same promise here, made at
/// a size that outstrips the descriptor limit itself.
pub const MAX_WATCHES: usize = 1024;

/// How deeply epoll instances may nest - one instance watching another -
/// before `epoll_ctl` answers ELOOP. The man page's number.
const MAX_NESTING: usize = 5;

/// One entry of the interest list.
struct Entry {
    /// The descriptor number as the caller spelled it. The number and the
    /// description below identify the entry together, the way Linux's
    /// interest list keys them.
    fd: RawFd,
    /// The open file description the number named at ADD time. Holding it
    /// is what Linux does too: an entry outlives a `close` of its number
    /// only until the next look, and a `dup` of the number before then still
    /// finds the same description here.
    file: Arc<File>,
    /// The events asked for, input flags (`EPOLLET`, `EPOLLONESHOT`, ...)
    /// included.
    events: u32,
    /// The `data` word, handed back verbatim on every report.
    data: u64,
    /// What this entry last saw for `EPOLLET`: bit 0 the read condition,
    /// bit 1 the write condition. An edge is a bit that was clear and is
    /// now set; see [`Epoll::collect`] for when the record is taken.
    et_last: u8,
    /// `EPOLLONESHOT`: reported once, then dark until a MOD re-arms it. The
    /// man page's words - "the user must call epoll_ctl() with
    /// EPOLL_CTL_MOD to rearm" - are what makes this a flag the *entry*
    /// keeps rather than one the wait consults.
    dead: bool,
}

/// One report a wait hands back: the events that occurred, and the caller's
/// `data` word for the descriptor they occurred on.
pub struct Reported {
    pub events: u32,
    pub data: u64,
}

/// One epoll instance: an anonymous inode whose whole body is the interest
/// list. The description the descriptor table names is an `Arc<File>` over
/// this, so a `dup` of the epfd is the same instance and a `fork` hands the
/// child the same one - which is Linux's answer to both.
pub struct Epoll {
    ino: Ino,
    state: Mutex<RefCell<Vec<Entry>>>,
}

impl Epoll {
    /// A fresh instance with an empty interest list.
    pub fn new(ino: Ino) -> Arc<Epoll> {
        Arc::new(Epoll {
            ino,
            state: Mutex::new(RefCell::new(Vec::new())),
        })
    }

    /// `epoll_ctl(2)`'s ADD, MOD and DEL, with the man page's errors in
    /// Linux's order of refusal:
    ///
    /// * `EBADF` - the target descriptor is not open;
    /// * `EINVAL` - the operation is not one of the three, the target is
    ///   the epfd itself, `EPOLLEXCLUSIVE` is misused (on MOD, on an entry
    ///   that had it, on an epoll target, or beside an event it may not
    ///   accompany), or a MOD's target never registered;
    /// * `EPERM` - the target is a regular file or directory, which Linux
    ///   refuses to poll through epoll at all;
    /// * `EEXIST` / `ENOENT` - ADD of a registered pair, or MOD/DEL of an
    ///   unregistered one;
    /// * `ELOOP` - this ADD would nest epoll instances too deeply or into
    ///   a circle;
    /// * `ENOSPC` - the interest list is full.
    ///
    /// `caller` is the calling process's descriptor table: it is what
    /// turns a descriptor number into a description, and what tells a
    /// live entry from one whose number has since been closed.
    pub fn ctl(
        &self,
        caller: &FdTable,
        epfd: RawFd,
        op: usize,
        fd: RawFd,
        events: u32,
        data: u64,
    ) -> Result<(), isize> {
        use crate::syscall::err;

        // Entries whose number no longer names their description come out
        // first, so every question below - EEXIST, ENOENT, EPERM - is asked
        // about a list that holds only what the caller still has open,
        // which is the list Linux keeps.
        critical_section::with(|cs| {
            let mut entries = self.state.borrow(cs).borrow_mut();
            entries.retain(|entry| alive(caller, entry));

            match op {
                EPOLL_CTL_ADD => {
                    if fd == epfd {
                        return Err(err::EINVAL);
                    }
                    let file = caller.clone_at(fd).map_err(|_| err::EBADF)?;
                    // Linux's refusal set: epoll does not watch what can
                    // never make a caller wait. A regular file is always
                    // ready and a directory is not a thing anyone polls,
                    // and pretending to watch them would hand a caller
                    // spurious wakeups in both directions.
                    //
                    // The *kind* alone cannot tell a pipe or a socket from
                    // a file - both report `NodeKind::File` - so the
                    // two who can wait are named before the refusal lands:
                    // a pipe can block a reader, and a socket is the whole
                    // reason epoll exists.
                    let epollable = match file.metadata().kind {
                        NodeKind::File => {
                            file.inode().as_pipe().is_some() || file.inode().socket_handle() != 0
                        }
                        NodeKind::Dir => false,
                        _ => true,
                    };
                    if !epollable {
                        return Err(err::EPERM);
                    }
                    if events & EPOLLEXCLUSIVE != 0 {
                        // The one flag with a restricted vocabulary: the
                        // man page allows EPOLLIN, EPOLLOUT, EPOLLWAKEUP
                        // and EPOLLET beside it - EPOLLERR and EPOLLHUP
                        // ride along for free, as always - and refuses
                        // everything else.
                        const EXCLUSIVE_OK: u32 = EPOLLEXCLUSIVE
                            | EPOLLIN
                            | EPOLLOUT
                            | EPOLLWAKEUP
                            | EPOLLET
                            | EPOLLERR
                            | EPOLLHUP;
                        if events & !EXCLUSIVE_OK != 0 {
                            return Err(err::EINVAL);
                        }
                        // And it is an ADD-only flag that may not aim at
                        // an epoll instance - a herd is what it exists to
                        // prevent, and an epoll target would feed one.
                        if file.inode().as_epoll().is_some() {
                            return Err(err::EINVAL);
                        }
                    }
                    // The same pair may not register twice; a *number* that
                    // was registered, closed and reused may - it names a
                    // different description now, which is a different entry.
                    if entries
                        .iter()
                        .any(|entry| entry.fd == fd && Arc::ptr_eq(&entry.file, &file))
                    {
                        return Err(err::EEXIST);
                    }
                    if entries.len() >= MAX_WATCHES {
                        return Err(err::ENOSPC);
                    }
                    // Watching an epoll instance has to stay acyclic and
                    // shallow: a wait on this one walks what the target
                    // watches, and a circle would walk for ever.
                    if let Some(target) = file.inode().as_epoll() {
                        nesting_check(self, target)?;
                    }
                    entries.push(Entry {
                        fd,
                        file,
                        events,
                        data,
                        // The edge detector starts dark, so a descriptor
                        // that is already ready when it is registered is
                        // reported once at the first wait that looks -
                        // which is epoll(7)'s answer to "is ADD of a ready
                        // fd an event?".
                        et_last: 0,
                        dead: false,
                    });
                    Ok(())
                }
                EPOLL_CTL_MOD => {
                    if events & EPOLLEXCLUSIVE != 0 {
                        // ADD-only, on pain of EINVAL - the man page again.
                        return Err(err::EINVAL);
                    }
                    let entry = find_mut(&mut entries, fd).ok_or(err::ENOENT)?;
                    if entry.events & EPOLLEXCLUSIVE != 0 {
                        // An entry that was registered exclusive cannot be
                        // modified at all; the flag is a promise about who
                        // else is watching, and a MOD could break it.
                        return Err(err::EINVAL);
                    }
                    entry.events = events;
                    entry.data = data;
                    // MOD re-arms: a one-shot that has fired is live
                    // again, and the edge detector starts over.
                    entry.dead = false;
                    entry.et_last = 0;
                    Ok(())
                }
                EPOLL_CTL_DEL => {
                    let position = entries
                        .iter()
                        .position(|entry| entry.fd == fd)
                        .ok_or(err::ENOENT)?;
                    entries.remove(position);
                    Ok(())
                }
                _ => Err(err::EINVAL),
            }
        })
    }

    /// One scan of the interest list: report every entry whose descriptor
    /// can do something right now, filling `out` up to its capacity.
    ///
    /// The scan is also the bookkeeping pass, which is what makes the
    /// edge-triggered and one-shot answers honest:
    ///
    /// * an `EPOLLET` entry records what it saw - ready or not - so the
    ///   next scan reports a condition that *arrived* rather than one that
    ///   merely persisted. A report that could not be delivered because
    ///   `out` is full consumes nothing: the edge stays armed, and the next
    ///   wait hands it over, which is Linux's "undelivered events stay on
    ///   the ready list";
    /// * an `EPOLLONESHOT` entry goes dark once reported, until a MOD.
    ///
    /// Entries whose descriptor the caller has closed are dropped here, as
    /// Linux drops them at close.
    pub fn collect(&self, caller: &FdTable, out: &mut Vec<Reported>) {
        out.clear();
        critical_section::with(|cs| {
            let mut entries = self.state.borrow(cs).borrow_mut();
            entries.retain(|entry| alive(caller, entry));
            let capacity = out.capacity();
            for entry in entries.iter_mut() {
                let ready = entry.file.inode().poll_ready();
                let bits = report(entry, ready);
                if bits != 0 && out.len() < capacity {
                    out.push(Reported {
                        events: bits,
                        data: entry.data,
                    });
                    if entry.events & EPOLLET != 0 {
                        entry.et_last = ready_bits(ready);
                    }
                    if entry.events & EPOLLONESHOT != 0 {
                        entry.dead = true;
                    }
                } else if entry.events & EPOLLET != 0 && bits == 0 {
                    // No edge *now*, but the condition may have dipped
                    // since the last scan and a dip is what re-arms the
                    // detector: record what is true this instant.
                    entry.et_last = ready_bits(ready);
                }
            }
        });
    }

    /// Register `id` with every watched node, so a wait sleeps the way
    /// `poll(2)`'s does: the arrival that ends it is the node's own wakeup
    /// call, not the next clock tick. The counterpart takes every
    /// registration back when the wait is over, whatever it answered.
    pub fn subscribe_waits(&self, id: crate::sched::policy::TaskId) {
        critical_section::with(|cs| {
            for entry in self.state.borrow(cs).borrow().iter() {
                entry.file.inode().poll_subscribe(id);
            }
        });
    }

    /// The counterpart of [`Self::subscribe_waits`]: a wait that ended -
    /// in events, in a timeout, in an interruption - no longer wants to
    /// hear from the nodes it was watching.
    pub fn unsubscribe_waits(&self, id: crate::sched::policy::TaskId) {
        critical_section::with(|cs| {
            for entry in self.state.borrow(cs).borrow().iter() {
                entry.file.inode().poll_unsubscribe(id);
            }
        });
    }

    /// Whether any watched descriptor could report right now - the answer
    /// `poll` on the epfd itself wants, and the answer a wait's first look
    /// gives before it sleeps.
    ///
    /// Deliberately *without* the caller's table: `poll_ready` may be asked
    /// by a process that did not build this list (a shared epfd across a
    /// fork), and the honest answer there is one that never depends on
    /// someone else's descriptors. A stale entry can only make this answer
    /// spuriously true, and a spurious wakeup is the one lie `poll(2)`
    /// permits itself.
    pub fn any_ready(&self) -> bool {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow()
                .iter()
                .any(|entry| !entry.dead && report(entry, entry.file.inode().poll_ready()) != 0)
        })
    }
}

/// Whether `entry` is still the description its number names. A number
/// closed, or reused for something else, ends the entry - the fact Linux
/// acts on at close time, acted on here at the next look.
fn alive(caller: &FdTable, entry: &Entry) -> bool {
    caller
        .clone_at(entry.fd)
        .is_ok_and(|file| Arc::ptr_eq(&file, &entry.file))
}

/// The entry `fd` names, if the list holds one.
fn find_mut(entries: &mut [Entry], fd: RawFd) -> Option<&mut Entry> {
    entries.iter_mut().find(|entry| entry.fd == fd)
}

/// Pack a node's readiness into the two bits the edge detector keeps.
fn ready_bits(ready: Ready) -> u8 {
    (ready.read as u8) | (ready.write as u8) << 1
}

/// The events `entry` owes a wait that looks right now: the conditions the
/// caller asked for that hold, level-triggered unless the entry asked for
/// edges, and nothing at all for a one-shot that has already fired.
fn report(entry: &Entry, ready: Ready) -> u32 {
    if entry.dead {
        return 0;
    }
    // What the caller asked to hear about. EPOLLIN and EPOLLRDNORM are one
    // condition in two spellings, as are EPOLLOUT and EPOLLWRNORM - Linux
    // reports both together, and so does a report that wants a caller's
    // `events & EPOLLIN` test and its `events & EPOLLRDNORM` test to agree.
    let wants_read = entry.events & (EPOLLIN | EPOLLRDNORM) != 0;
    let wants_write = entry.events & (EPOLLOUT | EPOLLWRNORM) != 0;

    let (read, write) = if entry.events & EPOLLET != 0 {
        // An edge is a condition that was absent at the last scan and is
        // present at this one - not a condition that merely persists.
        (
            wants_read && ready.read && entry.et_last & 1 == 0,
            wants_write && ready.write && entry.et_last & 2 == 0,
        )
    } else {
        (wants_read && ready.read, wants_write && ready.write)
    };

    let mut bits = 0;
    if read {
        bits |= EPOLLIN | EPOLLRDNORM;
    }
    if write {
        bits |= EPOLLOUT | EPOLLWRNORM;
    }
    bits
}

/// `epoll_ctl(2)`'s ELOOP: would adding `target` under `self` make a
/// circle, or a chain of epoll instances deeper than the man page's five?
///
/// The walk follows interest lists, instance by instance, and stops on
/// either finding `self` - a circle, however it got there - or running out
/// of depth. It never borrows `self`'s list: the circle is discovered
/// *before* descending into the instance that closes it.
fn nesting_check(this: &Epoll, target: &Epoll) -> Result<(), isize> {
    fn walk(this: &Epoll, ep: &Epoll, depth: usize) -> Result<(), isize> {
        if core::ptr::eq(ep, this) {
            // The instance being registered is already among this one's
            // ancestors: a circle, not a nest.
            return Err(crate::syscall::err::ELOOP);
        }
        if depth == 0 {
            return Err(crate::syscall::err::ELOOP);
        }
        let nested = critical_section::with(|cs| {
            ep.state
                .borrow(cs)
                .borrow()
                .iter()
                .filter_map(|entry| {
                    entry.file.inode().as_epoll().map(|ep| {
                        // Reborrow-free identity: the entry holds an Arc to
                        // the inode; `as_epoll` lends the Epoll inside it, and
                        // the walk only needs the pointer.
                        ep as *const Epoll
                    })
                })
                .collect::<Vec<_>>()
        });
        for ep in nested {
            // Safety in numbers only: each pointer came from an Arc the
            // interest list itself holds, and the list is borrowed for the
            // walk's duration.
            walk(this, unsafe { &*ep }, depth - 1)?;
        }
        Ok(())
    }
    walk(this, target, MAX_NESTING)
}

impl Inode for Epoll {
    /// An epoll set has no position: `lseek(2)` on one is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x7000_0000 + self.ino,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    /// The answer `poll` on the epfd itself reads: readable when something
    /// in the list could report, never writable - nobody writes to an
    /// epoll, and a writable answer would invite a caller to try.
    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.any_ready(),
            write: false,
        }
    }

    /// A read from the epfd itself. Linux refuses one with EINVAL, and so
    /// does this: the events are *collected* by `epoll_wait`, and a caller
    /// that read them as bytes would get a struct layout it never asked to
    /// parse.
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(VfsError::Invalid)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::Invalid)
    }

    fn as_epoll(&self) -> Option<&Epoll> {
        Some(self)
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::{
        EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, EPOLL_EVENT_DATA, EPOLL_EVENT_SIZE, EPOLLERR,
        EPOLLET, EPOLLEXCLUSIVE, EPOLLHUP, EPOLLIN, EPOLLMSG, EPOLLONESHOT, EPOLLOUT, EPOLLPRI,
        EPOLLRDBAND, EPOLLRDHUP, EPOLLRDNORM, EPOLLWAKEUP, EPOLLWRBAND, EPOLLWRNORM, Epoll,
        MAX_WATCHES,
    };
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    use crate::syscall::err;
    use crate::vfs::file::{FdTable, File};
    use crate::vfs::pipe;
    use crate::vfs::ramfs::RamFs;
    use crate::vfs::{FileSystem, Inode, NodeKind};

    /// A table with a fresh epoll instance installed, handing back the
    /// table, the instance and its descriptor number.
    fn epoll_fd() -> (FdTable, Arc<Epoll>, i32) {
        let table = FdTable::new();
        let epoll = Epoll::new(7);
        let fd = table.install(Arc::new(File::new(epoll.clone()))).unwrap();
        assert_eq!(fd, 0, "the epfd is the table's first descriptor");
        (table, epoll, fd)
    }

    /// Install a node in `table` and hand back its number.
    fn watch(table: &FdTable, node: Arc<dyn Inode>) -> i32 {
        table.install(Arc::new(File::new(node))).unwrap()
    }

    /// The ABI is not a choice: every bit and offset is the value the
    /// headers spell, pinned here so a typo reads as a red test rather than
    /// as an `EPOLLIN` that means something else.
    #[test]
    fn epoll_bits_and_layout_match_the_headers() {
        assert_eq!(EPOLLIN, 0x001);
        assert_eq!(EPOLLPRI, 0x002);
        assert_eq!(EPOLLOUT, 0x004);
        assert_eq!(EPOLLERR, 0x008);
        assert_eq!(EPOLLHUP, 0x010);
        assert_eq!(EPOLLRDNORM, 0x040);
        assert_eq!(EPOLLRDBAND, 0x080);
        assert_eq!(EPOLLWRNORM, 0x100);
        assert_eq!(EPOLLWRBAND, 0x200);
        assert_eq!(EPOLLMSG, 0x400);
        assert_eq!(EPOLLRDHUP, 0x2000);
        assert_eq!(EPOLLEXCLUSIVE, 1 << 28);
        assert_eq!(EPOLLWAKEUP, 1 << 29);
        assert_eq!(EPOLLONESHOT, 1 << 30);
        assert_eq!(EPOLLET, 1 << 31);
        assert_eq!(EPOLL_CTL_ADD, 1);
        assert_eq!(EPOLL_CTL_DEL, 2);
        assert_eq!(EPOLL_CTL_MOD, 3);
        // `struct epoll_event` is packed: twelve bytes, data at byte 4.
        assert_eq!(EPOLL_EVENT_SIZE, 12);
        assert_eq!(EPOLL_EVENT_DATA, 4);
        assert_eq!(MAX_WATCHES, 1024);
    }

    /// `epoll_ctl(2)`: the three operations and the four ordinary refusals,
    /// against a pipe the test can fill and drain.
    #[test]
    fn ctl_adds_modifies_and_deletes_with_the_man_pages_errors() {
        let (table, ep, epfd) = epoll_fd();
        let (read_end, _write_end) = pipe::pair();
        let rfd = watch(&table, read_end);

        // ADD, then the EEXIST the second ADD of the same pair owes.
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_ADD, rfd, EPOLLIN, 42),
            Ok(())
        );
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_ADD, rfd, EPOLLIN, 42),
            Err(err::EEXIST)
        );

        // MOD of an unregistered number, and DEL of one: ENOENT.
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_MOD, rfd + 3, EPOLLIN, 0),
            Err(err::ENOENT)
        );
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_DEL, rfd + 3, 0, 0),
            Err(err::ENOENT)
        );

        // An operation outside the three: EINVAL.
        assert_eq!(ep.ctl(&table, epfd, 0, rfd, EPOLLIN, 0), Err(err::EINVAL));

        // The epfd watching itself: EINVAL, by number, before anything else.
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_ADD, epfd, EPOLLIN, 0),
            Err(err::EINVAL)
        );

        // A target that is not open: EBADF.
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_ADD, 99, EPOLLIN, 0),
            Err(err::EBADF)
        );

        // MOD and DEL of the registered pair succeed.
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_MOD, rfd, EPOLLOUT, 7),
            Ok(())
        );
        assert_eq!(ep.ctl(&table, epfd, EPOLL_CTL_DEL, rfd, 0, 0), Ok(()));
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_DEL, rfd, 0, 0),
            Err(err::ENOENT)
        );
    }

    /// `epoll_ctl(2)`: EPERM for the two kinds Linux refuses to watch at
    /// all - a regular file, which can never make a reader wait, and a
    /// directory, which nobody polls.
    #[test]
    fn ctl_refuses_files_and_directories_like_linux() {
        let (table, ep, epfd) = epoll_fd();
        let root = RamFs::new().root();
        let ffd = watch(&table, root.create("f", NodeKind::File).unwrap());
        let dfd = watch(&table, root.create("d", NodeKind::Dir).unwrap());

        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_ADD, ffd, EPOLLIN, 0),
            Err(err::EPERM)
        );
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_ADD, dfd, EPOLLIN, 0),
            Err(err::EPERM)
        );
    }

    /// `epoll_ctl(2)`: EPOLLEXCLUSIVE is ADD-only, MOD refuses it twice -
    /// spoken and carried - and an epoll target refuses it outright.
    #[test]
    fn exclusive_is_add_only_with_a_short_vocabulary() {
        let (table, ep, epfd) = epoll_fd();
        let (read_end, _w) = pipe::pair();
        let rfd = watch(&table, read_end);

        // Beside an event it may not accompany.
        assert_eq!(
            ep.ctl(
                &table,
                epfd,
                EPOLL_CTL_ADD,
                rfd,
                EPOLLEXCLUSIVE | EPOLLONESHOT,
                0
            ),
            Err(err::EINVAL)
        );
        // The four it may, on ADD: accepted, and MOD then refuses the flag.
        assert_eq!(
            ep.ctl(
                &table,
                epfd,
                EPOLL_CTL_ADD,
                rfd,
                EPOLLEXCLUSIVE | EPOLLIN | EPOLLET,
                0
            ),
            Ok(())
        );
        assert_eq!(
            ep.ctl(
                &table,
                epfd,
                EPOLL_CTL_MOD,
                rfd,
                EPOLLEXCLUSIVE | EPOLLIN,
                0
            ),
            Err(err::EINVAL)
        );
        // And MOD of an entry that *has* it: refused as well.
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_MOD, rfd, EPOLLIN, 0),
            Err(err::EINVAL)
        );

        // Aimed at an epoll instance: EINVAL, herd or no herd.
        let e2 = Epoll::new(8);
        let e2fd = watch(&table, e2);
        assert_eq!(
            ep.ctl(
                &table,
                epfd,
                EPOLL_CTL_ADD,
                e2fd,
                EPOLLEXCLUSIVE | EPOLLIN,
                0
            ),
            Err(err::EINVAL)
        );
    }

    /// The wait's level-triggered answer: an entry reports for as long as
    /// its condition holds, carrying the caller's data word verbatim, and a
    /// condition nobody asked for is not reported.
    #[test]
    fn a_level_entry_reports_while_the_condition_holds() {
        let (table, ep, epfd) = epoll_fd();
        let (read_end, write_end) = pipe::pair();
        let rfd = watch(&table, read_end.clone());
        let _wfd = watch(&table, write_end.clone());
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_ADD, rfd, EPOLLIN, 0xfeed),
            Ok(())
        );

        let mut out = Vec::with_capacity(4);
        ep.collect(&table, &mut out);
        assert!(out.is_empty(), "an empty pipe is not readable");

        write_end.write_at(0, b"hi").unwrap();
        ep.collect(&table, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].events, EPOLLIN | EPOLLRDNORM);
        assert_eq!(out[0].data, 0xfeed);

        // Still readable: level-triggered reports it again.
        ep.collect(&table, &mut out);
        assert_eq!(out.len(), 1);

        // Drained: not readable, so nothing reports.
        let mut buf = [0u8; 2];
        read_end.read_at(0, &mut buf).unwrap();
        ep.collect(&table, &mut out);
        assert!(out.is_empty());
    }

    /// `EPOLLET`: a condition reports once when it arrives, not again while
    /// it persists, and a dip between scans is what re-arms the detector.
    #[test]
    fn an_edge_reports_once_until_the_condition_dips() {
        let (table, ep, epfd) = epoll_fd();
        let (read_end, write_end) = pipe::pair();
        let rfd = watch(&table, read_end.clone());
        let _wfd = watch(&table, write_end.clone());
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_ADD, rfd, EPOLLIN | EPOLLET, 1),
            Ok(())
        );

        let mut out = Vec::with_capacity(4);
        // Already-ready-at-ADD is an edge the first scan reports.
        write_end.write_at(0, b"a").unwrap();
        ep.collect(&table, &mut out);
        assert_eq!(out.len(), 1, "the first edge reports");

        ep.collect(&table, &mut out);
        assert!(out.is_empty(), "a persistent condition is not an edge");

        // Drained, and filled again: both scans see a change, so the
        // refill is a new edge.
        let mut buf = [0u8; 1];
        read_end.read_at(0, &mut buf).unwrap();
        ep.collect(&table, &mut out);
        assert!(out.is_empty(), "the drain itself is not an edge");
        write_end.write_at(0, b"b").unwrap();
        ep.collect(&table, &mut out);
        assert_eq!(out.len(), 1, "the refill is a new edge");
    }

    /// `EPOLLONESHOT`: reported once, then dark - even though the condition
    /// holds - until a MOD re-arms it.
    #[test]
    fn a_oneshot_reports_once_until_rearmed() {
        let (table, ep, epfd) = epoll_fd();
        let (read_end, write_end) = pipe::pair();
        let rfd = watch(&table, read_end);
        let _wfd = watch(&table, write_end.clone());
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_ADD, rfd, EPOLLIN | EPOLLONESHOT, 3),
            Ok(())
        );

        write_end.write_at(0, b"x").unwrap();
        let mut out = Vec::with_capacity(4);
        ep.collect(&table, &mut out);
        assert_eq!(out.len(), 1);
        ep.collect(&table, &mut out);
        assert!(out.is_empty(), "the one shot has been spent");

        // MOD re-arms - the man page's rule - and the still-present
        // condition reports again.
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_MOD, rfd, EPOLLIN | EPOLLONESHOT, 3),
            Ok(())
        );
        ep.collect(&table, &mut out);
        assert_eq!(out.len(), 1);
    }

    /// Closing a descriptor ends its entry: the next scan reports nothing
    /// for it, and the operations that name it answer ENOENT - the shape
    /// Linux's close-time removal gives a caller that asks afterwards.
    #[test]
    fn closing_a_watched_descriptor_reaps_its_entry() {
        let (table, ep, epfd) = epoll_fd();
        let (read_end, write_end) = pipe::pair();
        let rfd = watch(&table, read_end);
        let _wfd = watch(&table, write_end.clone());
        assert_eq!(ep.ctl(&table, epfd, EPOLL_CTL_ADD, rfd, EPOLLIN, 0), Ok(()));
        write_end.write_at(0, b"z").unwrap();
        assert!(ep.any_ready());

        table.close(rfd).unwrap();
        let mut out = Vec::with_capacity(4);
        ep.collect(&table, &mut out);
        assert!(out.is_empty(), "a closed fd reports nothing");
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_MOD, rfd, EPOLLIN, 0),
            Err(err::ENOENT)
        );

        // A number reused for a different description is a different entry:
        // ADD succeeds where the old entry sat.
        let (other_read, _other_write) = pipe::pair();
        assert_eq!(rfd, watch(&table, other_read));
        assert_eq!(ep.ctl(&table, epfd, EPOLL_CTL_ADD, rfd, EPOLLIN, 0), Ok(()));
    }

    /// The epfd's own `poll` answer: readable when something in the list
    /// could report, and not writable - which is also what a *nested*
    /// epoll's wait sees when it watches this one.
    #[test]
    fn the_epfd_itself_polls_readable_when_a_watch_is_ready() {
        let (table, ep, epfd) = epoll_fd();
        let epoll_node = ep.clone() as Arc<dyn Inode>;
        assert!(!epoll_node.poll_ready().read);

        let (read_end, write_end) = pipe::pair();
        let rfd = watch(&table, read_end);
        let _wfd = watch(&table, write_end.clone());
        assert_eq!(ep.ctl(&table, epfd, EPOLL_CTL_ADD, rfd, EPOLLIN, 0), Ok(()));

        write_end.write_at(0, b"!").unwrap();
        assert!(epoll_node.poll_ready().read);
        assert!(!epoll_node.poll_ready().write);
    }

    /// `epoll_ctl(2)`'s ELOOP: an epoll may watch an epoll, but not itself
    /// - not by dup, and not around a circle of others.
    #[test]
    fn nesting_is_bounded_and_circles_are_refused() {
        // A dup of the epfd, at another number, still names this instance:
        // ADDing it is a circle, and the answer is ELOOP.
        let (table, ep, epfd) = epoll_fd();
        let dup = table.install(table.clone_at(epfd).unwrap()).unwrap();
        assert_eq!(
            ep.ctl(&table, epfd, EPOLL_CTL_ADD, dup, EPOLLIN, 0),
            Err(err::ELOOP)
        );

        // A chain of six, each watching the next: the *first* of them is
        // five watchers deep, and adding it to a fresh instance walks
        // past the man page's allowance.
        let chain = FdTable::new();
        let deep = build_chain(&chain, 6);
        let (t, root, root_fd) = epoll_fd();
        let chain_head_fd = watch(&t, deep);
        assert_eq!(
            root.ctl(&t, root_fd, EPOLL_CTL_ADD, chain_head_fd, EPOLLIN, 0),
            Err(err::ELOOP)
        );

        // Five links is the allowance, and the first of a five-link
        // chain is accepted.
        let (t2, shallow, shallow_fd) = epoll_fd();
        let within = build_chain(&t2, 5);
        let head2_fd = watch(&t2, within);
        assert_eq!(
            shallow.ctl(&t2, shallow_fd, EPOLL_CTL_ADD, head2_fd, EPOLLIN, 0),
            Ok(())
        );
    }

    /// `count` epoll instances, each installed in `table` and each watching
    /// the next, handing back the first - the one that therefore sits
    /// `count - 1` watchers deep.
    fn build_chain(table: &FdTable, count: usize) -> Arc<Epoll> {
        let mut first: Option<Arc<Epoll>> = None;
        let mut last: Option<Arc<Epoll>> = None;
        let mut last_fd = 0;
        for i in 0..count {
            let node = Epoll::new((100 + i) as u64);
            let fd = watch(table, node.clone());
            if let Some(p) = last {
                assert_eq!(p.ctl(table, last_fd, EPOLL_CTL_ADD, fd, EPOLLIN, 0), Ok(()));
            } else {
                first = Some(node.clone());
            }
            last = Some(node);
            last_fd = fd;
        }
        first.expect("count is at least one")
    }
}
