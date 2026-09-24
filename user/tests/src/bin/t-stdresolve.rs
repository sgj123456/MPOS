//! t-stdresolve - the resolver shape wget uses: `getaddrinfo`, twenty times.
//!
//! The hand-built queries of the raw test suite pin one shape at a time;
//! musl's `getaddrinfo` walks a different one - an A query and an AAAA
//! query back to back from one socket, answers arriving in either order,
//! each read with a poll and a timeout - and that interleaving is the only
//! DNS shape the daemons on the machine actually run. Twenty resolutions of
//! two real names, counted: a kernel that drops one answer in twenty looks
//! fine at the console and is exactly the `bad address` a user sees.

use std::net::ToSocketAddrs;

fn main() {
    println!("t-stdresolve");
    let mut failed = false;
    let mut ok = 0usize;
    let mut tries = 0usize;

    for round in 0..10 {
        for host in ["baidu.com", "gitee.com"] {
            tries += 1;
            let started = std::time::Instant::now();
            match (host, 80u16).to_socket_addrs() {
                Ok(mut addrs) => match addrs.next() {
                    Some(addr) => {
                        ok += 1;
                        println!("  {host} -> {addr} in {} ms", started.elapsed().as_millis());
                    }
                    None => println!("FAIL: round {round} {host} resolved to nothing"),
                },
                Err(err) => println!("FAIL: round {round} {host}: {err}"),
            }
        }
    }

    println!("resolved {ok}/{tries}");
    if ok != tries {
        failed = true;
    }
    if failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
