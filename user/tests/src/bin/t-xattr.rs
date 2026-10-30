//! t-xattr - the extended-attribute family has a store on the in-memory
//! filesystems, and its answers are Linux's.
//!
//! The contract this replaced was the family's first shape: every well-formed
//! request was validated and then refused with `EOPNOTSUPP` - honest for a
//! filesystem with no xattr support, but wrong for the ramfs/tmpfs this kernel
//! *does* keep, where Linux's tmpfs stores `user.*` attributes. A caller told
//! "not supported" goes looking for a different medium; a caller told
//! `ENODATA` knows the medium is fine and the name is not set. So the refusal
//! moved to the filesystems that really keep no store (ext4, procfs, devfs),
//! and the in-memory ones answer Linux's way.
//!
//! Measured on a current host (Linux 7.2.11, `/dev/shm`, uid 1000; the
//! instruments are the `.workbuddy/probe_xattr*.c` sketches, the table is
//! `docs/notes/` section 3.15):
//!
//!  * `setxattr`/`getxattr`/`removexattr` answer on a regular file, a directory
//!    and a unix socket; `user.*` is the namespace temp filesystems keep;
//!  * a zero-length buffer is the *length probe*: the answer is the value's
//!    length and the buffer pointer is never touched (`(void*)1` included);
//!  * a short buffer is `ERANGE`, a missing attribute `ENODATA`, an unreadable
//!    value pointer `EFAULT`, a value past 64 KiB `E2BIG`;
//!  * `XATTR_CREATE` on an existing attribute is `EEXIST`, `XATTR_REPLACE` on a
//!    missing one `ENODATA` - and **both bits together are not `EINVAL`**: they
//!    combine into "must exist and must not exist", so the answer follows the
//!    attribute's existence (the old pin asserted the opposite; the measurement
//!    is why the assertion changed);
//!  * an empty name is `ERANGE` and a bare namespace prefix (`"user."`) is
//!    `EINVAL`, and both are decided before the path is walked;
//!  * `listxattr` lays the names end to end, each NUL-terminated, so the total
//!    carries one separator per name - the last one included - and the probe
//!    answers the same way for a zero-length or a short buffer;
//!  * a FIFO, a device node and a symlink answer `EPERM` on set and
//!    `ENODATA`/`0` on get/list, because the rule is the generic layer's and
//!    those kinds keep no attributes at all;
//!  * the `f*` forms work over an `O_RDONLY` descriptor and over a directory
//!    descriptor; an `O_PATH` one is `EBADF` on Linux, which this kernel cannot
//!    claim because its `open` does not make one (a divergence named in the
//!    notes, not pinned here).
//!
//! # Divergences this pin records
//!
//! Linux's tmpfs also keeps `security.*` (world-readable) and `trusted.*`
//! (root-only) because it has LSM hooks and per-caller visibility to judge
//! them. This kernel has neither, so every namespace but `user.*` answers
//! `EOPNOTSUPP` - Linux's own answer for a namespace no filesystem handles.
//! On a host run those two claims accept Linux's measured `EPERM` for a
//! non-root caller, and the procfs set claim accepts the `EACCES` procfs
//! answers before it looks at attributes; the guest run pins this kernel.
//!
//! # Why every claim is `libc`
//!
//! The crate's rule allows `libc` only where the crate cannot express the
//! claim, and here that is the whole family: `rustix` has no xattr wrapper at
//! all - not the names, not the flags word, not the length-probe spelling - so
//! these are the clause's unbound call and go through `libc`'s named wrappers
//! rather than a raw `syscall(2)`. The old pin used the raw door because a
//! wrapper takes the value as a readable slice; the `EFAULT` claim here passes
//! address 1 through the wrapper instead, which is an address no process can
//! read on either machine.

use std::ffi::CString;
use std::io::Error;

use mpos_user_tests::Report;

const FILE: &str = "/tmp/t-xattr.file";
const DIR: &str = "/tmp/t-xattr.d";
const FIFO: &str = "/tmp/t-xattr.fifo";
const LINK: &str = "/tmp/t-xattr.link";
const SOCK: &str = "/tmp/t-xattr.sock";
const MNT: &str = "/tmp/t-xattr.mnt";
const PROCFS: &str = "/proc/self/status";

const XATTR_CREATE: i32 = 1;
const XATTR_REPLACE: i32 = 2;

fn cs(text: &str) -> CString {
    CString::new(text).expect("no interior NUL")
}

