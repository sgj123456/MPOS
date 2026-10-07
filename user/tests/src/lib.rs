//! The std-side tests' shared verdict protocol.
//!
//! Every program in this crate reports the same way: one printed line per claim
//! as it is made, then a verdict and an exit status the harness reads. Until this
//! file existed, each of the thirty wrote its own copy of that - a local
//! `struct Report` with a `check` method - which is also how they drifted:
//! `t-process-vm-libc` ends on "all passed" where its neighbours end on `PASS`.
//!
//! The `=== RUN` / `=== PASS` / `=== FAIL` lines around a program are the
//! *runner's*, not the program's (`xtask` writes them from the exit status); what
//! a program owes the runner is a clean status, and what it owes a person
//! reading the log is the per-claim detail. That division is why this helper
//! prints and exits rather than reporting to the runner.
//!
//! The no-std conformance suite (`user/abitest`) reports the same way through
//! `lin::check_true`/`lin::check_eq`/`lin::done` and the same exit codes. This is
//! the std side's half of one protocol - which is what makes the two suites
//! able to run under the same reader, and what let the suite move from one to the
//! other (`PLAN.md` §15.11).

/// The claims one test has made, and whether any of them failed.
pub struct Report {
    failed: bool,
}

impl Default for Report {
    fn default() -> Self {
        Self::new()
    }
}

impl Report {
    pub fn new() -> Self {
        Self { failed: false }
    }

    /// One claim, printed as it is made: a run that hangs or dies still shows how
    /// far it got, which is the difference between "this failed" and "this never
    /// ran".
    pub fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }

    /// The verdict, and the exit status the runner turns into `=== PASS` or
    /// `=== FAIL`. Exits rather than panicking: a claim that did not hold is a
    /// result about the kernel, not a crash in the test.
    pub fn finish(self) -> ! {
        if self.failed {
            println!("FAILED");
            std::process::exit(1);
        }
        println!("PASS");
        std::process::exit(0);
    }
}
