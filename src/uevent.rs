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
//! **And userland sends on it too, which is the other half of udev.** The
//! device manager hands each event to one of its workers with a *unicast* to
//! that worker's own monitor socket (`udev_monitor_send_device(monitor,
//! worker->monitor, dev)`), and becomes a listener's news only because it
//! *broadcasts* the processed device to the group `udevadm monitor` joined. So
//! this module also owns the family's write side: every socket has a port id
//! (`nl_pid`), unicast is addressed to one, a send with a group mask and no port
//! id goes to every socket that joined it, and each queued message carries the
//! address and the credentials its reader is told it came from.
//!
//! That half is measured against Linux rather than recalled, because udev's
//! protocol is built on the details. `NETLINK_KOBJECT_UEVENT`, this host's
//! kernel: a socket bound with `nl_pid == 0` is given the process's pid when
//! that number is free and a hash-derived one when it is not ("usually, but not
//! necessarily the pid", as libudev's own comment puts it); binding a port id
//! another socket holds is `EADDRINUSE`, and binding twice is `EINVAL`; a
//! unicast reaches its destination and reports the *sender's* port id with
//! `nl_groups == 0`, an unknown port id is `ECONNREFUSED`, and the sender never
//! receives its own message; a group send reports the *destination group* as the
//! source's `nl_groups`, reaches the lowest group in the mask (Linux computes
//! `ffs` of it, so `nl_groups = 6` reaches group 2 and not group 4) and never the
//! sender, and returns the byte count even when the group is empty; a send with
//! neither a port id nor a group is accepted and goes to the kernel's own
//! address; and a socket that never bound is given a port id by its first send.
//!
//! Also here: the numbering. A device's `major:minor` is what `/sys` reports,
//! what `/dev` nodes are made from, and what a listener matches events
//! against, so the kernel computes it once. See [`Numbering`].

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use critical_section::Mutex;

use crate::err;

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
    /// The `KEY=value` lines this device's subsystem adds beyond the words
    /// every event carries.
    ///
    /// Linux has the same slot - its `envp` is the kernel's own lines plus
    /// whatever the caller appended - and a listener reads the extras as the
    /// reason the event happened. The one this kernel sends is `HOTPLUG=1` on
    /// a DRM connector change, which is the word a compositor's udev monitor
    /// matches on: weston's DRM backend re-probes its connectors for that
    /// property and logs nothing for an event without it.
    pub extra: &'static [(&'static str, &'static str)],
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
        // The subsystem's own words, before the sequence number: Linux appends
        // its extras in the same place, and `SEQNUM` is last there too.
        for &(name, value) in self.extra {
            key(name, value);
        }
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

/// A netlink socket, as far as the kernel is concerned: an address, a queue of
/// messages and whether its owner asked to receive them.
struct Subscriber {
    id: usize,
    /// Whether `bind(2)` - or the implicit bind a first send performs - has
    /// named this socket. An unbound socket receives nothing, which is what
    /// Linux does with one, and has no port id for anyone to address.
    bound: bool,
    /// The `nl_pid` a peer writes to reach this socket. Not assumed to be a
    /// pid: Linux hands out the process's pid when it is free and a
    /// hash-derived number when it is not, and libudev's own comment says so
    /// ("usually, but not necessarily the pid") because it reads its address
    /// back with `getsockname` rather than guessing.
    pid: u32,
    /// The groups the socket joined; a broadcast goes to every socket whose
    /// mask intersects the destination's, and the kernel's own events go to
    /// [`GROUP_KERNEL`] alone.
    groups: u32,
    /// `SO_PASSCRED`: the owner wants each message stamped with its sender's
    /// credentials. Recorded rather than acted on - every message this module
    /// delivers carries them, which is a deliberate divergence from Linux's
    /// gate and the reason a listener that never asked still recognises the
    /// kernel by `uid 0`.
    passcred: bool,
    /// A shared buffer rather than a `Vec` each: a send runs inside a critical
    /// section, and handing one more recipient a copy of the message must not
    /// mean allocating there - see [`publish`].
    queue: VecDeque<Message>,
}

/// One message waiting in a socket's queue, with the address and the identity a
/// reader is told it came from.
///
/// The address is *not* the sender's own bind - Linux fills `msg_name` from the
/// delivery: a unicast reports the sender's `nl_pid` and `nl_groups == 0`, a
/// broadcast reports the destination group in `nl_groups`, and a kernel event
/// reports the all-zero address every listener recognises.
pub struct Message {
    /// The event's sequence number, for a kernel-published event.
    pub seq: u64,
    pub bytes: Arc<Vec<u8>>,
    /// The sender's `nl_pid`: 0 is the kernel.
    pub from_pid: u32,
    /// The group the message was broadcast to, or 0 if it was unicast.
    pub group: u32,
    /// The sender's credentials. All zero for the kernel's own messages, which
    /// is how a listener tells kernel origin from a peer's.
    pub cred: Cred,
}

