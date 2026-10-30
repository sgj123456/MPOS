//! Extended attributes: the store a memory filesystem keeps, and the rules
//! Linux's *generic* layer applies to a name before any filesystem is asked.
//!
//! The split is Linux's own, and it is worth keeping because the two halves
//! answer different questions. A name is checked against one vocabulary for
//! every filesystem (`user.*` is the only namespace this kernel serves, and a
//! name outside it is `EOPNOTSUPP` - Linux's answer to a namespace it has no
//! handler for, which is exactly what this kernel is). The *store* then answers
//! what one filesystem can hold: whether the inode's kind may carry an attribute
//! at all, what happens when two attributes collide, and how many bytes a
//! caller sees.
//!
//! Measured on Linux 7.2.11 first (uid 1000, `/dev/shm` a tmpfs), and the
//! numbers below are that run rather than a reading of the man page
//! (`.workbuddy/probe_xattr{2,3,4,5,6}.c`, 2026-10-30):
//!
//! - `setxattr("user.a","v1",2,0)` answers 0; `getxattr(..., buf, 0)` answers 2
//!   - the length the caller needs, with the buffer pointer never touched - and
//!   a buffer one byte short is `ERANGE`, not a short read.
//! - `XATTR_CREATE` on a name already set is `EEXIST`; `XATTR_REPLACE` on a name
//!   nobody set is `ENODATA`; any other flag bit is `EINVAL`. **Both bits
//!   together is not `EINVAL`**: the same call answers `EEXIST` when the
//!   attribute is there and `ENODATA` when it is not - the existence rules are
//!   applied, and the contradiction the man page describes does not come up.
//! - a name longer than 255 bytes is `ERANGE`, an empty name is `ERANGE`, and a
//!   name that is *only* a namespace prefix (`"user."`) is `EINVAL`.
//! - a value larger than 65536 bytes is `E2BIG`; an empty value is a value.
//! - `listxattr(..., NULL, 0)` is the total length of the NUL-separated names,
//!   a buffer one byte short is `ERANGE`, and an inode with none answers 0.
//! - `user.*` lands on a regular file, a directory and a **socket**; on a FIFO,
//!   a device node and a symlink (the `l*` forms) every `set` is `EPERM` and
//!   `get` is `ENODATA`, `list` 0.
//! - a descriptor opened `O_PATH` answers `EBADF`, where a plain read-only one
//!   answers normally.
//! - a filesystem with no store at all answers `EOPNOTSUPP` to everything,
//!   which is what this kernel already answered before the store existed.
//!
//! Two divergences are deliberate and named in `docs/notes/` §3.15: Linux's
//! tmpfs also keeps the `security.*` and `trusted.*` namespaces (the first
//! readable by anyone, the second visible only to root), and this kernel keeps
//! neither - it has no LSM and no per-caller visibility to hide one - so a name
//! in either namespace answers `EOPNOTSUPP` here where Linux's *root* would
//! store it. Each value is capped at Linux's own `XATTR_SIZE_MAX`, and the
//! store's bytes are charged to the filesystem's [`Budget`] - the shape `RamFs`
//! already gives file contents - because an attribute is an allocation on the
//! caller's behalf, and a mount with a `size=` is a promise about everything it
//! keeps. Linux's tmpfs does not count attributes against its `size=` (measured:
//! forty 64 KiB attributes on a mount sized 1 MiB all landed), so this is a
//! named divergence in the direction of the worst-case rule.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

/// Linux's `XATTR_NAME_MAX`: one byte of the name budget is the terminating
/// NUL, so a caller may pass 255 bytes of name and no more.
pub const NAME_MAX: usize = 255;

/// Linux's `XATTR_SIZE_MAX`: the largest single value a filesystem may be
/// asked to hold. A bigger one is `E2BIG`, not a slow failure.
pub const VALUE_MAX: usize = 65536;

