//! t-process-vm - `process_vm_readv` (310) and `process_vm_writev` (311):
//! moving bytes into and out of another process's address space.
//!
//! A forked child gets its own address space - a copy, not a window onto the
//! parent's - so the only way for the parent to see the child's buffer is
//! through these two calls, and the test has to be a handshake. The child
//! plants a known pattern in a static, writes the word that static lives at
//! into a pipe, and then parks on a second pipe; the parent reads and
//! overwrites the range while the child is stopped there, so the bytes cannot
//! move underneath the transfer. Pinned here: the byte counts a successful
//! read and write return, the bytes that cross (the parent's 15 overwrite the
//! child's first 15 and leave its tail), the `EINVAL` a nonzero `flags` word
//! earns, the `ESRCH` a pid with no task earns, and the `EFAULT` an
//! unreachable remote range earns.
//!
//! Two mpos divergences are worth naming. First, a page that is still
//! copy-on-write is *not* broken by the remote writer - it writes the shared
//! frame, where Linux's `FOLL_WRITE` would copy first - because breaking it
//! needs the tracee's own `Mm` accounting rather than a physical address; the
//! child writes its buffer in user mode before the handshake, which is what
//! takes the share out of the way. Second, the pid bound is mpos's task-table
//! size: every pid at or above `MAX_TASKS` is `ESRCH` without consulting the
//! table, which happens to be the same answer Linux gives for a pid no process
//! holds. A transfer that stops partway answers the bytes that did cross rather
//! than `EFAULT`; that is Linux's own rule too, not a divergence.
//!
//! What the remote side *must* check - and what the checks below pin - is that
//! the page it lands on is one the process itself could reach. `PROT_NONE` is
//! mapped *present but unreachable* here, so a later `mprotect` can hand the
//! contents back - which is exactly the shape the kernel's own half has. A walk
//! that asked only for a present entry would answer for both: the child's guard
//! page, and above all the supervisor half of every address space. Linux
//! answers `EFAULT` to a remote `PROT_NONE` page, and so does this kernel.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The child's own `PROT_NONE` page, at an address clear of both the image and
/// [`lin::GUARD_PAGE`] - the two refusals being pinned are *different* ones
/// ("present but supervisor" against "no entry at all"), so they have to be two
/// pages. The child maps it itself, because a mapping the parent made after the
/// fork is not one the child has.
const CHILD_NONE: usize = 0x5800_0000;

/// The 20 bytes the child plants in its own copy of [`CHILD_BUF`] before it
/// signals, and the 15 the parent writes over the front of them.
const PATTERN: &[u8; 20] = b"CHILD-DATA-012345678";
const PARENT_BYTES: &[u8; 15] = b"PARENT-WROTE-IT";

