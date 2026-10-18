#![no_std]

//! The user namespace: the table an id is translated through at the boundary.
//!
//! A container runtime's first call is `unshare(CLONE_NEWUSER)`, and everything
//! it does afterwards rests on what that call buys: a mapping from the ids its
//! processes *see* to the ids the machine *stores*, and the privilege to set it
//! up without being root already. This is the sixth of `namespaces(7)`'s eight
//! this kernel has built (`docs/notes/06-process-management.md` §6.3 is the
//! ladder and the measurements, taken on this developer's Linux 7.2 before any
//! of this was written).
//!
//! Four facts about the shape drove every line below, and all four are measured
//! rather than recalled:
//!
//! * **A mapping is a translation at the boundary, not a rename.** The kernel
//!   stores one id, in the parent namespace's numbering; every *observation* of
//!   it is mapped through the observing namespace's table. With `0 <- 65534`
//!   written, a file whose stored id is 0 reads `uid=65534` from inside (0 is
//!   not in the table, so it comes back as [`NO_MAP_ID`]) and a file the caller
//!   owns outside reads `uid=0` inside.
//! * **The map starts empty, so the maker lands on the overflow id.** The instant
//!   `unshare(CLONE_NEWUSER)` returns, the caller's `getuid()` reads 65534: its
//!   own stored id has no inside name *yet*. Writing the map is what gives it
//!   one.
//! * **The map may be written once.** A second write, a line naming an id the
//!   writer does not own (unprivileged), and a line wider than one id are all
//!   `EPERM` - the map is a promise made once, and Linux makes it hard to
//!   rewrite because a process's view of "who am I" must not move under it.
//! * **`gid_map` waits for `setgroups=deny`.** An unprivileged writer must write
//!   `deny` to `/proc/<pid>/setgroups` before the gid map is accepted - the
//!   dance `newgidmap` performs, and the reason the supplementary groups are a
//!   door of their own.
//!
//! **What is not here**: the number the namespace is known by (`src/vfs/ns.rs`
//! mints it, from the one pool every kind draws from), the slot on the task that
//! holds one of these, and the readers that ask it to translate (the `getuid`
//! family, `stat`'s four doors, and the unix socket's
//! `SO_PEERCRED`/`SCM_CREDENTIALS`). This is the value and its grammar; those are
//! whose it is and who reads it.

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, Ordering};
use critical_section::Mutex;

/// The id an unmapped id reads back as.
///
/// Linux's `overflowuid`/`overflowgid`, whose compiled default is 65534 and
/// which is what `getuid()` answers the instant a caller has unshared into a
/// namespace with an empty table - measured (`probe-userns.c`: `getuid()` is
/// 65534 between the `unshare` and the first map write).
pub const NO_MAP_ID: u32 = 65534;

/// The most a mapping may hold, which is also Linux's identity map's width: the
/// whole `u32` id space, as `"0 0 4294967295"`.
pub const ID_SPAN: u32 = u32::MAX;

/// One line of a map: a run of ids, given by where it starts on each side and
/// how many ids it covers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MapLine {
    /// The first id as seen *inside* the namespace.
    pub inside: u32,
    /// The first id as the parent namespace numbers it - what the kernel stores.
    pub outside: u32,
    /// How many consecutive ids the run covers. At least one.
    pub len: u32,
}

impl MapLine {
    /// The inside id an outside (stored) id reads as, or `None` when the run
    /// does not cover it.
    fn to_inner(self, outside: u32) -> Option<u32> {
        let within = outside.checked_sub(self.outside)?;
        if within < self.len {
            Some(self.inside.wrapping_add(within))
        } else {
            None
        }
    }

    /// The outside (stored) id an inside id names, or `None` when it names none.
    fn to_outer(self, inside: u32) -> Option<u32> {
        let within = inside.checked_sub(self.inside)?;
        if within < self.len {
            Some(self.outside.wrapping_add(within))
        } else {
            None
        }
    }
}