/// "Create it, or fail if it is already there."
pub const XATTR_CREATE: u32 = 0x1;
/// "Replace it, or fail if it is not there."
pub const XATTR_REPLACE: u32 = 0x2;

/// Why an attribute operation refused, in the vocabulary the syscall layer
/// turns into an errno. Deliberately not [`crate::vfs::VfsError`]: that enum is
/// a filesystem's answers to *node* operations, and half of these errno numbers
/// (`ERANGE`, `ENODATA`, `E2BIG`) have no node operation to belong to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// ERANGE: the caller's buffer cannot hold what it asked for, or the name
    /// it gave is outside the 255-byte budget.
    Range,
    /// EINVAL: a flags word with a bit Linux does not define, or a name that is
    /// only a namespace prefix.
    Invalid,
    /// EOPNOTSUPP: a namespace this kernel keeps no handler for.
    Unsupported,
    /// EPERM: the inode's kind may not carry `user.*` attributes at all.
    NotPermitted,
    /// ENODATA: nobody set that attribute.
    NoData,
    /// EEXIST: `XATTR_CREATE` named an attribute that is already set.
    Exists,
    /// E2BIG: the value is larger than [`VALUE_MAX`].
    TooBig,
    /// ENOSPC: the filesystem has no room left for the bytes this attribute
    /// would keep. Linux's tmpfs does not count attributes against its `size=`
    /// (measured: forty 64 KiB attributes on a mount sized 1 MiB were all
    /// accepted), so this is this kernel's own bound - a named divergence, and
    /// the reason a mount with room is still a mount whose heap is bounded
    /// (`docs/notes/` §3.20).
    NoSpace,
}

/// The room an attribute's bytes come out of.
///
/// The store lives on the kernel heap, which has no per-mount budget of its
/// own, so the filesystem that keeps the inode supplies one. Without it the
/// number of attributes a caller can set would be unbounded - each `set` is an
/// allocation on the caller's behalf, which is exactly the shape the worst-case
/// rule forbids (`AGENTS.md` §3, rule 4).
pub trait Budget: Send + Sync {
    /// Take `bytes` more room, or refuse. Refusing is the caller's `ENOSPC`.
    fn charge(&self, bytes: u64) -> bool;
    /// Give `bytes` back: a removed or replaced attribute, or an inode whose
    /// last name went away.
    fn release(&self, bytes: u64);
}

/// Which vocabulary the name belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Namespace {
    /// `user.*` - the one namespace this kernel serves.
    User,
    /// Anything else: another namespace's prefix, or no prefix at all.
    Other,
}

/// Check an attribute name the way Linux's generic layer does, and say which
/// namespace it is in.
///
/// The answer for [`Namespace::Other`] is not an error in this function because
/// it is not an error *shape*: it is a name from a vocabulary this kernel keeps
/// no handler for, and every operation on it answers `EOPNOTSUPP` - the same
/// number an unhooked namespace gets on a real Linux (`"system.foo"` and
/// `"foo.bar"` were both measured answering it).
pub fn classify(name: &str) -> Result<Namespace, Error> {
    if name.is_empty() || name.len() > NAME_MAX {
        return Err(Error::Range);
    }
    // A name that is exactly a namespace prefix names no attribute. Measured:
    // `"user."` is EINVAL and `"security."` is EINVAL on the get path.
    for prefix in ["user.", "security.", "trusted.", "system."] {
        if name == prefix {
            return Err(Error::Invalid);
        }
    }
    if name.starts_with("user.") {
        Ok(Namespace::User)
    } else {
        Ok(Namespace::Other)
    }
}

/// The flags word, checked the way Linux's generic layer checks it: the two
/// bits it defines, and nothing else.
///
/// Both bits at once are *allowed* here - see the module header's measurement:
/// Linux applies the existence rules and lets `XATTR_CREATE` win when the
/// attribute is there and `XATTR_REPLACE` when it is not, rather than answering
/// the `EINVAL` the man page describes.
pub fn check_flags(flags: u32) -> Result<(), Error> {
    if flags & !(XATTR_CREATE | XATTR_REPLACE) != 0 {
        Err(Error::Invalid)
    } else {
        Ok(())
    }
}

