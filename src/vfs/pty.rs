//! Pseudo-terminals: the `/dev/ptmx` master and the `/dev/pts/N` slaves.
//!
//! A pty is a terminal pair with a wire between them instead of a UART. One
//! end - the *master* - is not a terminal at all: it is a byte channel that a
//! program like a terminal emulator holds. The other end - the *slave* - is a
//! real terminal with a line discipline, termios and a `/dev/pts/N` name, and
//! that is where a shell does its editing. Bytes written to the master are
//! typed at the slave; bytes the slave prints are read from the master.
//!
//! This kernel holds masters itself, for its virtual consoles: `/dev/tty1`
//! through `/dev/tty6` are the slave ends of six pairs minted at boot, and
//! the console multiplexer - the thing that turns the one serial wire the
//! machine has into six terminals - reads their masters. `/dev/ptmx` is the
//! allocator for everyone else: it is one name, and every open of it mints a
//! new pair and hands back that pair's master, which is why
//! [`Inode::open_replacement`](super::Inode::open_replacement) exists at all -
//! a shared inode cannot be "a different thing each time".
//!
//! # The setup sequence the lock protects
//!
//! `posix_openpt` then `grantpt` then `unlockpt` then `open("/dev/pts/N")`.
//! A slave that is opened before `unlockpt` would be one whose owner is not
//! yet set, so opening a locked slave is `EIO` - the lock is what makes the
//! sequence mean something rather than being four calls in a hopeful order.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use critical_section::Mutex;

use super::tty::{Tty, TtyOutput};
use super::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};
use crate::sched::policy::TaskId;
use crate::term::vc::VC_COUNT;

/// How many pairs may exist at once, as `/proc/sys/kernel/pty/max` reports.
/// Linux's default; the number a library reads before deciding whether it is
/// worth asking for one more.
pub const MAX: u32 = 4096;

/// How much a slave may have printed that the master has not yet taken.
///
/// The bound is what makes an unstoppable flood stoppable. Without it a program
/// writing a big file to its terminal (`bat` over a minified HTML) never blocks:
/// the bytes pile into memory, the program runs to its end and exits, and the
/// only thing left is the wire, which nothing can now interrupt or switch away
/// from. With it the writer parks once the buffer is full: it stays alive and
/// still in the foreground, so Ctrl-C reaches it, and the mux keeps carrying
/// the buffered tail out between chunks, so `^A <num>` switches on the next lap
/// rather than after the whole file has crawled over COM1.
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
const TO_MASTER_MAX: usize = 64 * 1024;

/// The `tty` major and the minor a pty slave's device number starts at. Linux
/// uses 136 for `/dev/pts/*`, and 5:2 for `/dev/ptmx` itself.
const PTS_MAJOR: u32 = 136;
const PTMX_DEV_ID: (u32, u32) = (5, 2);

/// One pair.
pub struct Pty {
    number: u32,
    /// What the slave's `st_rdev` reports: `(4, n)` for a virtual console's
    /// slave, `(136, n)` for one from `/dev/ptmx`. `ttyname(3)` tells the
    /// two apart by exactly this.
    slave_dev_id: (u32, u32),
    state: Mutex<RefCell<State>>,
    /// The slave's terminal: its line discipline, its termios, its editing.
    /// The master has no line discipline of its own here - it is the wire.
    slave: Arc<Tty>,
}

