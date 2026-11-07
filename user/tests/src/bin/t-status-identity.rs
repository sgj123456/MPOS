//! `/proc/<pid>/status` prints the identity lines: the umask, the tracer, the
//! no-new-privs flag, the seccomp mode and the five capability words.
//!
//! `status_of` printed `Umask:\t0022`, `TracerPid:\t0`, `NoNewPrivs:\t0`,
//! `Seccomp:\t0`, `Seccomp_filters:\t0` and four constant capability words while
//! the kernel held every one of those facts: the group's `FsStruct::umask`, the
//! task's `trace.tracer`, `no_new_privs`, `seccomp_strict`/`seccomp_filters`, and
//! the credential model's four masks.
//!
//! Measured on Linux 7.2.14 (`build/scratch/status2/p.c`, musl static):
//!
//! ```text
//! baseline                          Umask: 0022  TracerPid: 0  NoNewPrivs: 0  Seccomp: 0  Seccomp_filters: 0
//! after umask(077) + PR_SET_NO_NEW_PRIVS   Umask: 0077   NoNewPrivs: 1
//! after seccomp(SET_MODE_FILTER, one filter)  Seccomp: 2  Seccomp_filters: 1
//! tracee's status (PTRACE_TRACEME child)    TracerPid: 684234   (its tracer's pid)
//! tracer's own status                       TracerPid: 0
//! CapInh/Prm/Eff/Bnd: sixteen lowercase hex digits; CapAmb 0 (no file caps)
//! ```
//!
//! Every claim is a movement or two-door agreement: the capability words are
//! compared against `capget(2)` and `PR_CAPBSET_READ`, which is the same fact at
//! another door, and the umask is read back after `umask(077)` rather than
//! assumed.

use mpos_user_tests::{Report, host_run};

const NOBODY: u32 = 65_534;

/// The lines this test reads.
#[derive(Clone, Default)]
struct Identity {
    umask: String,
    tracer_pid: u64,
    no_new_privs: u64,
    seccomp: u64,
    seccomp_filters: u64,
    cap_inh: u64,
    cap_prm: u64,
    cap_eff: u64,
    cap_bnd: u64,
}

fn line<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    for each in text.lines() {
        if let Some(rest) = each.strip_prefix(name) {
            return Some(rest.trim_start_matches(':').trim());
        }
    }
    None
}

fn number(text: &str, name: &str) -> Option<u64> {
    line(text, name)?.parse().ok()
}

fn hex_word(text: &str, name: &str) -> Option<u64> {
    u64::from_str_radix(line(text, name)?, 16).ok()
}

fn read_identity(path: &str) -> Option<Identity> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(Identity {
        umask: line(&text, "Umask:")?.to_string(),
        tracer_pid: number(&text, "TracerPid:")?,
        no_new_privs: number(&text, "NoNewPrivs:")?,
        seccomp: number(&text, "Seccomp:")?,
        seccomp_filters: number(&text, "Seccomp_filters:")?,
        cap_inh: hex_word(&text, "CapInh:")?,
        cap_prm: hex_word(&text, "CapPrm:")?,
        cap_eff: hex_word(&text, "CapEff:")?,
        cap_bnd: hex_word(&text, "CapBnd:")?,
    })
}

/// The `Groups:` line as the gids it lists.
fn groups_of(path: &str) -> Option<Vec<u32>> {
    let text = std::fs::read_to_string(path).ok()?;
    line(&text, "Groups:").map(|rest| {
        rest.split_whitespace()
            .filter_map(|gid| gid.parse().ok())
            .collect()
    })
}

fn self_identity() -> Option<Identity> {
    read_identity("/proc/self/status")
}

