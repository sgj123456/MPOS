//! t_stdargs - the Linux startup contract, as Rust std sees it.
//!
//! The kernel lays a process's stack out the Linux way: `argc` at the bottom,
//! then the argv pointers, their NULL, the envp pointers, their NULL, and the
//! auxv. A Linux-built libc hands `(argc, argv, envp)` to `main` from that
//! block, and Rust std's `env::args()` and `env::var()` read through it. This
//! test pins the contract from the std side, with no mpos libc involved: the
//! shell runs `t_stdargs one two three`, and the program verifies that
//! `env::args()` returns exactly those four words and that `env::vars()` is
//! empty - the shell spawns through a convention that carries no environment,
//! and an absent environment must come back empty, not as garbage.
//!
//! Not in the gate for now: the first `malloc` this std image makes still
//! runs into the pre-existing musl allocation bug (an absurd `mmap` length)
//! and dies before `main` runs. It goes back into SHELL_SCRIPT when that is
//! fixed.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let want = ["t_stdargs", "one", "two", "three"];
    let env_ok = std::env::vars().next().is_none();

    if args.len() == want.len()
        && args.iter().zip(want.iter()).all(|(a, w)| a == w)
        && env_ok
    {
        println!("t_stdargs: argc={} argv0={} env=empty", args.len(), args[0]);
    } else {
        eprintln!(
            "t_stdargs: mismatch: args={args:?} env_empty={env_ok}",
            args = args,
            env_ok = env_ok
        );
        std::process::exit(1);
    }
}