/// One attribute.
struct Entry {
    /// The full name, prefix included - the store keeps no namespace field
    /// because `classify` has already decided which one this is.
    name: String,
    value: Vec<u8>,
    /// What this entry was charged to the store's budget: the name and the
    /// value, because both are bytes the mount keeps for as long as it does.
    charged: u64,
}

/// One inode's attributes: the store a memory filesystem keeps.
///
/// One `critical_section::Mutex` over the vector is the same exclusion the rest
/// of `RamFs` uses - interrupts off is mutual exclusion on one core - and the
/// borrow never spans a call that could park or allocate, because the whole of
/// a `set` (the one operation that allocates) happens between the checks and
/// the push, with no user memory touched while it is held.
pub struct Vault {
    entries: Mutex<RefCell<Vec<Entry>>>,
    /// The room these bytes come out of, when the filesystem has one.
    budget: Option<Arc<dyn Budget>>,
}

impl Vault {
    /// An empty store. `const` so a filesystem may build one in a static.
    pub const fn new() -> Vault {
        Vault {
            entries: Mutex::new(RefCell::new(Vec::new())),
            budget: None,
        }
    }

    /// An empty store whose bytes are charged to `budget` - the shape every
    /// `RamFs` node uses, so a mount's room covers what its attributes keep.
    pub fn with_budget(budget: Arc<dyn Budget>) -> Vault {
        Vault {
            entries: Mutex::new(RefCell::new(Vec::new())),
            budget: Some(budget),
        }
    }

    /// Take room for one more entry, refusing the operation if the filesystem
    /// has none. `delta` is signed because a replacement may shrink a value
    /// while its name stays charged.
    fn charge(&self, bytes: u64) -> Result<(), Error> {
        match &self.budget {
            None => Ok(()),
            Some(budget) if budget.charge(bytes) => Ok(()),
            Some(_) => Err(Error::NoSpace),
        }
    }

    fn release(&self, bytes: u64) {
        if let Some(budget) = &self.budget {
            budget.release(bytes);
        }
    }

    /// Read one attribute. `buf` is the caller's buffer: `None` is the size
    /// probe `getxattr(name, NULL, 0)`, whose answer is the length and must not
    /// touch anything; `Some` is a fill, and too short is `ERANGE` rather than
    /// a partial copy.
    pub fn get(&self, name: &str, buf: Option<&mut [u8]>) -> Result<usize, Error> {
        critical_section::with(|cs| {
            let entries = self.entries.borrow(cs).borrow();
            let Some(entry) = entries.iter().find(|entry| entry.name == name) else {
                return Err(Error::NoData);
            };
            let want = entry.value.len();
            match buf {
                None => Ok(want),
                // The copy lands inside the section: it writes the caller's own
                // memory, never parks and never allocates, so no other task can
                // be waiting on this lock while it runs.
                Some(buf) if buf.len() < want => Err(Error::Range),
                Some(buf) => {
                    buf[..want].copy_from_slice(&entry.value);
                    Ok(want)
                }
            }
        })
    }