/// What `capget(2)` answers for the calling process: the three words status
/// prints as `CapInh`/`CapPrm`/`CapEff` - the same fact at another door.
fn capget() -> Option<(u64, u64, u64)> {
    #[repr(C)]
    #[derive(Default)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let mut header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = [Data::default(); 2];
    let answer = unsafe {
        libc::syscall(
            libc::SYS_capget,
            &mut header as *mut Header,
            data.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if answer != 0 {
        return None;
    }
    let word = |low: u32, high: u32| u64::from(low) | (u64::from(high) << 32);
    Some((
        word(data[0].inheritable, data[1].inheritable),
        word(data[0].permitted, data[1].permitted),
        word(data[0].effective, data[1].effective),
    ))
}

/// The bounding set as `PR_CAPBSET_READ` answers it, bit `cap` for each cap the
/// kernel still holds in the set.
fn bounding_set() -> Option<u64> {
    let mut word = 0u64;
    for cap in 0..=40 {
        // `PR_CAPBSET_READ` is 23 (21 is `PR_CAP_AMBIENT`, which answers about
        // the ambient word instead and would read as an empty bounding set).
        let answer = unsafe { libc::prctl(23, cap as libc::c_ulong, 0, 0, 0) };
        if answer < 0 {
            return None;
        }
        if answer == 1 {
            word |= 1u64 << cap;
        }
    }
    Some(word)
}

extern "C" fn quiet_handler(_signal: libc::c_int) {}

fn main() {
    let mut r = Report::new();

    let start = self_identity();
    r.check(
        &format!("status prints the identity lines ({})", start.is_some()),
        start.is_some(),
    );
    let start = start.clone().unwrap_or_default();

    // The umask: `umask(2)` returns the old mask, and status shows the new one
    // as four octal digits.
    let previous = unsafe { libc::umask(0o077) };
    let after = self_identity().unwrap_or_else(|| start.clone());
    r.check(
        &format!(
            "umask(077) shows as the four octal digits 0077 ({})",
            after.umask
        ),
        after.umask == "0077",
    );
    unsafe { libc::umask(previous) };

    // No-new-privs: a one-way flag, and the line is where a program reads it.
    let nnp_before = start.no_new_privs;
    let set = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    let flagged = self_identity().unwrap_or_else(|| start.clone());
    r.check(
        &format!(
            "PR_SET_NO_NEW_PRIVS shows as NoNewPrivs ({nnp_before} -> {})",
            flagged.no_new_privs
        ),
        set == 0 && (flagged.no_new_privs == 1 || nnp_before == 1),
    );

    // Seccomp: the mode and the filter count, which is what a loader checks
    // before it trusts the sandbox it installed.
    let filters_before = flagged.seccomp_filters;
    let mode_before = flagged.seccomp;
    let program: [libc::sock_filter; 1] = [libc::sock_filter {
        code: 0x06, // BPF_RET
        jt: 0,
        jf: 0,
        k: 0x7fff_0000, // SECCOMP_RET_ALLOW
    }];
    let filter = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_ptr() as *mut libc::sock_filter,
    };
    let installed = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            1u64, // SECCOMP_SET_MODE_FILTER
            0u64,
            &filter as *const libc::sock_fprog,
        )
    };
    let filtered = self_identity().unwrap_or_else(|| flagged.clone());
    r.check(
        &format!(
            "a filter shows as Seccomp 2 and one filter ({mode_before} -> {}, {} -> {})",
            filtered.seccomp, filters_before, filtered.seccomp_filters
        ),
        installed == 0 && filtered.seccomp == 2 && filtered.seccomp_filters == filters_before + 1,
    );

    // The capability words are the credential model's own masks, so they must
    // agree with `capget(2)` and with `PR_CAPBSET_READ` - three doors, one fact.
    let (inheritable, permitted, effective) = capget().unwrap_or((0, 0, 0));
    let bounding = bounding_set().unwrap_or(0);
    let caps = self_identity().unwrap_or_else(|| filtered.clone());
    r.check(
        &format!(
            "status's CapInh/Prm/Eff are capget's words ({:016x}/{:016x}/{:016x} against {:016x}/{:016x}/{:016x})",
            caps.cap_inh, caps.cap_prm, caps.cap_eff, inheritable, permitted, effective
        ),
        caps.cap_inh == inheritable
            && caps.cap_prm == permitted
            && caps.cap_eff == effective,
    );
    r.check(
        &format!(
            "and its CapBnd is the set PR_CAPBSET_READ answers ({:016x} against {:016x})",
            caps.cap_bnd, bounding
        ),
        caps.cap_bnd == bounding,
    );

    // A *dropped* identity is where a constant capability word tells the
    // truth about nothing: Linux's `setuid` to an unprivileged user empties the
    // effective and permitted sets, so the file and `capget(2)` must both say
    // zero. A root payload prints the same words either way, which is why this
    // claim runs in a child that dropped.
    let mut pipe_fds = [0i32; 2];
    unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
    let dropped_child = unsafe { libc::fork() };
    if dropped_child == 0 {
        unsafe {
            libc::close(pipe_fds[0]);
            libc::dup2(pipe_fds[1], 1);
        }
        unsafe {
            libc::setgroups(0, std::ptr::null());
            libc::setgid(NOBODY);
            libc::setuid(NOBODY);
        }
        let (inh, prm, eff) = capget().unwrap_or((0, 0, 0));
        let own = self_identity();
        let (status_prm, status_eff) = own
            .as_ref()
            .map(|identity| (identity.cap_prm, identity.cap_eff))
            .unwrap_or((u64::MAX, u64::MAX));
        let text = format!("{status_prm} {status_eff} {prm} {eff} {inh}\n");
        unsafe {
            let bytes = text.as_bytes();
            libc::write(1, bytes.as_ptr() as *const libc::c_void, bytes.len());
            libc::_exit(0);
        }
    }
    unsafe { libc::close(pipe_fds[1]) };
    let mut answer = [0u8; 128];
    let read = unsafe {
        libc::read(
            pipe_fds[0],
            answer.as_mut_ptr() as *mut libc::c_void,
            answer.len(),
        )
    };
    unsafe { libc::close(pipe_fds[0]) };
    let mut dropped_status = 0;
    let _ = unsafe { libc::waitpid(dropped_child, &mut dropped_status, 0) };
    let text = if read > 0 {
        String::from_utf8_lossy(&answer[..read as usize]).to_string()
    } else {
        String::new()
    };
    let words: Vec<u64> = text
        .split_whitespace()
        .filter_map(|word| word.parse().ok())
        .collect();
    let (status_prm, status_eff) = words
        .first()
        .zip(words.get(1))
        .map_or((u64::MAX, u64::MAX), |(a, b)| (*a, *b));
    let (capget_prm, capget_eff) = words
        .get(2)
        .zip(words.get(3))
        .map_or((u64::MAX, u64::MAX), |(a, b)| (*a, *b));
    r.check(
        &format!(
            "a dropped identity empties the two capability words ({capget_prm:016x}/{:016x})",
            capget_eff
        ),
        capget_prm == 0 && capget_eff == 0,
    );
    r.check(
        &format!(
            "and status says the same ({status_prm:016x}/{status_eff:016x} against {capget_prm:016x}/{capget_eff:016x})"
        ),
        status_prm == capget_prm && status_eff == capget_eff,
    );

    // The tracer: a tracee's own file names the task that attached it, and its
    // tracer's file names nobody.
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe {
            libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0);
            libc::raise(libc::SIGSTOP);
            libc::_exit(0);
        }
    }
    let mut status = 0;
    let waited = unsafe { libc::waitpid(child, &mut status, libc::WUNTRACED) };
    let tracee_pid =
        read_identity(&format!("/proc/{child}/status")).map(|identity| identity.tracer_pid);
    let tracer_pid = self_identity().map(|identity| identity.tracer_pid);
    r.check(
        &format!(
            "a tracee's status names its tracer ({tracee_pid:?} against this pid {})",
            std::process::id()
        ),
        waited == child && tracee_pid == Some(std::process::id() as u64),
    );
    r.check(
        &format!("and the tracer's own status names none ({tracer_pid:?})"),
        tracer_pid == Some(0),
    );
    unsafe {
        libc::kill(child, libc::SIGKILL);
        libc::waitpid(child, &mut status, 0);
    }

    // The `Groups:` line is the supplementary group list, and `getgroups(2)` is
    // the other door onto it. Measured on Linux 7.2.14
    // (`build/scratch/status2/g.c`): `Groups:\t953 966 992 998 1000 ` - one
    // space after every gid, the last included. A payload that starts with no
    // supplementary groups is the clean case: the line is empty, then it lists
    // exactly what `setgroups(2)` was given.
    let groups_before = groups_of("/proc/self/status");
    if !host_run() {
        let mut none: libc::gid_t = 0;
        let cleared = unsafe { libc::setgroups(0, &mut none) };
        let empty = groups_of("/proc/self/status");
        r.check(
            &format!("setgroups(0) empties the Groups line ({empty:?})"),
            cleared == 0 && empty == Some(Vec::new()),
        );

        let chosen: [libc::gid_t; 3] = [10, 20, 30];
        let set = unsafe { libc::setgroups(chosen.len(), chosen.as_ptr()) };
        let listed = groups_of("/proc/self/status");
        r.check(
            &format!("setgroups(10, 20, 30) lists them in that order ({listed:?})"),
            set == 0 && listed == Some(vec![10, 20, 30]),
        );
        let mut read = [0 as libc::gid_t; 8];
        let count = unsafe { libc::getgroups(read.len() as libc::c_int, read.as_mut_ptr()) };
        let through_getgroups = if count > 0 {
            Some(read[..count as usize].to_vec())
        } else {
            None
        };
        r.check(
            &format!("and getgroups reports the same list ({through_getgroups:?})"),
            through_getgroups == Some(vec![10, 20, 30]),
        );
    } else {
        r.check(
            &format!("the Groups line reads as a list ({groups_before:?})"),
            groups_before.is_some(),
        );
    }

    let _ = quiet_handler as usize;
    r.finish();
}