/// The errno of a call that answered `-1`, or `None` when it did not fail.
fn errno_of(ret: i64) -> Option<i32> {
    if ret == -1 {
        Error::last_os_error().raw_os_error()
    } else {
        None
    }
}

fn failed(ret: i64, want: i32) -> bool {
    errno_of(ret) == Some(want)
}

/// This kernel's answer, or - on a host run - the answer real Linux gives for
/// the same claim where the two are known to diverge. The guest run pins ours.
fn ours_or_linux(mine: bool, linux: bool) -> bool {
    mine || (mpos_user_tests::host_run() && linux)
}

fn setxattr(path: &str, name: &str, value: &[u8], flags: i32) -> i64 {
    let (p, n) = (cs(path), cs(name));
    unsafe {
        libc::setxattr(
            p.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            flags,
        ) as i64
    }
}

/// `setxattr` whose value pointer is address 1 - the `EFAULT` claim.
fn setxattr_bad_pointer(path: &str, name: &str, size: usize) -> i64 {
    let (p, n) = (cs(path), cs(name));
    unsafe { libc::setxattr(p.as_ptr(), n.as_ptr(), 1 as *const libc::c_void, size, 0) as i64 }
}

fn getxattr(path: &str, name: &str, buf: &mut [u8]) -> i64 {
    let (p, n) = (cs(path), cs(name));
    unsafe { libc::getxattr(p.as_ptr(), n.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) as i64 }
}

/// `getxattr`'s length probe: a zero size and a pointer no-one could read.
fn getxattr_probe(path: &str, name: &str) -> i64 {
    let (p, n) = (cs(path), cs(name));
    unsafe { libc::getxattr(p.as_ptr(), n.as_ptr(), 1 as *mut libc::c_void, 0) as i64 }
}

fn lgetxattr_probe(path: &str, name: &str) -> i64 {
    let (p, n) = (cs(path), cs(name));
    unsafe { libc::lgetxattr(p.as_ptr(), n.as_ptr(), 1 as *mut libc::c_void, 0) as i64 }
}

fn lsetxattr(path: &str, name: &str, value: &[u8]) -> i64 {
    let (p, n) = (cs(path), cs(name));
    unsafe {
        libc::lsetxattr(
            p.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        ) as i64
    }
}

fn listxattr(path: &str, buf: &mut [u8]) -> i64 {
    let p = cs(path);
    unsafe { libc::listxattr(p.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) as i64 }
}

fn listxattr_probe(path: &str) -> i64 {
    let p = cs(path);
    unsafe { libc::listxattr(p.as_ptr(), 1 as *mut libc::c_char, 0) as i64 }
}

fn llistxattr_probe(path: &str) -> i64 {
    let p = cs(path);
    unsafe { libc::llistxattr(p.as_ptr(), 1 as *mut libc::c_char, 0) as i64 }
}

fn removexattr(path: &str, name: &str) -> i64 {
    let (p, n) = (cs(path), cs(name));
    unsafe { libc::removexattr(p.as_ptr(), n.as_ptr()) as i64 }
}

fn fsetxattr(fd: i32, name: &str, value: &[u8]) -> i64 {
    let n = cs(name);
    unsafe { libc::fsetxattr(fd, n.as_ptr(), value.as_ptr().cast(), value.len(), 0) as i64 }
}

fn fgetxattr(fd: i32, name: &str, buf: &mut [u8]) -> i64 {
    let n = cs(name);
    unsafe { libc::fgetxattr(fd, n.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) as i64 }
}

fn flistxattr(fd: i32, buf: &mut [u8]) -> i64 {
    unsafe { libc::flistxattr(fd, buf.as_mut_ptr().cast(), buf.len()) as i64 }
}

fn fremovexattr(fd: i32, name: &str) -> i64 {
    let n = cs(name);
    unsafe { libc::fremovexattr(fd, n.as_ptr()) as i64 }
}

/// The long name: 256 bytes, one past the limit.
fn long_name() -> String {
    let mut name = String::from("user.");
    name.push_str(&"a".repeat(251));
    name
}