/// `struct ucred`: who sent a message. The uevent family's own copy of the
/// type, because `net::unix` is bare-metal only and this module is not.
#[derive(Clone, Copy, Default)]
pub struct Cred {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

/// The one group a kobject-uevent socket has: kernel-originated events.
const GROUP_KERNEL: u32 = 1;

/// How many events one socket may have waiting.
const QUEUE_MAX: usize = 256;

/// Where assigned port ids start.
///
/// Above the range a pid occupies in practice, so an assigned id cannot be
/// mistaken for one - and monotonic rather than hashed, which is the one place
/// this departs from Linux: a hash can collide and is retried, a counter cannot
/// and does not need to be. Reuse is not modelled either; Linux's `nl_pid`
/// values are recycled with the processes behind them, and nothing in this
/// machine addresses a socket that has gone.
const PORT_BASE: u32 = 0x4000_0000;

/// Hands out [`PORT_BASE`]-upwards port ids. Monotonic, so no two sockets are
/// ever given the same one.
static NEXT_PORT: AtomicU32 = AtomicU32::new(PORT_BASE);

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
            pid: 0,
            groups: 0,
            passcred: false,
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

/// `bind(2)`: name the socket and join groups.
///
/// `pid` is the `nl_pid` the caller asked for: zero means "give me one", which
/// is what every daemon asks and what libudev reads back with `getsockname`.
/// `prefer` is the requesting process's pid - Linux's first choice for an
/// auto-assigned port id - and the *caller* supplies it: the task table lives
/// under the syscall/net layers, and this module keeps its host testability by
/// never reaching up for it (the host build passes 0, and a zero preference
/// falls through to the counter).
/// The three refusals are Linux's and each was measured: a socket that is
/// already bound is `EINVAL` (a second `bind` is not how groups are added), an
/// explicit port id another socket holds is `EADDRINUSE`, and a socket this
/// module does not own is `EBADF`.
pub fn bind(id: usize, pid: u32, groups: u32, prefer: u32) -> Result<(), isize> {
    critical_section::with(|cs| {
        let mut subs = SUBSCRIBERS.borrow(cs).borrow_mut();
        if !subs.iter().any(|s| s.id == id) {
            return Err(err::EBADF);
        }
        if subs.iter().any(|s| s.id == id && s.bound) {
            return Err(err::EINVAL);
        }
        if pid != 0 && subs.iter().any(|s| s.bound && s.pid == pid) {
            return Err(err::EADDRINUSE);
        }
        let assigned = if pid != 0 {
            pid
        } else {
            assign_port(&subs, prefer)
        };
        if let Some(sub) = subs.iter_mut().find(|s| s.id == id) {
            sub.bound = true;
            sub.pid = assigned;
            sub.groups = groups;
        }
        Ok(())
    })
}

/// The address a socket was bound to, for `getsockname`: the port id and the
/// group mask, or `None` for a socket that never bound - which Linux reports as
/// the all-zero address rather than as an error.
pub fn port_of(id: usize) -> Option<(u32, u32)> {
    critical_section::with(|cs| {
        SUBSCRIBERS
            .borrow(cs)
            .borrow()
            .iter()
            .find(|s| s.id == id)
            .filter(|s| s.bound)
            .map(|s| (s.pid, s.groups))
    })
}

/// `setsockopt(SO_PASSCRED)`. See [`Subscriber::passcred`] for what it does and
/// does not change.
pub fn set_passcred(id: usize, on: bool) {
    critical_section::with(|cs| {
        if let Some(sub) = SUBSCRIBERS
            .borrow(cs)
            .borrow_mut()
            .iter_mut()
            .find(|s| s.id == id)
        {
            sub.passcred = on;
        }
    });
}

/// Whether the socket's owner asked for credentials. Read back by
/// `getsockopt`, and by the receive path for the one thing the option decides
/// here: nothing - every message is stamped - so the two agree with what the
/// caller asked for.
pub fn passcred(id: usize) -> bool {
    critical_section::with(|cs| {
        SUBSCRIBERS
            .borrow(cs)
            .borrow()
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.passcred)
            .unwrap_or(false)
    })
}