struct State {
    /// Bytes on their way from the slave to the master: what the slave printed,
    /// and the echo of what was typed at it.
    to_master: VecDeque<u8>,
    /// Set until `unlockpt` (`TIOCSPTLCK` with zero) clears it.
    locked: bool,
    /// Tasks blocked reading the master. A producer cannot name them, but the
    /// pair can, and a wake aimed anywhere else would miss.
    waiters: Vec<TaskId>,
    /// Tasks blocked in [`Pty::push_master_backpressure`], waiting for room in
    /// `to_master` the way the readers wait for bytes in it. `take_master`
    /// creates the room and wakes them.
    write_waiters: Vec<TaskId>,
    /// `O_NONBLOCK` on the description holding the *master*.
    ///
    /// The pair's two ends are one terminal and two descriptions, and Linux
    /// keeps one `O_NONBLOCK` per description: a supervisor that opened the
    /// master nonblocking must not thereby change what the slave's own reader
    /// sees. So the master's bit lives here, on the pair, while the slave's
    /// lives on its [`Tty`] - each end reachable by its own inode
    /// (`PtyMaster::as_pty` against `PtySlave::as_tty`), which is what lets
    /// `sync_nonblocking` put the bit where it belongs.
    master_nonblocking: bool,
}

impl Pty {
    fn new(number: u32, slave_dev_id: (u32, u32)) -> Arc<Pty> {
        Arc::new(Pty {
            number,
            slave_dev_id,
            slave: Arc::new(Tty::new(slave_dev_id, TtyOutput::Pty(number))),
            state: Mutex::new(RefCell::new(State {
                to_master: VecDeque::new(),
                locked: true,
                waiters: Vec::new(),
                write_waiters: Vec::new(),
                master_nonblocking: false,
            })),
        })
    }

    /// The pair's number: what `TIOCGPTN` reports, and the name the slave
    /// carries in `/dev/pts` when it came from `/dev/ptmx`.
    pub fn number(&self) -> u32 {
        self.number
    }

    /// The slave terminal - the end a shell talks to.
    pub fn slave(&self) -> &Arc<Tty> {
        &self.slave
    }

    /// The device number the master reports, as `st_rdev`.
    pub fn master_dev_id(&self) -> (u32, u32) {
        PTMX_DEV_ID
    }

    /// The device number the slave reports, as `st_rdev`.
    pub fn slave_dev_id(&self) -> (u32, u32) {
        self.slave_dev_id
    }

