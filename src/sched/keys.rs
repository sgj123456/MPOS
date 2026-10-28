//! The kernel keyring: keys, keyrings and the answers the `KEYCTL_*` family
//! gives them.
//!
//! **Why this module lives in `sched`, not in `vfs`.** A key is not a file: its
//! identity is a serial the caller holds, its lifetime is a task's (or a
//! thread group's), and its permission model is the kernel's own `possessor`
//! class, which is decided by which keyrings the *calling task* can reach. That
//! is process state, exactly like the System V queues in `sysv_msg` and the
//! semaphore sets in `sysv_sem`, and the `syscall -> sched` edge those two
//! already record is the one this module uses. Nothing here touches hardware,
//! so the whole model is host-testable; the handlers that copy from user memory
//! are `syscall/linux/keys.rs`'s.
//!
//! **The shapes are Linux's, measured first** (`docs/notes/` §6.30, host Linux
//! 7.2.11-arch1-1, probed through `add_key`/`keyctl` as uid 1000):
//!
//! - a user key's payload is at most 32 767 bytes; a description at most 4 095;
//! - `keyctl(READ)` answers the *whole* payload length and copies nothing when
//!   the caller's buffer is smaller, so a caller learns the size by asking;
//! - `DESCRIBE` is `"<type>;<uid>;<gid>;<perm %08x>;<desc>"` and the default
//!   permission word is `0x3f010000` - every bit to the possessor, `VIEW` to
//!   the owner, nothing to group or other;
//! - the state check runs **before** the permission check: a revoked key
//!   answers `EKEYREVOKED` to `READ`, `DESCRIBE`, `SEARCH` and `UPDATE` alike;
//! - an invalidated key answers `ENOKEY`, as if it had never existed;
//! - `SEARCH` recurses into nested keyrings; `UPDATE` on a keyring is
//!   `EOPNOTSUPP`; unlinking a key that is not in that ring is `ENOENT`;
//! - possession is a permission class and nothing else: a key unlinked from
//!   every ring is still *found* by serial, and `READ` on it answers `EACCES`
//!   because the owner class holds only `VIEW`;
//! - a forked child inherits the session keyring; its process and thread
//!   keyrings start empty;
//! - an unprivileged user's quota is 20 000 bytes over 200 keys, and a key that
//!   would cross it answers `EDQUOT`.
//!
//! **Named divergences** (each a refusal or a narrower answer, never a success
//! for something not done):
//!
//! - No upcall. Linux's `request_key(2)` runs `/sbin/request-key` through the
//!   kernel when the search misses; this kernel has no such helper, so a miss
//!   is `ENOKEY` even when callout information is supplied. A program that
//!   relies on the upcall gets the same answer it would get if the helper were
//!   slow to answer, and the gap is named here rather than papered over.
//! - No key lifetimes (`EKEYEXPIRED` is defined but nothing expires) and no
//!   `KEYCTL_GET_SECURITY` (SELinux's security blob has no analogue here).
//! - A key unlinked from its last keyring is kept (charged to its owner's
//!   quota) until it is revoked or invalidated. Linux drops the last reference
//!   at unlink time; keeping the object means a serial a program still holds
//!   keeps answering with the key's real state instead of `ENOKEY`, and the
//!   cost is quota the program can reclaim with `KEYCTL_INVALIDATE`.
//! - There is one set of keyrings for the one user this machine boots with, and
//!   `CAP_SYS_ADMIN` is spelled "uid 0" - the owner-change and set-permission
//!   paths answer with the capability check this kernel actually has.
//! - A keyring holds at most 256 links (`EDQUOT` past it); Linux's bound is the
//!   same shape, different number.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicU32, Ordering};
use critical_section::Mutex;

use super::TaskId;
use crate::err::{
    EACCES, EDQUOT, EINVAL, EKEYREVOKED, ENODEV, ENOENT, ENOKEY, ENOTDIR, EOPNOTSUPP,
};

/// A key's handle, as `key_serial_t` sees it: a small positive number.
pub type Serial = u32;

/// The two key types this kernel serves. Anything else is `ENODEV`, which is
/// Linux's answer for an unknown type name (measured).
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyType {
    User,
    Keyring,
}

/// The three states a key can be in. Linux also has "expired" and "dead"; this
/// kernel has no lifetimes, so the two it can reach are `Revoked` (the caller
/// said so) and `Invalidated` (the caller asked for the key to vanish).
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Live,
    Revoked,
    Invalidated,
}

/// The permission ladder's bits, in the order Linux puts them in the word.
const PERM_VIEW: u32 = 0x01;
const PERM_READ: u32 = 0x02;
const PERM_WRITE: u32 = 0x04;
const PERM_SEARCH: u32 = 0x08;
const PERM_LINK: u32 = 0x10;
// `PERM_SETATTR` is deliberately absent: `SETPERM`/`CHOWN` are gated on
// privilege rather than on the class bits (measured: an ordinary owner of a
// key is refused `EACCES`), so no permission check ever consults that bit. The
// bit itself still travels through a `DESCRIBE` word, which is a raw u32.

/// The default permission word, measured: possessor `0x3f`, owner `VIEW`,
/// group and other none.
pub const DEFAULT_PERM: u32 = 0x3f01_0000;

/// Linux's special `key_serial_t` ids, negative so they cannot collide with a
/// real serial.
pub const KEY_SPEC_THREAD_KEYRING: i32 = -1;
pub const KEY_SPEC_PROCESS_KEYRING: i32 = -2;
pub const KEY_SPEC_SESSION_KEYRING: i32 = -3;
pub const KEY_SPEC_USER_KEYRING: i32 = -4;
pub const KEY_SPEC_USER_SESSION_KEYRING: i32 = -5;
pub const KEY_SPEC_GROUP_KEYRING: i32 = -6;
pub const KEY_SPEC_REQKEY_AUTH_KEY: i32 = -7;
pub const KEY_SPEC_REQUESTOR_KEYRING: i32 = -8;

/// A user key's payload ceiling, measured: 32 767 is accepted, 32 768 is
/// `EINVAL`.
pub const MAX_PAYLOAD: usize = 32_767;
/// A description's ceiling, measured: 4 095 accepted, 4 096 `EINVAL`.
pub const MAX_DESC: usize = 4_095;
/// A keyring's link ceiling: the same shape as Linux's bound, a number this
/// kernel chooses because a link list is a `Vec` (`EDQUOT` past it).
const MAX_LINKS: usize = 256;