/// The next free port id: the caller's own pid when nothing holds it, and a
/// number from the counter otherwise. Linux's own order of preference.
fn assign_port(subs: &[Subscriber], prefer: u32) -> u32 {
    if prefer != 0 && !subs.iter().any(|s| s.bound && s.pid == prefer) {
        return prefer;
    }
    loop {
        let candidate = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        if !subs.iter().any(|s| s.bound && s.pid == candidate) {
            return candidate;
        }
    }
}

/// One userland send: `sendto`/`sendmsg` on a device-event socket, and what
/// udevd's manager uses to hand an event to a worker.
///
/// `dest` is the address the caller named - `(nl_pid, nl_groups)` - or `None`
/// for a send with no address at all, which Linux treats as the kernel's own
/// address. The errnos are the measured ones: a unicast to a port id nobody
/// holds is `ECONNREFUSED`, a unicast into a queue with no room is `ENOBUFS`
/// (Linux would rather tell the sender than drop a handoff on the floor), and a
/// broadcast is accepted whether or not anybody is in the group - which is what
/// libudev relies on when it says a multicast `ECONNREFUSED` would be
/// "expected", and better than that here.
///
/// The payload is opaque. What libudev puts in it - `struct
/// udev_monitor_netlink_header`, then the device's properties - is the
/// library's protocol, not the kernel's.
pub fn send(id: usize, dest: Option<(u32, u32)>, data: &[u8], cred: Cred) -> isize {
    // Built outside the critical section, as `publish` does, so the section
    // holds only list surgery. One allocation, shared by every recipient.
    let encoded = Arc::new(data.to_vec());
    let mut queued = false;
    let answer = critical_section::with(|cs| {
        let mut subs = SUBSCRIBERS.borrow(cs).borrow_mut();
        let Some(index) = subs.iter().position(|s| s.id == id) else {
            return err::EBADF;
        };
        // A socket that never bound is named by its first send, which is what
        // Linux's autobind does and what makes a reply's source address
        // something a peer can answer.
        if !subs[index].bound {
            let assigned = assign_port(&subs, cred.pid);
            subs[index].bound = true;
            subs[index].pid = assigned;
        }
        let from = subs[index].pid;
        match dest {
            // No address, or the kernel's own: accepted, and there is no
            // reader on this side of the kernel to queue it for.
            None | Some((0, 0)) => data.len() as isize,
            Some((pid, 0)) => {
                let Some(target) = subs
                    .iter_mut()
                    .find(|s| s.id != id && s.bound && s.pid == pid)
                else {
                    return err::ECONNREFUSED;
                };
                if target.queue.len() >= QUEUE_MAX {
                    return err::ENOBUFS;
                }
                target.queue.push_back(Message {
                    seq: 0,
                    bytes: Arc::clone(&encoded),
                    from_pid: from,
                    group: 0,
                    cred,
                });
                queued = true;
                data.len() as isize
            }
            // A broadcast: every member but the sender, which Linux does not
            // deliver to and udev's manager would only have to filter out.
            //
            // One group, not the whole mask: Linux computes `ffs(nl_groups)`
            // and delivers to that group alone, so a caller that sets two bits
            // reaches only the lower one - measured (`nl_groups = 6` reaches
            // group 2 and not group 4, `nl_groups = 12` reaches group 4 and not
            // 8). Every daemon here sets one bit, and matching the artifact
            // costs nothing while a divergence would be one more thing to know.
            Some((0, mask)) => {
                let group = mask & mask.wrapping_neg();
                for sub in subs.iter_mut() {
                    if sub.id == id || !sub.bound || sub.groups & group == 0 {
                        continue;
                    }
                    if sub.queue.len() >= QUEUE_MAX {
                        sub.queue.pop_front();
                    }
                    sub.queue.push_back(Message {
                        seq: 0,
                        bytes: Arc::clone(&encoded),
                        from_pid: from,
                        group,
                        cred,
                    });
                    queued = true;
                }
                data.len() as isize
            }
            // A port id *and* a group: Linux treats the port id as the
            // destination and the group as what the message reports, which is
            // the unicast case with a different `nl_groups` in the answer.
            Some((pid, mask)) => {
                let Some(target) = subs
                    .iter_mut()
                    .find(|s| s.id != id && s.bound && s.pid == pid)
                else {
                    return err::ECONNREFUSED;
                };
                if target.queue.len() >= QUEUE_MAX {
                    return err::ENOBUFS;
                }
                target.queue.push_back(Message {
                    seq: 0,
                    bytes: Arc::clone(&encoded),
                    from_pid: from,
                    group: mask,
                    cred,
                });
                queued = true;
                data.len() as isize
            }
        }
    });
    // A message nobody is woken for is a message that never arrives: the
    // listeners on this bus park in `poll(2)`, and a worker waiting for a
    // handoff is exactly that. The wake list belongs to the network stack -
    // which on a quiet machine does not poll at all - so it is called here,
    // outside every borrow, the discipline `publish` documents.
    #[cfg(mpos_net)]
    if queued {
        crate::net::wake_waiters();
    }
    #[cfg(not(mpos_net))]
    let _ = queued;
    answer
}