    fn push_master(&self, bytes: &[u8]) {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .to_master
                .extend(bytes.iter().copied());
        });
    }

    /// Copy as much of `bytes` as fits under [`TO_MASTER_MAX`], and report how
    /// many that was. The rest is the caller's to offer again once the master
    /// takes some - see [`push_master_backpressure`](Self::push_master_backpressure).
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    fn push_master_bounded(&self, bytes: &[u8]) -> usize {
        critical_section::with(|cs| {
            let mut state = self.state.borrow(cs).borrow_mut();
            let room = TO_MASTER_MAX.saturating_sub(state.to_master.len());
            let n = room.min(bytes.len());
            state.to_master.extend(bytes[..n].iter().copied());
            n
        })
    }

    /// Whether `to_master` has room a blocked writer can use. The condition the
    /// back-pressure wait turns on, and what `take_master` makes true.
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    fn master_room(&self) -> bool {
        critical_section::with(|cs| self.state.borrow(cs).borrow().to_master.len() < TO_MASTER_MAX)
    }

    /// Mirror the master description's `O_NONBLOCK` onto the pair, where its
    /// read (and a slave's write that would park) consults it.
    pub fn set_master_nonblocking(&self, on: bool) {
        critical_section::with(|cs| {
            self.state.borrow(cs).borrow_mut().master_nonblocking = on;
        });
    }

    /// Whether the master's description asked not to wait.
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    pub fn master_nonblocking(&self) -> bool {
        critical_section::with(|cs| self.state.borrow(cs).borrow().master_nonblocking)
    }

    /// Push a slave's printed bytes, parking the writing task when the buffer
    /// is full until the master takes enough for the rest.
    ///
    /// The task-context path, and the one that holds the buffer to
    /// [`TO_MASTER_MAX`]: it is the back-pressure that keeps a flood from
    /// running ahead of the one serial wire and out of anyone's reach. Because
    /// it parks through the interruptible wait, a catchable signal ends it - a
    /// Ctrl-C kills the blocked writer rather than leaving it to be killed
    /// after it has already dumped the whole file into memory, and a program
    /// that installed a handler instead is told `EINTR` at its own write.
    /// Joining the wait list before the first attempt mirrors the console's
    /// transmit ring: room that appears between a failed push and the sleep
    /// must have someone to tell.
    ///
    /// The answer is the write's own, in three shapes: everything accepted; a
    /// *partial* count when the wait ended early (`O_NONBLOCK`, or a signal
    /// with bytes already in), which is the answer that cannot duplicate bytes
    /// if the caller retries; or the error that ended a wait that had accepted
    /// nothing - `Again` for a full buffer under `O_NONBLOCK`, `Interrupted`
    /// for a signal, the two errnos Linux gives a tty write for those two
    /// cases.
    #[cfg(target_os = "none")]
    fn push_master_backpressure(&self, bytes: &[u8], nonblocking: bool) -> VfsResult<usize> {
        let id = crate::sched::current();
        self.subscribe_writer(id);
        let mut at = 0;
        while at < bytes.len() {
            at += self.push_master_bounded(&bytes[at..]);
            // Tell the pump there is something to carry before possibly waiting
            // for it to make room; otherwise a full buffer with a sleeping
            // master is nobody waiting on anybody.
            self.wake_master_waiters();
            if at == bytes.len() {
                break;
            }
            let ended = if nonblocking {
                None
            } else {
                match crate::sched::block_until_interruptible(|| self.master_room()) {
                    crate::sched::WaitEnd::Interrupted => Some(VfsError::Interrupted),
                    crate::sched::WaitEnd::Condition | crate::sched::WaitEnd::Deadline => None,
                }
            };
            if let Some(error) = ended {
                self.unsubscribe_writer(id);
                return if at > 0 { Ok(at) } else { Err(error) };
            }
            if nonblocking {
                self.unsubscribe_writer(id);
                return if at > 0 { Ok(at) } else { Err(VfsError::Again) };
            }
        }
        self.unsubscribe_writer(id);
        Ok(at)
    }

    /// Add the running task to the writers' wait list, before it parks on room.
    #[cfg(target_os = "none")]
    fn subscribe_writer(&self, id: TaskId) {
        critical_section::with(|cs| {
            let mut state = self.state.borrow(cs).borrow_mut();
            if !state.write_waiters.contains(&id) {
                state.write_waiters.push(id);
            }
        });
    }

    /// Take the running task back off the list - the wait ended however it ended.
    #[cfg(target_os = "none")]
    fn unsubscribe_writer(&self, id: TaskId) {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .write_waiters
                .retain(|&w| w != id);
        });
    }

    /// Tell the writers that the master took some bytes and room appeared.
    fn wake_writer_waiters(&self) {
        let waiters =
            critical_section::with(|cs| self.state.borrow(cs).borrow().write_waiters.clone());
        for id in waiters {
            #[cfg(target_os = "none")]
            crate::sched::wake(id);
            #[cfg(not(target_os = "none"))]
            let _ = id;
        }
    }

    /// Wake everyone waiting on the master. Membership is the subscribers'
    /// business - `subscribe_master` and `unsubscribe_master` own it - because
    /// a waiter that is woken but keeps waiting (the mux, a blocked master
    /// read) must still be here for the next push. Consuming the list here
    /// would drop every reader after its first wake, and an output pipe that
    /// works exactly once is not a pipe.
    fn wake_master_waiters(&self) {
        let waiters = critical_section::with(|cs| self.state.borrow(cs).borrow().waiters.clone());
        for id in waiters {
            #[cfg(target_os = "none")]
            crate::sched::wake(id);
            #[cfg(not(target_os = "none"))]
            let _ = id;
        }
    }

    /// Whether the master has bytes waiting - the mux's parking condition,
    /// and `poll(2)`'s read half for a master descriptor.
    pub fn master_ready(&self) -> bool {
        critical_section::with(|cs| !self.state.borrow(cs).borrow().to_master.is_empty())
    }

    /// Take up to `want` bytes the slave printed, if any.
    pub fn take_master(&self, want: usize) -> Option<Vec<u8>> {
        let taken = critical_section::with(|cs| {
            let mut state = self.state.borrow(cs).borrow_mut();
            if state.to_master.is_empty() {
                return None;
            }
            let take = state.to_master.len().min(want);
            Some(state.to_master.drain(..take).collect())
        });
        // Room the writers parked on only exists once the master has taken it,
        // so this is where a blocked slave is told to go on.
        if taken.is_some() {
            self.wake_writer_waiters();
        }
        taken
    }

    /// Join the list `master_input` wakes when the slave has printed something.
    pub fn subscribe_master(&self, id: TaskId) {
        critical_section::with(|cs| {
            let mut state = self.state.borrow(cs).borrow_mut();
            if !state.waiters.contains(&id) {
                state.waiters.push(id);
            }
        });
    }

    pub fn unsubscribe_master(&self, id: TaskId) {
        critical_section::with(|cs| {
            self.state
                .borrow(cs)
                .borrow_mut()
                .waiters
                .retain(|&w| w != id);
        });
    }

    fn locked(&self) -> bool {
        critical_section::with(|cs| self.state.borrow(cs).borrow().locked)
    }

    /// `TIOCSPTLCK`: zero unlocks the pair, anything else locks it again.
    pub fn set_locked(&self, locked: bool) {
        critical_section::with(|cs| {
            self.state.borrow(cs).borrow_mut().locked = locked;
        });
    }

    /// Bytes typed at the master become input at the slave, through the
    /// slave's own line discipline - the shell sees edited lines, echo and
    /// interrupts exactly as it would on a serial terminal, because that is
    /// the code path they run through.
    fn write_slave(&self, bytes: &[u8]) {
        for &byte in bytes {
            self.slave.receive_byte(byte);
        }
    }

    /// The master's blocking read. The console multiplexer is the reader in
    /// practice: it parks here until the slave prints, then carries the bytes
    /// to the serial wire. The reader joins the wake list for the length of
    /// the read, the way [`Tty::read`] does on the slave side - and like that
    /// read, it ends with `EINTR` when a catchable signal arrives, because a
    /// terminal program that owns this end has the same timeouts aimed at it as
    /// the program on the slave end does. The master has no `VMIN`/`VTIME`
    /// clock: a master read is raw, and the queue either has a frame or does
    /// not.
    ///
    /// A master description that set `O_NONBLOCK` - which is how a supervisor
    /// with an event loop drives its pair, the same shape the slave side is
    /// opened in - takes whatever is queued and answers `EAGAIN` rather than
    /// parking. The look comes *before* the park for that reason, and it is
    /// also the safer order for the blocking case: the park re-tests the
    /// condition under the scheduler's lock, so a byte pushed between the look
    /// and the sleep has someone waiting on it either way.
    #[cfg(target_os = "none")]
    fn read_master(&self, buf: &mut [u8]) -> VfsResult<usize> {
        let id = crate::sched::current();
        self.subscribe_master(id);
        let got = loop {
            if let Some(bytes) = self.take_master(buf.len()) {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                break Ok(n);
            }
            if self.master_nonblocking() {
                break Err(VfsError::Again);
            }
            match crate::sched::block_until_interruptible(|| self.master_ready()) {
                crate::sched::WaitEnd::Interrupted => break Err(VfsError::Interrupted),
                // Woken by output, or by anything else that ends a sleep: the
                // look above is what decides, and a lost race costs one pass.
                crate::sched::WaitEnd::Condition | crate::sched::WaitEnd::Deadline => {}
            }
            // Another reader won the race; there may be more coming.
        };
        self.unsubscribe_master(id);
        got
    }
}