/// How many keyrings a possession search walks through before giving up.
/// Linux bounds its own possession walk the same way (`keyring_search` retries
/// until it stops finding new keyrings); a cycle is impossible for a keyring
/// tree this kernel only grows downward, but the bound keeps a hostile link
/// chain from spinning.
const POSSESSION_DEPTH: usize = 8;

/// The one caller a key operation is asked for. `groups` is the *supplementary*
/// set: the primary gid is `gid`.
#[derive(Clone)]
pub struct Caller {
    /// The effective user id - the class a key's `user` bits are granted to.
    pub uid: u32,
    /// The effective group id.
    pub gid: u32,
    /// Supplementary groups, from `/proc/<pid>/status`'s `Groups:` line.
    pub groups: Vec<u32>,
    /// The task asking; its thread keyring is keyed by this.
    pub tid: TaskId,
    /// The thread group; process and session keyrings are keyed by this.
    pub tgid: TaskId,
}

impl Caller {
    /// Who is asking: the scheduler's own identity, on a booted machine. The
    /// host build has one synthetic caller (uid 0) because there is no task.
    #[cfg(target_os = "none")]
    pub fn current() -> Caller {
        let identity = super::current_identity();
        Caller {
            uid: identity.euid,
            gid: identity.egid,
            groups: identity.groups.clone(),
            tid: super::current(),
            tgid: super::current_tgid(),
        }
    }

    /// The host build's single caller. Host tests construct their own to keep
    /// quota and ring state apart.
    #[cfg(not(target_os = "none"))]
    pub fn current() -> Caller {
        Caller {
            uid: 0,
            gid: 0,
            groups: Vec::new(),
            tid: TaskId(0),
            tgid: TaskId(0),
        }
    }
}

/// What `READ` hands the handler: a user key's bytes, or a keyring's links as
/// the serials Linux returns (each a `u32`, little-endian, as it copies them).
#[derive(Debug, PartialEq, Eq)]
pub enum ReadData {
    Bytes(Vec<u8>),
    Links(Vec<Serial>),
}

impl ReadData {
    /// The length Linux's `READ` would answer: the payload's, or four bytes per
    /// link.
    pub fn len(&self) -> usize {
        match self {
            ReadData::Bytes(bytes) => bytes.len(),
            ReadData::Links(links) => links.len() * 4,
        }
    }

    /// The bytes to copy when the caller's buffer is big enough. Link serials
    /// are the little-endian `u32`s a `keyring_read` copies on both machines -
    /// which is the machine's own order, and both are little-endian here.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_wire(&self) -> Vec<u8> {
        match self {
            ReadData::Bytes(bytes) => bytes.clone(),
            ReadData::Links(links) => {
                let mut bytes = Vec::with_capacity(links.len() * 4);
                for link in links {
                    bytes.extend_from_slice(&link.to_le_bytes());
                }
                bytes
            }
        }
    }
}

/// One key or keyring. The object is shared: keyrings hold `Serial`s, not
/// pointers, so a key's lifetime is the table's and the serial is the identity
/// (the divergence about unlink above is exactly this choice).
struct Key {
    serial: Serial,
    ty: KeyType,
    desc: String,
    owner: Mutex<RefCell<(u32, u32)>>,
    perm: Mutex<RefCell<u32>>,
    state: Mutex<Cell<State>>,
    payload: Mutex<RefCell<Payload>>,
}

/// A key's payload: bytes for a user key, link serials for a keyring.
enum Payload {
    Bytes(Vec<u8>),
    Links(Vec<Serial>),
}

impl Key {
    fn new(serial: Serial, ty: KeyType, desc: String, owner: (u32, u32)) -> Arc<Key> {
        let payload = match ty {
            KeyType::User => Payload::Bytes(Vec::new()),
            KeyType::Keyring => Payload::Links(Vec::new()),
        };
        Arc::new(Key {
            serial,
            ty,
            desc,
            owner: Mutex::new(RefCell::new(owner)),
            perm: Mutex::new(RefCell::new(DEFAULT_PERM)),
            state: Mutex::new(Cell::new(State::Live)),
            payload: Mutex::new(RefCell::new(payload)),
        })
    }

    fn ty(&self) -> KeyType {
        self.ty
    }

    fn owner(&self) -> (u32, u32) {
        critical_section::with(|cs| *self.owner.borrow(cs).borrow())
    }

    fn state(&self) -> State {
        critical_section::with(|cs| self.state.borrow(cs).get())
    }

    fn mode(&self) -> u32 {
        critical_section::with(|cs| *self.perm.borrow(cs).borrow())
    }

    fn bytes(&self) -> Vec<u8> {
        critical_section::with(|cs| match &*self.payload.borrow(cs).borrow() {
            Payload::Bytes(bytes) => bytes.clone(),
            Payload::Links(_) => Vec::new(),
        })
    }

    fn links(&self) -> Vec<Serial> {
        critical_section::with(|cs| match &*self.payload.borrow(cs).borrow() {
            Payload::Links(links) => links.clone(),
            Payload::Bytes(_) => Vec::new(),
        })
    }

    fn set_bytes(&self, bytes: Vec<u8>) {
        critical_section::with(|cs| *self.payload.borrow(cs).borrow_mut() = Payload::Bytes(bytes));
    }

    /// Link a key into this ring. Linking a key already linked is a no-op that
    /// succeeds (Linux's own `key_link` treats a duplicate as "already there").
    fn link(&self, serial: Serial) -> Result<(), isize> {
        critical_section::with(|cs| {
            let mut payload = self.payload.borrow(cs).borrow_mut();
            match &mut *payload {
                Payload::Links(links) => {
                    if links.contains(&serial) {
                        return Ok(());
                    }
                    if links.len() >= MAX_LINKS {
                        return Err(EDQUOT);
                    }
                    links.push(serial);
                    Ok(())
                }
                Payload::Bytes(_) => Err(EINVAL),
            }
        })
    }

