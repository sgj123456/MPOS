//! `eventfd(2)`: a counter a process can wait on.
//!
//! It exists so that one thread can be woken by another without a pipe and
//! without polling: a write adds to the counter, a read takes it away, and a
//! `poll(2)` on the descriptor says whether there is anything to take. That is
//! all it does, and it is enough - `nlplug-findfs`'s worker thread tells its
//! main loop it has finished by writing one to an eventfd.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;

use super::{Ino, Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

/// One eventfd.
pub struct EventFd {
    ino: Ino,
    counter: Mutex<RefCell<u64>>,
    /// Who is asleep waiting for the counter to move. A blocked task sleeps
    /// until someone wakes it - the deadline is "never" - so a writer has to
    /// name the readers it made a difference to.
    waiters: Mutex<RefCell<Vec<crate::sched::TaskId>>>,
    /// `EFD_NONBLOCK`, as the fd's `F_SETFL` keeps it in sync: a read of an
    /// empty counter answers `EAGAIN` instead of sleeping. Without this a
    /// caller that drains the change-signaller after a poll *timeout* - fish's
    /// fd-monitor loop does exactly that, 256 msec at a time - sleeps here
    /// forever while holding the very mutex its main thread is waiting for.
    nonblocking: AtomicBool,
    /// `EFD_SEMAPHORE`: a read takes **one** rather than the whole counter, so
    /// the descriptor counts available slots and each `read` claims one.
    ///
    /// Measured on Linux 7.3.4 (`build/scratch/flagsw/sem.c`): a semaphore with
    /// 3 reads `1`, `1`, `1` and then `EAGAIN` (non-blocking); a plain eventfd
    /// with 3 reads `3` and is then empty. The flag also changes the write
    /// ceiling: a plain eventfd saturates at `0xfffffffffffffffe`, while a
    /// semaphore write that would pass it is **`EAGAIN`** (measured,
    /// `sem2.c`), because a semaphore represents a count that cannot overflow.
    semaphore: bool,
}

impl EventFd {
    pub fn new(ino: Ino, initial: u64, nonblocking: bool) -> Arc<EventFd> {
        Self::with_semaphore(ino, initial, nonblocking, false)
    }

    /// The same node in the `EFD_SEMAPHORE` shape (`docs/notes/` §6.176).
    pub fn with_semaphore(
        ino: Ino,
        initial: u64,
        nonblocking: bool,
        semaphore: bool,
    ) -> Arc<EventFd> {
        Arc::new(EventFd {
            ino,
            counter: Mutex::new(RefCell::new(initial)),
            waiters: Mutex::new(RefCell::new(Vec::new())),
            nonblocking: AtomicBool::new(nonblocking),
            semaphore,
        })
    }

    /// Whether this is the `EFD_SEMAPHORE` shape.
    pub fn is_semaphore(&self) -> bool {
        self.semaphore
    }

    pub fn set_nonblocking(&self, on: bool) {
        self.nonblocking.store(on, Ordering::Relaxed);
    }

    fn value(&self) -> u64 {
        critical_section::with(|cs| *self.counter.borrow(cs).borrow())
    }
}

impl Inode for EventFd {
    fn as_eventfd(&self) -> Option<&EventFd> {
        Some(self)
    }

    /// An event counter has no position: `lseek(2)` on one is `ESPIPE`.
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            // Event descriptors are anonymous; the number only has to be one
            // nothing else claims.
            ino: 0x5000_0000 + self.ino,
            kind: NodeKind::Anon { mode: 0o600 },
            size: 0,
        }
    }

    /// Linux's name for the descriptor in `/proc/<pid>/fd`: an eventfd is an
    /// `anon_inode`, and `readlink` reports that spelling rather than this
    /// kernel's `anon:[ino]` fallback (measured on a current host,
    /// `.workbuddy/probe_anon_fd_stat.c`).
    fn fd_name(&self) -> Option<alloc::string::String> {
        Some(alloc::string::String::from("anon_inode:[eventfd]"))
    }

    /// The `eventfd` block Linux's `show_fdinfo` adds: the counter, an
    /// identifier, and whether the semaphore shape is on.
    ///
    /// Measured whole on Linux 7.2.14 (`build/scratch/fdinfo/{pertype,radix2}.c`):
    ///
    /// ```text
    /// eventfd-count:                5
    /// eventfd-id: 291
    /// eventfd-semaphore: 0
    /// ```
    ///
    /// Three details, each measured rather than assumed. The counter prints in
    /// **hex** (`255` reads `ff`, `4660` reads `1234`) right-aligned in a
    /// sixteen-wide field, so the padding is part of the line. The id is a
    /// decimal, unique per instance. And the semaphore word is decimal `0` or
    /// `1`, which is the creation mode and not the current count.
    ///
    /// `eventfd-semaphore` is the creation **mode**, not the current count, so
    /// it stays `1` for a semaphore however many reads have taken from it -
    /// measured (`build/scratch/fdinfo/pertype.c`: the word is `0` or `1` and
    /// does not follow the counter). It reports
    /// [`EventFd::is_semaphore`], which this item made reachable: before it,
    /// `eventfd2(EFD_SEMAPHORE)` answered `EOPNOTSUPP` and the field could only
    /// ever be `0`.
    ///
    /// `eventfd-id` reports [`EventFd::ino`], this instance's own number from
    /// the machine's anonymous-inode counter. Linux's id comes from a separate
    /// per-boot counter, so the *numbers* differ between the two kernels - what
    /// the field promises is that an id names one instance for its lifetime,
    /// and that is what this one does. The alternative was a second counter
    /// existing only for a debug line.
    fn fdinfo_lines(&self) -> Option<alloc::string::String> {
        Some(alloc::format!(
            "eventfd-count: {:16x}\neventfd-id: {}\neventfd-semaphore: {}\n",
            self.value(),
            self.ino,
            u8::from(self.semaphore)
        ))
    }

    /// A read takes the counter and leaves zero behind - the semaphore shape,
    /// which is the only mode this kernel offers (no `EFD_SEMAPHORE`).
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.len() < 8 {
            return Err(VfsError::Invalid);
        }
        loop {
            let taken = critical_section::with(|cs| {
                let mut counter = self.counter.borrow(cs).borrow_mut();
                if *counter == 0 {
                    None
                } else if self.semaphore {
                    // One per read, decrementing - the shape a caller uses as a
                    // counting semaphore. Measured: three reads on a counter of
                    // 3 answer 1, 1, 1 then `EAGAIN`.
                    *counter -= 1;
                    Some(1)
                } else {
                    let value = *counter;
                    *counter = 0;
                    Some(value)
                }
            });
            if let Some(value) = taken {
                buf[..8].copy_from_slice(&value.to_ne_bytes());
                return Ok(8);
            }
            if self.nonblocking.load(Ordering::Relaxed) {
                return Err(VfsError::Again);
            }
            // The wait a signal may end: a `SIGALRM` or a child's death is
            // what brings an event-loop caller back to look at its own state,
            // and sleeping through it would leave it parked on a counter
            // nobody has raised.
            self.wait_nonzero()?;
        }
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        if buf.len() < 8 {
            return Err(VfsError::Invalid);
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buf[..8]);
        let value = u64::from_ne_bytes(bytes);
        let refused = critical_section::with(|cs| {
            let mut counter = self.counter.borrow(cs).borrow_mut();
            // `EVENTFD_MAX` is `0xfffffffffffffffe`, the largest count a plain
            // eventfd holds. Two shapes answer an overflow differently, and
            // both are measured (`build/scratch/flagsw/sem2.c`): a plain
            // eventfd **saturates**, while a semaphore write that would pass
            // the maximum is `EAGAIN` - a semaphore stands for available slots,
            // and a saturated count would tell a caller it may take one more
            // than exists.
            //
            // **Both shapes refuse**, which is the reading this item had to
            // correct: the first version saturated a plain eventfd, following
            // the comment that had been there, and the measurement says Linux
            // answers `EAGAIN` for one too (`build/scratch/flagsw/ovf.c`:
            // `plain: write MAX -> 8, then 1 more -> -1 errno=11`). A count
            // that saturated would tell a caller its previous writes were all
            // accepted when the last one was not.
            const MAX: u64 = 0xffff_ffff_ffff_fffe;
            if counter.checked_add(value).is_none_or(|next| next > MAX) {
                true
            } else {
                *counter += value;
                false
            }
        });
        if refused {
            return Err(VfsError::Again);
        }
        // Whoever is waiting on the counter looks again. On the host there is
        // no scheduler to wake and nothing asleep, so the list is empty and
        // the loop is not there at all.
        #[cfg(target_os = "none")]
        {
            let waiters = critical_section::with(|cs| self.waiters.borrow(cs).borrow().clone());
            for id in waiters {
                crate::sched::wake(id);
            }
        }
        Ok(8)
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.value() != 0,
            write: true,
        }
    }

    /// An eventfd is ready for writing always, so a writer never waits here -
    /// but a *reader's* wait is this node's event to deliver, and it is not
    /// only `read(2)` that waits: `poll`/`select`/`epoll_wait` park a task on
    /// the same question and subscribe through this door. A node that keeps
    /// its own parker list and does not also register the pollers leaves them
    /// asleep until their own timeout - measured on a probe boot: a thread
    /// writing an eventfd after 300 ms woke an `epoll_wait(-1)`-style park
    /// only at its 3000 ms timeout, while the same shape on a pipe and on a
    /// socketpair woke in 0-1 ms. That is the shape `async-io`/`zbus` use for
    /// every cross-thread wake, and a compositor whose event loop is parked on
    /// one looks frozen until a signal happens to interrupt the wait.
    fn poll_subscribe(&self, id: crate::sched::policy::TaskId) {
        critical_section::with(|cs| {
            let mut waiters = self.waiters.borrow(cs).borrow_mut();
            if !waiters.contains(&id) {
                waiters.push(id);
            }
        });
    }

    fn poll_unsubscribe(&self, id: crate::sched::policy::TaskId) {
        critical_section::with(|cs| {
            self.waiters.borrow(cs).borrow_mut().retain(|&w| w != id);
        });
    }
}

