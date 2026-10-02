#![no_std]

//! The UTS namespace: the two names a machine calls itself.
//!
//! `uname(2)`'s `nodename` and `domainname` are not facts about the hardware.
//! They are facts about a *namespace*, and Linux has kept them that way so that
//! a container can call itself `web-01` inside while the host it runs on stays
//! `build-host`. Everything here follows from that one sentence: the names are
//! an object a task holds, `fork` hands the child the same object (a child
//! shares its parent's namespace until it asks for its own), and
//! `unshare(CLONE_NEWUTS)` hands the caller a **copy** - Linux's
//! `copy_utsname`, and the whole reason a `sethostname` inside a new namespace
//! cannot be seen from outside it.
//!
//! The names live in an object rather than in two fields on the task because a
//! namespace is *shared*: every child holds the same one until it unshares, and
//! a namespace outlives the task that named it for exactly as long as something
//! else holds a reference. `Arc` is that sentence. Two `String`s copied per
//! task would be the same behaviour with a way to get it wrong.
//!
//! **The names are read through three doors, and they have to agree.** Two are
//! syscalls - `sethostname`/`setdomainname` and `uname` - and the third is
//! `/proc/sys/kernel/hostname`, the file `sysctl -w` and OpenRC's `hostname`
//! service write. Before this the pair lived in `vfs::procfs` as one
//! process-wide value, which made "one fact, one home" true for the machine and
//! false for a namespace; the home is the namespace now and procfs reads the
//! *caller's*, so the door that writes and the door that reads cannot disagree
//! about which namespace they are talking about. That move also closed two
//! disagreements the old shape had already grown: `uname` hardcoded its
//! `domainname` to `(none)`, and `/proc/sys/kernel/domainname` both hardcoded
//! its read and dropped its write, so `setdomainname(2)` had no observable
//! effect through any door at all.

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use core::cell::RefCell;
use critical_section::Mutex;

/// The name a namespace reports until somebody sets one.
///
/// Linux's default is `(none)`; this kernel answers `mpos`, because the first
/// thing a boot's init usually prints is a banner with the hostname in it and
/// `(none)` there reads as a failure to look one up. That is a deliberate
/// divergence and the only one in this file.
pub const DEFAULT_NODENAME: &str = "mpos";

/// The domain a namespace reports until somebody names one.
///
/// `(none)` is Linux's own answer for a machine nobody has named, and this
/// copies it rather than inventing a friendlier one: unlike the hostname,
/// nothing prints a domain at boot, so there is no first impression to spoil.
pub const DEFAULT_DOMAINNAME: &str = "(none)";

/// One UTS namespace: the pair of names `uname(2)` reports.
///
/// The two names are behind the kernel's ordinary
/// `critical_section::Mutex<RefCell<_>>` because a namespace is shared: a parent
/// and every child that has not unshared point at the same object, so a
/// `sethostname` must be visible to all of them and not merely to the caller,
/// and it must be visible without any of them having to ask the scheduler who
/// wrote it.
///
/// An *empty* name is how "nobody has set one" is spelled, and the getters
/// answer the default for it. That is the convention the process-wide pair
/// used before the namespace existed, kept because it makes a fresh namespace
/// read as a named machine (`mpos`) rather than as a machine that failed to
/// find its own name.
pub struct Uts {
    nodename: Mutex<RefCell<String>>,
    domainname: Mutex<RefCell<String>>,
}

impl Uts {
    /// A namespace nobody has named: both names empty, so both getters answer
    /// their defaults. What a boot starts with, and what a copy of an unnamed
    /// namespace holds.
    fn unnamed() -> Arc<Uts> {
        Arc::new(Uts {
            nodename: Mutex::new(RefCell::new(String::new())),
            domainname: Mutex::new(RefCell::new(String::new())),
        })
    }

    /// The name this namespace goes by - `uname`'s `nodename`, `hostname(1)`'s
    /// answer, and `/proc/sys/kernel/hostname`'s contents.
    pub fn nodename(&self) -> String {
        critical_section::with(|cs| {
            let name = self.nodename.borrow(cs).borrow();
            if name.is_empty() {
                String::from(DEFAULT_NODENAME)
            } else {
                name.clone()
            }
        })
    }