/// One user namespace: the two maps its members' ids are translated through, the
/// number it is known by, and the two flags the map files turn on.
///
/// The maps are behind the ordinary `critical_section::Mutex<RefCell<_>>`
/// because a namespace is *shared*: every task in it reads the same table, so a
/// write - which happens once, before most of them exist - has to be visible to
/// all of them and not merely to the writer.
pub struct UserNs {
    /// The number this namespace is known by: `user:[<id>]` in
    /// `/proc/<pid>/ns/user`, and what `setns` compares two of them by.
    id: u64,
    /// The namespace this one was made *in*: `None` only for the machine's own,
    /// which is the root of the ladder and has nothing above it.
    ///
    /// The ladder is what a capability question walks (see [`Caller`]): a
    /// namespace is owned by the one its maker stood in at the moment of the
    /// call, never by the one it ends up holding - which is why a user
    /// namespace's parent is the namespace it was *made from* rather than
    /// itself.
    parent: Option<Arc<UserNs>>,
    /// The uid that made this namespace, as the **parent** numbers it.
    ///
    /// Linux keeps it beside the parent for one reason: the owner of a
    /// namespace holds every capability over it and everything below it, whether
    /// or not its own effective set carries the bit - the clause
    /// [`Caller::capable_in`] spells out.
    owner: u32,
    uid: Mutex<RefCell<Vec<MapLine>>>,
    gid: Mutex<RefCell<Vec<MapLine>>>,
    /// Whether `setgroups(2)` is still permitted for the members.
    ///
    /// Starts `true` (the file reads `allow`), because that is Linux's state for
    /// a fresh namespace and what the machine's own answers; an unprivileged
    /// gid-map writer has to turn it off first.
    setgroups_allowed: AtomicBool,
    /// Whether each map has been written. One write is all a map gets.
    uid_written: AtomicBool,
    gid_written: AtomicBool,
}

impl UserNs {
    /// A namespace nobody has written a map into: empty tables, `setgroups`
    /// still allowed.
    ///
    /// Empty rather than a copy of the parent's, because that is what
    /// `unshare(CLONE_NEWUSER)` produces - Linux's `create_user_ns` makes one
    /// with no extents - and it is exactly why the caller reads [`NO_MAP_ID`]
    /// until it writes the map.
    ///
    /// `parent` is the namespace the maker is standing in when it calls, and
    /// `owner` that maker's uid as the parent numbers it: the two facts a
    /// capability question walks up (see [`Caller`]).
    pub fn fresh(id: u64, parent: Arc<UserNs>, owner: u32) -> Arc<UserNs> {
        Arc::new(UserNs {
            id,
            parent: Some(parent),
            owner,
            uid: Mutex::new(RefCell::new(Vec::new())),
            gid: Mutex::new(RefCell::new(Vec::new())),
            setgroups_allowed: AtomicBool::new(true),
            uid_written: AtomicBool::new(false),
            gid_written: AtomicBool::new(false),
        })
    }