    fn unlink(&self, serial: Serial) -> Result<(), isize> {
        critical_section::with(|cs| {
            let mut payload = self.payload.borrow(cs).borrow_mut();
            match &mut *payload {
                Payload::Links(links) => match links.iter().position(|link| *link == serial) {
                    Some(at) => {
                        links.remove(at);
                        Ok(())
                    }
                    None => Err(ENOENT),
                },
                Payload::Bytes(_) => Err(ENOENT),
            }
        })
    }

    fn clear(&self) {
        critical_section::with(|cs| {
            if let Payload::Links(links) = &mut *self.payload.borrow(cs).borrow_mut() {
                links.clear();
            }
        })
    }
}

/// One thread group's process and session keyrings.
#[derive(Clone, Copy, Default)]
struct GroupRings {
    process: Option<Serial>,
    session: Option<Serial>,
}

/// One user's keyrings: the user keyring and the user-session keyring, both
/// lazily made (Linux makes the same two on first use).
#[derive(Clone, Copy, Default)]
struct UserRings {
    user: Option<Serial>,
    session: Option<Serial>,
}

/// A user's key quota. Linux's unprivileged numbers are the measured ones; uid
/// 0 gets the kernel's root quota, because a booted machine's init *is* root
/// and a root quota of 20 000 bytes would fail the first real user of the
/// interface.
fn limits(uid: u32) -> (u32, u32) {
    if uid == 0 {
        (1_000_000, 25_000_000)
    } else {
        (200, 20_000)
    }
}

/// What a user has spent: keys created and payload bytes held.
#[derive(Clone, Copy, Default)]
struct Usage {
    keys: u32,
    bytes: u32,
}

static KEYS: Mutex<RefCell<BTreeMap<Serial, Arc<Key>>>> = Mutex::new(RefCell::new(BTreeMap::new()));
static NEXT_SERIAL: AtomicU32 = AtomicU32::new(1);
static THREAD_RINGS: Mutex<RefCell<BTreeMap<usize, Serial>>> =
    Mutex::new(RefCell::new(BTreeMap::new()));
static GROUP_RINGS: Mutex<RefCell<BTreeMap<usize, GroupRings>>> =
    Mutex::new(RefCell::new(BTreeMap::new()));
static USER_RINGS: Mutex<RefCell<BTreeMap<u32, UserRings>>> =
    Mutex::new(RefCell::new(BTreeMap::new()));
static USAGE: Mutex<RefCell<BTreeMap<u32, Usage>>> = Mutex::new(RefCell::new(BTreeMap::new()));

fn next_serial() -> Serial {
    NEXT_SERIAL.fetch_add(1, Ordering::Relaxed)
}

fn get(serial: Serial) -> Option<Arc<Key>> {
    critical_section::with(|cs| KEYS.borrow(cs).borrow().get(&serial).cloned())
}

fn install(key: Arc<Key>) {
    critical_section::with(|cs| {
        KEYS.borrow(cs).borrow_mut().insert(key.serial, key);
    });
}

/// Look a key up the way every operation starts: the state check comes first,
/// so a revoked key is `EKEYREVOKED` even for a caller with no permission, and
/// an invalidated key is indistinguishable from one that never existed
/// (measured: either answer can come first and both were observed).
fn lookup(serial: Serial) -> Result<Arc<Key>, isize> {
    let key = get(serial).ok_or(ENOKEY)?;
    match key.state() {
        State::Live => Ok(key),
        State::Revoked => Err(EKEYREVOKED),
        State::Invalidated => Err(ENOKEY),
    }
}

fn charge(uid: u32, keys: i64, bytes: i64) -> Result<(), isize> {
    critical_section::with(|cs| {
        let mut usage = USAGE.borrow(cs).borrow_mut();
        let entry = usage.entry(uid).or_default();
        let (max_keys, max_bytes) = limits(uid);
        let next_keys = entry.keys as i64 + keys;
        let next_bytes = entry.bytes as i64 + bytes;
        if next_keys > max_keys as i64 || next_bytes > max_bytes as i64 {
            return Err(EDQUOT);
        }
        entry.keys = next_keys.max(0) as u32;
        entry.bytes = next_bytes.max(0) as u32;
        Ok(())
    })
}

/// The serials of every keyring the caller can reach *directly* (its thread,
/// process, session, user and user-session rings). Absent rings are not made:
/// possession walks what exists.
fn direct_rings(caller: &Caller) -> Vec<Serial> {
    let mut rings = Vec::new();
    critical_section::with(|cs| {
        if let Some(serial) = THREAD_RINGS.borrow(cs).borrow().get(&caller.tid.0) {
            rings.push(*serial);
        }
        if let Some(group) = GROUP_RINGS.borrow(cs).borrow().get(&caller.tgid.0) {
            if let Some(serial) = group.process {
                rings.push(serial);
            }
            if let Some(serial) = group.session {
                rings.push(serial);
            }
        }
        if let Some(user) = USER_RINGS.borrow(cs).borrow().get(&caller.uid) {
            if let Some(serial) = user.user {
                rings.push(serial);
            }
            if let Some(serial) = user.session {
                rings.push(serial);
            }
        }
    });
    rings
}

/// Is this key reachable from the caller's keyrings? Reachability is what the
/// possessor permission class *is*: a key linked in a ring the caller holds, or
/// in a keyring that is itself reachable, is possessed. The walk does not
/// consult the intermediate keyrings' `SEARCH` permission - Linux does, and
/// this kernel's one-user model cannot observe the difference it would make.
fn possessed(serial: Serial, caller: &Caller) -> bool {
    let mut frontier = direct_rings(caller);
    let mut seen = frontier.clone();
    for _ in 0..POSSESSION_DEPTH {
        if frontier.is_empty() {
            return false;
        }
        if frontier.contains(&serial) {
            return true;
        }
        let mut next = Vec::new();
        for ring in frontier {
            let Some(key) = get(ring) else { continue };
            if key.ty() != KeyType::Keyring {
                continue;
            }
            for link in key.links() {
                if !seen.contains(&link) {
                    seen.push(link);
                    next.push(link);
                }
            }
        }
        frontier = next;
    }
    false
}

