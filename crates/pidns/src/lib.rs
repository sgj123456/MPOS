#![no_std]

//! The PID namespace: what a task is *called* inside a namespace.
//!
//! Every other kind of namespace isolates a thing - two names, a mount table,
//! a network stack. This one isolates a **number**. A task that unshares
//! `CLONE_NEWPID` hands its children a namespace in which the first of them is
//! pid 1, and from that point on the number a program reads out of `getpid(2)`,
//! the number it writes into `kill(2)`, and the name `/proc/<n>` resolves to are
//! all answered in *the namespace the asking task is in* rather than globally.
//! The process itself never moves: Linux's `unshare(CLONE_NEWPID)` renumbers the
//! caller's future children and not the caller, whose own `getpid` is what it
//! always was.
//!
//! That one sentence is why this object exists at all. A pid here *is* the task
//! table slot (`sched::policy::TaskId`), and a slot number is both global and
//! reused, so a namespace cannot be spelled "the number, plus an offset": the
//! same task needs a different number per namespace, and the same number needs
//! to mean different tasks in two namespaces. So the mapping is a table, and
//! this crate is the table.
//!
//! # The chain, and why the init namespace is the identity map
//!
//! A task is a member of every namespace from the root down to its own: creating
//! it registers it at each level, with a number of that level's choosing. The
//! root level is special and it is the load-bearing decision in this file - **it
//! has no table at all**, because its numbers *are* the slot numbers. That is not
//! an optimisation, it is what keeps a machine that never unshares byte-for-byte
//! what it was before this crate existed: every existing pid, every `/proc/<n>`
//! look-up and every `/proc/<n>/ns/pid` answer is unchanged, and the whole
//! mechanism is only reachable through a `unshare`/`setns`/`clone(CLONE_NEWPID)`
//! that asks for it. It also happens to be Linux's own shape: the init
//! namespace's numbers are the global ones.
//!
//! # Numbers are never reused, and that is deliberate
//!
//! Linux's answer to "who is this number" is a refcounted `struct pid` whose
//! lifetime is its own, which is what makes a pid a promise: a number that
//! named a process cannot later name an unrelated one. This crate does not
//! build that object. What it does instead is the conservative half - a
//! namespace's allocator only ever moves forward, so a number that has been
//! released is **spent**, and a caller that forgets to release one leaks a
//! number rather than handing a stale promise to a stranger. Two consequences
//! are named rather than hidden:
//!
//! * the *init* namespace still numbers by slot, so slot reuse there is exactly
//!   what `docs/notes/` §7.1 records as an open problem, unmoved by this crate;
//! * `next` is a `u32` and does not wrap - a namespace that forks four billion
//!   times is not a case this kernel models, and saying so is cheaper than
//!   pretending the counter is exhaustive.
//!
//! Linux's allocator is cyclic and skips live numbers (`idr_alloc_cyclic`), so
//! the forward-only rule matches it for every namespace that does not wrap.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};
use critical_section::Mutex;

/// One PID namespace: a two-way map between task slots and the numbers this
/// namespace knows them by.
///
/// Two maps rather than one, because both directions are asked and neither is
/// derivable cheaply: `number_of(slot)` is what `getpid` and a `/proc/<pid>`
/// entry for a known process answer, and `task_of(number)` is what `kill`,
/// `wait4` and a `/proc/<n>` path segment ask. The root holds neither - see the
/// header - and answers both by identity.
pub struct PidNs {
    /// The number this namespace is known by: `pid:[<id>]` in
    /// `/proc/<pid>/ns/pid`, and what `setns` compares two of them by. Given
    /// rather than minted here, because the pool is one pool for every kind
    /// (`crate::ns::mint`), exactly as `Uts`'s is.
    ns_id: u64,
    /// The namespace this one was made inside, `None` for the init namespace.
    /// It is the whole of "a task is a member of its ancestors": a number is
    /// allocated at every level of this chain, which is what lets a parent
    /// address a child that lives in a deeper namespace than its own.
    parent: Option<Arc<PidNs>>,
    /// Whether the namespace is **sealed**: its init has exited, and nothing may
    /// be created in it again.
    ///
    /// Linux's rule, measured: a task whose children go into a namespace whose
    /// init has exited answers `ENOMEM` to `fork`
    /// (`.workbuddy/probe_fork_after_init.c`; `docs/notes/` §6.3), and the state
    /// is what `zap_pid_ns_processes` leaves behind - the namespace's child
    /// reaper is gone, so there is nothing left to reap a new init or to speak
    /// for the namespace at all. A pid namespace is therefore a *one-shot*
    /// thing, which is also why its number 1 can never be handed out again: the
    /// cursor only moves forward, and this flag says why that is not merely a
    /// property of the counter.
    sealed: AtomicBool,
    numbers: Mutex<RefCell<Numbers>>,
}