    /// The machine's own namespace: the identity map, written from the start.
    ///
    /// `"0 0 4294967295"` is the whole id space mapped to itself, which is what
    /// a real kernel reports for `/proc/self/uid_map` before anything unshares
    /// (measured), so every id translates to itself and the machine's readers
    /// behave exactly as they did before this type existed. Written from the
    /// start for the same reason: a write to the machine's map is `EPERM`,
    /// because there is nothing left for it to say.
    pub fn machine(id: u64) -> Arc<UserNs> {
        let ns = Arc::new(UserNs {
            id,
            // The machine's own: the root of the ladder, made by nobody, so
            // there is no parent to walk up to and no owner to consult.
            parent: None,
            owner: 0,
            uid: Mutex::new(RefCell::new(Vec::new())),
            gid: Mutex::new(RefCell::new(Vec::new())),
            setgroups_allowed: AtomicBool::new(true),
            uid_written: AtomicBool::new(false),
            gid_written: AtomicBool::new(false),
        });
        let identity = MapLine {
            inside: 0,
            outside: 0,
            len: ID_SPAN,
        };
        critical_section::with(|cs| {
            *ns.uid.borrow(cs).borrow_mut() = alloc::vec![identity];
            *ns.gid.borrow(cs).borrow_mut() = alloc::vec![identity];
        });
        ns.uid_written.store(true, Ordering::Relaxed);
        ns.gid_written.store(true, Ordering::Relaxed);
        ns
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// The namespace this one was made in: `None` for the machine's own, which
    /// is the top of the ladder.
    pub fn parent(&self) -> Option<Arc<UserNs>> {
        self.parent.as_ref().map(Arc::clone)
    }

    /// The uid that made this namespace, as the parent numbers it.
    pub fn owner(&self) -> u32 {
        self.owner
    }

    fn uid_snapshot(&self) -> Vec<MapLine> {
        critical_section::with(|cs| self.uid.borrow(cs).borrow().clone())
    }

    fn gid_snapshot(&self) -> Vec<MapLine> {
        critical_section::with(|cs| self.gid.borrow(cs).borrow().clone())
    }

    /// The id a stored (parent-numbered) uid reads back as *here*.
    ///
    /// [`NO_MAP_ID`] when the table does not cover it, which is the whole
    /// content of the first measured row: a namespace with an empty table shows
    /// every stored id as 65534, its own included.
    pub fn uid_to_inner(&self, stored: u32) -> u32 {
        translate(&self.uid_snapshot(), stored, true)
    }

    /// The id a uid *seen here* is stored as: the direction a setuid or a
    /// `chown` needs.
    pub fn uid_to_stored(&self, inner: u32) -> u32 {
        translate(&self.uid_snapshot(), inner, false)
    }

    pub fn gid_to_inner(&self, stored: u32) -> u32 {
        translate(&self.gid_snapshot(), stored, true)
    }

    pub fn gid_to_stored(&self, inner: u32) -> u32 {
        translate(&self.gid_snapshot(), inner, false)
    }

    pub fn setgroups_allowed(&self) -> bool {
        self.setgroups_allowed.load(Ordering::Relaxed)
    }

    /// `/proc/<pid>/setgroups`' two possible contents, each a measured word.
    pub fn setgroups_text(&self) -> &'static str {
        if self.setgroups_allowed() {
            "allow\n"
        } else {
            "deny\n"
        }
    }

    /// `/proc/<pid>/uid_map`'s contents: one ten-wide line per run.
    ///
    /// The field widths are Linux's and are reproduced rather than approximated
    /// (measured: the machine's single line is 33 bytes, `%10u %10u %10u\n`), so
    /// a `diff` against a real kernel's file is a fair comparison.
    pub fn uid_map_text(&self) -> String {
        map_text(&self.uid_snapshot())
    }

    pub fn gid_map_text(&self) -> String {
        map_text(&self.gid_snapshot())
    }

    /// Write `/proc/<pid>/uid_map`.
    ///
    /// `privileged` is whether the writer holds `CAP_SETUID` in the *parent*
    /// namespace - the capability that lifts the one-line rule; `writer` is the
    /// writer's own stored uid, which is the only id an unprivileged one may
    /// name on the outside. The three measured refusals are all `EPERM` and all
    /// here: a second write, a foreign outside id, and a run wider than one.
    pub fn write_uid_map(&self, text: &str, privileged: bool, writer: u32) -> Result<(), isize> {
        self.write_map(&self.uid, &self.uid_written, text, privileged, writer)
    }

    /// Write `/proc/<pid>/gid_map`, which additionally waits for `setgroups=deny`.
    ///
    /// `privileged` is whether the writer holds `CAP_SETGID` in the parent; an
    /// unprivileged writer is refused `EPERM` until `setgroups` reads `deny` -
    /// measured in that order (`probe-userns2.c`: the gid write before the
    /// `deny` is refused, the same write after is accepted).
    pub fn write_gid_map(&self, text: &str, privileged: bool, writer: u32) -> Result<(), isize> {
        if !privileged && self.setgroups_allowed() {
            return Err(mpos_err::EPERM);
        }
        self.write_map(&self.gid, &self.gid_written, text, privileged, writer)
    }