/// The child's side of the transfer, in the child's own address space. The
/// child writes it in user mode - which is what breaks the copy-on-write
/// share with the parent, since the kernel's cross-process write deliberately
/// does not - and the parent reads and overwrites it through `process_vm_*`.
/// 32 bytes so the 20-byte pattern has room in it.
static mut CHILD_BUF: [u8; 32] = *b"CHILD-DATA-012345678\0\0\0\0\0\0\0\0\0\0\0\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-process-vm\n");
    let mut failed = false;

    // Three pipes, because the two processes have to take turns: the child
    // hands over the address, the parent hands over a release byte once it is
    // done, and the child reports back what the write left in its buffer.
    let mut hand = [0i32; 2]; // child -> parent: the address of CHILD_BUF
    let mut rel = [0i32; 2]; // parent -> child: the release byte
    let mut rep = [0i32; 2]; // child -> parent: the 20 bytes afterwards

    let p1 = lin::pipe2(&mut hand, 0);
    let p2 = lin::pipe2(&mut rel, 0);
    let p3 = lin::pipe2(&mut rep, 0);
    lin::check_eq(&mut failed, b"the handshake pipe is made", p1, 0);
    lin::check_eq(&mut failed, b"the release pipe is made", p2, 0);
    lin::check_eq(&mut failed, b"the report pipe is made", p3, 0);
    if p1 != 0 || p2 != 0 || p3 != 0 {
        lin::done(failed);
    }

    let child = lin::fork();
    if child == 0 {
        // ---- the child ----
        let _ = lin::close(hand[0]);
        let _ = lin::close(rel[1]);
        let _ = lin::close(rep[0]);

        // A real user-mode write of the pattern. The kernel's cross-process
        // writer reaches a page by translation and does not break a
        // copy-on-write one, so without this write the parent's
        // `process_vm_writev` below would land in a frame the parent still
        // shares - and this process would be reporting from its own copy.
        unsafe {
            let dst = core::ptr::addr_of_mut!(CHILD_BUF) as *mut u8;
            let mut i = 0;
            while i < PATTERN.len() {
                core::ptr::write(dst.add(i), PATTERN[i]);
                i += 1;
            }
        }

        let addr = core::ptr::addr_of_mut!(CHILD_BUF) as usize;
        // A second word: a page of this process's own that is *mapped* but not
        // reachable - `PROT_NONE`, which this kernel keeps present. Zero if it
        // could not be made, so the parent reports rather than assumes.
        let none = lin::mmap(
            CHILD_NONE,
            4096,
            lin::PROT_NONE,
            lin::MAP_PRIVATE | lin::MAP_ANONYMOUS | lin::MAP_FIXED,
            -1,
            0,
        );
        let none = if none == CHILD_NONE as isize {
            none as usize
        } else {
            0
        };
        let mut words = [0u8; 16];
        words[..8].copy_from_slice(&addr.to_ne_bytes());
        words[8..].copy_from_slice(&none.to_ne_bytes());
        let _ = lin::write(hand[1], &words);

        // Park until the parent has finished reading and writing. This is the
        // whole reason the test is a handshake: `process_vm_*` of a *running*
        // child would be reading a page it might be changing underneath.
        let mut go = [0u8; 1];
        let _ = lin::read(rel[0], &mut go);

        // Report what the parent left in the buffer.
        let mut report = [0u8; 20];
        unsafe {
            let src = core::ptr::addr_of!(CHILD_BUF) as *const u8;
            let mut i = 0;
            while i < 20 {
                report[i] = core::ptr::read(src.add(i));
                i += 1;
            }
        }
        let _ = lin::write(rep[1], &report);
        lin::exit_group(0);
    }

    // ---- the parent ----
    lin::check_true(&mut failed, b"fork gave the parent a child pid", child > 0);
    if child <= 0 {
        lin::done(failed);
    }
    let _ = lin::close(hand[1]);
    let _ = lin::close(rel[0]);
    let _ = lin::close(rep[1]);

    // The child says where its buffer is, and where its `PROT_NONE` page is;
    // the parent never assumes the address a fork happens to reproduce.
    let mut addr_bytes = [0u8; 16];
    let taken = lin::read(hand[0], &mut addr_bytes);
    lin::check_eq(
        &mut failed,
        b"the child sends the words its buffer and its guard page live at",
        taken,
        16,
    );
    let mut buf_word = [0u8; 8];
    buf_word.copy_from_slice(&addr_bytes[..8]);
    let remote_at = usize::from_ne_bytes(buf_word);
    let mut none_word = [0u8; 8];
    none_word.copy_from_slice(&addr_bytes[8..]);
    let none_at = usize::from_ne_bytes(none_word);
    lin::check_true(
        &mut failed,
        b"and that word is a real address",
        remote_at > 0,
    );
    let remote = remote_at as *mut u8;

    // `process_vm_readv`: the whole 20-byte range crosses, and it is the
    // child's pattern.
    let mut got = [0u8; 20];
    let local_read = [lin::Iovec {
        base: got.as_mut_ptr(),
        len: 20,
    }];
    let remote_read = [lin::Iovec {
        base: remote,
        len: 20,
    }];
    lin::check_eq(
        &mut failed,
        b"process_vm_readv moves the whole range",
        lin::process_vm_readv(child as i32, &local_read, &remote_read, 0),
        20,
    );
    lin::check_true(
        &mut failed,
        b"and what crossed is the child's pattern",
        &got == PATTERN,
    );

    // `process_vm_writev`: the parent's 15 bytes land in the child.
    let mut send = *PARENT_BYTES;
    let local_write = [lin::Iovec {
        base: send.as_mut_ptr(),
        len: 15,
    }];
    let remote_write = [lin::Iovec {
        base: remote,
        len: 15,
    }];
    lin::check_eq(
        &mut failed,
        b"process_vm_writev moves the whole range",
        lin::process_vm_writev(child as i32, &local_write, &remote_write, 0),
        15,
    );

    // The one flag bit Linux defines is "none"; any other word is EINVAL.
    lin::check_eq(
        &mut failed,
        b"a nonzero flags word is EINVAL on the read",
        lin::process_vm_readv(child as i32, &local_read, &remote_read, 1),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a nonzero flags word is EINVAL on the write",
        lin::process_vm_writev(child as i32, &local_write, &remote_write, 1),
        lin::EINVAL,
    );

    // A pid with no task behind it. -1 is checked before the range, and 9999
    // is past mpos's task table; Linux answers ESRCH to both for the same
    // reason - there is no such process.
    lin::check_eq(
        &mut failed,
        b"pid -1 names no task: ESRCH",
        lin::process_vm_readv(-1, &local_read, &remote_read, 0),
        lin::ESRCH,
    );
    lin::check_eq(
        &mut failed,
        b"a pid past the task table names no task: ESRCH",
        lin::process_vm_readv(9999, &local_read, &remote_read, 0),
        lin::ESRCH,
    );
    lin::check_eq(
        &mut failed,
        b"and the write half refuses the same pid",
        lin::process_vm_writev(9999, &local_write, &remote_write, 0),
        lin::ESRCH,
    );

    // A remote range with no page behind it. `GUARD_PAGE` is the harness's
    // far-away word and this test never maps it, so the page-table walk finds
    // no entry at all.
    let unmapped = [lin::Iovec {
        base: lin::GUARD_PAGE as *mut u8,
        len: 20,
    }];
    lin::check_eq(
        &mut failed,
        b"a remote range with no page behind it is EFAULT on the read",
        lin::process_vm_readv(child as i32, &local_read, &unmapped, 0),
        lin::EFAULT,
    );
    lin::check_eq(
        &mut failed,
        b"a remote range with no page behind it is EFAULT on the write",
        lin::process_vm_writev(child as i32, &local_write, &unmapped, 0),
        lin::EFAULT,
    );

    // The other refusal, and the one that is a *hole* rather than a formality:
    // the child's page that is mapped but not reachable from ring 3. The walk
    // has to ask for the user bit, not merely for a present entry, or these
    // two lines read the child's guard page - and the supervisor half of every
    // address space is the same shape.
    if none_at > 0 {
        let guarded = [lin::Iovec {
            base: none_at as *mut u8,
            len: 20,
        }];
        lin::check_eq(
            &mut failed,
            b"a remote PROT_NONE page is EFAULT on the read",
            lin::process_vm_readv(child as i32, &local_read, &guarded, 0),
            lin::EFAULT,
        );
        lin::check_eq(
            &mut failed,
            b"a remote PROT_NONE page is EFAULT on the write",
            lin::process_vm_writev(child as i32, &local_write, &guarded, 0),
            lin::EFAULT,
        );
    } else {
        lin::check_true(
            &mut failed,
            b"the child could not make a PROT_NONE page, so the supervisor-page refusal was left unchecked",
            false,
        );
    }

    // Release the child and collect what it reports: the parent's 15 bytes at
    // the front, the child's own tail behind them.
    let go = [1u8];
    let _ = lin::write(rel[1], &go);

    let mut report = [0u8; 20];
    let reported = lin::read(rep[0], &mut report);
    lin::check_eq(
        &mut failed,
        b"the child reports its whole buffer back",
        reported,
        20,
    );
    let mut want = [0u8; 20];
    want[..15].copy_from_slice(PARENT_BYTES);
    want[15..].copy_from_slice(&PATTERN[15..]);
    lin::check_true(
        &mut failed,
        b"the front is the parent's bytes and the tail is still the child's",
        &report == &want,
    );

    let mut status = 0i32;
    let _ = lin::wait4(child as usize, &mut status, 0);
    lin::check_eq(
        &mut failed,
        b"the child exits cleanly",
        lin::wexitstatus(status) as isize,
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-process-vm: panicked\n")
}