impl EventFd {
    /// Block until the counter is non-zero. Kernel-only: a host build has no
    /// scheduler to block on, and its tests read the ready path.
    ///
    /// Interruptible, like every other blocking wait a syscall parks in: a
    /// catchable signal ends it with `EINTR` and the counter untouched, which
    /// is the shape `select(2)`'s man page gives an eventfd for exactly this
    /// reason.
    #[cfg(target_os = "none")]
    fn wait_nonzero(&self) -> VfsResult<()> {
        let me = crate::sched::current();
        critical_section::with(|cs| {
            let mut waiters = self.waiters.borrow(cs).borrow_mut();
            if !waiters.contains(&me) {
                waiters.push(me);
            }
        });
        let end = crate::sched::block_until_interruptible(|| self.value() != 0);
        critical_section::with(|cs| {
            self.waiters.borrow(cs).borrow_mut().retain(|&id| id != me);
        });
        match end {
            crate::sched::WaitEnd::Interrupted => Err(VfsError::Interrupted),
            _ => Ok(()),
        }
    }

    #[cfg(not(target_os = "none"))]
    fn wait_nonzero(&self) -> VfsResult<()> {
        panic!("blocking on an eventfd needs the kernel scheduler");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two shapes differ in exactly two places, and both are the flag's
    /// whole meaning: a semaphore's read takes **one** and decrements, and a
    /// write past the ceiling is `EAGAIN` in **both** shapes rather than
    /// saturating.
    ///
    /// Measured on Linux 7.3.4 (`build/scratch/flagsw/{sem,sem2,ovf}.c`,
    /// `docs/notes/` §6.176). The saturation half is a corrected reading: this
    /// kernel's comment said "saturating, as Linux's counter is", and Linux
    /// answers `EAGAIN` - so a claim that saturated would have been pinning the
    /// comment rather than the behaviour.
    #[test]
    fn a_semaphore_takes_one_per_read_and_neither_shape_overflows() {
        // Three in, three reads of one out, then empty.
        let semi = EventFd::with_semaphore(2, 3, true, true);
        assert!(semi.is_semaphore());
        let mut got = Vec::new();
        for _ in 0..3 {
            let mut buf = [0u8; 8];
            assert_eq!(semi.read_at(0, &mut buf), Ok(8));
            got.push(u64::from_ne_bytes(buf));
        }
        assert_eq!(got, [1, 1, 1], "one per read, not the whole counter");
        let mut buf = [0u8; 8];
        assert_eq!(semi.read_at(0, &mut buf), Err(VfsError::Again));

        // The neighbour: the same counter without the flag comes out whole.
        let plain = EventFd::new(3, 3, true);
        assert!(!plain.is_semaphore());
        let mut buf = [0u8; 8];
        assert_eq!(plain.read_at(0, &mut buf), Ok(8));
        assert_eq!(u64::from_ne_bytes(buf), 3);

        // The ceiling, in both shapes: a write that would pass
        // `0xfffffffffffffffe` is `EAGAIN`, measured on Linux for a plain
        // eventfd too (`ovf.c`: `plain: write MAX -> 8, then 1 more -> EAGAIN`).
        const MAX: u64 = 0xffff_ffff_ffff_fffe;
        for fd in [
            EventFd::new(4, 0, true),
            EventFd::with_semaphore(5, 0, true, true),
        ] {
            assert_eq!(fd.write_at(0, &MAX.to_ne_bytes()), Ok(8));
            assert_eq!(
                fd.write_at(0, &1u64.to_ne_bytes()),
                Err(VfsError::Again),
                "a write past the maximum is refused, not saturated"
            );
            // Read it back in whatever the shape's unit is, to prove nothing
            // was silently added: the plain one holds MAX, the semaphore MAX
            // ones one read at a time.
            let mut buf = [0u8; 8];
            assert_eq!(fd.read_at(0, &mut buf), Ok(8));
            let want = if fd.is_semaphore() { 1 } else { MAX };
            assert_eq!(u64::from_ne_bytes(buf), want);
        }
    }

    #[test]
    fn a_write_adds_and_a_read_takes_everything() {
        let fd = EventFd::new(1, 0, false);
        assert!(!fd.poll_ready().read, "nothing written yet");
        let mut buf = [0u8; 8];
        assert_eq!(fd.write_at(0, &3u64.to_ne_bytes()).unwrap(), 8);
        assert!(fd.poll_ready().read);
        assert_eq!(fd.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(u64::from_ne_bytes(buf), 3);
        assert!(!fd.poll_ready().read, "the counter was taken");
    }

    #[test]
    fn a_nonblocking_eventfd_answers_eagain_on_an_empty_counter() {
        // The shape fish's fd-monitor loop depends on: poll says not-readable,
        // the caller drains the signaller anyway, and the read has to come
        // back rather than sleep - the caller holds a lock its main thread
        // needs, and a sleeping drain holds that lock for ever.
        let fd = EventFd::new(3, 0, true);
        let mut buf = [0u8; 8];
        assert!(matches!(fd.read_at(0, &mut buf), Err(VfsError::Again)));
        fd.write_at(0, &1u64.to_ne_bytes()).unwrap();
        assert_eq!(fd.read_at(0, &mut buf).unwrap(), 8);
        assert!(matches!(fd.read_at(0, &mut buf), Err(VfsError::Again)));
    }

    /// The `eventfd` block is Linux's measured bytes, in Linux's radixes.
    ///
    /// Measured on Linux 7.2.14 (`build/scratch/fdinfo/radix2.c`): a counter of
    /// 5, 255 and 4660 answered `eventfd-count:                5`,
    /// `...               ff` and `...             1234` - sixteen-wide,
    /// right-aligned, **hex** - which is why this compares whole lines rather
    /// than a substring. A renderer using `{}` would print `255` where Linux
    /// prints `ff`, and one using `{:16}` would pad with the wrong digit.
    #[test]
    fn the_eventfd_block_is_linuxs_measured_lines() {
        let fd = EventFd::new(291, 5, false);
        let text = fd.fdinfo_lines().expect("an eventfd has a block");
        assert_eq!(
            text,
            "eventfd-count:                5\neventfd-id: 291\neventfd-semaphore: 0\n"
        );

        // 255 reads `ff`, the radix a decimal renderer would get wrong.
        let hex = EventFd::new(7, 255, false);
        let text = hex.fdinfo_lines().unwrap();
        assert!(
            text.starts_with("eventfd-count:               ff\n"),
            "{text}"
        );

        // The semaphore word is always 0: `eventfd2` refuses EFD_SEMAPHORE.
        assert!(text.contains("eventfd-semaphore: 0\n"), "{text}");
    }

    #[test]
    fn an_eventfd_starts_at_its_initial_value() {
        let fd = EventFd::new(2, 7, false);
        let mut buf = [0u8; 8];
        fd.read_at(0, &mut buf).unwrap();
        assert_eq!(u64::from_ne_bytes(buf), 7);

        // The ceiling is `0xfffffffffffffffe`, **not** `u64::MAX`, and a write
        // past it is `EAGAIN` rather than a saturation - both measured on Linux
        // 7.3.4 (`build/scratch/flagsw/ovf.c`: `write MAX -> 8`, `write 1 more
        // -> -1 EAGAIN`). This test used to assert that two `u64::MAX` writes
        // left `u64::MAX` ("no wrap"), which pinned the *comment* beside the
        // counter ("saturating, as Linux's counter is") rather than Linux: the
        // maximum is one less than that word, and the overflow is reported.
        const MAX: u64 = 0xffff_ffff_ffff_fffe;
        fd.write_at(0, &MAX.to_ne_bytes()).unwrap();
        assert_eq!(
            fd.write_at(0, &1u64.to_ne_bytes()),
            Err(VfsError::Again),
            "a write past the maximum is EAGAIN, not a saturating add"
        );
        fd.read_at(0, &mut buf).unwrap();
        assert_eq!(
            u64::from_ne_bytes(buf),
            MAX,
            "and the count is the maximum, not wrapped"
        );
    }
}