fn main() {
    let mut r = Report::new();

    std::fs::create_dir_all(DIR).expect("fixture directory");
    std::fs::write(FILE, b"payload").expect("fixture file");

    // ---------------------------------------------------------------- store
    r.check(
        "a set on a fresh file answers 0",
        setxattr(FILE, "user.a", b"v1", 0) == 0,
    );
    r.check(
        "the length probe answers the value's length and never reads the pointer",
        getxattr_probe(FILE, "user.a") == 2,
    );
    let mut buf = [0u8; 2];
    r.check(
        "the value reads back",
        getxattr(FILE, "user.a", &mut buf) == 2 && &buf == b"v1",
    );
    let mut short = [0u8; 1];
    r.check(
        "a buffer one byte short is ERANGE",
        failed(getxattr(FILE, "user.a", &mut short), libc::ERANGE),
    );
    r.check(
        "an attribute that was never set is ENODATA",
        failed(getxattr(FILE, "user.absent", &mut buf), libc::ENODATA),
    );
    r.check(
        "a value pointer this kernel cannot read is EFAULT",
        failed(setxattr_bad_pointer(FILE, "user.a", 1), libc::EFAULT),
    );
    r.check(
        "a directory keeps user.* too",
        setxattr(DIR, "user.a", b"v", 0) == 0 && getxattr_probe(DIR, "user.a") == 1,
    );

    // --------------------------------------------------------------- flags
    r.check(
        "XATTR_CREATE on an attribute that exists is EEXIST",
        failed(setxattr(FILE, "user.a", b"v2", XATTR_CREATE), libc::EEXIST),
    );
    r.check(
        "XATTR_REPLACE on an attribute that exists answers 0 and replaces it",
        setxattr(FILE, "user.a", b"v2", XATTR_REPLACE) == 0 && getxattr_probe(FILE, "user.a") == 2,
    );
    r.check(
        "XATTR_REPLACE on an attribute that does not exist is ENODATA",
        failed(
            setxattr(FILE, "user.absent", b"v", XATTR_REPLACE),
            libc::ENODATA,
        ),
    );
    r.check(
        "an undefined flags bit is EINVAL",
        failed(setxattr(FILE, "user.a", b"v", 0x4), libc::EINVAL),
    );
    r.check(
        "a bad flags word is EINVAL even on a missing path",
        failed(
            setxattr("/tmp/definitely-absent", "user.a", b"v", 0x4),
            libc::EINVAL,
        ),
    );
    // Measured: the two bits are not contradictory, so both together follow the
    // attribute's existence like either bit alone. The old pin asserted EINVAL.
    r.check(
        "CREATE|REPLACE on an existing attribute is EEXIST, not EINVAL",
        failed(
            setxattr(FILE, "user.a", b"v", XATTR_CREATE | XATTR_REPLACE),
            libc::EEXIST,
        ),
    );
    r.check(
        "CREATE|REPLACE on a missing one is ENODATA, not EINVAL",
        failed(
            setxattr(FILE, "user.absent", b"v", XATTR_CREATE | XATTR_REPLACE),
            libc::ENODATA,
        ),
    );

    // ---------------------------------------------------------------- limits
    let big = vec![b'v'; 65536];
    r.check(
        "a 64 KiB value lands and reads back",
        setxattr(FILE, "user.big", &big, 0) == 0 && getxattr_probe(FILE, "user.big") == 65536,
    );
    r.check(
        "one byte past 64 KiB is E2BIG",
        failed(
            setxattr(FILE, "user.big", &vec![b'v'; 65537], 0),
            libc::E2BIG,
        ),
    );
    r.check(
        "the empty value is a value: set answers 0, the probe answers 0",
        setxattr(FILE, "user.empty", b"", 0) == 0 && getxattr_probe(FILE, "user.empty") == 0,
    );
    r.check(
        "a name past 255 bytes is ERANGE",
        failed(setxattr(FILE, &long_name(), b"v", 0), libc::ERANGE),
    );
    r.check(
        "an empty name is ERANGE before the path is walked",
        failed(
            setxattr("/tmp/definitely-absent", "", b"v", 0),
            libc::ERANGE,
        ),
    );
    r.check(
        "a bare namespace prefix is EINVAL",
        failed(setxattr(FILE, "user.", b"v", 0), libc::EINVAL),
    );

    // ------------------------------------------------------- namespaces
    // The kernel keeps `user.*` only. `system.*` and an unprefixed name are
    // Linux's own "no handler" answer; `security.*`/`trusted.*` are refused the
    // same way because this kernel has no LSM and no per-caller visibility.
    fn namespace_refused(path: &str, name: &str) -> bool {
        let mut probe = [0u8; 8];
        failed(setxattr(path, name, b"v", 0), libc::EOPNOTSUPP)
            && failed(getxattr(path, name, &mut probe), libc::EOPNOTSUPP)
    }
    r.check(
        "system.* is EOPNOTSUPP to set and to get",
        namespace_refused(FILE, "system.foo"),
    );
    r.check(
        "a name with no namespace is EOPNOTSUPP too",
        namespace_refused(FILE, "foo.bar"),
    );
    for name in ["security.foo", "trusted.foo"] {
        r.check(
            format!("{name} is refused (a named divergence: EOPNOTSUPP on a kernel with no LSM)")
                .as_str(),
            ours_or_linux(
                namespace_refused(FILE, name),
                failed(setxattr(FILE, name, b"v", 0), libc::EPERM),
            ),
        );
    }

    // ------------------------------------------------------------- listing
    let expected = ("user.a".len() + 1 + "user.big".len() + 1 + "user.empty".len() + 1) as i64;
    r.check(
        "the list probe counts every name and its separator",
        listxattr_probe(FILE) == expected,
    );
    let mut names = vec![0u8; listxattr_probe(FILE) as usize];
    // Each name is NUL-terminated (so the total carries one separator per name,
    // the last one included - measured: one attribute of six bytes lists as
    // seven). The order is not a promise, so the claim is about the encoding and
    // the set, not the sequence.
    let listed = |names: &[u8], want: &[&[u8]]| {
        let mut got: Vec<&[u8]> = names
            .split(|b| *b == 0)
            .filter(|part| !part.is_empty())
            .collect();
        let mut want: Vec<&[u8]> = want.to_vec();
        got.sort();
        want.sort();
        got == want
    };
    r.check(
        "the exact-size buffer holds every name NUL-terminated",
        listxattr(FILE, &mut names) == names.len() as i64
            && names.last() == Some(&0)
            && listed(&names, &[b"user.a", b"user.big", b"user.empty"]),
    );
    let mut small = vec![0u8; names.len() - 1];
    r.check(
        "a short list buffer is ERANGE",
        failed(listxattr(FILE, &mut small), libc::ERANGE),
    );

    // ------------------------------------------------------------- removal
    r.check(
        "removexattr answers 0 and the attribute is gone",
        removexattr(FILE, "user.a") == 0 && getxattr_probe(FILE, "user.a") == -1,
    );
    r.check(
        "removing an attribute that is not there is ENODATA",
        failed(removexattr(FILE, "user.a"), libc::ENODATA),
    );

    // -------------------------------------------------------------- kinds
    let fifo = cs(FIFO);
    unsafe {
        libc::unlink(fifo.as_ptr());
        libc::mkfifo(fifo.as_ptr(), 0o600);
    }
    r.check(
        "a FIFO refuses user.* with EPERM",
        failed(setxattr(FIFO, "user.a", b"v", 0), libc::EPERM),
    );
    r.check(
        "a FIFO answers ENODATA to a get and lists nothing",
        failed(getxattr_probe(FIFO, "user.a"), libc::ENODATA) && listxattr_probe(FIFO) == 0,
    );

    let _ = std::fs::remove_file(LINK);
    std::os::unix::fs::symlink(FILE, LINK).expect("fixture symlink");
    r.check(
        "the l-form refuses a symlink's own user.* with EPERM",
        failed(lsetxattr(LINK, "user.a", b"v"), libc::EPERM),
    );
    r.check(
        "the l-form answers ENODATA and lists nothing on a symlink",
        lgetxattr_probe(LINK, "user.a") == -1 && llistxattr_probe(LINK) == 0,
    );
    r.check(
        "the following form reaches the symlink's target",
        setxattr(LINK, "user.b", b"v", 0) == 0,
    );

    let _ = std::fs::remove_file(SOCK);
    let listener = std::os::unix::net::UnixListener::bind(SOCK).expect("fixture socket");
    r.check(
        "a unix socket keeps user.* (the generic rule lets it)",
        setxattr(SOCK, "user.a", b"v", 0) == 0 && getxattr_probe(SOCK, "user.a") == 1,
    );
    drop(listener);

    // ------------------------------------------------------------- descriptors
    use std::os::unix::io::AsRawFd;
    let file = std::fs::File::open(FILE).expect("fixture file");
    let fd = file.as_raw_fd();
    let mut one = [0u8; 1];
    r.check(
        "the f-forms work over an O_RDONLY descriptor",
        fsetxattr(fd, "user.fd", b"v") == 0 && fgetxattr(fd, "user.fd", &mut one) == 1,
    );
    let mut fd_names = vec![0u8; flistxattr(fd, &mut []) as usize];
    r.check(
        "flistxattr lists the descriptor's file",
        flistxattr(fd, &mut fd_names) == fd_names.len() as i64,
    );
    r.check(
        "fremovexattr removes it",
        fremovexattr(fd, "user.fd") == 0 && fgetxattr(fd, "user.fd", &mut one) == -1,
    );
    r.check(
        "fsetxattr on a closed descriptor is EBADF",
        failed(fsetxattr(9999, "user.a", b"v"), libc::EBADF),
    );
    r.check(
        "flistxattr on a closed descriptor is EBADF",
        failed(flistxattr(9999, &mut one), libc::EBADF),
    );

    // ------------------------------------- a filesystem with no store at all
    // procfs keeps no attributes. On this kernel the answer is the family's
    // refusal for every door; Linux refuses a non-root set with EACCES before it
    // looks at attributes, which is the divergence a host run accepts.
    r.check(
        "a filesystem with no store answers EOPNOTSUPP, not ENOSYS",
        ours_or_linux(
            failed(setxattr(PROCFS, "user.a", b"v", 0), libc::EOPNOTSUPP)
                && failed(getxattr_probe(PROCFS, "user.a"), libc::EOPNOTSUPP),
            failed(setxattr(PROCFS, "user.a", b"v", 0), libc::EACCES)
                && failed(getxattr_probe(PROCFS, "user.a"), libc::EOPNOTSUPP),
        ),
    );
    // A missing path is the resolver's answer, never the store's: the refusal is
    // only meaningful after the ordinary argument checks ran.
    r.check(
        "setxattr on a missing path is ENOENT",
        failed(
            setxattr("/tmp/definitely-absent", "user.a", b"v", 0),
            libc::ENOENT,
        ),
    );
    r.check(
        "getxattr on a missing path is ENOENT",
        failed(
            getxattr_probe("/tmp/definitely-absent", "user.a"),
            libc::ENOENT,
        ),
    );

    // ------------------------------------------------------------- tmpfs mount
    std::fs::create_dir_all(MNT).expect("mount point");
    let (source, target, fstype) = (cs("tmpfs"), cs(MNT), cs("tmpfs"));
    let mounted = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        ) == 0
    };
    // Mounting needs privilege, which the payload run has and a host reference
    // run may not; the guest run pins the mount, the host run records that it
    // could not try.
    r.check(
        "a tmpfs can be mounted over the fixture directory",
        mounted || mpos_user_tests::host_run(),
    );
    if mounted {
        let inside = format!("{MNT}/value");
        std::fs::write(&inside, b"payload").expect("file inside the mount");
        r.check(
            "a tmpfs mount keeps user.* like the ramfs root does",
            setxattr(&inside, "user.a", b"v", 0) == 0 && getxattr_probe(&inside, "user.a") == 1,
        );
        unsafe {
            libc::umount2(target.as_ptr(), 0);
        }
    }

    // ------------------------------------- a mount's room covers its attributes
    //
    // This is this kernel's own bound, not Linux's: a tmpfs here spends its
    // `size=` on everything it keeps, attributes included, while Linux's tmpfs
    // counts only page cache - measured, forty 64 KiB attributes landed on a
    // mount sized 1 MiB (`docs/notes/` §3.19). The guest run pins the bound; a
    // host run cannot (Linux accepts all three), so the claim is ours-only and
    // the divergence is named where the store is.
    let opts = cs("size=64k");
    let sized = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            opts.as_ptr().cast(),
        ) == 0
    };
    if sized {
        let inside = format!("{MNT}/kept");
        std::fs::write(&inside, b"").expect("an empty file in the sized mount");
        let value = vec![0u8; 22 * 1024];
        r.check(
            "a size=64k mount holds two 22 KiB attributes",
            setxattr(&inside, "user.a", &value, 0) == 0
                && setxattr(&inside, "user.b", &value, 0) == 0,
        );
        r.check(
            "a mount's size= covers the attributes it keeps, so the third is ENOSPC",
            failed(setxattr(&inside, "user.c", &value, 0), libc::ENOSPC)
                || mpos_user_tests::host_run(),
        );
        r.check(
            "an attribute given back is room for the next one",
            removexattr(&inside, "user.b") == 0 && setxattr(&inside, "user.c", &value, 0) == 0,
        );
        unsafe {
            libc::umount2(target.as_ptr(), 0);
        }
    }

    // --------------------------------------------------------------- cleanup
    drop(file);
    let _ = std::fs::remove_dir_all(DIR);
    let _ = std::fs::remove_file(FILE);
    let _ = std::fs::remove_file(FIFO);
    let _ = std::fs::remove_file(LINK);
    let _ = std::fs::remove_file(SOCK);
    let _ = std::fs::remove_dir(MNT);

    r.finish()
}