/// The next event waiting for this socket, or `None` when there is none.
pub fn recv(id: usize) -> Option<Message> {
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
                sub.queue.push_back(Message {
                    seq,
                    bytes: Arc::clone(&encoded),
                    // The kernel's own address and credentials: `nl_pid 0`,
                    // `nl_groups 1` (the group this went to), and a zero
                    // `ucred`. That is how a listener tells a kernel event from
                    // a peer's without trusting anything in the payload.
                    from_pid: 0,
                    group: GROUP_KERNEL,
                    cred: Cred::default(),
                });
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
    #[cfg(mpos_net)]
    if queued {
        crate::net::wake_waiters();
    }
    #[cfg(not(mpos_net))]
    let _ = queued;
}

/// The device numbering the whole system agrees on.
///
/// Linux numbers block devices by driver: virtio-blk takes major 254, and
/// each disk's partitions get minors in order - `/dev/vda` is 254:0, `vda1`
/// is 254:1, and so on. `/sys/class/block/*/dev` prints exactly this, `/dev`
/// nodes are made from it, and a listener matching an event against a device
/// compares it. One function computes it, so the three cannot disagree.
pub struct Numbering {
    pub devices: Vec<BlockEntry>,
}

/// One name in that numbering, with everything its three readers need.
///
/// This used to be a tuple of a name and two numbers, and each reader derived
/// the rest for itself - which is how `/devices/virtio/block/<name>` came to be
/// spelled out in three files, and why the third of them would have had to
/// guess what a `sda` hangs under. A name does not say which driver a disk came
/// from, so the tree position travels with the disk
/// ([`crate::blk::Disk::devpath`]) and is carried here rather than derived.
pub struct BlockEntry {
    /// The name under `/dev`, and the `DEVNAME` a listener matches on.
    pub name: String,
    pub major: u32,
    pub minor: u32,
    /// Whether this name is a whole disk rather than a partition of one. The
    /// two are numbered the same way and read differently: only a partition's
    /// minor is an index into a partition table.
    pub whole: bool,
    /// The device-tree prefix the name hangs under, without the name:
    /// `/devices/virtio/block`.
    pub devpath: &'static str,
    /// What the name refers to, in 512-byte sectors - the `#blocks` column
    /// `/proc/partitions` prints, and the one thing about a block device that
    /// is neither a name nor a number but is read by a program that opens this
    /// file to decide what to mount.
    pub sectors: u64,
}

/// Major used for virtio block devices, as Linux assigns it.
pub const VIRTIO_BLK_MAJOR: u32 = 254;

/// Major used for SCSI and SATA disks, as Linux assigns it.
///
/// A SATA disk is `sda` and *not* `vda`, and the major is half of why: two disks
/// from two drivers are told apart by userland through their numbers before it
/// has read anything else, and `/proc/partitions` is where it reads them. A
/// kernel that numbered a SATA disk under 254 would be claiming it came from a
/// driver it did not come from.
pub const SCSI_DISK_MAJOR: u32 = 8;

impl BlockEntry {
    /// The names one disk contributes to the numbering: the disk itself, then
    /// its partitions.
    ///
    /// A disk with no partition table contributes its own name and nothing else,
    /// which is the answer to the question the single-disk version got wrong: it
    /// gated the whole disk on having a partition table, so an unpartitioned
    /// disk had *no* name at all - not `/dev/sda`, not a line in
    /// `/proc/partitions`, nothing a program could open. That is a disk a
    /// partitioner cannot partition. Linux numbers the whole disk whatever is on
    /// it (`/dev/sda` exists before and after `fdisk`), and the block layer is a
    /// name for storage rather than a name for a partition table.
    ///
    /// A pure function of what a disk is, rather than a walk over the machine's
    /// disks, so that this rule can be pinned on the host - where there is no
    /// disk to have no partition table on.
    pub fn of_disk(
        name: &str,
        major: u32,
        devpath: &'static str,
        capacity_sectors: u64,
        partitions: &[(u64, u64)],
    ) -> Vec<BlockEntry> {
        let mut out = Vec::with_capacity(partitions.len() + 1);
        out.push(BlockEntry {
            name: String::from(name),
            major,
            minor: 0,
            whole: true,
            devpath,
            sectors: capacity_sectors,
        });
        for (index, &(_, sectors)) in partitions.iter().enumerate() {
            // The partition minors come from the table order, which is what
            // `/sys` reports and what mkinitfs's `vda*` names mean.
            out.push(BlockEntry {
                name: alloc::format!("{name}{}", index + 1),
                major,
                minor: (index + 1) as u32,
                whole: false,
                devpath,
                sectors,
            });
        }
        out
    }
}