/// The pair registry: every live pair, by number. The virtual consoles are
/// minted into it at boot and never leave it; `/dev/ptmx` adds to it as long
/// as there are numbers left.
static PAIRS: Mutex<RefCell<BTreeMap<u32, Arc<Pty>>>> = Mutex::new(RefCell::new(BTreeMap::new()));
static NEXT_NUMBER: AtomicU32 = AtomicU32::new(1);

/// Mint the virtual consoles. Called once, from `tty::init`, before `/dev` is
/// built - the `ttyN` names have to have pairs behind them by the time a
/// getty looks.
pub fn init() {
    for number in 1..=VC_COUNT {
        allocate(number, (4, number));
    }
}

/// Allocate the pair numbered `number`, or answer the one that exists.
/// Virtual consoles call this once each at boot; nothing else uses explicit
/// numbers.
pub(crate) fn allocate(number: u32, slave_dev_id: (u32, u32)) -> Arc<Pty> {
    critical_section::with(|cs| {
        let mut pairs = PAIRS.borrow(cs).borrow_mut();
        pairs
            .entry(number)
            .or_insert_with(|| Pty::new(number, slave_dev_id))
            .clone()
    })
}

/// The virtual console `/dev/tty<n>` is the slave of, 1-based.
pub fn vc(index: u32) -> Arc<Pty> {
    allocate(index, (4, index))
}

