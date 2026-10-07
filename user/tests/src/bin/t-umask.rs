//! t-umask - the creation mask, and the mode a create is actually given.
//!
//! Pinned here, per umask(2): the call answers what the mask *was* while
//! storing what it is given, so a program can save and restore it; the mask
//! is the bits a create must clear out of the mode it was asked for, so a
//! create of `0666` under `0077` arrives `0600`; and only the low twelve
//! bits survive - those are all a mode has, and `umask(017777)` leaves
//! `07777` behind on the next read. The mask is the process's own, which is
//! the reason the test restores it on the way out.
//!
//! # The word above the twelve is the whole reason this one is subtle
//!
//! `rustix::process::umask` takes and returns a `Mode`, and `Mode::from`
//! (`from_raw_mode`) masks with the low twelve bits on *this* side - so
//! `umask(Mode::from(0o17777))` would send `0o7777` and the claim that the
//! *kernel* drops the bits would hold for a reason that has nothing to do with
//! the kernel. The word has to arrive whole, and a bitflags type is what carries
//! it: `Mode` declares bitflags' unnamed flag (`const _ = !0`), so
//! `Mode::from_bits_retain(0o17777)` is the generated call for exactly this, and
//! the truncation the claim is about is the kernel's. Same distinction as
//! `t-fallocate`'s mode bit and `t-unlinkat`'s flag word - and the reason a
//! rustix *enum* is a different thing entirely (`user/tests/Cargo.toml`).
//!
//! mpos applies the mask where Linux does, in the create path
//! (`masked_mode`, which reads `sched::current_umask`), so no check here records
//! a divergence. `umask` has a real number on both machines (`nr_aarch64.rs`:
//! `UMASK = 166`; `nr_x86_64.rs`: `95`), and the per-claim output is the same on
//! both.
//!
//! The file is the test's own under `/tmp`, which both runners back with a
//! tmpfs - the claim is about the mode a create is given, not about which
//! filesystem remembers it.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, fstat, openat, unlinkat};
use rustix::process::umask;

/// The file whose created mode the mask is read off.
const PATH: &str = "/tmp/t-umask.f";

fn main() {
    println!("t-umask");
    let mut r = Report::new();

    let _ = std::fs::remove_file(PATH);

    // The first call answers the inherited mask, whatever it is; the point of
    // the next one is that it answers what the first *set*.
    let m0 = umask(Mode::from(0o022));

    // The round trip: set 0077, be told the mask is 0022.
    r.check(
        "umask answers the mask it is replacing",
        umask(Mode::from(0o077)).bits() == 0o022,
    );

    // What the mask is *for*: a create's mode has the mask's bits cleared.
    // 0666 & !0077 is 0600, and that is what the file must carry.
    match openat(
        CWD,
        PATH,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
        Mode::from(0o666),
    ) {
        Ok(fd) => {
            r.check("the file is created", true);
            match fstat(&fd) {
                Ok(st) => {
                    r.check("fstat succeeds", true);
                    r.check(
                        "a create of 0666 under umask 0077 is stored as 0600",
                        Mode::from_raw_mode(st.st_mode).bits() & 0o777 == 0o600,
                    );
                }
                Err(_) => {
                    r.check("fstat succeeds", false);
                    r.check("a create of 0666 under umask 0077 is stored as 0600", false);
                }
            }
        }
        Err(_) => {
            r.check("the file is created", false);
            r.check("fstat succeeds", false);
            r.check("a create of 0666 under umask 0077 is stored as 0600", false);
        }
    }

    // Only the twelve bits a mode has are kept: 017777 stores 07777. See the
    // module comment for why the word goes in through `from_bits_retain`.
    r.check(
        "setting a mask with bits above the twelve answers the old mask",
        umask(Mode::from_bits_retain(0o17777)).bits() == 0o077,
    );
    r.check(
        "the mask kept only its low twelve bits",
        umask(Mode::empty()).bits() == 0o7777,
    );

    // The mask is process state, not machine state: put back what was found.
    // The first call answers the zero the check above left behind; the second
    // reads the restored mask back and must answer what was found.
    let _ = umask(m0);
    r.check(
        "the mask is restored to what the test found",
        umask(m0).bits() == m0.bits(),
    );

    let _ = unlinkat(CWD, PATH, AtFlags::empty());
    r.finish()
}
