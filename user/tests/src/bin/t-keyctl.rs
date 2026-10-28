//! t-keyctl - the kernel keyring: `add_key(2)`, `request_key(2)` and
//! `keyctl(2)`.
//!
//! The keyring is a small object store with a serial for a handle, a
//! permission word with four classes (possessor, owner, group, other), and a
//! second object type - the keyring - that holds serials. This program pins the
//! caller-visible contract, measured on the host (Linux 7.2.11) before this
//! kernel grew it; the measured table and the named divergences are
//! `docs/notes/` §6.30.
//!
//! What a caller actually branches on, and what is checked here:
//!
//! * `keyctl(READ)` answers the whole payload length and copies nothing when
//!   the caller's buffer is too small, so a size probe is a NULL or short
//!   buffer (measured: 5 into a 10-byte key answered 10 and copied nothing).
//! * `keyctl(DESCRIBE)` is `"<type>;<uid>;<gid>;<perm %08x>;<desc>"`, and the
//!   default permission word is `0x3f010000`.
//! * `keyctl(UPDATE)` on a keyring is `EOPNOTSUPP`; an unknown command is
//!   `EOPNOTSUPP` too, and a keyring's `READ` is its link list, four bytes per
//!   serial, little-endian.
//! * state is checked before permission: a revoked key answers `EKEYREVOKED`
//!   to `READ`, `DESCRIBE` and `UPDATE`; an invalidated one is `ENOKEY`
//!   everywhere, as if it had never existed.
//! * possession is a *permission class*, not a findability filter: a key
//!   unlinked from every ring is still found by serial, and `READ` on it is
//!   `EACCES` (the owner class holds `VIEW` only) while `DESCRIBE` still works.
//! * `SEARCH` recurses into nested keyrings; `UNLINK` of a key that is not in
//!   the ring is `ENOENT`.
//! * a forked child inherits the session keyring and nothing else: its process
//!   keyring starts empty (measured with the same ring serial in both).
//!
//! Two claims are about the *caller's* privilege, and both run only in the
//! shape that privilege makes true: `SETPERM` as an ordinary owner is
//! `EACCES` (measured, and the host run is an ordinary user) while the booted
//! machine's init is root and may rewrite the word. Quota (`EDQUOT`) is not
//! claimed here because the root quota is far above any payload this program
//! can hand `add_key`; the host unit tests cover the quota path directly. The
//! `request_key` upcall gap (a miss is `ENOKEY` even with callout information)
//! is claimed in the shape a program sees either way.

use mpos_user_tests::Report;
use std::ffi::{CStr, CString};

/// Linux's special negative `key_serial_t` ids.
const KEY_SPEC_THREAD_KEYRING: i32 = -1;
const KEY_SPEC_PROCESS_KEYRING: i32 = -2;
const KEY_SPEC_SESSION_KEYRING: i32 = -3;

/// The `keyctl(2)` commands this program uses. These are Linux's append-only
/// command numbers.
const KEYCTL_GET_KEYRING_ID: libc::c_long = 0;
const KEYCTL_JOIN_SESSION_KEYRING: libc::c_long = 1;
const KEYCTL_UPDATE: libc::c_long = 2;
const KEYCTL_REVOKE: libc::c_long = 3;
const KEYCTL_SETPERM: libc::c_long = 4;
const KEYCTL_DESCRIBE: libc::c_long = 6;
const KEYCTL_CLEAR: libc::c_long = 7;
const KEYCTL_LINK: libc::c_long = 8;
const KEYCTL_UNLINK: libc::c_long = 9;
const KEYCTL_SEARCH: libc::c_long = 10;
const KEYCTL_READ: libc::c_long = 11;
const KEYCTL_INVALIDATE: libc::c_long = 21;

/// The raw errnos the measured table names, as negative Linux values (the
/// program compares raw codes so its expectations read like the table).
const ENOENT: i32 = -2;
const EACCES: i32 = -13;
const EFAULT: i32 = -14;
const ENODEV: i32 = -19;
const EINVAL: i32 = -22;
const EOPNOTSUPP: i32 = -95;
const ENOKEY: i32 = -126;
const EKEYREVOKED: i32 = -128;

