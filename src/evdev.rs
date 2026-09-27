//! The evdev event window and the clients that read it.
//!
//! The input driver is bare metal: it needs a virtio ring, the PCI bus and the
//! kernel's clock, and the host build has none of them. But the part of it that
//! decides *who reads what* is arithmetic over a queue of records, so that part
//! lives here, ungated, where a host build can argue with it. What is left in
//! `drivers/virtio/input.rs` is the device, the ioctl surface, and the encoding
//! of one event into a record.
//!
//! The shape is Linux's, gathered in one place:
//!
//! * a *client* is an open file description, and every client of a device sees
//!   every record - two readers are two readers, not two turns at one queue;
//! * a client's position is a sequence number in the device's record stream, so
//!   a record leaving the window behind it cannot shift what it has not read;
//! * while a description holds `EVIOCGRAB` the records are that description's
//!   own, and a reader that is not it is told nothing at all - not the events,
//!   and not a false "not ready" that would be indistinguishable from an idle
//!   device.
//!
//! The identity of a client is the address of its `File`
//! ([`crate::vfs::File::ofd`]), which is the same key `flock` and `EVIOCGRAB`
//! arbitrate by: one description is one client, however many descriptors name
//! it.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::sched::policy::TaskId;

/// One `struct input_event` on x86_64: `timeval` (two `long`s), then the type,
/// code and value. Twenty-four bytes, in the little-endian order the ABI
/// spells them.
pub const RECORD: usize = 24;

/// How many records a device keeps for its readers.
///
/// The window holds what the service harvested that nobody has read yet; the
/// bound is what keeps a device with no reader at all - an input node before a
/// compositor opens it - from pinning memory for the life of the machine, and
/// the oldest records are the stalest.
const WINDOW: usize = 256;

/// The records a device has produced, seen as a window a client reads through.
///
/// Linux gives every evdev client a buffer of its own, so two readers of one
/// node each see every event and neither takes the other's. This keeps one
/// window and a *position* per client instead: the same picture with the
/// copying shared. A position is a sequence number rather than an index into
/// the window, which is what lets the window drop its oldest record - the drop
/// moves the floor rather than the records, so a client that has not read yet
/// still names what it meant to.
struct Window {
    /// The records still held, oldest first.
    records: VecDeque<[u8; RECORD]>,
    /// The sequence number of `records.front()`. The number of records the
    /// device has ever produced is `first + records.len()`, which is also the
    /// number a record pushed now would carry.
    first: u64,
}

impl Window {
    const fn new() -> Window {
        Window {
            records: VecDeque::new(),
            first: 0,
        }
    }

    /// The sequence number a record pushed now would carry - one past the last
    /// the device has produced.
    fn next(&self) -> u64 {
        self.first + self.records.len() as u64
    }

    /// Queue one record, dropping the oldest when the window is full. Dropping
    /// it is what moves `first`, and so what makes a client that fell behind
    /// lose the records it never read - the accounting Linux does with
    /// `SYN_DROPPED`, done here by moving the floor under the client.
    fn push(&mut self, record: [u8; RECORD]) {
        if self.records.len() == WINDOW {
            self.records.pop_front();
            self.first += 1;
        }
        self.records.push_back(record);
    }

    /// The position a client that has just appeared starts from: the next
    /// record, not the oldest held.
    ///
    /// This is Linux's empty per-client buffer. A reader that opens a device
    /// after the events did has missed them, and an input stream has no way to
    /// hand over what it has already forgotten; the alternative, starting at
    /// the oldest record still in the window, would deliver a keystroke that
    /// happened before the reader existed.
    fn start(&self) -> u64 {
        self.next()
    }

    /// Whether a client at `pos` has a record waiting.
    fn has(&self, pos: u64) -> bool {
        pos.max(self.first) < self.next()
    }

    /// Take whole records from `pos` into `buf`: how many bytes landed, and the
    /// position after them.
    ///
    /// A position below the floor is lifted to it - the records it names are
    /// gone, and the oldest held is the only honest place to resume.
    fn take(&self, pos: u64, buf: &mut [u8]) -> (usize, u64) {
        let from = pos.max(self.first);
        let mut at = (from - self.first) as usize;
        let capacity = buf.len() / RECORD;
        let mut taken = 0;
        while taken < capacity {
            let Some(record) = self.records.get(at) else {
                break;
            };
            buf[taken * RECORD..(taken + 1) * RECORD].copy_from_slice(record);
            taken += 1;
            at += 1;
        }
        (taken * RECORD, from + taken as u64)
    }
}

/// One reader of a device: the open file description it reads through, and
/// where it has read to.
struct Client {
    ofd: u64,
    /// The sequence number of the next record this client will read.
    pos: u64,
}