/// Mint a fresh pair for a `/dev/ptmx` open. `ENOSPC` past the pool's end -
/// the same error Linux answers, and the one `posix_openpt` is specified to
/// hand back rather than a panic or a wraparound onto a live pair.
pub fn allocate_dynamic() -> VfsResult<Arc<Pty>> {
    // A plain increment, retried on collision: two opens in a row on one CPU
    // cannot interleave inside the critical section, but a number that a
    // boot-time mint already took is not one to hand out twice.
    loop {
        let number = NEXT_NUMBER.fetch_add(1, Ordering::Relaxed);
        if number > MAX {
            return Err(VfsError::NoSpace);
        }
        if critical_section::with(|cs| {
            let mut pairs = PAIRS.borrow(cs).borrow_mut();
            if pairs.contains_key(&number) {
                return false;
            }
            pairs.insert(number, Pty::new(number, (PTS_MAJOR, number)));
            true
        }) {
            let pty = critical_section::with(|cs| PAIRS.borrow(cs).borrow()[&number].clone());
            return Ok(pty);
        }
    }
}

/// The pair by number, if it lives.
pub fn lookup(number: u32) -> Option<Arc<Pty>> {
    critical_section::with(|cs| PAIRS.borrow(cs).borrow().get(&number).cloned())
}

/// Every live pair, in number order. The console multiplexer subscribes to
/// each new one, and `/dev/pts` lists the dynamic ones.
pub fn pairs() -> alloc::vec::Vec<Arc<Pty>> {
    critical_section::with(|cs| PAIRS.borrow(cs).borrow().values().cloned().collect())
}

/// How many pairs are open, for `/proc/sys/kernel/pty/nr`.
pub fn open_count() -> usize {
    pairs().len()
}

/// A slave's printed bytes on their way to the master, from a *task*: the
/// buffer is bounded at [`TO_MASTER_MAX`] and a full one is waited on, so the
/// caller gets the write's own answer back - a partial count, `EAGAIN` or
/// `EINTR` (see [`Pty::push_master_backpressure`]).
///
/// The context is a parameter now, because the thing this used to ask,
/// `crate::hal::interrupts_enabled()`, is false in an interrupt handler *and*
/// inside every syscall - `MSR_FMASK` masks `IF` on `syscall` entry - so a
/// userland write never parked and never met the bound at all, while a kernel
/// task's did. `t-wait-eintr` measured it: a 96 KiB write to a slave whose
/// master nobody read returned 96 KiB and grew the buffer past its 64 KiB cap.
/// Only the call site knows whether it may sleep, so only the call site can
/// say.
pub fn master_input(number: u32, bytes: &[u8], nonblocking: bool) -> VfsResult<usize> {
    let Some(pty) = lookup(number) else {
        // The pair is gone: the bytes go nowhere, and pretending otherwise
        // would be a write that reports success into a closed terminal. Linux
        // answers `EIO` once a pty's master is gone, which is this.
        return Err(VfsError::Corrupt);
    };
    #[cfg(target_os = "none")]
    {
        pty.push_master_backpressure(bytes, nonblocking)
    }
    // The host has no scheduler to park in; the pair tests drive `Pty`'s own
    // methods and read the bytes back out of the queue. Nothing can fill
    // there, so `O_NONBLOCK` has nothing to answer.
    #[cfg(not(target_os = "none"))]
    {
        let _ = nonblocking;
        pty.push_master(bytes);
        pty.wake_master_waiters();
        Ok(bytes.len())
    }
}