/// The errno as Linux's negative value (the program's table is written in
/// those terms), read straight after the failing call.
fn raw_errno() -> i32 {
    -std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn add_key(ty: &CStr, desc: &CStr, payload: Option<&[u8]>, ring: i32) -> Result<i32, i32> {
    let (ptr, len) = match payload {
        Some(bytes) => (bytes.as_ptr() as usize, bytes.len()),
        None => (0, 0),
    };
    add_key_at(ty, desc, ptr, len, ring)
}

/// `add_key` with an explicit buffer pointer, so a NULL payload with a
/// non-zero length can be asked for (the measured answer is `EFAULT`).
fn add_key_at(ty: &CStr, desc: &CStr, ptr: usize, len: usize, ring: i32) -> Result<i32, i32> {
    let out = unsafe {
        libc::syscall(
            libc::SYS_add_key as libc::c_long,
            ty.as_ptr(),
            desc.as_ptr(),
            ptr,
            len as libc::c_long,
            ring as libc::c_long,
        )
    };
    if out < 0 {
        Err(raw_errno())
    } else {
        Ok(out as i32)
    }
}

fn keyctl(
    op: libc::c_long,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
) -> Result<libc::c_long, i32> {
    let out = unsafe { libc::syscall(libc::SYS_keyctl as libc::c_long, op, a2, a3, a4, a5) };
    if out < 0 { Err(raw_errno()) } else { Ok(out) }
}

fn request_key(ty: &CStr, desc: &CStr, callout: Option<&CStr>, dest: i32) -> Result<i32, i32> {
    let callout_ptr = callout.map(|c| c.as_ptr() as usize).unwrap_or(0);
    let out = unsafe {
        libc::syscall(
            libc::SYS_request_key as libc::c_long,
            ty.as_ptr(),
            desc.as_ptr(),
            callout_ptr,
            dest as libc::c_long,
        )
    };
    if out < 0 {
        Err(raw_errno())
    } else {
        Ok(out as i32)
    }
}

fn cstr(text: &str) -> CString {
    CString::new(text).expect("no interior NUL")
}

/// A description unique to *this run*, for every key the pin adds.
///
/// The session keyring outlives the program when it is run twice in one
/// session - a re-run, and `cargo xtask test-host`'s sweep of 240 programs - so
/// a fixed description makes the second run's `add_key` find the first run's
/// key: an invalidated one revives, a revoked one refuses the update, and the
/// state claims then answer from the previous run's leftovers. Measured while
/// fixing exactly that: `an invalidated key is ENOKEY` failed one run in three.
fn uniq(name: &str) -> CString {
    cstr(&std::format!("{name}-{}", std::process::id()))
}

fn euid() -> u32 {
    unsafe { libc::geteuid() }
}

fn egid() -> u32 {
    unsafe { libc::getegid() }
}

/// `keyctl(READ)` into a buffer: the answer's length, or the errno. A NULL
/// buffer is the size probe.
fn read_key(serial: i32, buffer: Option<&mut [u8]>) -> Result<usize, i32> {
    let (ptr, len) = match buffer {
        Some(bytes) => (bytes.as_mut_ptr() as usize, bytes.len()),
        None => (0, 0),
    };
    keyctl(KEYCTL_READ, serial as usize, ptr, len, 0).map(|len| len as usize)
}

fn describe(serial: i32, buffer: &mut [u8]) -> Result<usize, i32> {
    keyctl(
        KEYCTL_DESCRIBE,
        serial as usize,
        buffer.as_mut_ptr() as usize,
        buffer.len(),
        0,
    )
    .map(|len| len as usize)
}

fn describe_text(serial: i32) -> Option<String> {
    let mut buffer = std::vec![0u8; 512];
    let len = describe(serial, &mut buffer).ok()?;
    // DESCRIBE's answer is NUL-terminated and its length includes the NUL.
    let text = buffer[..len].split(|byte| *byte == 0).next()?;
    String::from_utf8(text.to_vec()).ok()
}

fn get_ring(spec: i32, create: bool) -> Result<i32, i32> {
    keyctl(
        KEYCTL_GET_KEYRING_ID,
        spec as usize,
        if create { 1 } else { 0 },
        0,
        0,
    )
    .map(|serial| serial as i32)
}

fn search(ring: i32, ty: &CStr, desc: &CStr) -> Result<i32, i32> {
    keyctl(
        KEYCTL_SEARCH,
        ring as usize,
        ty.as_ptr() as usize,
        desc.as_ptr() as usize,
        0,
    )
    .map(|serial| serial as i32)
}

fn main() {
    let mut r = Report::new();
    let session = get_ring(KEY_SPEC_SESSION_KEYRING, true).expect("the session keyring is made");
    r.check("a session keyring is made by the first ask", session > 0);
    r.check(
        "GET_KEYRING_ID answers the same serial for create=0",
        get_ring(KEY_SPEC_SESSION_KEYRING, false) == Ok(session),
    );
    r.check(
        "there is no thread keyring until one is asked for",
        get_ring(KEY_SPEC_THREAD_KEYRING, false) == Err(ENOKEY),
    );
    let thread = get_ring(KEY_SPEC_THREAD_KEYRING, true).expect("make a thread keyring");
    r.check(
        "create=1 makes the thread keyring and create=0 then reports it",
        thread > 0 && get_ring(KEY_SPEC_THREAD_KEYRING, false) == Ok(thread),
    );

    // --- a user key: add, read, describe, update --------------------------------
    let ty = cstr("user");
    let desc_text = std::format!("t-keyctl-payload-{}", std::process::id());
    let desc = cstr(&desc_text);
    let serial = add_key(&ty, &desc, Some(b"0123456789"), session).expect("add the user key");
    r.check("add_key answers a positive serial", serial > 0);
    let mut small = [0u8; 5];
    let answer = read_key(serial, Some(&mut small)).expect("read into a small buffer");
    r.check(
        "READ answers the whole length even when the buffer is short",
        answer == 10,
    );
    r.check(
        "READ copies nothing when the buffer is short",
        small == [0u8; 5],
    );
    r.check(
        "READ with a NULL buffer answers the length",
        read_key(serial, None) == Ok(10),
    );
    let mut big = [0u8; 64];
    let answer = read_key(serial, Some(&mut big)).expect("read into a big buffer");
    r.check(
        "READ copies the payload when the buffer can hold it",
        answer == 10 && &big[..10] == b"0123456789",
    );

    let expected_describe = std::format!("user;{};{};3f010000;{desc_text}", euid(), egid());
    r.check(
        "DESCRIBE is type;uid;gid;perm;desc with the measured default word",
        describe_text(serial).as_deref() == Some(expected_describe.as_str()),
    );
    let mut one = [0u8; 1];
    r.check(
        "DESCRIBE answers its length even into a one-byte buffer",
        // The length counts the terminating NUL (measured: 30 for a 29-byte
        // description).
        describe(serial, &mut one) == Ok(expected_describe.len() + 1),
    );

    // The second add of the same description updates in place, same serial.
    let again = add_key(&ty, &desc, Some(b"hello again"), session).expect("add again");
    r.check(
        "adding the same description answers the same serial",
        again == serial,
    );
    let mut updated = [0u8; 32];
    let len = read_key(serial, Some(&mut updated)).expect("read the update");
    r.check(
        "the second add updated the payload in place",
        len == 11 && &updated[..11] == b"hello again",
    );
    r.check(
        "UPDATE answers zero and the payload is the new one",
        keyctl(
            KEYCTL_UPDATE,
            serial as usize,
            b"tiny".as_ptr() as usize,
            4,
            0,
        ) == Ok(0)
            && read_key(serial, None) == Ok(4),
    );

    // --- add_key's refusals -----------------------------------------------------
    r.check(
        "an unknown type is ENODEV",
        add_key(
            &cstr("nonsense"),
            &uniq("t-keyctl-bad"),
            Some(b"x"),
            session,
        ) == Err(ENODEV),
    );
    let too_long = cstr(&"d".repeat(4096));
    r.check(
        "a 4 096-byte description is EINVAL",
        add_key(&ty, &too_long, Some(b"x"), session) == Err(EINVAL),
    );
    r.check(
        "a NULL payload with a length is EFAULT",
        add_key_at(&ty, &uniq("t-keyctl-null"), 0, 5, session) == Err(EFAULT),
    );
    r.check(
        "adding into a ring this task does not hold is ENOKEY",
        add_key(&ty, &uniq("t-keyctl-orphan"), Some(b"x"), 0x0fff_ffff) == Err(ENOKEY),
    );

    // --- keyrings: links, search, clear ----------------------------------------
    let in_ring = std::format!("t-keyctl-in-ring-{}", std::process::id());
    let deep_text = std::format!("t-keyctl-deep-{}", std::process::id());
    let ring_ty = cstr("keyring");
    let ring = add_key(&ring_ty, &uniq("t-keyctl-ring"), None, session).expect("make a keyring");
    r.check("a keyring is made like any other key", ring > 0);
    let key = add_key(&ty, &cstr(&in_ring), Some(b"payload"), ring).expect("key in ring");
    r.check(
        "SEARCH finds a key by type and description",
        search(session, &ty, &cstr(&in_ring)) == Ok(key),
    );
    let mut links = [0u8; 16];
    let len = read_key(ring, Some(&mut links)).expect("read a keyring");
    r.check(
        "a keyring's READ is its links, four bytes per serial, little-endian",
        len == 4 && u32::from_le_bytes([links[0], links[1], links[2], links[3]]) == key as u32,
    );
    // A nested keyring is reached through its parent.
    let nested = add_key(&ring_ty, &uniq("t-keyctl-nested"), None, ring).expect("nested ring");
    let deep = add_key(&ty, &cstr(&deep_text), Some(b"deep"), nested).expect("deep key");
    r.check(
        "SEARCH recurses into a nested keyring",
        search(session, &ty, &cstr(&deep_text)) == Ok(deep),
    );
    r.check(
        "UPDATE on a keyring is EOPNOTSUPP",
        keyctl(KEYCTL_UPDATE, ring as usize, b"x".as_ptr() as usize, 1, 0) == Err(EOPNOTSUPP),
    );
    r.check(
        "LINK moves a key into another ring",
        keyctl(KEYCTL_LINK, key as usize, session as usize, 0, 0) == Ok(0),
    );
    r.check(
        "UNLINK removes it",
        keyctl(KEYCTL_UNLINK, key as usize, session as usize, 0, 0) == Ok(0),
    );
    r.check(
        "UNLINK of a key that is not in that ring is ENOENT",
        keyctl(KEYCTL_UNLINK, key as usize, session as usize, 0, 0) == Err(ENOENT),
    );
    r.check(
        "CLEAR empties a keyring",
        keyctl(KEYCTL_CLEAR, ring as usize, 0, 0, 0) == Ok(0),
    );
    r.check(
        "a cleared keyring reads as no links",
        read_key(ring, None) == Ok(0),
    );

    // --- states: revoke, invalidate --------------------------------------------
    let owned = add_key(&ty, &uniq("t-keyctl-revoke"), Some(b"x"), session).expect("key to revoke");
    r.check(
        "REVOKE succeeds",
        keyctl(KEYCTL_REVOKE, owned as usize, 0, 0, 0) == Ok(0),
    );
    r.check(
        "a revoked key's READ is EKEYREVOKED",
        read_key(owned, None) == Err(EKEYREVOKED),
    );
    r.check(
        "a revoked key's DESCRIBE is EKEYREVOKED too",
        describe(owned, &mut [0u8; 64]) == Err(EKEYREVOKED),
    );
    let invalid =
        add_key(&ty, &uniq("t-keyctl-invalidate"), Some(b"x"), session).expect("key to invalidate");
    r.check(
        "INVALIDATE succeeds",
        keyctl(KEYCTL_INVALIDATE, invalid as usize, 0, 0, 0) == Ok(0),
    );
    // Invalidate is Linux's *asynchronous* retirement: it unlinks the key from
    // every keyring and schedules the destroy, so a READ by serial answers one
    // of two things depending on whether the garbage collector has run yet -
    // measured with strace on 7.2.11, the same binary in the same session
    // answering `EACCES` (unlinked, still alive) on one run and `ENOKEY`
    // (destroyed) on the next. This kernel's invalidate is synchronous and
    // answers `ENOKEY` every time; the claim below is the pair Linux can give,
    // and the exact answer is claimed on the machine.
    let invalidated_read = read_key(invalid, None);
    r.check(
        "an invalidated key is unreadable: ENOKEY, or EACCES before Linux destroys it",
        matches!(invalidated_read, Err(ENOKEY) | Err(EACCES)),
    );
    if !mpos_user_tests::host_run() {
        r.check(
            "this kernel retires it synchronously: READ is ENOKEY, not EACCES",
            invalidated_read == Err(ENOKEY),
        );
    }

    // --- possession is a permission class, not a findability filter ------------
    let lonely = add_key(&ty, &uniq("t-keyctl-lonely"), Some(b"x"), session).expect("lonely key");
    r.check(
        "UNLINK from the only ring succeeds",
        keyctl(KEYCTL_UNLINK, lonely as usize, session as usize, 0, 0) == Ok(0),
    );
    r.check(
        "an unlinked key is still found by serial, and READ is EACCES",
        read_key(lonely, None) == Err(EACCES),
    );
    r.check(
        "an unlinked key still DESCRIBEs (the owner keeps VIEW)",
        describe_text(lonely).map(|text| text.contains("t-keyctl-lonely")) == Some(true),
    );

    // --- privilege decides SETPERM ---------------------------------------------
    let perms = add_key(&ty, &uniq("t-keyctl-perm"), Some(b"x"), session).expect("key for perms");
    let setperm = keyctl(KEYCTL_SETPERM, perms as usize, 0x3f3f_3f3f, 0, 0);
    if euid() == 0 {
        r.check(
            "the privileged caller may rewrite the permission word",
            setperm == Ok(0),
        );
    } else {
        r.check(
            "an ordinary owner of a key is refused SETPERM with EACCES",
            setperm == Err(EACCES),
        );
    }

    // --- an unknown command is refused the same way -----------------------------
    r.check(
        "an unknown keyctl command is EOPNOTSUPP",
        keyctl(999, 0, 0, 0, 0) == Err(EOPNOTSUPP),
    );

    // --- a fork inherits the session keyring and nothing else ------------------
    add_key(
        &ty,
        &uniq("t-keyctl-in-process-ring"),
        Some(b"x"),
        KEY_SPEC_PROCESS_KEYRING,
    )
    .expect("key in the process ring");
    let parent_session = get_ring(KEY_SPEC_SESSION_KEYRING, false).expect("parent session");
    match unsafe { libc::fork() } {
        0 => {
            let ok = get_ring(KEY_SPEC_SESSION_KEYRING, false) == Ok(parent_session)
                && get_ring(KEY_SPEC_PROCESS_KEYRING, false) == Err(ENOKEY)
                && get_ring(KEY_SPEC_THREAD_KEYRING, false) == Err(ENOKEY);
            std::process::exit(if ok { 0 } else { 1 });
        }
        child if child > 0 => {
            let mut status = 0;
            unsafe { libc::waitpid(child, &mut status, 0) };
            r.check(
                "a forked child inherits the session ring and has no process ring",
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            );
        }
        _ => r.check("fork for the inheritance claim", false),
    }

    // --- request_key with no helper behind it ----------------------------------
    // Measured: a miss answers ENOKEY (the callout path exists, but a miss is
    // still a miss); the dest ring must be one this task holds, which the
    // session ring is here.
    r.check(
        "request_key for a missing key is ENOKEY, even with callout information",
        request_key(
            &ty,
            &cstr("t-keyctl-absent"),
            Some(&cstr("/nonexistent")),
            get_ring(KEY_SPEC_SESSION_KEYRING, false).expect("session ring"),
        ) == Err(ENOKEY),
    );

    // --- a join names a session ring -------------------------------------------
    let joined = keyctl(
        KEYCTL_JOIN_SESSION_KEYRING,
        cstr("t-keyctl-session").as_ptr() as usize,
        0,
        0,
        0,
    )
    .expect("join a named session ring");
    r.check("JOIN_SESSION_KEYRING answers a new serial", joined > 0);
    r.check(
        "GET_KEYRING_ID reports the joined session ring",
        get_ring(KEY_SPEC_SESSION_KEYRING, false) == Ok(joined as i32),
    );
    // The ring this task no longer holds can still be named by serial, and
    // asking to use it as request_key's destination is refused by permission,
    // not by existence (measured: EACCES).
    r.check(
        "request_key into a ring this task no longer holds is EACCES",
        request_key(&ty, &cstr("t-keyctl-absent"), None, session) == Err(EACCES),
    );

    // --- request_key with no helper behind it ----------------------------------

    r.finish();
}