    /// Write one attribute, applying `XATTR_CREATE`/`XATTR_REPLACE`.
    pub fn set(&self, name: &str, value: &[u8], flags: u32) -> Result<(), Error> {
        if value.len() > VALUE_MAX {
            return Err(Error::TooBig);
        }
        check_flags(flags)?;
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            match entries.iter_mut().find(|entry| entry.name == name) {
                Some(entry) => {
                    if flags & XATTR_CREATE != 0 {
                        return Err(Error::Exists);
                    }
                    let want = (name.len() + value.len()) as u64;
                    let had = entry.charged;
                    if want > had {
                        self.charge(want - had)?;
                    } else {
                        self.release(had - want);
                    }
                    entry.value.clear();
                    entry.value.extend_from_slice(value);
                    entry.charged = want;
                    Ok(())
                }
                None => {
                    if flags & XATTR_REPLACE != 0 {
                        return Err(Error::NoData);
                    }
                    let want = (name.len() + value.len()) as u64;
                    self.charge(want)?;
                    entries.push(Entry {
                        name: name.to_string(),
                        value: value.to_vec(),
                        charged: want,
                    });
                    Ok(())
                }
            }
        })
    }

    /// Drop one attribute.
    pub fn remove(&self, name: &str) -> Result<(), Error> {
        critical_section::with(|cs| {
            let mut entries = self.entries.borrow(cs).borrow_mut();
            match entries.iter().position(|entry| entry.name == name) {
                Some(at) => {
                    let entry = entries.remove(at);
                    self.release(entry.charged);
                    Ok(())
                }
                None => Err(Error::NoData),
            }
        })
    }

    /// List every name, NUL-separated, in the shape `listxattr` fills. `None` is
    /// the size probe; a short buffer is `ERANGE`.
    ///
    /// The order is the store's own - Linux documents the order of a list as
    /// unspecified, and no caller may depend on it, so insertion order is as
    /// good as any and costs nothing to keep.
    pub fn list(&self, buf: Option<&mut [u8]>) -> Result<usize, Error> {
        critical_section::with(|cs| {
            let entries = self.entries.borrow(cs).borrow();
            let want: usize = entries.iter().map(|entry| entry.name.len() + 1).sum();
            match buf {
                None => Ok(want),
                Some(buf) if buf.len() < want => Err(Error::Range),
                Some(buf) => {
                    let mut at = 0;
                    for entry in entries.iter() {
                        let end = at + entry.name.len();
                        buf[at..end].copy_from_slice(entry.name.as_bytes());
                        buf[end] = 0;
                        at = end + 1;
                    }
                    Ok(want)
                }
            }
        })
    }
}

impl Drop for Vault {
    /// Give every byte this store still holds back to the filesystem's room. An
    /// inode's store drops with its last name, so a mount that has been filled
    /// by attributes is empty again once the files are gone.
    fn drop(&mut self) {
        let total = critical_section::with(|cs| {
            self.entries
                .borrow(cs)
                .borrow()
                .iter()
                .map(|entry| entry.charged)
                .sum::<u64>()
        });
        self.release(total);
    }
}