/// The same push from an **interrupt context** - the echo of a byte that has
/// just arrived, running in the handler that carried it - where nothing may
/// park: the bytes go straight onto the wire and the bound is not consulted,
/// because the alternative is sleeping inside an interrupt.
pub fn master_input_interrupt(number: u32, bytes: &[u8]) -> usize {
    let Some(pty) = lookup(number) else {
        return bytes.len();
    };
    pty.push_master(bytes);
    pty.wake_master_waiters();
    bytes.len()
}

/// The `/dev/ptmx` node: one name, no fixed device. Opening it mints a pair
/// and hands back the master, which is the whole of what this inode does -
/// reading or writing *it* would be a caller that never opened the master,
/// and Linux answers that the same way: no read, no write, just the open.
pub struct Ptmx;

/// `/dev/ptmx` is one shared inode, and its mode is one shared record:
/// `chmod` on the name sticks for every later open, the way Linux's single
/// misc node behaves (see `devfs::NodePerms`).
static PTMX_PERMS: super::devfs::NodePerms = super::devfs::NodePerms::new(0o666, 0, 0);

impl Inode for Ptmx {
    fn keeps_modes(&self) -> bool {
        PTMX_PERMS.keeps()
    }

    fn mode(&self) -> u32 {
        PTMX_PERMS.mode()
    }

    fn set_mode(&self, mode: u32) {
        PTMX_PERMS.set_mode(mode);
    }

    fn owner(&self) -> (u32, u32) {
        PTMX_PERMS.owner()
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        PTMX_PERMS.set_owner(uid, gid);
    }

    fn atime(&self) -> u64 {
        PTMX_PERMS.atime()
    }

    fn mtime(&self) -> u64 {
        PTMX_PERMS.mtime()
    }

    fn set_times(&self, atime: u64, mtime: u64) -> VfsResult<()> {
        PTMX_PERMS.set_times(atime, mtime);
        Ok(())
    }

    /// The pty multiplexer has no position either: `lseek` is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    fn rdev(&self) -> Option<(u32, u32)> {
        Some(PTMX_DEV_ID)
    }

    fn open_replacement(&self, _flags: usize) -> VfsResult<Option<Arc<dyn Inode>>> {
        let pty = allocate_dynamic()?;
        Ok(Some(Arc::new(PtyMaster { pty })))
    }
}

/// A pair's master: a byte channel, not a terminal. What is written here is
/// typed at the slave; what the slave prints is read here. `isatty` on it is
/// false, which is the difference between the two ends that the man pages
/// keep coming back to.
pub struct PtyMaster {
    pub(crate) pty: Arc<Pty>,
}