    /// Write `/proc/<pid>/setgroups`: the two words `allow` and `deny`.
    ///
    /// Refused `EPERM` once the gid map is written, which is Linux's rule and
    /// the reason the order is forced: turning `setgroups` back on after a map
    /// exists could grant supplementary groups the map was written without.
    pub fn write_setgroups(&self, text: &str) -> Result<(), isize> {
        if self.gid_written.load(Ordering::Relaxed) {
            return Err(mpos_err::EPERM);
        }
        match text.trim() {
            "allow" => self.setgroups_allowed.store(true, Ordering::Relaxed),
            "deny" => self.setgroups_allowed.store(false, Ordering::Relaxed),
            _ => return Err(mpos_err::EINVAL),
        }
        Ok(())
    }

    fn write_map(
        &self,
        slot: &Mutex<RefCell<Vec<MapLine>>>,
        written: &AtomicBool,
        text: &str,
        privileged: bool,
        writer: u32,
    ) -> Result<(), isize> {
        if written.load(Ordering::Relaxed) {
            return Err(mpos_err::EPERM);
        }
        let lines = parse_map(text).map_err(|_| mpos_err::EINVAL)?;
        if !privileged {
            // One id, self-named, and nothing else: the single line an
            // unprivileged writer is allowed. Linux's `map_write` reduces to the
            // same three conditions for a caller without the capability.
            match lines.as_slice() {
                [one] if one.len == 1 && one.outside == writer => {}
                _ => return Err(mpos_err::EPERM),
            }
        }
        critical_section::with(|cs| *slot.borrow(cs).borrow_mut() = lines);
        written.store(true, Ordering::Relaxed);
        Ok(())
    }
}

/// What a caller brings to a capability question about *another* object.
///
/// Three facts, because the question is not "does the caller hold the bit" -
/// that one needs no namespace at all - but "is the caller privileged over
/// *this* namespace", and Linux answers it by walking from the namespace being
/// asked about up to the caller's own (`cap_capable`). The three are the things
/// that walk reads: where the caller stands, who it is, and what it holds.
pub struct Caller {
    /// The namespace the caller's capability sets are scoped to.
    pub ns: Arc<UserNs>,
    /// The caller's effective uid, **as the machine stores it** - the numbering
    /// [`UserNs::owner`] is kept in, so the two can be compared.
    pub euid: u32,
    /// The caller's effective capability set, held *in* [`Self::ns`].
    pub effective: u64,
}

impl Caller {
    /// `ns_capable(target, cap)`: is this caller privileged over `target`?
    ///
    /// Measured on a current host (`.workbuddy/probe_ns_owner.c`, Linux 7.2.9,
    /// a root caller in each of four places) before any of this was written:
    ///
    /// | the caller | the namespace being asked about | answer |
    /// |---|---|---|
    /// | the machine's root | a mount namespace the machine owns | served |
    /// | inside a **bare** user namespace | the mount namespace it stands in | `EPERM` |
    /// | inside a user namespace | one made **inside** it | served |
    /// | the machine's root | a child user namespace | served |
    /// | inside a user namespace | a **sibling** of it | `EPERM` |
    /// | inside a user namespace | one made inside it | served |
    /// | inside a user namespace | its **parent** | `EPERM` |
    ///
    /// The walk that produces that table, and the reason a "is the caller
    /// root" question cannot:
    ///
    /// 1. the namespace being asked about *is* the caller's - the answer is the
    ///    capability set, and nothing else consults it;
    /// 2. the machine's own is reached without meeting the caller's, so a
    ///    namespace below it is not privileged over it (the `EPERM` in row two
    ///    and the sibling's, and the reason a user-namespace root cannot reach
    ///    back out);
    /// 3. the caller **owns** the next one down - it made that namespace, so it
    ///    holds every capability in it and below it *whether or not its own set
    ///    carries the bit*;
    /// 4. otherwise up one rung, and around again.
    ///
    /// Clause three is the one worth stating twice: it is what makes a
    /// namespace a *grant* rather than a view, and it is why a caller whose
    /// effective set is empty can still join what it made.
    pub fn capable_in(&self, target: &Arc<UserNs>, cap: u32) -> bool {
        if cap > mpos_cred::CAP_LAST_CAP {
            return false;
        }
        let mut node = Arc::clone(target);
        loop {
            if Arc::ptr_eq(&node, &self.ns) {
                return self.effective & (1u64 << cap) != 0;
            }
            let Some(parent) = node.parent() else {
                // The machine's own namespace, reached from below: the walk is
                // over and the caller is not privileged over it.
                return false;
            };
            if Arc::ptr_eq(&parent, &self.ns) && node.owner() == self.euid {
                return true;
            }
            node = parent;
        }
    }
}

