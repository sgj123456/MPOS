//! Device events: what the kernel tells userland when a device appears.
//!
//! Linux narrates its device tree over a netlink socket - `NETLINK_KOBJECT_UEVENT`
//! - and every piece of userland that has to *find* a device listens there:
//! udev, mdev's hotplug mode, and mkinitfs's `nlplug-findfs`, which is what
//! decides where the root filesystem is. A kernel that publishes no events is
//! a kernel whose initramfs cannot find its root, no matter how correct its
//! devices are.
//!
//! The message format is the uevent envp block: one line naming the event
//! (`add@/devices/...`), then `KEY=VALUE` entries, each NUL-terminated. The
//! kernel sends it to every socket that asked for the group, and the receiver
//! recognises kernel origin - not by trusting a field, but by the credentials
//! the kernel stamps on what it sends: `uid 0`, `nl_pid 0`.
//!
//! Also here: the numbering. A device's `major:minor` is what `/sys` reports,
//! what `/dev` nodes are made from, and what a listener matches events
//! against, so the kernel computes it once. See [`Numbering`].

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};

use critical_section::Mutex;

/// One device event, in the shape userland reads it.
pub struct Event {
    /// `add`, `remove`, `change`.
    pub action: &'static str,
    /// Where the device sits in the device tree, e.g. `/devices/virtio/block/vda3`.
    pub devpath: String,
    pub subsystem: &'static str,
    /// What it is called under `/dev`: `vda3`.
    pub devname: String,
    pub major: u32,
    pub minor: u32,
}

/// Which separator a field of a uevent record has to keep out of its own bytes.
///
/// The record is built out of two of them: the NUL that ends every line, and
/// the `@` that separates the action from the devpath on the first one. A name
/// carrying either is indistinguishable from the boundary it pretends to be,
/// and the line that gets forged out of it is one udevd acts on. Nothing in
/// this kernel mints such a name - a device name comes from the driver that
/// registers it, and a devpath from the device tree - so this is the seam where
/// that stops being an assumption about callers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Split {
    /// `action@devpath`: `@` means something here.
    AtAndNul,
    /// A `KEY=value` line, where only the NUL ends anything.
    Nul,
}

fn field(value: &str, split: Split) -> impl Iterator<Item = u8> + '_ {
    value
        .bytes()
        .filter(move |&byte| byte != 0 && (split != Split::AtAndNul || byte != b'@'))
}

impl Event {
    /// The envp block a listener parses: the message line, then the keys.
    ///
    /// The keys are the ones `nlplug-findfs` reads (ACTION, DEVPATH, SUBSYSTEM,
    /// DEVNAME, MAJOR, MINOR), spelled the way Linux spells them - userland
    /// decides what to do next from exactly these.
    ///
    /// The `SEQNUM` line is the one that is easy to leave out and expensive
    /// to omit. It is not decoration: `udevd` orders its event queue by it
    /// (`is_devpath_busy` compares each event's number against the one it is
    /// waiting behind), so a kernel that sends every event with the same
    /// number - which is what sending none of them amounts to, the property
    /// reading back as zero - tells udevd that every event is still waiting
    /// behind another. It then queues them all and starts none: no rules run,
    /// no device is tagged `ID_INPUT`, and no compositor ever sees a
    /// keyboard. Linux appends the line last, from a counter bumped once per
    /// send, and that is what this does.
    pub fn encode(&self) -> Vec<u8> {
        self.encode_seq().0
    }

    /// [`encode`](Self::encode) with the sequence number the message carries,
    /// so the netlink header can be built with the same number the payload
    /// names - `nlmsg_seq` and `SEQNUM=` are one fact, and a listener that
    /// compares one against the other cannot be told anything by two answers.
    pub fn encode_seq(&self) -> (Vec<u8>, u64) {
        let mut out = Vec::new();
        out.extend(field(self.action, Split::AtAndNul));
        out.push(b'@');
        out.extend(field(&self.devpath, Split::AtAndNul));
        out.push(0);
        let mut key = |k: &str, v: &str| {
            out.extend_from_slice(k.as_bytes());
            out.push(b'=');
            out.extend(field(v, Split::Nul));
            out.push(0);
        };
        key("ACTION", self.action);
        key("DEVPATH", &self.devpath);
        key("SUBSYSTEM", self.subsystem);
        if !self.devname.is_empty() {
            key("DEVNAME", &self.devname);
        }
        key("MAJOR", &alloc::format!("{}", self.major));
        key("MINOR", &alloc::format!("{}", self.minor));
        let seqnum = SEQNUM.fetch_add(1, Ordering::Relaxed) + 1;
        key("SEQNUM", &alloc::format!("{}", seqnum));
        out.push(0);
        (out, seqnum)
    }
}