impl Inode for PtyMaster {
    /// A pty is a terminal: no position, so `lseek` is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    fn as_pty(&self) -> Option<&Pty> {
        Some(&self.pty)
    }

    /// The master answers the terminal ioctls, as it does on Linux: a pty pair
    /// is *one* terminal with two ends, so its termios, its window size and
    /// its foreground process group belong to the pair rather than to an end.
    ///
    /// This is load-bearing for every program that drives a pty. A supervisor
    /// holds the master and nothing else - it has no other descriptor to ask -
    /// so it decides whether it can do job control at all by asking *this*
    /// end: `sudo` calls `isatty`/`TCGETS` on the master, and a master that
    /// says `ENOTTY` makes it skip the `tcsetpgrp` that would put the command
    /// it started in the foreground. The command's shell then finds itself in
    /// the background of its own terminal, stops itself, and waits forever for
    /// a parent that has already decided the terminal is not a terminal.
    fn as_tty(&self) -> Option<&Tty> {
        Some(self.pty.slave().as_ref())
    }

    fn rdev(&self) -> Option<(u32, u32)> {
        Some(self.pty.master_dev_id())
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.pty.master_ready(),
            write: true,
        }
    }

    fn poll_subscribe(&self, id: TaskId) {
        self.pty.subscribe_master(id);
    }

    fn poll_unsubscribe(&self, id: TaskId) {
        self.pty.unsubscribe_master(id);
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        #[cfg(target_os = "none")]
        return self.pty.read_master(buf);
        // On the host there is no scheduler to park in; the tests drive the
        // pair through `Pty`'s own methods instead.
        #[cfg(not(target_os = "none"))]
        return Ok(self
            .pty
            .take_master(buf.len())
            .map(|bytes| {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                n
            })
            .unwrap_or(0));
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.pty.write_slave(buf);
        Ok(buf.len())
    }
}

/// A dynamic pair's slave as `/dev/pts/N` serves it: the real terminal, with
/// a check in front of the open - a pair still locked by its creator refuses
/// its slave with `EIO`, which is the `grantpt`/`unlockpt` dance's whole
/// point. The virtual consoles are never locked after boot and are registered
/// as their bare [`Tty`]s, without this wrapper.
pub struct PtySlave {
    pty: Arc<Pty>,
}

impl PtySlave {
    pub fn new(pty: Arc<Pty>) -> Arc<PtySlave> {
        Arc::new(PtySlave { pty })
    }
}

