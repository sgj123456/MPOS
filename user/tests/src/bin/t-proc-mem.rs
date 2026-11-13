//! `/proc/<pid>/mem`: another task's address space as one byte stream, addressed
//! by virtual address.
//!
//! Measured on a current host first (Linux 7.2.14, `build/scratch/pmem/`,
//! `gcc -O1`), uid 1000:
//!
//! ```text
//! open /proc/self/mem     mode 0600, st_size 0, for O_RDONLY and O_RDWR alike
//! pread 16 at base+1      the bytes - the offset is the address, byte granular
//! pread 16, untouched     sixteen zeros (a read fault maps the zero page)
//! pread 16 at a hole      short: the 8 bytes before it (rc=8)
//! pwrite 8 across it      short: the 4 bytes before it (rc=4)
//! pread at vaddr 0        -1 EIO(5);   at 0x7fff_fff000: EIO; at 1<<47: EIO
//! pwrite 8 at PROT_READ   rc=8, and the child reads the bytes back (FOLL_FORCE)
//! pwrite 8 at PROT_NONE   rc=8, and a read of it answers the same bytes
//! pwrite through O_RDONLY -1 EBADF(9);  pread through O_WRONLY -1 EBADF(9)
//! zombie's mem open       -1 ESRCH(3);  after waitpid: -1 ENOENT(2)
//! stranger's open (65534) -1 EACCES(13), for a zombie as for a live task
//! ```
//!
//! Every claim below is a **movement** rather than a constant: a mapping is
//! made, touched, and then asked about; a hole is made by unmapping and the
//! short answer is compared with the bytes that were left on the near side; a
//! read-only page is written through the stream and the page's own owner reads
//! the value back.
//!
//! What is *not* claimed, and why (the kernel names these gaps in `docs/notes/`
//! §6.152): the descriptor holds no `mm`, so a target that dies after the open
//! answers `ESRCH` where Linux keeps serving from the captured address space;
//! there is no swap and no huge page here, so neither bit can be produced.
//! `MPOS_ON_HOST=1` runs every claim below (except the guest-only stranger, which
//! needs root to drop) against real Linux.

use std::ffi::CString;
use std::os::fd::RawFd;
use std::os::unix::fs::PermissionsExt;

use mpos_user_tests::{Report, host_run};

const PAGE: usize = 4096;

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Open a task's `mem` node with the given flags, answering the errno instead
/// when the open is refused.
/// Drop one capability from the calling task's own effective and permitted sets.
///
/// The `capset` ABI is Linux's: a version word and a pid (0 means "me"), then two
/// `{effective, permitted, inheritable}` words - the second only exists for
/// capabilities past 31. Measured on 7.2.14: a process may always *lower* its own
/// sets, and clearing a bit from both words is what makes `ptrace_may_access`
/// answer "no" for a caller that is otherwise root.
fn drop_capability(bit: u32) -> bool {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    let mut header = Header {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [Data::default(); 2];
    let got = unsafe {
        libc::syscall(
            libc::SYS_capget,
            &mut header as *mut Header,
            data.as_mut_ptr(),
        )
    };
    if got != 0 {
        return false;
    }
    let word = (bit / 32) as usize;
    let mask = 1u32 << (bit % 32);
    data[word].effective &= !mask;
    data[word].permitted &= !mask;
    let set = unsafe { libc::syscall(libc::SYS_capset, &header as *const Header, data.as_ptr()) };
    set == 0
}

fn open_mem(pid: i32, flags: i32) -> Result<RawFd, i32> {
    let path = CString::new(format!("/proc/{pid}/mem")).expect("no interior nul");
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(last_errno());
    }
    Ok(fd)
}

/// `pread` at a virtual address: the file's offset *is* the address.
fn pread_at(fd: RawFd, addr: usize, len: usize) -> Result<Vec<u8>, i32> {
    let mut buf = vec![0u8; len];
    let n = unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), len, addr as libc::off_t) };
    if n < 0 {
        return Err(last_errno());
    }
    buf.truncate(n as usize);
    Ok(buf)
}