/// Which permission class applies, and does that class carry `need`? The ladder
/// is Linux's: possessor, then owner, then group, then other.
fn may(key: &Key, caller: &Caller, need: u32, is_possessed: bool) -> bool {
    let perm = key.mode();
    let class = if is_possessed {
        // The possessor nibble sits in the top byte of the word, which is why
        // the default is `0x3f010000` for a key its owner possesses.
        (perm >> 24) & 0x3f
    } else if caller.uid == key.owner().0 {
        (perm >> 16) & 0x3f
    } else if caller.gid == key.owner().1 || caller.groups.contains(&key.owner().1) {
        (perm >> 8) & 0x3f
    } else {
        perm & 0x3f
    };
    class & need == need
}

/// `CAP_SYS_ADMIN`, spelled with the one capability bit this machine's model
/// can answer: the uid-0 caller. Linux's capability check is the same question
/// with a wider vocabulary (`docs/notes/` §6.30 names the divergence).
fn privileged(caller: &Caller) -> bool {
    caller.uid == 0
}

fn make_ring(caller: &Caller, desc: String) -> Result<Serial, isize> {
    charge(caller.uid, 1, 0)?;
    let serial = next_serial();
    install(Key::new(
        serial,
        KeyType::Keyring,
        desc,
        (caller.uid, caller.gid),
    ));
    Ok(serial)
}

fn resolve_keyring(serial: Serial, caller: &Caller, need: u32) -> Result<Arc<Key>, isize> {
    let key = lookup(serial)?;
    if key.ty() != KeyType::Keyring {
        return Err(ENOTDIR);
    }
    if !possessed(serial, caller) {
        // A ring the caller cannot reach is `ENOKEY`, not `EACCES`: Linux's
        // `lookup_user_key` refuses to say a non-possessed key exists
        // (measured: `add_key` into a ring this task does not hold).
        return Err(ENOKEY);
    }
    if !may(&key, caller, need, true) {
        return Err(EACCES);
    }
    Ok(key)
}

fn install_thread_ring(caller: &Caller, serial: Serial) {
    critical_section::with(|cs| {
        THREAD_RINGS
            .borrow(cs)
            .borrow_mut()
            .insert(caller.tid.0, serial);
    });
}

fn install_group_ring(caller: &Caller, serial: Serial, session: bool) {
    critical_section::with(|cs| {
        let mut groups = GROUP_RINGS.borrow(cs).borrow_mut();
        let group = groups.entry(caller.tgid.0).or_default();
        if session {
            group.session = Some(serial);
        } else {
            group.process = Some(serial);
        }
    });
}

fn install_user_ring(uid: u32, serial: Serial, session: bool) {
    critical_section::with(|cs| {
        let mut users = USER_RINGS.borrow(cs).borrow_mut();
        let rings = users.entry(uid).or_default();
        if session {
            rings.session = Some(serial);
        } else {
            rings.user = Some(serial);
        }
    });
}

/// The default name a made-on-demand ring carries. Linux uses the same
/// `"==thread=="`, `"==process=="`, `"==session=="`, `"==user=="` and
/// `"==user-session=="` descriptions (the strings its own `/proc/keys` shows).
fn default_ring(caller: &Caller, spec: i32, create: bool) -> Result<Serial, isize> {
    let existing = critical_section::with(|cs| match spec {
        KEY_SPEC_THREAD_KEYRING => THREAD_RINGS.borrow(cs).borrow().get(&caller.tid.0).copied(),
        KEY_SPEC_PROCESS_KEYRING => GROUP_RINGS
            .borrow(cs)
            .borrow()
            .get(&caller.tgid.0)
            .and_then(|group| group.process),
        KEY_SPEC_SESSION_KEYRING => GROUP_RINGS
            .borrow(cs)
            .borrow()
            .get(&caller.tgid.0)
            .and_then(|group| group.session),
        KEY_SPEC_USER_KEYRING => USER_RINGS
            .borrow(cs)
            .borrow()
            .get(&caller.uid)
            .and_then(|rings| rings.user),
        KEY_SPEC_USER_SESSION_KEYRING => USER_RINGS
            .borrow(cs)
            .borrow()
            .get(&caller.uid)
            .and_then(|rings| rings.session),
        _ => None,
    });
    if let Some(serial) = existing {
        return Ok(serial);
    }
    if !create {
        return Err(ENOKEY);
    }
    let (desc, install) = match spec {
        KEY_SPEC_THREAD_KEYRING => (String::from("==thread=="), 1u8),
        KEY_SPEC_PROCESS_KEYRING => (String::from("==process=="), 2),
        KEY_SPEC_SESSION_KEYRING => (String::from("==session=="), 3),
        KEY_SPEC_USER_KEYRING => (String::from("==user=="), 4),
        KEY_SPEC_USER_SESSION_KEYRING => (String::from("==user-session=="), 5),
        _ => return Err(EINVAL),
    };
    let serial = make_ring(caller, desc)?;
    match install {
        1 => install_thread_ring(caller, serial),
        2 => install_group_ring(caller, serial, false),
        3 => install_group_ring(caller, serial, true),
        4 => install_user_ring(caller.uid, serial, false),
        _ => install_user_ring(caller.uid, serial, true),
    }
    Ok(serial)
}

/// `KEYCTL_GET_KEYRING_ID`: the serial a `key_serial_t` id names, making the
/// named default ring when `create` asks for one. A positive id must be a
/// keyring the caller possesses - Linux answers `ENOKEY` for anything else
/// (measured).
pub fn get_keyring_id(caller: &Caller, id: i32, create: bool) -> Result<Serial, isize> {
    match id {
        KEY_SPEC_THREAD_KEYRING
        | KEY_SPEC_PROCESS_KEYRING
        | KEY_SPEC_SESSION_KEYRING
        | KEY_SPEC_USER_KEYRING
        | KEY_SPEC_USER_SESSION_KEYRING => default_ring(caller, id, create),
        KEY_SPEC_GROUP_KEYRING => Err(EOPNOTSUPP),
        KEY_SPEC_REQKEY_AUTH_KEY | KEY_SPEC_REQUESTOR_KEYRING => Err(ENOKEY),
        _ if id >= 0 => {
            let serial = id as Serial;
            let key = lookup(serial)?;
            if key.ty() != KeyType::Keyring {
                return Err(ENOTDIR);
            }
            if !possessed(serial, caller) {
                return Err(ENOKEY);
            }
            Ok(serial)
        }
        _ => Err(EINVAL),
    }
}