/// The records one input device has produced, and the readers of them.
///
/// Everything here is plain state: the driver holds it behind a lock, and the
/// host build holds it bare.
pub struct Records {
    window: Window,
    /// One position per open file description that has read this device. An
    /// entry is made when the description opens the node and dropped when its
    /// last descriptor closes, which is the lifetime Linux gives a client.
    clients: Vec<Client>,
    /// The open file description holding `EVIOCGRAB`, or 0 for nobody.
    grab: u64,
    /// The tasks parked on this device, each beside the client it waits as. The
    /// client is half of the entry because a wake has to be able to be
    /// *withheld*: waking a reader that would find nothing is a task switch
    /// that ends the way it started.
    waiters: Vec<(u64, TaskId)>,
    /// Whether the device behind this window has been taken away. A window whose
    /// device is gone is not an idle one: nobody may read it any more, and a
    /// reader that is waiting has to be told.
    closed: bool,
}

impl Default for Records {
    /// A device that has produced nothing and has nobody reading it, which is
    /// what [`Records::new`] builds.
    fn default() -> Self {
        Records::new()
    }
}

impl Records {
    pub const fn new() -> Records {
        Records {
            window: Window::new(),
            clients: Vec::new(),
            grab: 0,
            waiters: Vec::new(),
            closed: false,
        }
    }

    /// Queue one already-encoded record and note that the clients waiting for
    /// one are due. The caller does the waking, since that wants the
    /// scheduler's lock and this must not hold another one when it takes it.
    pub fn push(&mut self, record: [u8; RECORD]) {
        self.window.push(record);
    }

    /// A description has opened this device: give it a position now rather than
    /// at its first read.
    ///
    /// Linux hands a client its own event buffer at `open`, so an event that
    /// arrives between the open and the first read is still the client's. There
    /// is no per-client storage here to hand out, but a position taken at the
    /// open makes the same promise: without it, the gap between the open and
    /// the first read would be a hole no reader ever sees.
    pub fn opened(&mut self, ofd: u64) {
        self.position(ofd);
    }

    /// The description is gone; forget where it had read to.
    ///
    /// A position is keyed by an address, and addresses are reused: left
    /// behind, the entry would be inherited by whatever description lands on
    /// that address next, which would then either re-read records it never saw
    /// or skip records it did.
    pub fn forget(&mut self, ofd: u64) {
        self.clients.retain(|c| c.ofd != ofd);
    }

    /// The description holding the grab, or 0.
    pub fn grabbed_by(&self) -> u64 {
        self.grab
    }

    /// Claim (`on`) or drop the device for `ofd`. `false` is the answer to a
    /// claim by someone else while it is held - Linux's `EBUSY`. Dropping is
    /// never refused, and a release by a description that does not hold the
    /// device leaves it exactly as it was, as on Linux.
    pub fn set_grab(&mut self, on: bool, ofd: u64) -> bool {
        if !on {
            if self.grab == ofd {
                self.grab = 0;
                // The records that arrived under the grab were the grabber's,
                // and are nobody else's now that it is over: the other clients
                // were told nothing while it was held, so they resume from the
                // next record rather than from wherever they had got to.
                self.skip_to_now(ofd);
            }
            return true;
        }
        // Asking again while you already hold it is not a conflict - it is the
        // same claim, and a client that re-claims on a device it owns (which is
        // what a compositor restarting on the same fd does) must not be told
        // `EBUSY` by itself.
        if self.grab == ofd {
            return true;
        }
        if self.grab != 0 {
            return false;
        }
        self.grab = ofd;
        true
    }

    /// Lift every client but `keeper` to the end of the window: the records
    /// below it were delivered to the grabber alone and are not re-delivered to
    /// anyone once the grab is over.
    fn skip_to_now(&mut self, keeper: u64) {
        let next = self.window.next();
        for client in self.clients.iter_mut() {
            if client.ofd != keeper {
                client.pos = client.pos.max(next);
            }
        }
    }

    /// Whether `ofd` may read this device at all.
    ///
    /// While some description holds the grab, the records belong to it, and a
    /// reader that is not it is told nothing - not the grabber's records under
    /// another name, and not a false "not ready" that would be indistinguishable
    /// from an idle device. It waits, which is what Linux does.
    fn may_read(&self, ofd: u64) -> bool {
        !self.closed && (self.grab == 0 || self.grab == ofd)
    }

    /// Take the device away. Nothing more will be pushed, and whoever is
    /// reading is at the end of input - see [`Records::all_waiters`], which is
    /// what a caller wakes them with.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Whether the device has been taken away.
    pub fn closed(&self) -> bool {
        self.closed
    }