/// `pwrite` at a virtual address - what a debugger's poke uses.
fn pwrite_at(fd: RawFd, addr: usize, bytes: &[u8]) -> Result<usize, i32> {
    let n = unsafe { libc::pwrite(fd, bytes.as_ptr().cast(), bytes.len(), addr as libc::off_t) };
    if n < 0 {
        return Err(last_errno());
    }
    Ok(n as usize)
}

fn map(pages: usize, prot: i32, flags: i32) -> *mut u8 {
    let len = pages * PAGE;
    let p = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, -1, 0) };
    assert!(p != libc::MAP_FAILED, "mmap of {pages} pages failed");
    p.cast()
}

/// Fault a page in by storing into it, and keep the store from being elided.
fn touch(p: *mut u8, len: usize) {
    unsafe {
        std::ptr::write_bytes(p, 0x41, len);
        std::hint::black_box(p);
    }
}

fn unmap(p: *mut u8, pages: usize) {
    let len = pages * PAGE;
    unsafe {
        libc::munmap(p.cast(), len);
    }
}

/// What `/proc/<pid>/stat`'s state word says, so a test can wait for a child to
/// become a zombie rather than guess at a sleep.
fn state_of(pid: i32) -> char {
    let path = format!("/proc/{pid}/stat");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return ' ';
    };
    // The state word is the first field after the command's `)`, which is the
    // last `)` in the line because the command is parenthesised.
    let Some(close) = text.rfind(')') else {
        return ' ';
    };
    text[close + 1..]
        .split_whitespace()
        .next()
        .and_then(|word| word.chars().next())
        .unwrap_or(' ')
}