/// One direction of the translation, over a table, with [`NO_MAP_ID`] for an id
/// the table does not cover (`inner` selects the direction: true is
/// stored-to-seen, false is seen-to-stored).
fn translate(table: &[MapLine], id: u32, inner: bool) -> u32 {
    for line in table {
        let found = if inner {
            line.to_inner(id)
        } else {
            line.to_outer(id)
        };
        if let Some(mapped) = found {
            return mapped;
        }
    }
    NO_MAP_ID
}

/// The map file's text: one ten-wide line per run, `%10u %10u %10u`.
fn map_text(table: &[MapLine]) -> String {
    let mut out = String::new();
    for line in table {
        let _ = writeln!(
            out,
            "{:>10} {:>10} {:>10}",
            line.inside, line.outside, line.len
        );
    }
    out
}

/// Parse a map file the way Linux's `map_write` does, refusing the syntaxes it
/// refuses.
///
/// Accepted: one or more lines of three whitespace-separated `u32`s, leading and
/// surrounding whitespace free. Refused (`EINVAL`): an empty text, a line that
/// is not three numbers, a zero length, more than five runs (Linux's
/// `UID_GID_MAP_MAX_EXTENTS`), and two runs that overlap on either side - an
/// ambiguous table is not a table, and a reader that had to pick would be
/// guessing.
fn parse_map(text: &str) -> Result<Vec<MapLine>, ()> {
    /// Linux's ceiling on a map's run count.
    const MAX_EXTENTS: usize = 5;

    let mut lines = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let inside = words.next().ok_or(())?.parse::<u32>().map_err(|_| ())?;
        let outside = words.next().ok_or(())?.parse::<u32>().map_err(|_| ())?;
        let len = words.next().ok_or(())?.parse::<u32>().map_err(|_| ())?;
        if words.next().is_some() || len == 0 {
            return Err(());
        }
        lines.push(MapLine {
            inside,
            outside,
            len,
        });
    }
    if lines.is_empty() || lines.len() > MAX_EXTENTS {
        return Err(());
    }
    for (i, a) in lines.iter().enumerate() {
        for b in &lines[i + 1..] {
            if overlaps(a.inside, a.len, b.inside, b.len)
                || overlaps(a.outside, a.len, b.outside, b.len)
            {
                return Err(());
            }
        }
    }
    Ok(lines)
}