/// The number of the last event sent, as Linux's `uevent_seqnum` counts it:
/// bumped once per event, never reused. `/sys/kernel/uevent_seqnum` reports it
/// and every message carries it, which is what lets a listener tell "this
/// arrived after that" from two copies of one event.
static SEQNUM: AtomicU64 = AtomicU64::new(0);

/// How many events have been sent, for `/sys/kernel/uevent_seqnum`.
pub fn seqnum() -> u64 {
    SEQNUM.load(Ordering::Relaxed)
}

/// A netlink socket, as far as the kernel is concerned: a queue of events and
/// whether its owner asked to receive them.
struct Subscriber {
    id: usize,
    /// Whether `bind(2)` has been called. An unbound socket receives nothing,
    /// which is what Linux does with one.
    bound: bool,
    /// The groups the socket bound to; the kernel broadcasts only to sockets
    /// that asked for this group.
    groups: u32,
    /// A shared buffer rather than a `Vec` each: the publisher runs inside a
    /// critical section, and handing one more listener a copy of the event must
    /// not mean allocating there - see [`publish`].
    /// Each entry is the event's sequence number and its bytes: the number is
    /// what the netlink header carries, beside the copy in the payload.
    queue: VecDeque<(u64, Arc<Vec<u8>>)>,
}

/// The one group a kobject-uevent socket has: kernel-originated events.
const GROUP_KERNEL: u32 = 1;

/// How many events one socket may have waiting.
const QUEUE_MAX: usize = 256;

/// Where netlink handles start. The network stack's registry numbers its
/// entries from one and `socket_handle_of_fd` cannot tell the two tables
/// apart, so netlink lives in a range of its own.
pub const NETLINK_ID_BASE: usize = 0x4000_0000;

/// `AF_NETLINK`, and the one protocol this kernel speaks on it.
pub const AF_NETLINK: u16 = 16;
pub const NETLINK_KOBJECT_UEVENT: u32 = 15;

static SUBSCRIBERS: Mutex<RefCell<Vec<Subscriber>>> = Mutex::new(RefCell::new(Vec::new()));
static NEXT_ID: Mutex<RefCell<usize>> = Mutex::new(RefCell::new(NETLINK_ID_BASE));

/// Open a netlink socket for device events. Returns the handle the fd table
/// keeps, which is this table's id. Nothing here needs the network stack: a
/// machine with no network card still has devices to announce.
pub fn open() -> usize {
    critical_section::with(|cs| {
        let seq = NEXT_ID.borrow(cs);
        let mut seq = seq.borrow_mut();
        let id = *seq;
        *seq += 1;
        SUBSCRIBERS.borrow(cs).borrow_mut().push(Subscriber {
            id,
            bound: false,
            groups: 0,
            // Sized here, in task context, so that the publisher never has to
            // grow it: [`publish`] runs inside a critical section, and
            // `VecDeque::push_back` reallocating there is an allocation with
            // the interrupts masked - the same hazard as copying the event, and
            // the reason a cold-plug that a listener was watching wrote nothing
            // at all. `QUEUE_MAX` is also the cap the publisher drops at, so the
            // length can never reach the capacity and force a reallocation.
            queue: VecDeque::with_capacity(QUEUE_MAX),
        });
        id
    })
}

/// Forget a socket. Events already queued go with it.
pub fn close(id: usize) {
    critical_section::with(|cs| {
        SUBSCRIBERS.borrow(cs).borrow_mut().retain(|s| s.id != id);
    });
}

/// `bind(2)`: from here on the socket receives events. `groups` is the
/// netlink group mask; a listener that wants everything - as `nlplug-findfs`
/// does, binding with `nl_groups = -1` - gets all of them.
pub fn bind(id: usize, groups: u32) {
    critical_section::with(|cs| {
        if let Some(sub) = SUBSCRIBERS
            .borrow(cs)
            .borrow_mut()
            .iter_mut()
            .find(|s| s.id == id)
        {
            sub.bound = true;
            sub.groups = groups;
        }
    });
}

/// The next event waiting for this socket, or `None` when there is none.
pub fn recv(id: usize) -> Option<(u64, Arc<Vec<u8>>)> {
    critical_section::with(|cs| {
        SUBSCRIBERS
            .borrow(cs)
            .borrow_mut()
            .iter_mut()
            .find(|s| s.id == id)?
            .queue
            .pop_front()
    })
}