    /// The domain this namespace belongs to. `(none)` until somebody names it,
    /// which is what Linux reports for a machine nobody has named.
    pub fn domainname(&self) -> String {
        critical_section::with(|cs| {
            let name = self.domainname.borrow(cs).borrow();
            if name.is_empty() {
                String::from(DEFAULT_DOMAINNAME)
            } else {
                name.clone()
            }
        })
    }

    /// Name this namespace. `sethostname(2)` and a write to
    /// `/proc/sys/kernel/hostname` are the two spellings, and both land here.
    pub fn set_nodename(&self, name: &str) {
        critical_section::with(|cs| {
            *self.nodename.borrow(cs).borrow_mut() = String::from(name);
        });
    }

    /// Name this namespace's domain: `setdomainname(2)` and a write to
    /// `/proc/sys/kernel/domainname`.
    pub fn set_domainname(&self, name: &str) {
        critical_section::with(|cs| {
            *self.domainname.borrow(cs).borrow_mut() = String::from(name);
        });
    }

    /// A fresh namespace holding a copy of these names - Linux's
    /// `copy_utsname`, which is what `unshare(CLONE_NEWUTS)` hands the caller
    /// and what `clone(CLONE_NEWUTS)` would hand a child.
    ///
    /// The *copy* is the point and not an implementation detail: the new
    /// namespace must start out looking exactly like the one it came from, so
    /// that a program which unshares and then asks what it is called is told
    /// the truth, and it must then be free to diverge, so that the next
    /// `sethostname` is invisible to the namespace it came from.
    pub fn copied(&self) -> Arc<Uts> {
        let (nodename, domainname) = critical_section::with(|cs| {
            (
                self.nodename.borrow(cs).borrow().clone(),
                self.domainname.borrow(cs).borrow().clone(),
            )
        });
        Arc::new(Uts {
            nodename: Mutex::new(RefCell::new(nodename)),
            domainname: Mutex::new(RefCell::new(domainname)),
        })
    }
}

/// The namespace a boot starts in - Linux's `init_uts_ns`.
///
/// Every task is given this one when it is created, and every `fork`/`clone`
/// hands it on by reference, so it is the single namespace a machine that never
/// calls `unshare` has. It is created on first use rather than at a fixed point
/// in the boot because the first task to ask for it may be the boot task
/// itself, which exists before anything that could own a table of namespaces.
static INIT: Mutex<RefCell<Option<Arc<Uts>>>> = Mutex::new(RefCell::new(None));

/// The namespace a boot starts in, created on first use.
pub fn init() -> Arc<Uts> {
    critical_section::with(|cs| {
        INIT.borrow(cs)
            .borrow_mut()
            .get_or_insert_with(Uts::unnamed)
            .clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_namespace_nobody_named_reports_the_defaults() {
        let uts = Uts::unnamed();
        assert_eq!(uts.nodename(), DEFAULT_NODENAME);
        assert_eq!(uts.domainname(), DEFAULT_DOMAINNAME);
    }

    /// The rule `unshare` rests on, in one test: the copy starts from the
    /// original's names - so a program that unshares and then asks what it is
    /// called is told the truth - and once it is named, the original does not
    /// follow. Both halves fail for a "namespace" that is really a shared
    /// global, which is what this kernel had before this module existed.
    #[test]
    fn a_copy_starts_from_the_names_and_then_diverges() {
        let outer = Uts::unnamed();
        outer.set_nodename("outer");
        outer.set_domainname("example.test");

        let inner = outer.copied();
        assert_eq!(inner.nodename(), "outer", "the copy did not inherit");
        assert_eq!(inner.domainname(), "example.test");

        inner.set_nodename("inner");
        assert_eq!(inner.nodename(), "inner");
        assert_eq!(
            outer.nodename(),
            "outer",
            "naming the copy renamed the namespace it came from"
        );
        // The domain is carried but not shared either: one name diverging must
        // not make the other follow, which a copy of the *object* rather than
        // of its contents would do.
        assert_eq!(outer.domainname(), "example.test");
    }

    /// One boot, one starting namespace: every caller that has not unshared is
    /// handed the same object, which is what makes a `sethostname` by the boot
    /// task visible to the shell it later starts.
    #[test]
    fn init_is_one_namespace_however_often_it_is_asked_for() {
        assert!(Arc::ptr_eq(&init(), &init()));
    }
}