/// `add_key(2)`: create a key of `ty` under `ring`'s keyring, or update the one
/// already there with the same type and description. Measured: the second add
/// of the same description updates the payload in place and answers the *same*
/// serial.
pub fn add_key(
    caller: &Caller,
    ty: &str,
    desc: &str,
    payload: &[u8],
    ring: i32,
) -> Result<Serial, isize> {
    let key_type = match ty {
        "user" => KeyType::User,
        "keyring" => KeyType::Keyring,
        _ => return Err(ENODEV),
    };
    if desc.len() > MAX_DESC {
        return Err(EINVAL);
    }
    let ring_serial = get_keyring_id(caller, ring, true)?;
    let ring_key = resolve_keyring(ring_serial, caller, PERM_WRITE)?;

    // An existing key with this description in this ring: update its payload.
    for link in ring_key.links() {
        let Some(candidate) = get(link) else { continue };
        if candidate.ty() != key_type || candidate.desc != desc {
            continue;
        }
        match key_type {
            KeyType::User => {
                if payload.len() > MAX_PAYLOAD {
                    return Err(EINVAL);
                }
                lookup(candidate.serial)?;
                if !may(
                    &candidate,
                    caller,
                    PERM_WRITE,
                    possessed(candidate.serial, caller),
                ) {
                    return Err(EACCES);
                }
                update_bytes(&candidate, caller.uid, payload)?;
            }
            KeyType::Keyring => {}
        }
        return Ok(candidate.serial);
    }

    // A new key: charge it, then make and link it.
    match key_type {
        KeyType::User => {
            if payload.len() > MAX_PAYLOAD {
                return Err(EINVAL);
            }
            charge(caller.uid, 1, payload.len() as i64)?;
        }
        KeyType::Keyring => charge(caller.uid, 1, 0)?,
    }
    let serial = next_serial();
    let key = Key::new(
        serial,
        key_type,
        String::from(desc),
        (caller.uid, caller.gid),
    );
    if key_type == KeyType::User {
        key.set_bytes(payload.to_vec());
    }
    install(key);
    if get(serial).is_none() {
        return Err(EINVAL);
    }
    // The link may fail (a full ring): the key is still in the table, and the
    // quota it took stays charged. Reported, not hidden.
    ring_key.link(serial)?;
    Ok(serial)
}

fn update_bytes(key: &Arc<Key>, uid: u32, payload: &[u8]) -> Result<(), isize> {
    let old = key.bytes().len() as i64;
    let delta = payload.len() as i64 - old;
    charge(uid, 0, delta)?;
    key.set_bytes(payload.to_vec());
    Ok(())
}

/// `KEYCTL_UPDATE`: replace a user key's payload. A keyring answers
/// `EOPNOTSUPP` (measured).
pub fn keyctl_update(caller: &Caller, serial: Serial, payload: &[u8]) -> Result<(), isize> {
    let key = lookup(serial)?;
    if key.ty() != KeyType::User {
        return Err(EOPNOTSUPP);
    }
    if payload.len() > MAX_PAYLOAD {
        return Err(EINVAL);
    }
    if !may(&key, caller, PERM_WRITE, possessed(serial, caller)) {
        return Err(EACCES);
    }
    update_bytes(&key, caller.uid, payload)?;
    Ok(())
}

/// `KEYCTL_REVOKE`: the key exists and nothing may be done with it again except
/// unlink it. Measured: `READ`, `DESCRIBE`, `SEARCH` and `UPDATE` all answer
/// `EKEYREVOKED` afterwards, because the state check runs first.
pub fn keyctl_revoke(caller: &Caller, serial: Serial) -> Result<(), isize> {
    let key = lookup(serial)?;
    if !may(&key, caller, PERM_WRITE, possessed(serial, caller)) {
        return Err(EACCES);
    }
    critical_section::with(|cs| key.state.borrow(cs).set(State::Revoked));
    Ok(())
}

/// `KEYCTL_INVALIDATE`: the key is gone - `ENOKEY` afterwards, as if it had
/// never existed (measured). Linux also unlinks it from every keyring and
/// schedules its destruction; this kernel marks it and clears its payload,
/// which is the observable half.
pub fn keyctl_invalidate(caller: &Caller, serial: Serial) -> Result<(), isize> {
    let key = lookup(serial)?;
    if !may(&key, caller, PERM_WRITE, possessed(serial, caller)) {
        return Err(EACCES);
    }
    critical_section::with(|cs| key.state.borrow(cs).set(State::Invalidated));
    key.set_bytes(Vec::new());
    // Unlink it from every keyring that holds it: a dead key should not appear
    // in a listing, and Linux's invalidate does the same.
    let ring_serials: Vec<Serial> = critical_section::with(|cs| {
        KEYS.borrow(cs)
            .borrow()
            .values()
            .filter(|key| key.ty() == KeyType::Keyring)
            .map(|key| key.serial)
            .collect()
    });
    for ring_serial in ring_serials {
        if let Some(ring) = get(ring_serial) {
            let _ = ring.unlink(serial);
        }
    }
    Ok(())
}

/// `KEYCTL_SETPERM`: rewrite the permission word. The measured answer for a
/// non-root owner is `EACCES`, because the default word gives the owner no
/// `SETATTR`; only a privileged caller can change it.
pub fn keyctl_setperm(caller: &Caller, serial: Serial, perm: u32) -> Result<(), isize> {
    let key = lookup(serial)?;
    if perm & !0x3f3f_3f3f != 0 {
        return Err(EINVAL);
    }
    // Measured: an ordinary owner that *possesses* the key is still refused -
    // Linux answers `EACCES` for uid 1000 on a key it made, even though the
    // possessor nibble carries `SETATTR`. Only a privileged caller may rewrite
    // the word (the module header names the divergence).
    if !privileged(caller) {
        return Err(EACCES);
    }
    critical_section::with(|cs| *key.perm.borrow(cs).borrow_mut() = perm);
    Ok(())
}