/// Whether this socket has an event waiting - what `poll(2)` reports.
pub fn ready(id: usize) -> bool {
    critical_section::with(|cs| {
        SUBSCRIBERS
            .borrow(cs)
            .borrow()
            .iter()
            .find(|s| s.id == id)
            .map(|s| !s.queue.is_empty())
            .unwrap_or(false)
    })
}

/// Whether `id` names a netlink socket this module owns.
pub fn exists(id: usize) -> bool {
    critical_section::with(|cs| SUBSCRIBERS.borrow(cs).borrow().iter().any(|s| s.id == id))
}

/// Publish an event to every socket that asked for device events.
///
/// This is the kernel half of `uevent`: called when a device is registered,
/// and again when userland writes to a device's `uevent` attribute - the
/// second path is how a listener that started *after* the devices existed
/// (an initramfs is exactly that) gets told about them.
pub fn publish(event: &Event) {
    // Built *before* the critical section, and shared rather than copied.
    //
    // This is a writer's path: it runs from device registration and from a
    // write to a device's `uevent` attribute, and the machine has to keep
    // working while a listener is attached - which is exactly when it is used.
    // Cloning the encoded bytes per subscriber allocated inside a section with
    // the interrupts masked, and the allocator's lock belongs to whichever task
    // last held it: with `udevd` running and queueing events, that write could
    // spin there with the holder unable to run, and the cold-plug stalled on the
    // first device it announced. A refcount bump allocates nothing, so the
    // section now holds only list surgery.
    let (bytes, seq) = event.encode_seq();
    let encoded = Arc::new(bytes);
    let mut queued = false;
    critical_section::with(|cs| {
        for sub in SUBSCRIBERS.borrow(cs).borrow_mut().iter_mut() {
            if sub.bound && sub.groups & GROUP_KERNEL != 0 {
                // Bounded: a listener that stops reading must not grow the
                // kernel's memory without limit. Linux drops on overflow too.
                if sub.queue.len() >= QUEUE_MAX {
                    sub.queue.pop_front();
                }
                sub.queue.push_back((seq, Arc::clone(&encoded)));
                queued = true;
            }
        }
    });
    // An event nobody is woken for is an event that never arrives. The listeners
    // on this bus are `udevd`'s workers and `udevadm monitor`, parked in
    // `poll(2)`; their wake list belongs to the network stack, which on a quiet
    // machine does not poll at all - so the queue filled, the daemon slept
    // through it, and its database stayed empty. Woken, it reads what was
    // queued and the cold-plug completes.
    #[cfg(target_os = "none")]
    if queued {
        crate::net::wake_waiters();
    }
}

/// The device numbering the whole system agrees on.
///
/// Linux numbers block devices by driver: virtio-blk takes major 254, and
/// each disk's partitions get minors in order - `/dev/vda` is 254:0, `vda1`
/// is 254:1, and so on. `/sys/class/block/*/dev` prints exactly this, `/dev`
/// nodes are made from it, and a listener matching an event against a device
/// compares it. One function computes it, so the three cannot disagree.
pub struct Numbering {
    /// (name under /dev, major, minor, whether it is a whole disk)
    pub devices: Vec<(String, u32, u32, bool)>,
}

/// Major used for virtio block devices, as Linux assigns it.
pub const VIRTIO_BLK_MAJOR: u32 = 254;

impl Numbering {
    /// The numbering of the disk the kernel booted from and its partitions.
    pub fn of_boot_disk() -> Numbering {
        let mut devices = Vec::new();
        let count = crate::blk::partition_count();
        if count > 0 {
            devices.push((String::from("vda"), VIRTIO_BLK_MAJOR, 0, true));
            for index in 0..count {
                // The partition minors come from the table order, which is
                // what `/sys` reports and what mkinitfs's vda* names mean.
                devices.push((
                    alloc::format!("vda{}", index + 1),
                    VIRTIO_BLK_MAJOR,
                    (index + 1) as u32,
                    false,
                ));
            }
        }
        Numbering { devices }
    }

    /// The numbers for one device name, if it is numbered here.
    pub fn find(&self, name: &str) -> Option<(u32, u32, bool)> {
        self.devices
            .iter()
            .find(|(n, _, _, _)| n == name)
            .map(|(_, major, minor, whole)| (*major, *minor, *whole))
    }
}

/// Taking turns on the one global bus, for tests. The crate has no `std` and
/// the host build has no `spin`, so this is a lock made of one atomic - which
/// is all a test that must not see another test's events needs.
#[cfg(test)]
pub(crate) mod test_bus {
    use core::sync::atomic::{AtomicBool, Ordering};