    /// Every registered wait, whichever client it belongs to.
    ///
    /// The device going away is the one event that is not *for* anybody in
    /// particular, so the filter [`Records::due_waiters`] applies is exactly
    /// wrong here: every waiter has to be woken to find out that it is over.
    pub fn all_waiters(&self) -> Vec<TaskId> {
        self.waiters.iter().map(|&(_, id)| id).collect()
    }

    /// Where `ofd` reads from, giving the description a position at the end of
    /// the window if this is the first time it has looked.
    fn position(&mut self, ofd: u64) -> u64 {
        if let Some(client) = self.clients.iter().find(|c| c.ofd == ofd) {
            return client.pos;
        }
        let pos = self.window.start();
        self.clients.push(Client { ofd, pos });
        pos
    }

    /// Whether a record is waiting for the client `ofd` right now.
    pub fn readable(&mut self, ofd: u64) -> bool {
        if !self.may_read(ofd) {
            return false;
        }
        let pos = self.position(ofd);
        self.window.has(pos)
    }

    /// Take records into `buf` for the client `ofd`, whole ones only, and move
    /// that client past them. Returns `None` when the buffer cannot hold even
    /// one - `EINVAL`, the answer Linux gives a read smaller than one event.
    ///
    /// A client that may not read yet (somebody else holds the grab) takes
    /// nothing and says so with a zero-byte answer: no records for you. What
    /// must not happen is a *nonzero* answer built from the grabber's records.
    pub fn take(&mut self, ofd: u64, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < RECORD {
            return None;
        }
        if !self.may_read(ofd) {
            return Some(0);
        }
        let at = match self.clients.iter().position(|c| c.ofd == ofd) {
            Some(at) => at,
            None => {
                let pos = self.window.start();
                self.clients.push(Client { ofd, pos });
                self.clients.len() - 1
            }
        };
        let (n, pos) = self.window.take(self.clients[at].pos, buf);
        self.clients[at].pos = pos;
        Some(n)
    }

    /// Park `id` on this device as the client `ofd`.
    pub fn subscribe(&mut self, ofd: u64, id: TaskId) {
        if !self.waiters.contains(&(ofd, id)) {
            self.waiters.push((ofd, id));
        }
    }

    /// Take every registration `id` has on this device back.
    pub fn unsubscribe(&mut self, id: TaskId) {
        self.waiters.retain(|&(_, waiter)| waiter != id);
    }

