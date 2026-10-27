//! t-mmap - one test for mmap, of which a musl strace shows three anonymous
//! mappings on the way up:
//!
//! - a 12288-byte RW region with MAP_STACK, the alt stack Rust std reserves
//! - a 4096-byte PROT_NONE page at a MAP_FIXED address, its guard
//! - a plain 4096-byte RW page, malloc's overflow past brk
//!
//! The test asks for the same three shapes and pins the contract they carry:
//! kernel-picked addresses come back page-aligned and non-overlapping,
//! MAP_FIXED comes back exactly where it was asked, anonymous memory arrives
//! zeroed, and the writable regions take writes. Whether PROT_NONE actually
//! faults on access needs fault delivery and is deliberately not probed.
//!
//! The refusals the man page fixes are pinned too - a zero length, a
//! file-backed request over a bad fd, and a MAP_FIXED that is unaligned or
//! past the user limit - along with the full shape of the address argument: a
//! free hint is honoured exactly, a hint already in use is ignored, and
//! MAP_FIXED takes its range and arrives freshly zeroed. On this raw syscall
//! surface an errno comes back as the negated return, Linux's own convention;
//! MAP_FAILED is what a libc wrapper builds from it.
//!
//! # The fixed addresses moved into a reserved arena
//!
//! The no-std version mapped at `0x5000_0000` and neighbours because its
//! static image lived at `0x400000` and nothing else could be there. Under a
//! dynamic musl those addresses belong to nobody in particular - and one of
//! them may well *be* somebody's mapping. The migrated program first maps a
//! 2 MiB PROT_NONE arena wherever the kernel puts it and runs every fixed /
//! hint / NOREPLACE claim inside that owned span, unmapping it at the end.
//! The claims themselves are unchanged: "MAP_FIXED answers exactly the
//! address asked" does not care which address was asked.
//!
//! Every call rides `libc::mmap`/`libc::munmap` rather than rustix, clause
//! (c) twice over: the six-word shape (the caller's *hint*, the refusal set
//! of a raw flag word like `MAP_HUGETLB`, the returned address itself) is
//! what the ABI promises, and rustix's typed wrappers would drop each of
//! those words. The numbers come from libc's per-machine table (`SYS_mmap`:
//! 9 / 222).

use mpos_user_tests::Report;

const PAGE: usize = 4096;
const PATTERN: u64 = 0x1234_5678_9abc_def0;
const PROT_NONE: libc::c_int = libc::PROT_NONE;
const PROT_RW: libc::c_int = libc::PROT_READ | libc::PROT_WRITE;
const ANON_PRIVATE: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;

/// Map inside our arena at `off` (page offset), with the given prot/flags.
fn mm(hint_off: usize, len: usize, prot: libc::c_int, flags: libc::c_int) -> isize {
    let addr = if hint_off == 0 {
        core::ptr::null_mut()
    } else {
        unsafe { ARENA_BASE.cast::<u8>().add(hint_off) as *mut libc::c_void }
    };
    unsafe { libc::mmap(addr, len, prot, flags, -1, 0) as isize }
}

static mut ARENA_BASE: *mut libc::c_void = core::ptr::null_mut();
/// The arena every fixed-offset claim lives in. It must contain the highest
/// offset the program maps (`0x30_0000`): a `MAP_FIXED` past its end takes
/// memory the *host* process owns, which is how this test once clobbered libc
/// and crashed in `exit()` after printing PASS.
const ARENA_LEN: usize = 4 * 1024 * 1024;

macro_rules! off {
    ($a:expr) => {
        $a as usize - unsafe { ARENA_BASE as usize }
    };
}

fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-mmap");
    let mut r = Report::new();

    // Own the neighborhood first: everything below happens inside a span
    // this process reserved, so no other mapping can make a claim false by
    // accident. PROT_NONE so touching it without asking is impossible.
    let arena = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            ARENA_LEN,
            PROT_NONE,
            ANON_PRIVATE,
            -1,
            0,
        )
    };
    if arena as isize == -1 || arena == libc::MAP_FAILED {
        r.check("the fixture arena maps", false);
        r.finish();
    }
    unsafe { ARENA_BASE = arena };
    r.check("the fixture arena maps", true);

    // The MAP_STACK region the alt stack would live in.
    let stack = mm(0, 12288, PROT_RW, ANON_PRIVATE | libc::MAP_STACK);
    r.check("the 12288-byte MAP_STACK mapping succeeds", stack > 0);
    r.check(
        "the kernel picks a page-aligned address",
        stack > 0 && (stack as usize).is_multiple_of(PAGE),
    );

    let first = unsafe { (stack as *const u64).read_volatile() };
    let third = unsafe { ((stack as usize + 8192) as *const u64).read_volatile() };
    r.check("anonymous memory arrives zeroed", first == 0 && third == 0);

    unsafe { (stack as *mut u64).write_volatile(PATTERN) };
    let back = unsafe { (stack as *const u64).read_volatile() };
    r.check("the region takes writes", back == PATTERN);

    // The guard page, at the fixed address.
    let guard_at = unsafe { ARENA_BASE as usize + 0x10_0000 };
    let guard = mm(
        off!(guard_at),
        PAGE,
        PROT_NONE,
        ANON_PRIVATE | libc::MAP_FIXED,
    );
    r.check(
        "MAP_FIXED maps exactly the address asked",
        guard == guard_at as isize,
    );

    // The plain page: fresh address, no overlap with the first mapping.
    let page = mm(0, PAGE, PROT_RW, ANON_PRIVATE);
    r.check(
        "a plain 4096-byte mapping succeeds",
        page > 0 && (page as usize).is_multiple_of(PAGE),
    );
    r.check(
        "it does not overlap the earlier mapping",
        !(page >= stack && page < stack + 12288),
    );

    // The refusals the man page fixes.
    r.check(
        "a zero length answers EINVAL",
        refused(mm(0, 0, PROT_RW, ANON_PRIVATE), libc::EINVAL),
    );

    // The man page's ENODEV is what a *valid* descriptor on an unmappable
    // filesystem earns. An fd that names nothing - -1 here - is checked
    // first: real Linux answers EBADF, and this kernel follows that order.
    // The shape that reaches the fd check on *both* machines: file-backed
    // (no MAP_ANONYMOUS) over descriptor -1. Linux ignores the fd when the
    // mapping is anonymous (measured here: ANON|fd=-1 answers 0), so the
    // no-std version's flags were not a passthrough for this claim.
    let r_badfd = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            PAGE,
            PROT_RW,
            libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    r.check(
        "a file-backed request over a bad fd answers EBADF",
        refused(r_badfd as isize, libc::EBADF),
    );

    let shared = mm(0, PAGE, PROT_RW, libc::MAP_SHARED | libc::MAP_ANONYMOUS);
    // Phase 5.1: a shared anonymous mapping is served as an *object* - an
    // unnamed memfd the mapping aliases - so it is a usable region and two
    // processes across a `fork` watch each other's writes through it. It used
    // to answer ENOSYS, and then to be a private range that merely happened
    // to be shared between threads.
    r.check(
        "MAP_SHARED|MAP_ANONYMOUS is served as a mapping",
        shared > 0,
    );
    if shared > 0 {
        let p = shared as *mut u8;
        unsafe {
            p.write(0xa5);
            r.check(
                "the shared anonymous region takes and reads back a write",
                p.read() == 0xa5,
            );
        }
    }

    r.check(
        "MAP_FIXED to an unaligned address answers EINVAL",
        refused(
            mm(0x123, PAGE, PROT_RW, ANON_PRIVATE | libc::MAP_FIXED),
            libc::EINVAL,
        ),
    );

    // mpos's address gate is its own (the no-std version pinned it at
    // 0x8000_0000_0000_0000); Linux refuses a past-limit MAP_FIXED with
    // ENOMEM instead (measured on this host), so the hosted branch accepts
    // either refusal while the payload pins EINVAL.
    let past_limit = unsafe {
        libc::mmap(
            0x8000_0000_0000_0000usize as *mut _,
            PAGE,
            PROT_RW,
            ANON_PRIVATE | libc::MAP_FIXED,
            -1,
            0,
        ) as isize
    };
    r.check(
        "MAP_FIXED past the user limit answers EINVAL",
        refused(past_limit, libc::EINVAL)
            || mpos_user_tests::host_run() && refused(past_limit, libc::ENOMEM),
    );

    // A hint with nothing mapped in the way is honoured exactly - and "nothing
    // in the way" is a narrower neighbourhood than it looks: every byte
    // *inside* the arena is a claim this process made, the run above the arena
    // is the stack's (the layout puts the stack at the top of the mmap region,
    // so the arena was placed underneath it), and the walk the kernel places
    // fresh mappings with leaves the previous mapping directly below the next
    // one. So the test makes its own hole: two pages are taken where the walk
    // puts them and the upper one is handed straight back, which leaves
    // exactly one page no VMA describes.
    let two = mm(0, 2 * PAGE, PROT_RW, ANON_PRIVATE);
    r.check(
        "the two pages for the hint map",
        two > 0 && (two as usize).is_multiple_of(PAGE),
    );
    let hint_at = two as usize + PAGE;
    let freed = unsafe { libc::munmap(hint_at as *mut _, PAGE) };
    r.check("and the page above them is freed again", freed == 0);
    let hinted =
        unsafe { libc::mmap(hint_at as *mut _, PAGE, PROT_RW, ANON_PRIVATE, -1, 0) as isize };
    r.check(
        "a free hint is honoured exactly",
        hinted == hint_at as isize,
    );

    // A hint that is already in use is ignored: the mapping goes wherever
    // there is room, which cannot be the hinted page.
    let busy_at = unsafe { ARENA_BASE as usize + 0x22_0000 };
    let owner = mm(off!(busy_at), PAGE, PROT_RW, ANON_PRIVATE | libc::MAP_FIXED);
    r.check(
        "the busy hint's owner maps fixed",
        owner == busy_at as isize,
    );
    let elsewhere = mm(off!(busy_at), PAGE, PROT_RW, ANON_PRIVATE);
    r.check(
        "a busy hint is ignored for a page-aligned map elsewhere",
        elsewhere > 0
            && (elsewhere as usize).is_multiple_of(PAGE)
            && !(elsewhere >= busy_at as isize && elsewhere < busy_at as isize + PAGE as isize),
    );

    // MAP_FIXED takes the range: whatever was mapped there is dropped and
    // fresh zeroed pages go down in its place.
    let take_at = unsafe { ARENA_BASE as usize + 0x24_0000 };
    let taken = mm(off!(take_at), PAGE, PROT_RW, ANON_PRIVATE | libc::MAP_FIXED);
    r.check(
        "the range MAP_FIXED will take maps fixed",
        taken == take_at as isize,
    );
    unsafe { (take_at as *mut u64).write_volatile(PATTERN) };
    let again = mm(off!(take_at), PAGE, PROT_RW, ANON_PRIVATE | libc::MAP_FIXED);
    r.check(
        "MAP_FIXED over a live mapping still answers the address",
        again == take_at as isize,
    );
    let back = unsafe { (take_at as *const u64).read_volatile() };
    r.check("the taken range arrives freshly zeroed", back == 0);

    // `MAP_FIXED_NOREPLACE` (Linux 4.17): the address is mandatory but an
    // occupied range is refused with `EEXIST`, not relocated and not
    // replaced - what separates it from a plain hint (ignored when in use)
    // and from `MAP_FIXED` (takes the range). And `MAP_HUGETLB` with no
    // huge-page pool is the honest `ENOMEM`, never a silent 4 KiB page that
    // would let the caller believe it had huge pages.
    // The free page is a hole this test makes, not an offset above the arena:
    // the arena is exactly 2 MiB and the old offset was past its end, inside
    // whatever the host's own layout had already put there.
    let two = mm(0, 2 * PAGE, PROT_RW, ANON_PRIVATE);
    r.check("the two pages for the NOREPLACE claim map", two > 0);
    let nr_at = two as usize + PAGE;
    let freed = unsafe { libc::munmap(nr_at as *mut _, PAGE) };
    r.check("and the page above them is freed again", freed == 0);
    let nr = unsafe {
        libc::mmap(
            nr_at as *mut _,
            PAGE,
            PROT_RW,
            ANON_PRIVATE | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        ) as isize
    };
    r.check(
        "MAP_FIXED_NOREPLACE on a free range answers the exact address",
        nr == nr_at as isize,
    );
    let nr2 = unsafe {
        libc::mmap(
            nr_at as *mut _,
            PAGE,
            PROT_RW,
            ANON_PRIVATE | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        ) as isize
    };
    r.check(
        "MAP_FIXED_NOREPLACE over a live range is EEXIST, not relocated",
        refused(nr2, libc::EEXIST),
    );
    // The conflict is asked *at our arena base*, where the range really is
    // occupied: Linux checks NOREPLACE's collision first (EEXIST, measured
    // here) while mpos rejects the contradictory pair before anything else
    // (EINVAL). The payload sweep pins EINVAL; the hosted run accepts
    // Linux's own order and says which answer it got.
    let conflict = mm(
        0,
        PAGE,
        PROT_RW,
        ANON_PRIVATE | libc::MAP_FIXED | libc::MAP_FIXED_NOREPLACE,
    );
    // Linux answers EPERM for the contradictory pair (measured here); mpos
    // refuses it EINVAL before anything else. The payload pins EINVAL.
    r.check(
        "MAP_FIXED together with MAP_FIXED_NOREPLACE is EINVAL",
        refused(conflict, libc::EINVAL)
            || mpos_user_tests::host_run() && refused(conflict, libc::EPERM),
    );
    let huge = mm(0, PAGE, PROT_RW, ANON_PRIVATE | libc::MAP_HUGETLB);
    r.check(
        "MAP_HUGETLB is refused with ENOMEM, not a silent 4 KiB page",
        refused(huge, libc::ENOMEM),
    );

    // Give the arena back (its inner mappings live inside it).
    let _ = unsafe { libc::munmap(ARENA_BASE, ARENA_LEN) };

    r.finish()
}