    static LOCK: AtomicBool = AtomicBool::new(false);

    pub(crate) struct BusLock;

    pub(crate) fn take() -> BusLock {
        while LOCK.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        BusLock
    }

    impl Drop for BusLock {
        fn drop(&mut self) {
            LOCK.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bus is global, so the tests that publish on it take turns: a test
    /// that counts what it receives cannot also receive another test's events.
    fn bus_lock() -> test_bus::BusLock {
        test_bus::take()
    }

    fn an_event() -> Event {
        Event {
            action: "add",
            devpath: String::from("/devices/virtio/block/vda3"),
            subsystem: "block",
            devname: String::from("vda3"),
            major: 254,
            minor: 3,
        }
    }

    #[test]
    fn an_event_encodes_as_the_envp_block_listeners_parse() {
        let _bus = bus_lock();
        // The wire format is the ABI here: a listener reads the message line
        // first, then KEY=VALUE entries, each NUL-terminated.
        let text = String::from_utf8(an_event().encode())
            .expect("the encoding is utf8")
            .replace('\0', "|");
        // The number itself is the kernel's counter, shared by every event the
        // machine has sent - so the test pins the line, not the value.
        assert!(text.ends_with("||"), "{text:?} ends with its terminator");
        let fields: Vec<&str> = text.trim_end_matches("||").split('|').collect();
        assert_eq!(fields[0], "add@/devices/virtio/block/vda3");
        assert_eq!(
            &fields[1..fields.len() - 1],
            [
                "ACTION=add",
                "DEVPATH=/devices/virtio/block/vda3",
                "SUBSYSTEM=block",
                "DEVNAME=vda3",
                "MAJOR=254",
                "MINOR=3"
            ]
        );
        let seqnum = fields
            .last()
            .expect("a last field")
            .strip_prefix("SEQNUM=")
            .expect("the number is last, as Linux appends it");
        assert!(seqnum.parse::<u64>().is_ok(), "{seqnum:?} is a number");
    }

    #[test]
    fn every_event_carries_a_later_number_than_the_last() {
        // What udevd's queue is ordered by: `is_devpath_busy` compares an
        // event's number with the one it is waiting behind, so two events that
        // shared a number would both be deferred forever - queued and never
        // started, which is a device manager that does nothing at all.
        let _bus = bus_lock();
        let first = seqnum_of(&an_event().encode());
        let second = seqnum_of(&an_event().encode());
        assert!(second > first, "{second} follows {first}");
        assert_eq!(seqnum(), second, "and the counter says so");
    }

    /// The `SEQNUM=` value out of an encoded event.
    fn seqnum_of(encoded: &[u8]) -> u64 {
        let text = String::from_utf8(encoded.to_vec()).expect("utf8");
        text.split('\0')
            .find_map(|field| field.strip_prefix("SEQNUM="))
            .and_then(|n| n.parse().ok())
            .expect("a sequence number")
    }

    #[test]
    fn only_bound_sockets_receive_events_and_the_queue_drains() {
        let _bus = bus_lock();
        let watcher = open();
        let other = open();
        publish(&an_event());
        assert!(!ready(watcher), "an unbound socket receives nothing");

        bind(watcher, 1);
        publish(&an_event());
        assert!(ready(watcher));
        assert!(!ready(other), "a socket that never bound stays empty");

        let event = recv(watcher).expect("the event is there");
        assert!(event.1.starts_with(b"add@"), "the message line comes first");
        assert!(
            event.1.ends_with(&[0, 0]),
            "the block ends with the empty entry that terminates it"
        );

        // Draining works, and it terminates. Not "the queue is empty
        // afterwards": the bus is shared, and another test publishing while
        // this one drains is not a failure of anything.
        let mut drained = 0;
        while recv(watcher).is_some() {
            drained += 1;
            assert!(drained < QUEUE_MAX + 1, "the queue is bounded");
        }

        // Closing one socket is one socket's business: the other stays.
        close(watcher);
        assert!(!exists(watcher));
        assert!(exists(other));
        close(other);
        assert!(!exists(other));
    }

    #[test]
    fn a_bounded_queue_keeps_the_newest_events() {
        let _bus = bus_lock();
        let watcher = open();
        bind(watcher, 1);
        for _ in 0..QUEUE_MAX + 8 {
            publish(&an_event());
        }
        let mut delivered = 0;
        while recv(watcher).is_some() {
            delivered += 1;
        }
        assert_eq!(delivered, QUEUE_MAX, "the queue is bounded, not unbounded");
        close(watcher);
    }
}