impl Numbering {
    /// The numbering of every disk this machine has, and of their partitions.
    ///
    /// Built from [`crate::blk::disks`], so a disk enters userland's namespace
    /// by being brought up rather than by being the one the kernel booted from.
    pub fn of_this_machine() -> Numbering {
        let mut devices = Vec::new();
        for disk in crate::blk::disks() {
            let partitions = crate::blk::part::partitions(disk.dev.as_ref());
            devices.extend(BlockEntry::of_disk(
                &disk.name,
                disk.major,
                disk.devpath,
                disk.dev.capacity_sectors(),
                &partitions,
            ));
        }
        Numbering { devices }
    }

    /// The numbers for one device name, if it is numbered here.
    pub fn find(&self, name: &str) -> Option<(u32, u32, bool)> {
        self.devices
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| (entry.major, entry.minor, entry.whole))
    }

    /// The device-tree prefix a name hangs under, for the devpath a device
    /// event about it carries.
    pub fn devpath_of(&self, name: &str) -> Option<&'static str> {
        self.devices
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.devpath)
    }
}

/// A major and a minor, packed into the one `dev_t` every reader of this
/// machine's device numbers decodes again.
///
/// This is libc's `makedev` and nothing of this kernel's own, because every
/// reader of the result decodes it with libc's macro: musl's `major()` and
/// `minor()`, the `maj:min` a mount record prints, the `dev` attribute
/// `/sys/class/block` publishes. The layout keeps the low twelve bits of the
/// major and the low eight of the minor in the first sixteen bits and puts the
/// *rest* of each above them, which is why both halves are spelled twice here
/// rather than once. A private packing would be a dialect, and a number that is
/// not libc's is a number that reads back as a different device.
///
/// Every pair this kernel has a name for - virtio's `254:0`, SCSI's `8:0`, a
/// tty's `4:x` - fits in those low bits, where this is just
/// `(major << 8) | minor`. It is not written that way because the anonymous
/// numbers a pseudo-filesystem gets do not fit: `stat` reports those as well,
/// and a packing that dropped their high bits would report two different
/// filesystems as one.
pub const fn makedev(major: u32, minor: u32) -> u64 {
    ((minor & 0xff) as u64)
        | (((major & 0xfff) as u64) << 8)
        | (((minor & !0xff) as u64) << 12)
        | (((major & !0xfff) as u64) << 32)
}

/// The major [`makedev`] packed into `dev`.
pub const fn major_of(dev: u64) -> u32 {
    (((dev >> 8) & 0xfff) as u32) | (((dev >> 32) as u32) & !0xfff)
}

/// The minor [`makedev`] packed into `dev`.
pub const fn minor_of(dev: u64) -> u32 {
    ((dev & 0xff) as u32) | (((dev >> 12) as u32) & !0xff)
}