/// Whether two runs that start at `a`/`b` and cover `alen`/`blen` share an id.
fn overlaps(a: u32, alen: u32, b: u32, blen: u32) -> bool {
    // Widened to u64 so a run ending at the top of the id space does not wrap.
    let (a, alen) = (u64::from(a), u64::from(alen));
    let (b, blen) = (u64::from(b), u64::from(blen));
    a < b + blen && b < a + alen
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A namespace made inside the machine's own by root: the fixture every row
    /// below wants, and the shape `unshare(CLONE_NEWUSER)` produces for a caller
    /// that was root outside it.
    fn child(id: u64) -> Arc<UserNs> {
        UserNs::fresh(id, UserNs::machine(0), 0)
    }

    /// The text is what a real kernel prints, pinned against the measurement
    /// rather than against this module's own formatter: `/proc/self/uid_map` on
    /// this developer's Linux reads exactly this, 33 bytes with the newline.
    #[test]
    fn the_map_text_is_what_a_real_kernel_prints() {
        let machine = UserNs::machine(1);
        assert_eq!(machine.uid_map_text(), "         0          0 4294967295\n");
        assert_eq!(machine.uid_map_text().len(), 33);
        assert_eq!(machine.gid_map_text(), "         0          0 4294967295\n");
        assert_eq!(machine.setgroups_text(), "allow\n");
        // And a fresh one reads empty, which is the state the caller of an
        // unshare is in.
        assert_eq!(child(2).uid_map_text(), "");
    }

    /// The machine's identity map translates every id to itself, both ways - the
    /// property that keeps this type from changing what a task that never
    /// unshared sees.
    #[test]
    fn the_machine_translates_every_id_to_itself() {
        let machine = UserNs::machine(1);
        for id in [0u32, 1, 1000, 65534, u32::MAX - 1] {
            assert_eq!(machine.uid_to_inner(id), id, "{id}");
            assert_eq!(machine.uid_to_stored(id), id, "{id}");
        }
        // ...and the top id is the one it does *not*: the identity line is
        // `0 0 4294967295`, a run of 4294967295 ids starting at 0, so the last
        // `u32` is outside it and reads as the overflow id. That is a real
        // kernel's answer too - its map has the same width - and it is the
        // reason `(uid_t)-1` is a value callers are told not to use.
        assert_eq!(machine.uid_to_inner(u32::MAX), NO_MAP_ID);
    }

    /// An empty table shows everything as the overflow id - which is where a
    /// caller lands the instant it unshares, and the first row of the measured
    /// table.
    #[test]
    fn an_empty_table_shows_the_overflow_id() {
        let ns = child(1);
        assert_eq!(ns.uid_to_inner(0), NO_MAP_ID);
        assert_eq!(ns.uid_to_inner(65534), NO_MAP_ID);
        assert_eq!(ns.uid_to_stored(0), NO_MAP_ID);
    }

    /// The measured round trip: with `0 <- 65534` written, a file owned by
    /// stored id 0 reads `65534` inside (it is not in the table), and the
    /// caller's own stored id 65534 reads `0` inside. The two directions are
    /// not each other's inverse, which is the whole point of the first measured
    /// row - a mapping is a translation at the boundary.
    #[test]
    fn a_mapping_translates_at_the_boundary_and_not_as_a_rename() {
        let ns = child(1);
        assert_eq!(ns.write_uid_map("0 65534 1\n", true, 0), Ok(()));
        assert_eq!(ns.uid_map_text(), "         0      65534          1\n");
        assert_eq!(ns.uid_to_inner(65534), 0);
        assert_eq!(
            ns.uid_to_inner(0),
            NO_MAP_ID,
            "stored 0 is not in the table"
        );
        assert_eq!(ns.uid_to_stored(0), 65534);
        assert_eq!(ns.uid_to_stored(5), NO_MAP_ID, "inside 5 names nothing");
    }

    /// The unprivileged rule, all three refusals measured: one id, self-named,
    /// once. Each is `EPERM` and each leaves the table unchanged.
    #[test]
    fn an_unprivileged_writer_may_name_only_itself_once() {
        let ns = child(1);
        // A foreign outside id.
        assert_eq!(
            ns.write_uid_map("0 0 1\n", false, 65534),
            Err(mpos_err::EPERM)
        );
        // Wider than one id.
        assert_eq!(
            ns.write_uid_map("0 65534 2\n", false, 65534),
            Err(mpos_err::EPERM)
        );
        assert_eq!(ns.uid_map_text(), "", "a refused write changed the table");
        // Its own id, one wide: accepted.
        assert_eq!(ns.write_uid_map("0 65534 1\n", false, 65534), Ok(()));
        // And then never again.
        assert_eq!(
            ns.write_uid_map("0 65534 1\n", false, 65534),
            Err(mpos_err::EPERM)
        );
        assert_eq!(ns.uid_map_text(), "         0      65534          1\n");
    }

    /// A privileged writer may map freely - any outside id, more than one run -
    /// but still only once.
    #[test]
    fn a_privileged_writer_may_map_freely_but_still_once() {
        let ns = child(1);
        assert_eq!(ns.write_uid_map("0 1000 1\n1 2000 10\n", true, 0), Ok(()));
        assert_eq!(ns.uid_to_inner(1000), 0);
        assert_eq!(ns.uid_to_inner(2000), 1);
        assert_eq!(ns.uid_to_inner(2009), 10);
        assert_eq!(ns.uid_to_inner(1), NO_MAP_ID, "outside 1 is in no run");
        assert_eq!(ns.write_uid_map("0 0 1\n", true, 0), Err(mpos_err::EPERM));
    }

    /// The gid map waits for `setgroups=deny` - measured in that order.
    #[test]
    fn the_gid_map_waits_for_setgroups_deny() {
        let ns = child(1);
        assert_eq!(
            ns.write_gid_map("0 65534 1\n", false, 65534),
            Err(mpos_err::EPERM),
            "an unprivileged gid write with setgroups still allowed"
        );
        assert_eq!(ns.write_setgroups("deny\n"), Ok(()));
        assert_eq!(ns.setgroups_text(), "deny\n");
        assert_eq!(ns.write_gid_map("0 65534 1\n", false, 65534), Ok(()));
        // Turning it back on once a gid map exists is refused.
        assert_eq!(ns.write_setgroups("allow\n"), Err(mpos_err::EPERM));
        // A privileged writer does not need the dance.
        let ns = child(2);
        assert_eq!(ns.write_gid_map("0 0 1\n", true, 0), Ok(()));
    }

    /// The grammar's refusals, and the guarantee that a refused write changes
    /// nothing.
    #[test]
    fn the_grammar_refuses_what_a_real_kernel_refuses() {
        for text in [
            "",               // nothing at all
            "0 0\n",          // a missing field
            "0 0 0\n",        // a zero-length run
            "0 0 1 extra\n",  // a fourth field
            "x 0 1\n",        // an unparsable field
            "0 0 1\n0 0 1\n", // the same run twice: overlapping
            "0 0 2\n1 5 1\n", // inside runs overlap
        ] {
            let ns = child(1);
            assert_eq!(
                ns.write_uid_map(text, true, 0),
                Err(mpos_err::EINVAL),
                "{text:?}"
            );
            assert_eq!(ns.uid_map_text(), "", "{text:?} changed something");
        }
    }

    /// A wide run reaches the top of the id space without wrapping, and the
    /// machine's own map is the case that matters.
    #[test]
    fn a_run_that_reaches_the_top_of_the_id_space_does_not_wrap() {
        let ns = child(1);
        assert_eq!(ns.write_uid_map("0 0 4294967295\n", true, 0), Ok(()));
        assert_eq!(ns.uid_to_inner(0), 0);
        assert_eq!(ns.uid_to_inner(1), 1);
        assert_eq!(ns.uid_to_inner(65534), 65534);
    }

    /// The measured walk, one row per clause: a caller in the machine's own
    /// namespace is asked about its own set, and a caller inside a child is not
    /// privileged over the machine's - the `EPERM` a user namespace buys by
    /// being below the thing it wants.
    #[test]
    fn a_namespace_below_the_machines_is_not_privileged_over_it() {
        let machine = UserNs::machine(1);
        let mine = UserNs::fresh(2, Arc::clone(&machine), 0);
        let root_outside = Caller {
            ns: Arc::clone(&machine),
            euid: 0,
            effective: 1 << mpos_cred::CAP_SYS_ADMIN,
        };
        // Its own: the capability set decides, and it holds the bit.
        assert!(root_outside.capable_in(&machine, mpos_cred::CAP_SYS_ADMIN));
        // ...and one made inside it, which is row one of the measured table.
        assert!(root_outside.capable_in(&mine, mpos_cred::CAP_SYS_ADMIN));
        // Without the bit, its *own* namespace refuses it - there the set is
        // the whole answer - but the one it made does not: owning a namespace
        // is the capability, and the set is not consulted (the clause the next
        // test pins).
        let dropped = Caller {
            effective: 0,
            ..root_outside
        };
        assert!(!dropped.capable_in(&machine, mpos_cred::CAP_SYS_ADMIN));
        assert!(dropped.capable_in(&mine, mpos_cred::CAP_SYS_ADMIN));

        // Inside the child, holding the whole set in it: its own namespace is
        // served, the machine's is not (row seven of the table - `EPERM`), and
        // neither is a *sibling* of it (row five), which is the same walk
        // stopping one rung later.
        let inside = Caller {
            ns: Arc::clone(&mine),
            euid: 0,
            effective: !0,
        };
        assert!(inside.capable_in(&mine, mpos_cred::CAP_SYS_ADMIN));
        assert!(!inside.capable_in(&machine, mpos_cred::CAP_SYS_ADMIN));
        let sibling = child(3);
        assert!(!inside.capable_in(&sibling, mpos_cred::CAP_SYS_ADMIN));
    }

    /// The clause that makes a namespace a grant: the owner of one holds every
    /// capability in it and below it **whether or not its own set carries the
    /// bit** - so a caller with an empty effective set is served a namespace it
    /// made, and refused one it did not.
    #[test]
    fn owning_a_namespace_is_the_capability_regardless_of_the_set() {
        let machine = UserNs::machine(1);
        let grandchild = child(4);
        let mine = UserNs::fresh(5, machine.clone(), 0);
        let grandchild_of_mine = UserNs::fresh(6, mine.clone(), 0);
        // Empty-handed, and made them both: served for each.
        let owner = Caller {
            ns: Arc::clone(&machine),
            euid: 0,
            effective: 0,
        };
        assert!(owner.capable_in(&mine, mpos_cred::CAP_SYS_ADMIN));
        assert!(owner.capable_in(&grandchild_of_mine, mpos_cred::CAP_SYS_ADMIN));
        // A namespace made by *another* uid is not owned, so it comes down to
        // the set - and that one is empty.
        let theirs = UserNs::fresh(7, Arc::clone(&machine), 1000);
        assert!(!owner.capable_in(&theirs, mpos_cred::CAP_SYS_ADMIN));
        // Nor is one that is merely somewhere else in the tree.
        assert!(!owner.capable_in(&grandchild, mpos_cred::CAP_SYS_ADMIN));
        // A caller the owner's clause does not describe still needs the bit:
        // uid 1000, empty-handed, is refused its own child made by root.
        let other = Caller {
            ns: Arc::clone(&machine),
            euid: 1000,
            effective: 0,
        };
        assert!(!other.capable_in(&mine, mpos_cred::CAP_SYS_ADMIN));
    }

    /// A bit no capability has is not held anywhere, whatever the walk says.
    #[test]
    fn a_capability_that_does_not_exist_is_not_held() {
        let machine = UserNs::machine(1);
        let caller = Caller {
            ns: Arc::clone(&machine),
            euid: 0,
            effective: !0,
        };
        assert!(caller.capable_in(&machine, mpos_cred::CAP_SYS_ADMIN));
        assert!(!caller.capable_in(&machine, mpos_cred::CAP_LAST_CAP + 1));
        assert!(!caller.capable_in(&machine, 999));
    }
}