    /// The parked waits a record landing now would satisfy: every registered
    /// waiter whose client may read it.
    ///
    /// Every allowed client has something the moment a record lands - a
    /// position can never be ahead of the end of the window, which is what
    /// `Window::has` says by construction - so the only question is who is
    /// allowed to read, and that is the grab.
    pub fn due_waiters(&self) -> Vec<TaskId> {
        self.waiters
            .iter()
            .filter(|&&(ofd, _)| self.may_read(ofd))
            .map(|&(_, id)| id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    /// Queue one record per tag, each carrying its tag in the first byte, so a
    /// test can tell which record a read returned.
    fn push(records: &mut Records, tags: &[u8]) {
        for &tag in tags {
            let mut record = [0u8; RECORD];
            record[0] = tag;
            records.push(record);
        }
    }

    /// Everything `ofd` can take right now: `None` for a buffer too small, an
    /// empty list for "nothing for this client".
    fn drain(records: &mut Records, ofd: u64) -> Option<Vec<u8>> {
        let mut buf = [0u8; RECORD * 8];
        let n = records.take(ofd, &mut buf)?;
        Some(buf[..n].chunks(RECORD).map(|c| c[0]).collect())
    }

    /// Two descriptions reading one device are two readers, not two turns at a
    /// shared queue: what one reads, the other still has.
    #[test]
    fn two_clients_of_one_device_each_read_every_record() {
        let mut records = Records::new();
        records.opened(1);
        records.opened(2);
        push(&mut records, &[0, 1]);

        assert_eq!(drain(&mut records, 1), Some(vec![0, 1]));
        assert_eq!(drain(&mut records, 2), Some(vec![0, 1]));
        assert_eq!(drain(&mut records, 1), Some(vec![]));
    }

    /// A description that appears after the events did has missed them, which is
    /// what Linux's empty per-client buffer says too. The alternative - starting
    /// at the oldest record still held - would deliver a keystroke that happened
    /// before the reader existed.
    #[test]
    fn a_client_that_appears_late_does_not_read_the_backlog() {
        let mut records = Records::new();
        push(&mut records, &[7, 8]);
        records.opened(5);

        assert_eq!(drain(&mut records, 5), Some(vec![]));
        push(&mut records, &[9]);
        assert_eq!(drain(&mut records, 5), Some(vec![9]));
    }

    /// A grab is the whole device: while it is held the records are the
    /// grabber's, and another reader of the same node is told nothing rather
    /// than handed them - which is the bug this pins.
    #[test]
    fn a_grab_keeps_the_events_from_every_other_reader() {
        let mut records = Records::new();
        records.opened(1);
        records.opened(2);
        assert!(records.set_grab(true, 1));
        push(&mut records, &[9]);

        assert_eq!(drain(&mut records, 2), Some(vec![]));
        assert!(!records.readable(2));
        assert_eq!(drain(&mut records, 1), Some(vec![9]));
        // And the device is not doubly claimable.
        assert!(!records.set_grab(true, 2));
    }

    /// The records that arrived under a grab are not handed over when it ends:
    /// every other client was told nothing while it was held, so it resumes at
    /// the next record rather than reading the grabbed period.
    #[test]
    fn releasing_a_grab_does_not_hand_the_window_over() {
        let mut records = Records::new();
        records.opened(1);
        records.opened(2);
        assert!(records.set_grab(true, 1));
        push(&mut records, &[5]);
        assert!(records.set_grab(false, 1));

        assert_eq!(drain(&mut records, 2), Some(vec![]));
        // The grabber read nothing of its own, and still has it.
        assert_eq!(drain(&mut records, 1), Some(vec![5]));
        push(&mut records, &[6]);
        assert_eq!(drain(&mut records, 2), Some(vec![6]));
    }

    /// A client that fell further behind than the window is long resumes at the
    /// oldest record still held. The records it never read are gone, and the
    /// position that named them is lifted to the floor rather than replaying
    /// whatever now sits at that index.
    #[test]
    fn a_position_below_the_window_resumes_at_the_oldest_record() {
        let mut records = Records::new();
        records.opened(1);
        push(&mut records, &[1, 2, 3]);
        let overflow: Vec<u8> = (0..WINDOW).map(|i| i as u8).collect();
        push(&mut records, &overflow);

        // The three originals have been dropped; the oldest record still held
        // is the first of the overflow, tagged 0.
        assert_eq!(drain(&mut records, 1), Some(vec![0, 1, 2, 3, 4, 5, 6, 7]));
    }

    /// The parked waits a landing record is for are only the ones whose client
    /// may read it, so a non-owner of a grabbed device is never woken to find
    /// nothing.
    #[test]
    fn a_waiter_is_due_only_if_its_client_may_read() {
        let mut records = Records::new();
        records.opened(1);
        records.opened(2);
        records.subscribe(1, TaskId(11));
        records.subscribe(2, TaskId(22));
        assert_eq!(records.due_waiters(), vec![TaskId(11), TaskId(22)]);

        assert!(records.set_grab(true, 1));
        assert_eq!(records.due_waiters(), vec![TaskId(11)]);
        records.unsubscribe(TaskId(11));
        assert!(records.due_waiters().is_empty());
    }

    /// A device that has been taken away is not an idle one: nobody may read it,
    /// and every wait for it is due - which is how a parked reader hears about
    /// `UI_DEV_DESTROY` instead of sleeping forever.
    #[test]
    fn a_closed_device_reads_nothing_and_wakes_everyone() {
        let mut records = Records::new();
        records.opened(1);
        records.opened(2);
        records.subscribe(2, TaskId(22));
        push(&mut records, &[1]);
        assert!(records.readable(1));

        records.close();
        assert!(!records.readable(1));
        assert_eq!(records.take(1, &mut [0u8; RECORD]), Some(0));
        assert_eq!(records.due_waiters(), Vec::new());
        assert_eq!(records.all_waiters(), vec![TaskId(22)]);
    }

    /// A read smaller than one record is `EINVAL`, not a zero-byte answer that
    /// would read as end of input.
    #[test]
    fn a_buffer_too_small_for_one_record_is_invalid() {
        let mut records = Records::new();
        push(&mut records, &[1]);
        assert_eq!(records.take(1, &mut [0u8; RECORD - 1]), None);
    }

    /// Forgetting a description's position is what keeps a reused address from
    /// being inherited: the next description to land on it starts from the end
    /// of the window, not from where the last reader stopped.
    #[test]
    fn a_forgotten_client_starts_from_the_end_of_the_window() {
        let mut records = Records::new();
        records.opened(1);
        push(&mut records, &[1, 2]);
        assert_eq!(drain(&mut records, 1), Some(vec![1, 2]));

        records.forget(1);
        records.opened(1);
        assert_eq!(drain(&mut records, 1), Some(vec![]));
    }

    /// The clientless read is a reader of its own, and one someone else's grab
    /// therefore excludes - what a `pread` on a grabbed device means.
    #[test]
    fn the_clientless_read_is_not_the_grabber() {
        let mut records = Records::new();
        assert!(records.set_grab(true, 1));
        records.opened(1);
        push(&mut records, &[4]);

        assert_eq!(drain(&mut records, 0), Some(vec![]));
        assert_eq!(drain(&mut records, 1), Some(vec![4]));
    }
}