/// `KEYCTL_CHOWN`: change a key's owner. Linux validates both ids (`-1` is
/// `EINVAL`), then requires `CAP_SYS_ADMIN` unless the ids already match the
/// key's own; this kernel spells capability as "uid 0" (named above).
pub fn keyctl_chown(caller: &Caller, serial: Serial, uid: u32, gid: u32) -> Result<(), isize> {
    let key = lookup(serial)?;
    if uid == u32::MAX || gid == u32::MAX {
        return Err(EINVAL);
    }
    if (uid, gid) != key.owner() && !privileged(caller) {
        return Err(EACCES);
    }
    critical_section::with(|cs| *key.owner.borrow(cs).borrow_mut() = (uid, gid));
    Ok(())
}

/// `KEYCTL_DESCRIBE`: `"<type>;<uid>;<gid>;<perm %08x>;<desc>"`, whose length is
/// what the call answers (measured: 36 bytes for a user key, 39 for a keyring).
pub fn keyctl_describe(caller: &Caller, serial: Serial) -> Result<String, isize> {
    let key = lookup(serial)?;
    if !may(&key, caller, PERM_VIEW, possessed(serial, caller)) {
        return Err(EACCES);
    }
    let ty = match key.ty() {
        KeyType::User => "user",
        KeyType::Keyring => "keyring",
    };
    Ok(alloc::format!(
        "{};{};{};{:08x};{}",
        ty,
        key.owner().0,
        key.owner().1,
        key.mode(),
        key.desc
    ))
}

/// `KEYCTL_READ`: a user key's payload or a keyring's link list. The handler
/// applies Linux's copy rule (answer the whole length, copy only what fits).
pub fn keyctl_read(caller: &Caller, serial: Serial) -> Result<ReadData, isize> {
    let key = lookup(serial)?;
    if !may(&key, caller, PERM_READ, possessed(serial, caller)) {
        return Err(EACCES);
    }
    match key.ty() {
        KeyType::User => Ok(ReadData::Bytes(key.bytes())),
        KeyType::Keyring => Ok(ReadData::Links(key.links())),
    }
}

/// `KEYCTL_LINK`: link a possessed key into a keyring. Both ends are checked:
/// `LINK` on the key and `WRITE` on the ring (measured: after unlinking from
/// every ring the key answers `EACCES` for `LINK`).
pub fn keyctl_link(caller: &Caller, serial: Serial, ring: i32) -> Result<(), isize> {
    let key = lookup(serial)?;
    let is_possessed = possessed(serial, caller);
    if !may(&key, caller, PERM_LINK, is_possessed) {
        return Err(EACCES);
    }
    let ring_serial = get_keyring_id(caller, ring, false)?;
    let ring_key = resolve_keyring(ring_serial, caller, PERM_WRITE)?;
    ring_key.link(serial)
}

/// `KEYCTL_UNLINK`: remove a key from a keyring it is linked in. A key that is
/// not in that ring is `ENOENT` (measured).
pub fn keyctl_unlink(caller: &Caller, serial: Serial, ring: i32) -> Result<(), isize> {
    if get(serial).is_none() {
        return Err(ENOKEY);
    }
    let ring_serial = get_keyring_id(caller, ring, false)?;
    let ring_key = resolve_keyring(ring_serial, caller, PERM_WRITE)?;
    ring_key.unlink(serial)
}

/// `KEYCTL_CLEAR`: empty a keyring. The keys themselves are untouched - the
/// divergence about their lifetime is stated in the module docs.
pub fn keyctl_clear(caller: &Caller, ring: i32) -> Result<(), isize> {
    let ring_serial = get_keyring_id(caller, ring, false)?;
    let ring_key = resolve_keyring(ring_serial, caller, PERM_WRITE)?;
    ring_key.clear();
    Ok(())
}