fn wait_for_zombie(pid: i32) -> char {
    for _ in 0..2000 {
        let state = state_of(pid);
        if state == 'Z' || state == 'X' {
            return state;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    state_of(pid)
}

fn main() {
    let mut report = Report::new();
    let host = host_run();
    let me = unsafe { libc::getpid() };

    // ---- the file's own shape, which is `pagemap`'s sibling -------------------
    let fd = open_mem(me, libc::O_RDWR).expect("own mem node opens");
    let meta = std::fs::metadata(format!("/proc/{me}/mem")).expect("fstat by path");
    let mode = meta.permissions().mode() & 0o7777;
    report.check(
        "the mem node opens and reports 0600 with no length",
        mode == 0o600 && meta.len() == 0,
    );

    // ---- a mapped address is read byte for byte ------------------------------
    let page = map(
        1,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
    );
    touch(page, PAGE);
    let mut pattern = [0u8; 32];
    for (i, byte) in pattern.iter_mut().enumerate() {
        *byte = (i as u8).wrapping_mul(3).wrapping_add(1);
    }
    unsafe {
        std::ptr::copy_nonoverlapping(pattern.as_ptr(), page, pattern.len());
        std::hint::black_box(page);
    }
    let base = page as usize;
    let read = pread_at(fd, base, 32).expect("read at a mapped address");
    report.check(
        "an address inside a mapping is served byte for byte",
        read == pattern,
    );
    // The offset is the address rather than a page index, so an unaligned one
    // is served from the byte it names - measured, `pread` of 16 at `base + 1`.
    let mut expected = vec![0u8; 16];
    unsafe { std::ptr::copy_nonoverlapping(page.add(1), expected.as_mut_ptr(), 16) };
    let unaligned = pread_at(fd, base + 1, 16).expect("read at an unaligned address");
    report.check(
        "the offset is the address, not a page index",
        unaligned == expected,
    );

    // ---- a page the task never touched reads zeros ---------------------------
    let lazy = map(
        4,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
    );
    touch(lazy, PAGE);
    let zeros = pread_at(fd, lazy as usize + 2 * PAGE, 16).expect("read at an untouched page");
    report.check(
        "an untouched page of a mapping reads zeros",
        zeros.len() == 16 && zeros.iter().all(|&b| b == 0),
    );
    unmap(lazy, 4);

    // ---- a transfer that runs off the mapping answers what it had ------------
    let eight = map(
        8,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
    );
    touch(eight, 8 * PAGE);
    unmap(unsafe { eight.add(4 * PAGE) }, 4);
    let eight_base = eight as usize;
    let short = pread_at(fd, eight_base + 4 * PAGE - 8, 16).expect("read across the hole");
    let mut near = vec![0u8; 8];
    unsafe { std::ptr::copy_nonoverlapping(eight.add(4 * PAGE - 8), near.as_mut_ptr(), 8) };
    report.check(
        "a read that runs off the mapping answers the bytes before the hole",
        short.len() == 8 && short == near,
    );
    let written = [0x41u8, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48];
    let took = pwrite_at(fd, eight_base + 4 * PAGE - 4, &written).expect("write across the hole");
    let back = pread_at(fd, eight_base + 4 * PAGE - 4, 4).expect("read what the write left");
    report.check(
        "a write that runs off the mapping answers what it took",
        took == 4 && back == written[..4],
    );
    unmap(eight, 4);

    // ---- a page in no mapping is EIO -----------------------------------------
    report.check(
        "an address in no mapping is EIO (vaddr 0)",
        pread_at(fd, 0, 16).err() == Some(libc::EIO),
    );
    let top = 1usize << 47;
    report.check(
        "an address at the top of the user half is EIO",
        pread_at(fd, top, 16).err() == Some(libc::EIO),
    );

    // ---- the mode refusals are the descriptor's ------------------------------
    let readable = open_mem(me, libc::O_RDONLY).expect("own mem node opens read-only");
    report.check(
        "a write through a read-only descriptor is EBADF",
        pwrite_at(readable, base, &[0u8; 8]).err() == Some(libc::EBADF),
    );
    unsafe { libc::close(readable) };
    let writable = open_mem(me, libc::O_WRONLY).expect("own mem node opens write-only");
    report.check(
        "a read through a write-only descriptor is EBADF",
        pread_at(writable, base, 8).err() == Some(libc::EBADF),
    );
    unsafe { libc::close(writable) };

    // ---- a child's memory, written through pages it cannot write ------------
    //
    // The child maps one `PROT_READ` page, one `PROT_NONE` page, and one private
    // page it then **forks over**: the last is the copy-on-write half, and it is
    // why the stream's write has to break the page before it stores. A write
    // that stored where the page is would land in the frame the grandchild also
    // maps, and the grandchild would read the parent's bytes.
    let mut up = [0i32; 2];
    let mut down = [0i32; 2];
    let mut cval = [0i32; 2];
    let mut cgo = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(up.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { libc::pipe(down.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { libc::pipe(cval.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { libc::pipe(cgo.as_mut_ptr()) }, 0);
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        unsafe {
            libc::close(up[0]);
            libc::close(down[1]);
            libc::close(cval[0]);
        }
        let ro = map(1, libc::PROT_READ, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS);
        let none = map(1, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS);
        let cow = map(
            1,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
        );
        unsafe {
            *cow = b'A';
            std::hint::black_box(cow);
        }
        let grandchild = unsafe { libc::fork() };
        if grandchild == 0 {
            // The grandchild's own view of the page it now shares, read **after**
            // the parent's poke: 'A' unless the write failed to break the copy.
            // Reading it before the poke would make the claim pass either way,
            // which is the shape of test that measures nothing.
            unsafe {
                libc::close(up[1]);
                libc::close(down[0]);
                libc::close(cgo[1]);
            }
            let mut go = [0u8; 1];
            if unsafe { libc::read(cgo[0], go.as_mut_ptr().cast(), 1) } != 1 {
                unsafe { libc::_exit(7) };
            }
            let seen = unsafe { *cow };
            let wrote = unsafe { libc::write(cval[1], (&seen as *const u8).cast(), 1) };
            unsafe { libc::_exit(if wrote == 1 { 0 } else { 4 }) };
        }
        // The 64-bit addresses cross as bytes, one `u64` each.
        let addresses = [
            ro as usize as u64,
            none as usize as u64,
            cow as usize as u64,
        ];
        let sent = unsafe {
            libc::write(
                up[1],
                addresses.as_ptr().cast(),
                std::mem::size_of_val(&addresses),
            )
        };
        if sent as usize != std::mem::size_of_val(&addresses) {
            unsafe { libc::_exit(2) };
        }
        let mut go = [0u8; 1];
        if unsafe { libc::read(down[0], go.as_mut_ptr().cast(), 1) } != 1 {
            unsafe { libc::_exit(5) };
        }
        let mut answer = [0u8; 9];
        unsafe {
            std::ptr::copy_nonoverlapping(ro, answer.as_mut_ptr(), 8);
            answer[8] = *cow;
            std::hint::black_box(ro);
            std::hint::black_box(cow);
        }
        let sent = unsafe { libc::write(up[1], answer.as_ptr().cast(), answer.len()) };
        unsafe { libc::_exit(if sent as usize == answer.len() { 0 } else { 6 }) };
    }
    unsafe {
        libc::close(up[1]);
        libc::close(down[0]);
        libc::close(cval[1]);
    }
    let mut addresses = [0u64; 3];
    let got = unsafe {
        libc::read(
            up[0],
            addresses.as_mut_ptr().cast(),
            std::mem::size_of_val(&addresses),
        )
    };
    assert_eq!(got as usize, std::mem::size_of_val(&addresses));
    let ro = addresses[0] as usize;
    let none = addresses[1] as usize;
    let cow = addresses[2] as usize;
    let child_fd = open_mem(child, libc::O_RDWR).expect("a child's mem node opens for its parent");
    let poke = [0x59u8, 0x0a, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
    let forced = pwrite_at(child_fd, ro, &poke).expect("write into a read-only page");
    let none_written = pwrite_at(child_fd, none, &poke).expect("write into a no-access page");
    let none_read = pread_at(child_fd, none, poke.len()).expect("read back the no-access page");
    let broke = pwrite_at(child_fd, cow, &[0x42]).expect("write into a shared page");
    // The poke has landed: let the grandchild look now, and only now. Its answer
    // is read before the child is released, so the two readers cannot race.
    assert_eq!(unsafe { libc::write(cgo[1], b"g".as_ptr().cast(), 1) }, 1);
    let mut grandchild_view = [0u8; 1];
    assert_eq!(
        unsafe { libc::read(cval[0], grandchild_view.as_mut_ptr().cast(), 1) },
        1
    );
    assert_eq!(unsafe { libc::write(down[1], b"g".as_ptr().cast(), 1) }, 1);
    let mut answer = [0u8; 9];
    assert_eq!(
        unsafe { libc::read(up[0], answer.as_mut_ptr().cast(), answer.len()) },
        answer.len() as isize
    );
    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
    let child_exit = (status >> 8) & 0xff;
    report.check(
        "a write into a read-only page is served, and the child sees it (FOLL_FORCE)",
        forced == poke.len() && child_exit == 0 && answer[..8] == poke,
    );
    report.check(
        "a page with no access at all takes the write and answers the read",
        none_written == poke.len() && none_read == poke,
    );
    report.check(
        "a write into a page a fork shares is broken first, so the fork keeps its copy",
        broke == 1 && answer[8] == 0x42 && grandchild_view[0] == b'A',
    );
    unsafe {
        libc::close(child_fd);
        libc::close(up[0]);
        libc::close(down[1]);
        libc::close(cval[0]);
        libc::close(cgo[1]);
    }

    // ---- a same-uid target that made itself undumpable ----------------------
    //
    // The mode word cannot see this one: the owner is the caller, `0600` allows
    // it, and the refusal comes from the ptrace question alone - which is why the
    // node asks `describes_task` as well as the open gate. Measured on Linux
    // 7.2.14 as uid 1000: `PR_SET_DUMPABLE 0` in a child, then the parent's open
    // of `/proc/<pid>/mem` answers `EACCES(13)`, while a dumpable child's open
    // succeeds.
    //
    // Guest-only, and for a reason worth writing down: the two sides must be the
    // *same* uid, and the opener must not hold `CAP_SYS_PTRACE` - which the payload
    // has in the guest because it runs as root, and root's capability answers yes
    // for a dumpable and an undumpable target alike. So the pair is two children
    // dropped to 65534 asking about each other, and a host run (uid 1000, cannot
    // drop) skips it the way it skips the stranger.
    if host {
        println!(
            "note: the undumpable pair needs two dropped same-uid children, skipped on the host"
        );
    } else {
        let mut pair = [0i32; 2];
        for (index, sealed) in [true, false].into_iter().enumerate() {
            let target = unsafe { libc::fork() };
            assert!(target >= 0, "fork failed");
            if target == 0 {
                unsafe {
                    libc::setresgid(65534, 65534, 65534);
                    libc::setresuid(65534, 65534, 65534);
                    // `setresuid` out of root clears dumpability, exactly as
                    // Linux's `commit_creds` does, so the sibling that must be
                    // readable has to ask for it back. (Measured on this machine:
                    // without this line both nodes read as root's, because
                    // `task_dump_owner` answers root for a task that is not
                    // dumpable - which is the security property, not an accident.)
                    libc::prctl(libc::PR_SET_DUMPABLE, if sealed { 0 } else { 1 }, 0, 0, 0);
                    std::thread::sleep(std::time::Duration::from_secs(30));
                    libc::_exit(0);
                }
            }
            pair[index] = target;
        }
        // Let both children run their `setresuid`/`prctl` before they are asked
        // about; a target still root-owned would answer the wrong question.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let prober = unsafe { libc::fork() };
        assert!(prober >= 0, "fork failed");
        if prober == 0 {
            unsafe {
                libc::setresgid(65534, 65534, 65534);
                libc::setresuid(65534, 65534, 65534);
            }
            let sealed_refused = open_mem(pair[0], libc::O_RDONLY).err();
            let dumpable_served = match open_mem(pair[1], libc::O_RDONLY) {
                Ok(fd) => {
                    unsafe { libc::close(fd) };
                    true
                }
                Err(_) => false,
            };
            let ok = sealed_refused == Some(libc::EACCES) && dumpable_served;
            unsafe { libc::_exit(if ok { 0 } else { 1 }) };
        }
        let mut status = 0;
        unsafe { libc::waitpid(prober, &mut status, 0) };
        let exit = (status >> 8) & 0xff;
        for target in pair {
            unsafe {
                libc::kill(target, libc::SIGKILL);
                libc::waitpid(target, &mut status, 0);
            }
        }
        report.check(
            "a same-uid target that made itself undumpable is EACCES, a dumpable one is served",
            exit == 0,
        );

        // The mode word cannot pin `describes_task` on its own: a target that is
        // not dumpable reads as *root's* node, so a stranger is refused by the
        // mode before the ptrace question is ever asked (cf4 - removing
        // `describes_task` from the node still passed every claim above). The
        // shape that separates them is a **root** caller that dropped
        // `CAP_SYS_PTRACE`: the node's `0600` owner lets it in, and only the
        // ptrace question says no - which is Linux's answer too (measured as
        // uid 1000: a same-uid undumpable target is `EACCES`).
        let quiet = unsafe { libc::fork() };
        assert!(quiet >= 0, "fork failed");
        if quiet == 0 {
            unsafe {
                libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
                std::thread::sleep(std::time::Duration::from_secs(30));
                libc::_exit(0);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        let capped = unsafe { libc::fork() };
        assert!(capped >= 0, "fork failed");
        if capped == 0 {
            let dropped = drop_capability(19);
            let refused = open_mem(quiet, libc::O_RDONLY).err();
            let ok = dropped && refused == Some(libc::EACCES);
            unsafe {
                libc::_exit(if ok {
                    0
                } else if dropped {
                    1
                } else {
                    2
                })
            };
        }
        let mut status = 0;
        unsafe { libc::waitpid(capped, &mut status, 0) };
        let exit = (status >> 8) & 0xff;
        unsafe {
            libc::kill(quiet, libc::SIGKILL);
            libc::waitpid(quiet, &mut status, 0);
        }
        report.check(
            "a root caller that dropped CAP_SYS_PTRACE is refused by the ptrace question",
            exit == 0,
        );
    }

    // ---- the open's two refusals ---------------------------------------------
    let zombie = unsafe { libc::fork() };
    assert!(zombie >= 0, "fork failed");
    if zombie == 0 {
        unsafe { libc::_exit(0) };
    }
    let observed = wait_for_zombie(zombie);
    let zombie_errno = open_mem(zombie, libc::O_RDONLY).err();
    report.check(
        "a zombie's mem node refuses the open with ESRCH",
        observed == 'Z' && zombie_errno == Some(libc::ESRCH),
    );
    let mut status = 0;
    unsafe { libc::waitpid(zombie, &mut status, 0) };
    report.check(
        "a collected task's mem node refuses the open with ENOENT",
        open_mem(zombie, libc::O_RDONLY).err() == Some(libc::ENOENT),
    );

    // ---- the stranger, which needs root to drop to ---------------------------
    //
    // Guest-only: the payload runs as root, so a child can become uid 65534 and
    // ask about a sibling it does not own. On a host run as uid 1000 it cannot
    // become 65534 at all, so the claim is skipped with a note - the same shape
    // `t-atime.rs` uses for its own stranger claim.
    if host {
        println!("note: the stranger's EACCES claim needs root to drop, skipped on the host");
    } else {
        let sibling = unsafe { libc::fork() };
        assert!(sibling >= 0, "fork failed");
        if sibling == 0 {
            unsafe {
                std::thread::sleep(std::time::Duration::from_secs(30));
                libc::_exit(0);
            }
        }
        // A zombie sibling too: Linux puts the permission word *before* the
        // address space, so a stranger asking about a dead task is `EACCES`
        // rather than `ESRCH` (measured, `build/scratch/pmem/p.c`). That ordering
        // is the whole reason the node asks `describes_task` as well as
        // `open_check`, and it is what a single ladder would get wrong.
        let dead = unsafe { libc::fork() };
        assert!(dead >= 0, "fork failed");
        if dead == 0 {
            unsafe { libc::_exit(0) };
        }
        wait_for_zombie(dead);
        let prober = unsafe { libc::fork() };
        assert!(prober >= 0, "fork failed");
        if prober == 0 {
            unsafe {
                libc::setresgid(65534, 65534, 65534);
                libc::setresuid(65534, 65534, 65534);
            }
            let live = open_mem(sibling, libc::O_RDONLY).err();
            let zombie = open_mem(dead, libc::O_RDONLY).err();
            let ok = live == Some(libc::EACCES) && zombie == Some(libc::EACCES);
            unsafe { libc::_exit(if ok { 0 } else { 1 }) };
        }
        let mut status = 0;
        unsafe { libc::waitpid(prober, &mut status, 0) };
        let exit = (status >> 8) & 0xff;
        unsafe {
            libc::kill(sibling, libc::SIGKILL);
            libc::waitpid(sibling, &mut status, 0);
            libc::waitpid(dead, &mut status, 0);
        }
        report.check(
            "a stranger's mem node refuses the open with EACCES, for a zombie too",
            exit == 0,
        );
    }

    unmap(page, 1);
    unsafe { libc::close(fd) };
    report.finish();
}
