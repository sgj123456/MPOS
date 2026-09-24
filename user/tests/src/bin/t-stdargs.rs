//! t-stdargs - the Linux startup contract, as Rust std sees it.
//!
//! The kernel lays a process's stack out the Linux way: `argc` at the bottom,
//! then the argv pointers, their NULL, the envp pointers, their NULL, and the
//! auxv. A Linux-built libc hands `(argc, argv, envp)` to `main` from that
//! block, and Rust std's `env::args()` and `env::var()` read through it. This
//! test pins the contract from the std side, with no mpos libc involved: the
//! shell runs `t-stdargs one two three`, and the program checks that
//! `env::args()` returns exactly those four words.
//!
//! The environment is deliberately not pinned to a value. An earlier version
//! demanded it be *empty*, which was true when the shell spawned children
//! with no environment at all and became false the moment it exported
//! anything - a test that fails when the system starts doing more is a test
//! of the wrong thing. What the kernel actually promises is that whatever is
//! there is *well formed*: a list of `NAME=VALUE`, each terminated, each with
//! a non-empty name. Garbage in that region is what the block layout exists
//! to prevent, and garbage is what this catches.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let want = ["t-stdargs", "one", "two", "three"];
    let env: Vec<(String, String)> = std::env::vars().collect();
    // A name with an `=` in it, or an empty one, means the strings on the
    // stack were not split where the ABI says they were - the pointers ran
    // past the end of the block, or the NULL terminator is missing.
    let env_ok = env.iter().all(|(k, _)| !k.is_empty() && !k.contains('='));

    if args.len() == want.len() && args.iter().zip(want.iter()).all(|(a, w)| a == w) && env_ok {
        println!(
            "t-stdargs: argc={} argv0={} env={} vars, all well formed",
            args.len(),
            args[0],
            env.len()
        );
        println!("STDARGS_OK");
    } else {
        eprintln!(
            "t-stdargs: mismatch: args={args:?} env={env:?} env_ok={env_ok}",
            args = args,
            env = env,
            env_ok = env_ok
        );
        std::process::exit(1);
    }
}