/// Taking turns on the one global bus, for tests. The crate has no `std` and
/// the host build has no `spin`, so this is a lock made of one atomic - which
/// is all a test that must not see another test's events needs.
///
/// **Every test that *publishes* has to take it, not only every test that
/// reads.** The bus is one global list of subscribers and a test that registers
/// a device publishes `add` events into every bound socket, including another
/// test's: this is what `devfs`'s tests do, and the omissions were silent for as
/// long as they were because the one test that counts exactly (`QUEUE_MAX` of
/// them) happened to be bounded at exactly that number either way. Adding a
/// foreign event to a full queue pushes one of the counted ones out, so the
/// count comes up short - and the reverse race, a publish landing while the
/// counter drains, makes it one too long. The fix belongs to the publisher:
/// `let _bus = crate::uevent::test_bus::take();` before the first `register`.
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

    /// A disk with nothing on it is still a disk, and it must still have a name.
    ///
    /// The single-disk version gated the whole disk on having a partition table,
    /// so an unpartitioned disk had no name at all: no `/dev/sda`, no line in
    /// `/proc/partitions`, nothing a program could open - which includes the
    /// partitioner that would have put a table on it. The measured card's own
    /// shape is Linux's: the whole disk exists before and after `fdisk`.
    #[test]
    fn a_disk_with_no_partition_table_still_has_a_name() {
        let one = BlockEntry::of_disk("sda", 8, "/devices/ahci/block", 8192, &[]);
        assert_eq!(one.len(), 1, "the disk itself, and no partitions");
        assert_eq!(one[0].name, "sda");
        assert_eq!(one[0].minor, 0);
        assert!(one[0].whole);
        assert_eq!(one[0].sectors, 8192, "and its size is its own");
        assert_eq!(one[0].devpath, "/devices/ahci/block");

        // With a table, the disk comes first and then the partitions in order,
        // numbered from 1 and carrying their own sizes.
        let three = BlockEntry::of_disk(
            "vda",
            254,
            "/devices/virtio/block",
            4096,
            &[(2048, 1024), (3072, 1024)],
        );
        assert_eq!(three.len(), 3);
        assert_eq!(three[0].name, "vda");
        assert_eq!(three[1].name, "vda1");
        assert_eq!(three[1].minor, 1);
        assert!(!three[1].whole);
        assert_eq!(three[1].sectors, 1024);
        assert_eq!(three[2].name, "vda2");
        assert_eq!(three[2].minor, 2);
    }

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
            extra: &[],
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

    /// The word a compositor matches on, pinned as a *position* as well as a
    /// value: a listener reads the block in order, and Linux puts a subsystem's
    /// own keys between the kernel's fixed ones and the sequence number.
    #[test]
    fn a_subsystems_own_keys_travel_in_the_block() {
        let _bus = bus_lock();
        let mut event = an_event();
        event.subsystem = "drm";
        event.action = "change";
        event.extra = &[("HOTPLUG", "1")];
        let text = String::from_utf8(event.encode())
            .expect("the encoding is utf8")
            .replace('\0', "|");
        let fields: Vec<&str> = text.trim_end_matches("||").split('|').collect();
        assert_eq!(
            &fields[fields.len() - 3..],
            ["MINOR=3", "HOTPLUG=1", fields[fields.len() - 1]],
            "{fields:?} carry the extra key before the sequence number"
        );
        assert!(
            fields.last().expect("a last field").starts_with("SEQNUM="),
            "{fields:?} still end with the counter"
        );
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

        bind(watcher, 0, GROUP_KERNEL, 0).expect("a fresh socket binds");
        publish(&an_event());
        assert!(ready(watcher));
        assert!(!ready(other), "a socket that never bound stays empty");

        let event = recv(watcher).expect("the event is there");
        assert!(
            event.bytes.starts_with(b"add@"),
            "the message line comes first"
        );
        assert!(
            event.bytes.ends_with(&[0, 0]),
            "the block ends with the empty entry that terminates it"
        );
        assert_eq!(
            (event.from_pid, event.group),
            (0, GROUP_KERNEL),
            "a kernel event comes from the kernel, on the kernel's group"
        );
        assert_eq!(
            (event.cred.uid, event.cred.pid),
            (0, 0),
            "and with the credentials a listener recognises it by"
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
        bind(watcher, 0, GROUP_KERNEL, 0).expect("a fresh socket binds");
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

    // -----------------------------------------------------------------
    // The write side: what udevd's manager does with an event it has read,
    // which is hand it to one worker by port id.
    // -----------------------------------------------------------------

    /// One bound socket with a port id of its own, released at the end.
    fn a_bound_listener(groups: u32) -> usize {
        let id = open();
        bind(id, 0, groups, 0).expect("a fresh socket binds");
        id
    }

    #[test]
    fn a_bound_socket_gets_an_address_and_the_rules_around_it() {
        let _bus = bus_lock();
        let first = open();
        let (pid, _) = {
            bind(first, 0, 0, 0).expect("a zero pid asks for one");
            port_of(first).expect("a bound socket has an address")
        };
        assert_ne!(pid, 0, "a socket bound with pid 0 is given one");

        // A caller-supplied preferred pid is Linux's first choice for the
        // auto-assigned port id, taken when free - the fact `net::socket`
        // now hands down from the task table instead of `uevent` reaching up
        // for it. The counter stays the fallback for a zero preference (the
        // host build) and a taken pid, both pinned above and below.
        let preferred = open();
        bind(preferred, 0, 0, 4242).expect("a zero pid with a preference");
        assert_eq!(
            port_of(preferred).expect("bound").0,
            4242,
            "the free preference is honoured, as Linux's autobind does"
        );
        // And a preference another socket already holds falls through to the
        // counter rather than failing or stealing it.
        let clash = open();
        bind(clash, 0, 0, 4242).expect("a taken preference still binds");
        assert_ne!(
            port_of(clash).expect("bound").0,
            4242,
            "the held preference is not given twice"
        );
        close(preferred);
        close(clash);

        // Binding a port id another socket holds is `EADDRINUSE`, which is what
        // a daemon that named itself after a stale pid has to hear.
        let second = open();
        assert_eq!(bind(second, pid, 0, 0), Err(err::EADDRINUSE));
        // And a socket that is already bound may not bind again - not to add a
        // group either, which is Linux's `EINVAL` and not a merge.
        assert_eq!(bind(first, 0, 0, 0), Err(err::EINVAL));

        // An explicit free port id is honoured as given.
        let third = open();
        bind(third, 0x1234_5678, GROUP_KERNEL, 0).expect("a free explicit id binds");
        assert_eq!(port_of(third), Some((0x1234_5678, GROUP_KERNEL)));

        // An unbound socket reports no address at all, which is Linux's
        // all-zero answer rather than an error.
        let unbound = open();
        assert_eq!(port_of(unbound), None);
        assert_eq!(bind(999_999, 0, 0, 0), Err(err::EBADF));

        for id in [first, second, third, unbound] {
            close(id);
        }
    }

    #[test]
    fn a_unicast_reaches_one_socket_and_names_its_sender() {
        let _bus = bus_lock();
        let manager = a_bound_listener(0);
        let worker = a_bound_listener(0);
        let empty = a_bound_listener(0);
        let (manager_pid, _) = port_of(manager).expect("bound");
        let (worker_pid, _) = port_of(worker).expect("bound");

        assert_eq!(
            send(manager, Some((worker_pid, 0)), b"handoff", Cred::default()),
            7,
            "a unicast answers the byte count"
        );
        let got = recv(worker).expect("the handoff is in the worker's queue");
        assert_eq!(&got.bytes[..], b"handoff");
        assert_eq!(got.from_pid, manager_pid, "the source is the sender");
        assert_eq!(got.group, 0, "a unicast arrives on no group");
        assert!(recv(manager).is_none(), "the sender does not get its own");
        assert!(recv(empty).is_none(), "and nobody else does either");

        // A port id nothing holds is refused - "nothing holds" being the case a
        // daemon meets when the socket it used to talk to is gone, so the
        // closed socket is what this asks.
        let (gone_pid, _) = port_of(empty).expect("bound");
        close(empty);
        assert_eq!(
            send(manager, Some((gone_pid, 0)), b"x", Cred::default()),
            err::ECONNREFUSED
        );
        // The address a message reports is the *sender's*, and the credentials
        // ride with it: that is how the worker's manager is recognised.
        let cred = Cred {
            pid: 42,
            uid: 0,
            gid: 0,
        };
        send(manager, Some((worker_pid, 0)), b"again", cred);
        let got = recv(worker).expect("the second handoff");
        assert_eq!(got.cred.pid, 42, "the sender's credentials came with it");

        for id in [manager, worker] {
            close(id);
        }
    }

    #[test]
    fn a_broadcast_goes_to_the_group_and_never_to_its_sender() {
        let _bus = bus_lock();
        let udev_group = 2;
        let sender = a_bound_listener(0);
        let member = a_bound_listener(udev_group);
        let other_group = a_bound_listener(4);
        let (member_pid, _) = port_of(member).expect("bound");
        let (sender_pid, _) = port_of(sender).expect("bound");
        let _ = member_pid;

        assert_eq!(
            send(sender, Some((0, udev_group)), b"news", Cred::default()),
            4,
            "a broadcast answers the byte count"
        );
        let got = recv(member).expect("the member is told");
        assert_eq!(&got.bytes[..], b"news");
        assert_eq!(got.from_pid, sender_pid);
        assert_eq!(
            got.group, udev_group,
            "the source reports the group it was sent to, as Linux does"
        );
        assert!(recv(other_group).is_none(), "another group is not told");
        assert!(recv(sender).is_none(), "and the sender is not told");

        // A group with nobody in it is still a successful send - libudev
        // tolerates `ECONNREFUSED` here, and answering the count is better than
        // being tolerated.
        assert_eq!(
            send(sender, Some((0, 16)), b"nobody", Cred::default()),
            6,
            "an empty group is not an error"
        );
        // A mask with two bits reaches the *lower* group only, which is what
        // Linux's `ffs` does and what was measured: `nl_groups = 6` reaches
        // group 2 and not group 4.
        let lower = a_bound_listener(2);
        let higher = a_bound_listener(4);
        assert_eq!(send(sender, Some((0, 2 | 4)), b"mixed", Cred::default()), 5);
        let got = recv(lower).expect("the lower group is told");
        assert_eq!(got.group, 2, "and it is told the group it went to");
        assert!(
            recv(higher).is_none(),
            "the higher bit of the mask is not a destination"
        );
        // The kernel's own address (no port id, no group) is accepted and has
        // no reader here.
        assert_eq!(send(sender, Some((0, 0)), b"kernel", Cred::default()), 6);
        assert_eq!(send(sender, None, b"kernel", Cred::default()), 6);

        for id in [sender, member, other_group, lower, higher] {
            close(id);
        }
    }

    #[test]
    fn a_first_send_names_a_socket_that_never_bound() {
        let _bus = bus_lock();
        let quiet = open();
        let listener = a_bound_listener(0);
        let (listener_pid, _) = port_of(listener).expect("bound");
        assert_eq!(port_of(quiet), None, "nothing bound it");

        assert_eq!(
            send(quiet, Some((listener_pid, 0)), b"hello", Cred::default()),
            5
        );
        let (pid, groups) = port_of(quiet).expect("the send named it");
        assert_ne!(pid, 0, "with a port id of its own");
        assert_eq!(groups, 0, "and no groups it never asked for");
        let got = recv(listener).expect("delivered");
        assert_eq!(got.from_pid, pid, "which is the address it is known by");

        close(quiet);
        close(listener);
    }

    #[test]
    fn a_full_unicast_queue_is_refused_rather_than_dropped() {
        let _bus = bus_lock();
        let sender = a_bound_listener(0);
        let worker = a_bound_listener(0);
        let (worker_pid, _) = port_of(worker).expect("bound");

        for _ in 0..QUEUE_MAX {
            assert!(send(sender, Some((worker_pid, 0)), b"x", Cred::default()) > 0);
        }
        // A handoff that cannot be queued has to be *said*: a dropped one is an
        // event udevd waits for forever, where an errno lets it retry or kill
        // the worker it cannot reach.
        assert_eq!(
            send(sender, Some((worker_pid, 0)), b"x", Cred::default()),
            err::ENOBUFS
        );

        close(sender);
        close(worker);
    }

    /// The packing is libc's, and the counterfactual is the reason: every
    /// reader of these numbers - musl's `major()`/`minor()`, a `maj:min` column,
    /// a `dev` attribute - decodes with libc's macro, so a round trip through
    /// our own encode and decode is not the test. The test is the *encoded
    /// value*, which is the one thing a private packing would get wrong while
    /// still round-tripping against itself.
    ///
    /// The three pairs are the three shapes that exist here: a virtio disk, a
    /// SCSI one, and a tty, plus an anonymous number - which is the case that
    /// only survives because the encoding is not `(major << 8) | minor`.
    #[test]
    fn the_device_number_is_packed_the_way_libc_decodes_it() {
        // 254:2 as glibc's makedev lays it out: the low byte of the major above
        // the low byte of the minor, nothing in the high half.
        assert_eq!(makedev(254, 2), 0xfe02);
        assert_eq!(major_of(0xfe02), 254);
        assert_eq!(minor_of(0xfe02), 2);

        // 8:17 - a SATA disk's first partition.
        assert_eq!(makedev(8, 17), 0x0811);
        assert_eq!(major_of(0x0811), 8);
        assert_eq!(minor_of(0x0811), 17);

        // 4:64 - the first pty-ish tty Linux numbers past the low byte, which is
        // where the two halves of the minor stop being the same byte.
        assert_eq!(major_of(makedev(4, 64)), 4);
        assert_eq!(minor_of(makedev(4, 64)), 64);

        // An anonymous number: major 0, a minor that will not fit in eight bits
        // once 256 pseudo-filesystems have been mounted. `(major << 8) | minor`
        // would fold it onto 0:0 and report two filesystems as one.
        let anon = makedev(0, 300);
        assert_eq!(major_of(anon), 0);
        assert_eq!(minor_of(anon), 300);
        assert_ne!(anon, makedev(0, 44), "whose low byte it shares");
    }
}