impl Default for Vault {
    fn default() -> Vault {
        Vault::new()
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;
    use alloc::vec;
    use core::sync::atomic::{AtomicU64, Ordering};

    /// A room that counts what it was asked for, so a test can see both the
    /// charge and the release - the filesystem's budget in miniature.
    struct Counting {
        free: AtomicU64,
        taken: AtomicU64,
    }

    impl Counting {
        fn new(free: u64) -> Arc<Counting> {
            Arc::new(Counting {
                free: AtomicU64::new(free),
                taken: AtomicU64::new(0),
            })
        }
    }

    impl Budget for Counting {
        fn charge(&self, bytes: u64) -> bool {
            if self.free.load(Ordering::Relaxed) < bytes {
                return false;
            }
            self.free.fetch_sub(bytes, Ordering::Relaxed);
            self.taken.fetch_add(bytes, Ordering::Relaxed);
            true
        }

        fn release(&self, bytes: u64) {
            self.free.fetch_add(bytes, Ordering::Relaxed);
            self.taken.fetch_sub(bytes, Ordering::Relaxed);
        }
    }

    /// The store spends the filesystem's room for the bytes it keeps, and gives
    /// them back when the attribute goes - the door that keeps a caller from
    /// growing the kernel heap without a bound (the worst-case rule).
    #[test]
    fn a_store_spends_the_room_of_the_bytes_it_keeps() {
        let room = Counting::new(64);
        let vault = Vault::with_budget(room.clone());
        vault.set("user.a", b"1234", 0).unwrap(); // name 6 + value 4
        assert_eq!(room.taken.load(Ordering::Relaxed), 10);
        assert_eq!(vault.set("user.b", &[0u8; 32], 0), Ok(()));
        assert_eq!(room.taken.load(Ordering::Relaxed), 10 + 38);
        // Nothing left for one more name and value.
        assert_eq!(vault.set("user.c", &[0u8; 32], 0), Err(Error::NoSpace));
        vault.remove("user.b").unwrap();
        assert_eq!(room.taken.load(Ordering::Relaxed), 10);
        assert_eq!(vault.set("user.c", &[0u8; 32], 0), Ok(()));
    }

    /// A replaced attribute is charged the difference, not the whole value
    /// again: growing it spends room, shrinking it gives room back.
    #[test]
    fn a_replacement_spends_only_the_difference() {
        let room = Counting::new(1024);
        let vault = Vault::with_budget(room.clone());
        vault.set("user.a", b"0123", 0).unwrap(); // 10 bytes
        vault.set("user.a", &[0u8; 100], 0).unwrap(); // 106 bytes
        assert_eq!(room.taken.load(Ordering::Relaxed), 106);
        vault.set("user.a", b"", 0).unwrap(); // 6 bytes: the name only
        assert_eq!(room.taken.load(Ordering::Relaxed), 6);
    }

    /// An inode whose last name goes away takes its attributes' room with it.
    #[test]
    fn a_dropped_store_gives_its_room_back() {
        let room = Counting::new(64);
        {
            let vault = Vault::with_budget(room.clone());
            vault.set("user.a", &[0u8; 20], 0).unwrap();
            assert_eq!(room.taken.load(Ordering::Relaxed), 26);
        }
        assert_eq!(room.taken.load(Ordering::Relaxed), 0);
        assert_eq!(room.free.load(Ordering::Relaxed), 64);
    }

    #[test]
    fn a_value_round_trips_and_the_probe_answers_its_length() {
        let vault = Vault::new();
        vault.set("user.a", b"v1", 0).unwrap();
        assert_eq!(vault.get("user.a", None), Ok(2), "the size probe");
        let mut buf = [0u8; 8];
        assert_eq!(vault.get("user.a", Some(&mut buf)), Ok(2));
        assert_eq!(&buf[..2], b"v1");
    }

    #[test]
    fn a_buffer_one_byte_short_is_erange_not_a_partial_fill() {
        let vault = Vault::new();
        vault.set("user.a", b"v1", 0).unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(vault.get("user.a", Some(&mut buf)), Err(Error::Range));
    }

    #[test]
    fn a_name_nobody_set_is_enodata_on_get_and_remove() {
        let vault = Vault::new();
        assert_eq!(vault.get("user.a", None), Err(Error::NoData));
        assert_eq!(vault.remove("user.a"), Err(Error::NoData));
    }

    #[test]
    fn create_refuses_an_existing_attribute_and_replace_a_missing_one() {
        let vault = Vault::new();
        vault.set("user.a", b"v1", XATTR_CREATE).unwrap();
        assert_eq!(vault.set("user.a", b"v2", XATTR_CREATE), Err(Error::Exists));
        assert_eq!(vault.set("user.b", b"v", XATTR_REPLACE), Err(Error::NoData));
        vault.set("user.a", b"v2", XATTR_REPLACE).unwrap();
        assert_eq!(vault.get("user.a", None), Ok(2));
    }

    /// Linux's measured shape, and the reason `check_flags` does not reject it:
    /// both bits mean "the existence rules apply", so the answer depends on
    /// whether the attribute is there.
    #[test]
    fn both_flag_bits_answer_by_existence_rather_than_einval() {
        let vault = Vault::new();
        assert_eq!(
            vault.set("user.a", b"v", XATTR_CREATE | XATTR_REPLACE),
            Err(Error::NoData)
        );
        vault.set("user.a", b"v", 0).unwrap();
        assert_eq!(
            vault.set("user.a", b"v", XATTR_CREATE | XATTR_REPLACE),
            Err(Error::Exists)
        );
    }

    #[test]
    fn an_undefined_flag_bit_is_einval() {
        assert_eq!(check_flags(0x4), Err(Error::Invalid));
        assert_eq!(check_flags(0x8), Err(Error::Invalid));
        assert_eq!(check_flags(XATTR_CREATE), Ok(()));
        assert_eq!(check_flags(XATTR_REPLACE), Ok(()));
    }

    #[test]
    fn a_value_past_the_size_max_is_e2big() {
        let vault = Vault::new();
        let big = vec![0u8; VALUE_MAX + 1];
        assert_eq!(vault.set("user.big", &big, 0), Err(Error::TooBig));
        let edge = vec![0u8; VALUE_MAX];
        assert_eq!(vault.set("user.edge", &edge, 0), Ok(()));
        assert_eq!(vault.get("user.edge", None), Ok(VALUE_MAX));
    }

    #[test]
    fn an_empty_value_is_a_value() {
        let vault = Vault::new();
        vault.set("user.empty", b"", 0).unwrap();
        assert_eq!(vault.get("user.empty", None), Ok(0));
        let mut nothing: [u8; 0] = [];
        assert_eq!(vault.get("user.empty", Some(&mut nothing)), Ok(0));
    }

    #[test]
    fn the_list_is_nul_separated_with_a_probe_and_an_erange() {
        let vault = Vault::new();
        assert_eq!(vault.list(None), Ok(0), "an empty store lists nothing");
        vault.set("user.a", b"1", 0).unwrap();
        vault.set("user.bb", b"2", 0).unwrap();
        let want = "user.a\0user.bb\0".len();
        assert_eq!(vault.list(None), Ok(want));
        let mut buf = [0u8; 32];
        assert_eq!(vault.list(Some(&mut buf)), Ok(want));
        assert_eq!(&buf[..want], b"user.a\0user.bb\0");
        let mut short = [0u8; 3];
        assert_eq!(vault.list(Some(&mut short)), Err(Error::Range));
    }

    #[test]
    fn remove_takes_one_name_and_leaves_the_rest() {
        let vault = Vault::new();
        vault.set("user.a", b"1", 0).unwrap();
        vault.set("user.b", b"2", 0).unwrap();
        vault.remove("user.a").unwrap();
        assert_eq!(vault.get("user.a", None), Err(Error::NoData));
        assert_eq!(vault.get("user.b", None), Ok(1));
        assert_eq!(vault.list(None), Ok("user.b\0".len()));
    }

    #[test]
    fn a_name_is_checked_against_linuxs_budget_and_prefixes() {
        assert_eq!(classify(""), Err(Error::Range));
        assert_eq!(classify("user."), Err(Error::Invalid));
        assert_eq!(classify("security."), Err(Error::Invalid));
        assert_eq!(classify("trusted."), Err(Error::Invalid));
        assert_eq!(classify("system."), Err(Error::Invalid));
        assert_eq!(classify("user.a"), Ok(Namespace::User));
        assert_eq!(classify("user.255"), Ok(Namespace::User));
        assert_eq!(classify("foo.bar"), Ok(Namespace::Other));
        assert_eq!(classify("security.foo"), Ok(Namespace::Other));
        assert_eq!(classify("trusted.foo"), Ok(Namespace::Other));
        assert_eq!(classify("system.foo"), Ok(Namespace::Other));
        let long = alloc::format!("user.{}", "x".repeat(NAME_MAX - 5));
        assert_eq!(long.len(), NAME_MAX);
        assert_eq!(classify(&long), Ok(Namespace::User));
        let longer = alloc::format!("user.{}", "x".repeat(NAME_MAX - 4));
        assert_eq!(classify(&longer), Err(Error::Range));
    }
}