/// The maps and the cursor, behind one lock: they are only ever read or written
/// together, and a number allocated without its reverse entry would be a number
/// nobody can resolve.
struct Numbers {
    /// The next number to hand out. Only ever moves forward.
    next: u32,
    /// Number (this namespace's) -> task slot.
    by_number: BTreeMap<u32, usize>,
    /// Task slot -> number (this namespace's).
    by_slot: BTreeMap<usize, u32>,
}

impl Default for Numbers {
    fn default() -> Self {
        // Linux's first number in a fresh namespace is 1, because 1 is the
        // namespace's init and `alloc_pid` starts from the low end.
        Numbers {
            next: 1,
            by_number: BTreeMap::new(),
            by_slot: BTreeMap::new(),
        }
    }
}

impl PidNs {
    /// The init namespace: the one a boot starts in, whose numbers are the task
    /// slots themselves.
    ///
    /// It stores nothing, so `number_of`/`task_of` are the identity and every
    /// task created here is numbered exactly as it was before namespaces
    /// existed. A machine that never calls `unshare(CLONE_NEWPID)` cannot
    /// observe this crate at all.
    pub fn root(ns_id: u64) -> Arc<PidNs> {
        Arc::new(PidNs {
            ns_id,
            parent: None,
            sealed: AtomicBool::new(false),
            numbers: Mutex::new(RefCell::new(Numbers::default())),
        })
    }

    /// A fresh namespace nested inside `parent` - what `unshare(CLONE_NEWPID)`
    /// gives the caller for its *children*, and what `clone(CLONE_NEWPID)`
    /// gives the child itself.
    ///
    /// It starts empty, which is the difference from every other namespace kind
    /// this kernel has: `unshare(CLONE_NEWUTS)` hands over a *copy* of the names
    /// (`copy_utsname`), and a mount namespace a copy of the table
    /// (`copy_mnt_ns`), because there is content to inherit. A PID namespace has
    /// no content - it is an empty numbering scheme, and the first task
    /// registered in it is 1.
    pub fn child(parent: &Arc<PidNs>, ns_id: u64) -> Arc<PidNs> {
        Arc::new(PidNs {
            ns_id,
            parent: Some(parent.clone()),
            sealed: AtomicBool::new(false),
            numbers: Mutex::new(RefCell::new(Numbers::default())),
        })
    }

    /// The number this namespace is known by, as `/proc/<pid>/ns/pid` prints it.
    pub fn ns_id(&self) -> u64 {
        self.ns_id
    }

    /// Seal the namespace: its init has exited, so nothing may be created in it
    /// again. Called once, by the task that *is* the init as it dies.
    pub fn seal(&self) {
        self.sealed.store(true, Ordering::Relaxed);
    }

