//! t_brk - one test for brk, which musl's allocator leans on before it ever
//! falls back to mmap. The strace shows both shapes:
//! `brk(NULL)` to learn the break, `brk(+0x2000)` to grow it by two pages.
//!
//! The contract pinned here: the fresh break is a positive page-aligned
//! address, growing returns the requested end, the grown region takes
//! writes on both of its pages, and shrinking returns the smaller break.
//! (Linux leaves the freed tail mapped or not as it pleases; only the
//! return value is pinned.)

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_brk\n");
    let mut failed = false;

    let cur = lin::brk(0);
    lin::check_true(&mut failed, b"brk(NULL) reports the initial break", cur > 0);
    lin::check_true(
        &mut failed,
        b"the break is page-aligned",
        cur > 0 && cur as usize % 4096 == 0,
    );

    let want = cur + 8192;
    let end = lin::brk(want as usize);
    lin::check_eq(
        &mut failed,
        b"growing by two pages returns the requested break",
        end,
        want,
    );

    const A: u64 = 0x1111_1111_1111_1111;
    const B: u64 = 0x2222_2222_2222_2222;
    unsafe {
        (cur as *mut u64).write_volatile(A);
        ((cur as usize + 4096) as *mut u64).write_volatile(B);
    }
    let back_a = unsafe { (cur as *const u64).read_volatile() };
    let back_b = unsafe { ((cur as usize + 4096) as *const u64).read_volatile() };
    lin::check_true(
        &mut failed,
        b"the grown heap takes writes on both pages",
        back_a == A && back_b == B,
    );

    let end = lin::brk((cur + 4096) as usize);
    lin::check_eq(
        &mut failed,
        b"shrinking returns the smaller break",
        end,
        cur + 4096,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_brk: panicked\n")
}