impl Inode for PtySlave {
    /// A pty is a terminal: no position, so `lseek` is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        let mut meta = self.pty.slave().metadata();
        meta.ino = self.pty.number() as u64;
        meta
    }

    fn as_tty(&self) -> Option<&Tty> {
        Some(self.pty.slave())
    }

    fn rdev(&self) -> Option<(u32, u32)> {
        Some(self.pty.slave_dev_id())
    }

    fn open_replacement(&self, _flags: usize) -> VfsResult<Option<Arc<dyn Inode>>> {
        if self.pty.locked() {
            return Err(VfsError::Corrupt);
        }
        Ok(None)
    }

    fn poll_ready(&self) -> Ready {
        self.pty.slave().poll_ready()
    }

    fn poll_subscribe(&self, id: TaskId) {
        self.pty.slave().poll_subscribe(id);
    }

    fn poll_unsubscribe(&self, id: TaskId) {
        self.pty.slave().poll_unsubscribe(id);
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.pty.slave().read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.pty.slave().write_at(offset, buf)
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::vec::Vec;

    use super::{Ptmx, PtyMaster, PtySlave, allocate, lookup, open_count, pairs};
    use crate::vfs::Inode;
    use crate::vfs::termios::{ONLCR, OPOST};

    fn drain_master(pty: &super::Pty) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(bytes) = pty.take_master(4096) {
            out.extend_from_slice(&bytes);
        }
        out
    }

    #[test]
    fn a_slave_line_arrives_whole_at_the_master() {
        let pty = allocate(900, (136, 900));
        // A shell writes a line through its slave's write path; the master
        // reads back those bytes with the newline a *terminal* owes its reader:
        // `ONLCR` is on in the default termios, so the program's `LF` leaves as
        // `CR LF`. A bare `LF` would move the emulator's cursor down without
        // returning it, and every line after the first would start in the
        // column the last one ended in - the staircase weston-terminal showed.
        assert_eq!(pty.slave().write_at(0, b"echo hi\n").unwrap(), 8);
        assert_eq!(&drain_master(&pty), b"echo hi\r\n");

        // The other direction: bytes typed at the master are typed at the
        // slave's line discipline. What the master sees back is the *echo* -
        // the completed line itself is the slave's readers' to take, exactly
        // as it would be from a serial terminal.
        for &b in b"ls\r" {
            pty.slave().receive_byte(b);
        }
        assert_eq!(&drain_master(&pty), b"ls\r\n");
        let mut buf = [0u8; 8];
        let n = pty.slave().take_now(&mut buf).unwrap();
        // Input is a different road from output: the reader gets the line with
        // one newline, because the carriage return Enter sends was translated
        // *in* (`ICRNL`) and never became a character of its own.
        assert_eq!(&buf[..n], b"ls\n");
    }

    #[test]
    fn a_program_may_turn_the_newline_translation_off() {
        let pty = allocate(904, (136, 904));
        // A full-screen program puts its terminal in raw mode and draws every
        // cell itself: it must get out exactly what it wrote, cursor control
        // included, with nothing rewritten behind its back.
        let mut tio = pty.slave().termios();
        tio.oflag &= !(OPOST | ONLCR);
        pty.slave().set_termios(tio);
        assert_eq!(pty.slave().write_at(0, b"line\n").unwrap(), 5);
        assert_eq!(&drain_master(&pty), b"line\n");
    }

    #[test]
    fn a_read_takes_what_there_is_and_reports_readiness() {
        let pty = allocate(901, (136, 901));
        assert!(!pty.master_ready(), "nothing printed yet");
        pty.slave().write_at(0, b"abc").unwrap();
        assert!(pty.master_ready());
        assert_eq!(pty.take_master(2).unwrap(), b"ab".to_vec());
        assert!(pty.master_ready(), "one byte left");
        assert_eq!(pty.take_master(8).unwrap(), b"c".to_vec());
        assert!(!pty.master_ready());
        assert_eq!(pty.take_master(8), None);
    }

    #[test]
    fn the_lock_refuses_the_slave_until_it_is_cleared() {
        let pty = allocate(902, (136, 902));
        let slave = PtySlave::new(pty.clone());
        // Locked as minted: `grantpt`'s job is to clear this.
        assert!(pty.locked());
        assert_eq!(
            slave.open_replacement(0).err(),
            Some(crate::vfs::VfsError::Corrupt)
        );
        pty.set_locked(false);
        assert!(slave.open_replacement(0).unwrap().is_none());
    }

    #[test]
    fn opening_ptmx_mints_a_fresh_pair_each_time() {
        // Other tests mint pairs concurrently, so the count only has to move
        // forward by two, not land exactly there.
        let before = open_count();
        let a = Ptmx.open_replacement(0).unwrap().unwrap();
        let b = Ptmx.open_replacement(0).unwrap().unwrap();
        assert!(open_count() >= before + 2, "each open is a new pair");

        let (a, b) = (a.as_pty().unwrap().number(), b.as_pty().unwrap().number());
        assert!(a != b, "two opens cannot share a pair");

        // The slaves the numbers name are findable, for `/dev/pts`.
        assert!(lookup(a).is_some() && lookup(b).is_some());
        assert!(pairs().iter().any(|p| p.number() == a));
    }

    #[test]
    fn the_master_is_a_terminal_too_and_carries_the_pair() {
        let pty = allocate(903, (136, 903));
        let master = PtyMaster { pty: pty.clone() };
        // Both ends are the terminal: a supervisor holds only the master, so
        // `isatty` on it is how it decides whether it can do job control. It
        // still reports its own `st_rdev`, which is what `ttyname` tells the
        // ends apart by.
        let as_tty = master.as_tty().expect("isatty(master) is true");
        assert!(
            core::ptr::eq(as_tty, pty.slave().as_ref()),
            "one terminal, two ends"
        );
        assert_eq!(master.as_pty().map(|p| p.number()), Some(903));
        assert_eq!(master.rdev(), Some((5, 2)));
    }
}