/// The recursive half of `KEYCTL_SEARCH`: walk a keyring's links, matching type
/// and description, and stop at the first hit. A revoked key is reported as
/// `EKEYREVOKED` rather than skipped (measured: `SEARCH` on a revoked key
/// answers `EKEYREVOKED`), and an invalidated one is skipped because it answers
/// `ENOKEY` everywhere else.
fn search_in(
    ring: Serial,
    caller: &Caller,
    ty: &str,
    desc: &str,
    depth: usize,
) -> Result<Option<Serial>, isize> {
    if depth > POSSESSION_DEPTH {
        return Ok(None);
    }
    let Some(ring_key) = get(ring) else {
        return Ok(None);
    };
    for link in ring_key.links() {
        let Some(candidate) = get(link) else { continue };
        let matches_type = match candidate.ty() {
            KeyType::User => ty == "user",
            KeyType::Keyring => ty == "keyring",
        };
        if matches_type && candidate.desc == desc {
            match candidate.state() {
                State::Invalidated => continue,
                State::Revoked => return Err(EKEYREVOKED),
                State::Live => return Ok(Some(link)),
            }
        }
        if candidate.ty() == KeyType::Keyring {
            if !may(&candidate, caller, PERM_SEARCH, possessed(link, caller)) {
                continue;
            }
            if let Some(found) = search_in(link, caller, ty, desc, depth + 1)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

/// `KEYCTL_SEARCH`: find a key by type and description below a keyring, and
/// optionally link the find into `dest`. Measured: the search recurses into
/// nested keyrings; a revoked match is `EKEYREVOKED`.
pub fn keyctl_search(
    caller: &Caller,
    ring: i32,
    ty: &str,
    desc: &str,
    dest: i32,
) -> Result<Serial, isize> {
    match ty {
        "user" | "keyring" => {}
        _ => return Err(ENODEV),
    }
    let ring_serial = get_keyring_id(caller, ring, false)?;
    let ring_key = resolve_keyring(ring_serial, caller, PERM_SEARCH)?;
    let found = search_in(ring_key.serial, caller, ty, desc, 0)?.ok_or(ENOKEY)?;
    if dest != 0 {
        keyctl_link(caller, found, dest)?;
    }
    Ok(found)
}

/// `request_key(2)`: search the caller's own keyrings for a key; no upcall, so
/// a miss is `ENOKEY` even when callout information is given (named above).
/// `dest` links the find into a keyring when it is not the default one.
pub fn request_key(
    caller: &Caller,
    ty: &str,
    desc: &str,
    callout: Option<&str>,
    dest: i32,
) -> Result<Serial, isize> {
    match ty {
        "user" | "keyring" => {}
        _ => return Err(ENODEV),
    }
    // Linux searches session, process, thread (then user) rings. This kernel's
    // rings are a smaller set, so the walk is the same list slightly reordered;
    // the observable shape - "whatever the caller holds" - is the same.
    // The destination ring, when named, is the caller's chosen starting point
    // and is checked first: Linux answers `EACCES` for a ring the caller cannot
    // search even though it can name its serial (measured: a session ring this
    // task no longer holds after joining another session).
    let mut first_ring: Option<i32> = None;
    if dest != 0 {
        let ring = lookup(dest as Serial)?;
        if ring.ty() != KeyType::Keyring {
            return Err(ENOTDIR);
        }
        if !may(
            &ring,
            caller,
            PERM_SEARCH,
            possessed(dest as Serial, caller),
        ) {
            return Err(EACCES);
        }
        first_ring = Some(dest);
    }
    let order = [
        KEY_SPEC_SESSION_KEYRING,
        KEY_SPEC_PROCESS_KEYRING,
        KEY_SPEC_THREAD_KEYRING,
        KEY_SPEC_USER_KEYRING,
        KEY_SPEC_USER_SESSION_KEYRING,
    ];
    if let Some(ring) = first_ring
        && let Ok(found) = keyctl_search(caller, ring, ty, desc, 0)
    {
        return Ok(found);
    }
    for spec in order {
        let Ok(ring) = get_keyring_id(caller, spec, false) else {
            continue;
        };
        if let Ok(found) = keyctl_search(caller, ring as i32, ty, desc, 0) {
            if dest != 0 {
                // Reflect the same link rule as SEARCH's dest.
                let _ = keyctl_link(caller, found, dest);
            }
            return Ok(found);
        }
    }
    let _ = callout;
    Err(ENOKEY)
}

/// `KEYCTL_JOIN_SESSION_KEYRING`: name (or make) a session keyring. A name that
/// already names a keyring the caller holds joins that one; a fresh name (or
/// none) makes a new session ring, which is what Linux does (measured: the call
/// answers a new serial, and `GET_KEYRING_ID` then reports it).
pub fn join_session_keyring(caller: &Caller, name: Option<&str>) -> Result<Serial, isize> {
    if let Some(name) = name {
        // `find_keyring_by_name`: a keyring is findable by its description, and
        // a session ring this call made is not linked anywhere else, so a
        // possession walk cannot rediscover it. Reach it the way Linux does.
        let mut existing: Option<Serial> = None;
        critical_section::with(|cs| {
            for key in KEYS.borrow(cs).borrow().values() {
                if key.ty() == KeyType::Keyring && key.desc == name {
                    existing = Some(key.serial);
                    break;
                }
            }
        });
        if let Some(serial) = existing {
            install_group_ring(caller, serial, true);
            return Ok(serial);
        }
    }
    let desc = match name {
        Some(name) => String::from(name),
        None => String::from("==session=="),
    };
    let serial = make_ring(caller, desc)?;
    install_group_ring(caller, serial, true);
    Ok(serial)
}

// --- lifetime -----------------------------------------------------------

/// A thread's keyrings go away with it, and a thread group's with its last
/// thread. Linux's own teardown is reference-counted; this kernel's table is
/// keyed by task, so the same two events are the same two removals. The keys
/// themselves stay in the table (the named divergence) and their quota stays
/// charged until they are invalidated.
pub fn exit_cleanup(task: TaskId, tgid: TaskId, group_dead: bool) {
    critical_section::with(|cs| {
        THREAD_RINGS.borrow(cs).borrow_mut().remove(&task.0);
        if group_dead {
            GROUP_RINGS.borrow(cs).borrow_mut().remove(&tgid.0);
        }
    });
}

/// A forked child inherits the session keyring and nothing else (measured: the
/// session serial is the same in parent and child; the child's process and
/// thread rings start empty). A `CLONE_THREAD` child shares its thread group,
/// so it has nothing to inherit - the group's entries are already the child's.
pub fn clone_cleanup(
    parent_tgid: TaskId,
    child_tgid: TaskId,
    child_tid: TaskId,
    shares_group: bool,
) {
    critical_section::with(|cs| {
        if shares_group {
            THREAD_RINGS.borrow(cs).borrow_mut().remove(&child_tid.0);
            return;
        }
        let session = GROUP_RINGS
            .borrow(cs)
            .borrow()
            .get(&parent_tgid.0)
            .and_then(|group| group.session);
        GROUP_RINGS.borrow(cs).borrow_mut().insert(
            child_tgid.0,
            GroupRings {
                process: None,
                session,
            },
        );
        THREAD_RINGS.borrow(cs).borrow_mut().remove(&child_tid.0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(n: usize) -> Caller {
        let uid = 4_000 + n as u32;
        Caller {
            uid,
            gid: uid,
            groups: Vec::new(),
            tid: TaskId(0x200 + n),
            tgid: TaskId(0x200 + n),
        }
    }

    const PROCESS: i32 = KEY_SPEC_PROCESS_KEYRING;
    const SESSION: i32 = KEY_SPEC_SESSION_KEYRING;

    #[test]
    fn a_user_key_round_trips_through_its_ring() {
        let me = caller(0);
        let serial = add_key(&me, "user", "t-round-trip", b"hello", PROCESS).expect("add");
        match keyctl_read(&me, serial).expect("read") {
            ReadData::Bytes(bytes) => assert_eq!(bytes, b"hello"),
            ReadData::Links(_) => panic!("a user key read as links"),
        }
        assert_eq!(
            keyctl_describe(&me, serial).expect("describe"),
            "user;4000;4000;3f010000;t-round-trip"
        );
        // The second add of the same description updates in place, same serial.
        let again =
            add_key(&me, "user", "t-round-trip", b"hello again", PROCESS).expect("add again");
        assert_eq!(again, serial);
        match keyctl_read(&me, serial).expect("read") {
            ReadData::Bytes(bytes) => assert_eq!(bytes, b"hello again"),
            ReadData::Links(_) => panic!("a user key read as links"),
        }
        keyctl_update(&me, serial, b"short").expect("update");
    }

    #[test]
    fn a_missing_serial_is_enokey_and_an_invalidated_one_too() {
        let me = caller(1);
        assert_eq!(keyctl_read(&me, 0x0fff_ffff), Err(ENOKEY));
        let serial = add_key(&me, "user", "t-invalidate", b"x", PROCESS).expect("add");
        assert_eq!(keyctl_invalidate(&me, serial), Ok(()));
        assert_eq!(keyctl_read(&me, serial), Err(ENOKEY));
        assert_eq!(keyctl_link(&me, serial, PROCESS), Err(ENOKEY));
    }

    #[test]
    fn a_revoked_key_answers_every_operation_with_ekeyrevoked() {
        let me = caller(2);
        let serial = add_key(&me, "user", "t-revoke", b"x", PROCESS).expect("add");
        assert_eq!(keyctl_revoke(&me, serial), Ok(()));
        assert_eq!(keyctl_read(&me, serial), Err(EKEYREVOKED));
        assert_eq!(keyctl_describe(&me, serial), Err(EKEYREVOKED));
        assert_eq!(keyctl_update(&me, serial, b"y"), Err(EKEYREVOKED));
        assert_eq!(
            keyctl_search(&me, PROCESS, "user", "t-revoke", 0),
            Err(EKEYREVOKED)
        );
    }

    #[test]
    fn an_unlinked_key_is_still_found_by_serial_and_read_is_eacces() {
        let me = caller(3);
        let serial = add_key(&me, "user", "t-unlinked", b"x", PROCESS).expect("add");
        assert_eq!(keyctl_unlink(&me, serial, PROCESS), Ok(()));
        // Possession is the permission class, not a findability filter: the
        // owner class holds VIEW only, so READ is EACCES while DESCRIBE works.
        assert_eq!(keyctl_read(&me, serial), Err(EACCES));
        assert!(keyctl_describe(&me, serial).is_ok());
        assert_eq!(keyctl_link(&me, serial, PROCESS), Err(EACCES));
        // A key that was never in the ring at all is ENOENT.
        assert_eq!(keyctl_unlink(&me, serial, PROCESS), Err(ENOENT));
    }

    #[test]
    fn a_search_walks_into_a_nested_keyring() {
        let me = caller(4);
        let outer = add_key(&me, "keyring", "t-outer", b"", PROCESS).expect("outer");
        let inner = add_key(&me, "keyring", "t-inner", b"", outer as i32).expect("inner");
        let key = add_key(&me, "user", "t-nested", b"payload", inner as i32).expect("key");
        assert_eq!(keyctl_search(&me, PROCESS, "user", "t-nested", 0), Ok(key));
        // And the link list is what READ answers for a keyring: one serial.
        match keyctl_read(&me, inner).expect("read ring") {
            ReadData::Links(links) => assert_eq!(links, alloc::vec![key]),
            ReadData::Bytes(_) => panic!("a keyring read as bytes"),
        }
    }

    #[test]
    fn a_keyring_clears_and_does_not_offer_update() {
        let me = caller(5);
        let ring = add_key(&me, "keyring", "t-clear", b"", PROCESS).expect("ring");
        let key = add_key(&me, "user", "t-clear-key", b"payload", ring as i32).expect("key");
        match keyctl_read(&me, ring).expect("read") {
            ReadData::Links(links) => assert_eq!(links, alloc::vec![key]),
            ReadData::Bytes(_) => panic!("a keyring read as bytes"),
        }
        assert_eq!(keyctl_update(&me, ring, b"x"), Err(EOPNOTSUPP));
        assert_eq!(keyctl_clear(&me, ring as i32), Ok(()));
        match keyctl_read(&me, ring).expect("read") {
            ReadData::Links(links) => assert!(links.is_empty()),
            ReadData::Bytes(_) => panic!("a keyring read as bytes"),
        }
    }

    #[test]
    fn a_session_ring_is_made_and_then_reported() {
        let me = caller(6);
        assert_eq!(get_keyring_id(&me, SESSION, false), Err(ENOKEY));
        let serial = join_session_keyring(&me, None).expect("join");
        assert_eq!(get_keyring_id(&me, SESSION, false), Ok(serial));
        // Joining the same name twice answers the same keyring.
        let named = join_session_keyring(&me, Some("t-session")).expect("named");
        assert_eq!(join_session_keyring(&me, Some("t-session")), Ok(named));
    }

    #[test]
    fn an_unprivileged_owner_cannot_set_permissions() {
        let me = caller(7);
        let serial = add_key(&me, "user", "t-perm", b"x", PROCESS).expect("add");
        assert_eq!(keyctl_setperm(&me, serial, 0x3f3f_3f3f), Err(EACCES));
        assert_eq!(keyctl_setperm(&me, serial, 0x0040_0000), Err(EINVAL));
    }

    #[test]
    fn a_forked_child_inherits_only_the_session_ring() {
        let parent = caller(8);
        let child_tgid = TaskId(0x999);
        let child_tid = TaskId(0x99a);
        let session = join_session_keyring(&parent, None).expect("join");
        add_key(&parent, "user", "t-inherited", b"x", PROCESS).expect("add");
        clone_cleanup(parent.tgid, child_tgid, child_tid, false);
        let child = Caller {
            uid: parent.uid,
            gid: parent.gid,
            groups: Vec::new(),
            tid: child_tid,
            tgid: child_tgid,
        };
        assert_eq!(get_keyring_id(&child, SESSION, false), Ok(session));
        assert_eq!(get_keyring_id(&child, PROCESS, false), Err(ENOKEY));
        assert_eq!(
            get_keyring_id(&child, KEY_SPEC_THREAD_KEYRING, false),
            Err(ENOKEY)
        );
    }

    #[test]
    fn the_unprivileged_byte_quota_answers_edquot() {
        // A fresh uid, so the quota this test spends is its own: the measured
        // unprivileged allowance is 20 000 bytes per user.
        let caller = caller(91);
        let session = get_keyring_id(&caller, SESSION, true).expect("session");
        let big = alloc::vec![b'x'; 19_990];
        assert!(add_key(&caller, "user", "quota-a", &big, session as i32).is_ok());
        let over = alloc::vec![b'y'; 20];
        assert_eq!(
            add_key(&caller, "user", "quota-b", &over, session as i32),
            Err(EDQUOT)
        );
        // A keyring under the same budget is still allowed; the refusal is the
        // byte charge, not the key count.
        assert!(add_key(&caller, "keyring", "quota-ring", &[], session as i32).is_ok());
    }
}