    /// Whether the namespace is sealed - the question a task asks about the
    /// namespace its next child would be born into, and the one that answers
    /// `ENOMEM` when the answer is yes.
    pub fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::Relaxed)
    }

    /// Whether this is an init namespace - the level whose numbers are the
    /// slots. Nothing outside needs to ask; it is here for the tests that pin
    /// the identity property.
    pub fn is_root(&self) -> bool {
        self.parent.is_none()
    }

    /// Register `slot` in this namespace and every namespace above it,
    /// answering the number this one chose.
    ///
    /// Idempotent per slot at every level, because the one caller that can have
    /// registered a task twice is a `setns` followed by a `fork`, and a second
    /// number for one task in one namespace would make `task_of` answer
    /// whichever of the two was written last.
    pub fn allocate(&self, slot: usize) -> u32 {
        let mut mine = 0;
        for (level, ns) in self.chain().iter().enumerate() {
            let number = ns.insert(slot);
            if level == 0 {
                mine = number;
            }
        }
        mine
    }

    /// Forget `slot` in this namespace and every namespace above it, which is
    /// what reaping a task does. The number stays spent (see the header).
    pub fn release(&self, slot: usize) {
        for ns in self.chain() {
            ns.forget(slot);
        }
    }

    /// The number this namespace knows `slot` by, if it is a member.
    ///
    /// `None` means "not in this namespace" - the answer that makes
    /// `getppid(2)` a 0 for a task whose parent stayed outside, and that stops
    /// `kill` from acting on a number it does not have.
    pub fn number_of(&self, slot: usize) -> Option<u32> {
        if self.is_root() {
            return Some(slot as u32);
        }
        critical_section::with(|cs| self.numbers.borrow(cs).borrow().by_slot.get(&slot).copied())
    }

    /// The task this namespace knows `number` as, if it has one.
    pub fn task_of(&self, number: u32) -> Option<usize> {
        if self.is_root() {
            return Some(number as usize);
        }
        critical_section::with(|cs| {
            self.numbers
                .borrow(cs)
                .borrow()
                .by_number
                .get(&number)
                .copied()
        })
    }

    /// Whether `slot` is this namespace's **init** - the task it knows by the
    /// number 1.
    ///
    /// The one task a namespace has rules of its own for, and both are
    /// measured on the host rather than read off a page
    /// (`.workbuddy/probe_pid1.c`, `probe_pid1_handler.c`; `docs/notes/` §6.3):
    /// signals sent to it by a member of its own namespace are discarded while
    /// their disposition is the default one - a signal it has installed a
    /// handler for is delivered like any other - and its own death takes the
    /// whole namespace with it. Both callers here ask the same question, and
    /// the answer is this: the number 1, which is why the number matters.
    ///
    /// The root namespace answers `true` for slot 1 too, and deliberately: the
    /// machine's init is the global init, and Linux protects it by the same
    /// rule (`is_global_init` in `sig_task_ignored`). What it does *not* get is
    /// the second one - nothing zaps the machine because its init died; the
    /// machine stops, which is `exit`'s own decision and not this crate's.
    pub fn is_init(&self, slot: usize) -> bool {
        self.number_of(slot) == Some(1)
    }

    /// Whether this namespace is a **strict ancestor** of `other`.
    ///
    /// The half of the init rule that decides who may force a signal through
    /// it: a sender in an ancestor namespace delivers SIGKILL/SIGSTOP where a
    /// sibling inside the namespace does not, and no other default signal
    /// either way (measured: a SIGKILL from outside killed the init, a SIGTERM
    /// from the same outside namespace was still ignored).
    pub fn is_ancestor_of(&self, other: &PidNs) -> bool {
        let mut here = other.parent.clone();
        while let Some(ns) = here {
            if ns.ns_id == self.ns_id {
                return true;
            }
            here = ns.parent.clone();
        }
        false
    }

    /// Whether `other` is this namespace or one nested inside it: the set of
    /// tasks this namespace speaks for, and the set its init's death takes with
    /// it.
    pub fn covers(&self, other: &PidNs) -> bool {
        self.ns_id == other.ns_id || self.is_ancestor_of(other)
    }

    /// The numbers `slot` is known by, **outermost first**: the numbers its
    /// ancestors' namespaces give it, then its own.
    ///
    /// This is the shape `/proc/<pid>/status`'s `NSpid` carries - Linux lists
    /// the task "in each of the PID namespaces of which [it] is a member",
    /// starting with the one the procfs was mounted in and going inward - and
    /// giving the *whole chain* rather than the reader's one number is the part
    /// that field's first cut here got wrong (measured: a task one namespace in
    /// reads `NSpid:\t43877\t2`, one two levels in `NSpid:\t43878\t3\t1`, and a
    /// reader outside reads the same line). A slot this namespace does not know
    /// is left out rather than guessed at.
    pub fn numbers(&self, slot: usize) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .chain()
            .iter()
            .filter_map(|ns| ns.number_of(slot))
            .collect();
        out.reverse();
        out
    }

    /// Each level of the chain with the number it gives `slot`:
    /// `(namespace id, number)` pairs, outermost first.
    ///
    /// [`numbers`](Self::numbers) is the same walk without the identities, and
    /// that is all a *live* task needs - the reader's own namespace asks for one
    /// level. A **record** needs the identities: the slot is gone by the time a
    /// `wait` reads it, and the waiter may stand at any level of the child's
    /// chain, so the number has to be matched to the namespace that gave it.
    pub fn numbered_chain(&self, slot: usize) -> Vec<(u64, u32)> {
        let mut out: Vec<(u64, u32)> = self
            .chain()
            .iter()
            .filter_map(|ns| ns.number_of(slot).map(|number| (ns.ns_id(), number)))
            .collect();
        out.reverse();
        out
    }

    /// This namespace, then its parent, and so on to the root.
    fn chain(&self) -> Vec<&PidNs> {
        let mut out = Vec::new();
        let mut here = Some(self);
        while let Some(ns) = here {
            out.push(ns);
            here = ns.parent.as_deref();
        }
        out
    }

    /// One level's half of [`PidNs::allocate`]: identity at the root, a fresh
    /// number anywhere else.
    fn insert(&self, slot: usize) -> u32 {
        if self.is_root() {
            return slot as u32;
        }
        critical_section::with(|cs| {
            let mut numbers = self.numbers.borrow(cs).borrow_mut();
            if let Some(&already) = numbers.by_slot.get(&slot) {
                return already;
            }
            let number = numbers.next;
            numbers.next += 1;
            numbers.by_number.insert(number, slot);
            numbers.by_slot.insert(slot, number);
            number
        })
    }

    /// One level's half of [`PidNs::release`].
    fn forget(&self, slot: usize) {
        if self.is_root() {
            return;
        }
        critical_section::with(|cs| {
            let mut numbers = self.numbers.borrow(cs).borrow_mut();
            if let Some(number) = numbers.by_slot.remove(&slot) {
                numbers.by_number.remove(&number);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole design rests on: a machine that never unshares
    /// numbers its tasks by slot, so nothing observable changes for it - and
    /// the identity is a property of the object rather than a special case in
    /// every caller.
    #[test]
    fn the_init_namespace_numbers_by_slot() {
        let root = PidNs::root(1);
        assert!(root.is_root());
        assert_eq!(root.number_of(7), Some(7));
        assert_eq!(root.task_of(7), Some(7));
        // Even a task nobody registered, because the root has nothing to
        // register: whether the slot is *live* is the task table's question.
        assert_eq!(root.task_of(63), Some(63));
    }

    /// `unshare(CLONE_NEWPID)`'s promise, in one assertion: the first task in a
    /// fresh namespace is pid 1 to itself.
    #[test]
    fn the_first_task_in_a_child_namespace_is_one() {
        let root = PidNs::root(1);
        let inner = PidNs::child(&root, 2);
        assert!(!inner.is_root());
        assert_eq!(inner.allocate(12), 1);
        assert_eq!(inner.number_of(12), Some(1));
        assert_eq!(inner.task_of(1), Some(12));
        // And a number nobody has is nobody: this is the answer `kill(2)` in
        // the new namespace must give for a slot that was never registered
        // there, rather than reaching the task that owns that slot globally.
        assert_eq!(inner.task_of(3), None);
        assert_eq!(inner.number_of(11), None);
    }

    /// A task is a member of every level: created in a child namespace, it is
    /// also a member of the root - which is what lets its parent, outside, name
    /// it, wait for it and see it in `/proc`.
    #[test]
    fn a_task_is_registered_at_every_level_with_that_levels_number() {
        let root = PidNs::root(1);
        let outer = PidNs::child(&root, 2);
        let inner = PidNs::child(&outer, 3);

        // Created *in* `inner` (its creator had unshared twice), numbered by
        // each level on the way up.
        let number = inner.allocate(12);
        assert_eq!(number, 1, "the innermost level must number it 1");
        assert_eq!(inner.number_of(12), Some(1));
        assert_eq!(
            outer.number_of(12),
            Some(1),
            "the outer level numbers it too"
        );
        assert_eq!(root.number_of(12), Some(12), "and the root keeps its slot");
        // From each level the *same* task is named, which is the whole point.
        for (ns, n) in [(&inner, 1u32), (&outer, 1), (&root, 12)] {
            assert_eq!(ns.task_of(n), Some(12));
        }
    }

    /// The chain with identities, which is what an exit *record* carries: the
    /// number at each level, each beside the namespace that gave it, outermost
    /// first. `numbers()` is the same walk without the identities, and a
    /// `wait` needs them - by the time it reads a death the slot is gone, and
    /// the waiter may stand at any level of the child's chain.
    #[test]
    fn the_numbered_chain_pairs_each_number_with_its_namespace() {
        let root = PidNs::root(11);
        let outer = PidNs::child(&root, 12);
        let inner = PidNs::child(&outer, 13);
        inner.allocate(20);
        assert_eq!(
            inner.numbered_chain(20),
            alloc::vec![(11, 20), (12, 1), (13, 1)],
            "outermost first, each number beside its namespace's identity"
        );
        // A slot the nested levels never registered still has the *root's*
        // number, because the init namespace numbers by slot and stores
        // nothing: its level is always there, and it is the only level that is.
        assert_eq!(inner.numbered_chain(21), alloc::vec![(11, 21)]);
        // The numbers alone are the same walk, in the same order.
        assert_eq!(inner.numbers(20), alloc::vec![20, 1, 1]);
    }

    /// Two tasks in one namespace, and two namespaces over one task set: the
    /// numbers are per namespace and cannot collide across levels.
    #[test]
    fn numbers_are_per_namespace() {
        let root = PidNs::root(1);
        let a = PidNs::child(&root, 2);
        let b = PidNs::child(&root, 3);
        assert_eq!(a.allocate(9), 1);
        assert_eq!(b.allocate(10), 1, "a sibling namespace starts at 1 again");
        assert_eq!(a.task_of(1), Some(9));
        assert_eq!(b.task_of(1), Some(10));
        assert_eq!(a.allocate(11), 2, "the next number follows the first");
    }

    /// The release rule, and the deliberate gap behind it: a number that has
    /// been released is *spent* rather than reused, so a stale promise cannot
    /// land on an unrelated task - at the price of a counter that only moves
    /// forward.
    #[test]
    fn a_released_number_is_spent_rather_than_reused() {
        let root = PidNs::root(1);
        let inner = PidNs::child(&root, 2);
        assert_eq!(inner.allocate(5), 1);
        inner.release(5);
        assert_eq!(inner.number_of(5), None);
        assert_eq!(inner.task_of(1), None);
        assert_eq!(
            inner.allocate(6),
            2,
            "the freed number was handed out again"
        );
    }

    /// Registering twice is registering once: `setns` into a namespace the task
    /// is already a member of, then a `fork`, must not leave two numbers
    /// pointing at one slot.
    #[test]
    fn allocating_the_same_slot_twice_answers_the_same_number() {
        let root = PidNs::root(1);
        let inner = PidNs::child(&root, 2);
        assert_eq!(inner.allocate(4), 1);
        assert_eq!(inner.allocate(4), 1);
        assert_eq!(inner.allocate(5), 2);
    }

    /// A released task stops being visible at every level, not only at the one
    /// it was released in - the property that keeps a parent from naming a
    /// child that is gone.
    #[test]
    fn release_reaches_the_whole_chain() {
        let root = PidNs::root(1);
        let outer = PidNs::child(&root, 2);
        let inner = PidNs::child(&outer, 3);
        let number = inner.allocate(8);
        assert_eq!((number, outer.number_of(8)), (1, Some(1)));
        inner.release(8);
        assert_eq!(inner.number_of(8), None);
        assert_eq!(outer.number_of(8), None, "the outer level still names it");
        assert_eq!(root.number_of(8), Some(8), "the root's identity map");
    }

    /// The init is the task this namespace knows as 1 - not the first slot
    /// registered, and not the lowest slot: a namespace whose first task has
    /// been released has an init again at the next number 1 (the counter only
    /// moves forward, so that is a *different* number-1 in a fresh namespace,
    /// and answering by slot would name the wrong task).
    #[test]
    fn the_init_is_the_task_numbered_one() {
        let root = PidNs::root(1);
        let inner = PidNs::child(&root, 2);
        assert_eq!(inner.allocate(12), 1);
        assert!(inner.is_init(12));
        assert!(!inner.is_init(13));
        assert_eq!(inner.allocate(13), 2);
        assert!(!inner.is_init(13), "number 2 is a member like any other");
        // The root: slot 1 is the machine's init, and the identity map is what
        // says so.
        assert!(root.is_init(1));
        assert!(!root.is_init(2));
    }

    /// A sealed namespace is one whose init has exited, and the state is
    /// permanently one-way: nothing un-seals it, because a pid namespace's init
    /// cannot be replaced (the number 1 has been spent, and Linux hands the
    /// namespace nothing new to speak for it).
    #[test]
    fn sealing_is_one_way_and_per_namespace() {
        let root = PidNs::root(1);
        let inner = PidNs::child(&root, 2);
        let sibling = PidNs::child(&root, 3);
        assert!(!root.is_sealed() && !inner.is_sealed() && !sibling.is_sealed());
        inner.seal();
        assert!(inner.is_sealed());
        assert!(
            !root.is_sealed(),
            "sealing a child is not sealing its parent"
        );
        assert!(!sibling.is_sealed(), "nor its sibling");
        // And the numbers still answer: the seal is about *creating* tasks in
        // it, not about the ones its readers can still name.
        assert_eq!(inner.allocate(6), 1);
        assert_eq!(inner.number_of(6), Some(1));
    }

    /// The chain a `/proc/<pid>/status` reader is shown: every level's number
    /// for one task, outermost first - one entry for a task in the init
    /// namespace, and the whole chain for one nested under it.
    #[test]
    fn the_chain_of_numbers_runs_outermost_first() {
        let root = PidNs::root(1);
        let outer = PidNs::child(&root, 2);
        let inner = PidNs::child(&outer, 3);
        // Registered at every level on the way in, as `allocate` does.
        assert_eq!(inner.allocate(12), 1);
        assert_eq!(inner.numbers(12), alloc::vec![12, 1, 1]);
        assert_eq!(outer.numbers(12), alloc::vec![12, 1]);
        // A task in the init namespace alone has one number there, which is what
        // Linux prints for a machine that never unshared.
        assert_eq!(root.numbers(7), alloc::vec![7]);
        // A slot the inner levels never registered is still a member of the
        // root - every task is - so its chain is the one number the outermost
        // level gives it, and nothing is invented for the levels in between.
        assert_eq!(inner.numbers(11), alloc::vec![11]);
    }

    /// A namespace is an ancestor of exactly the namespaces nested under it -
    /// the question the init's signal rule turns into "may this sender force a
    /// signal through", and the one its death asks about a task's membership.
    #[test]
    fn ancestor_and_coverage_follow_the_chain() {
        let root = PidNs::root(1);
        let outer = PidNs::child(&root, 2);
        let inner = PidNs::child(&outer, 3);
        let sibling = PidNs::child(&root, 4);

        assert!(root.is_ancestor_of(&outer));
        assert!(root.is_ancestor_of(&inner), "two levels down counts");
        assert!(!outer.is_ancestor_of(&root));
        assert!(
            !outer.is_ancestor_of(&sibling),
            "a sibling is not a descendant"
        );
        assert!(!outer.is_ancestor_of(&outer), "an ancestor is strict");

        // Coverage is the inclusive half: the set a namespace's init's death
        // takes with it, and the set a sender must be *outside* of to force a
        // signal through its init.
        assert!(outer.covers(&outer));
        assert!(outer.covers(&inner));
        assert!(!outer.covers(&sibling));
        assert!(root.covers(&inner), "the root covers everything");
    }
}
