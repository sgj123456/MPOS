//! t-drm-args - a card that survives a caller whose pointer is not its own.
//!
//! Every DRM request passes a struct, and this kernel reads and writes that
//! struct straight from kernel mode. An address that is not the caller's mapped
//! memory is therefore not a failed system call but a paging fault *in the
//! kernel*, and the machine cannot report one: it resets. That is how a
//! compositor died with nothing on the console - a freed request struct filled
//! with a poison value (`0xdead1111`) coming back as `objs_ptr` on the next
//! commit, sixty times a second.
//!
//! So the test is the crash, asked politely. Each case hands the card a pointer
//! it does not own - null, a poison value, an unmapped low address, a valid
//! struct with an array pointer inside it that is not the caller's - and the
//! answer must be `EFAULT`, the one Linux's `copy_*_user` gives for the same
//! request. And afterwards the card has to still work, because the bug was
//! never the wrong answer; it was the absence of any machine left to answer.
//!
//! What the move to the std side changes is *where the bytes are named*. The
//! no-std original spelled every request as a `[u8; 128]` and wrote its fields
//! at hand-counted offsets (`count_crtcs` at 36, `gamma_size` at 28). Here each
//! record is spelled as `drm.h` lays it out, with the offsets as compile-time
//! asserts - `t-fbdev`'s rule, and load-bearing for the same reason: neither
//! library binds any of these structs (measured for `t-fbdev`: `libc` has no
//! DRM record and `rustix` stops at the termios family), so a drifting field is
//! otherwise a silently misread word rather than a build failure.
//!
//! One fixture moves with it, and it is the one that makes this runnable on the
//! second machine at all. The no-std version hardcoded `crtc_id = 34`, a number
//! it could only have read off the first machine's card. The id is a constant
//! *of this kernel* - `CRTC_ID` in `src/drivers/drm/`, one of a fixed set
//! `GETRESOURCES` and the `GET*` ioctls agree on - so the number is the same on
//! both machines, but a test that asserts its own fixture is stronger than one
//! that restates it: this program asks `GETRESOURCES` for the crtc id and then
//! uses the one it was given, and the enumeration is a claim of its own.
//!
//! Not made on a host, and the reason is `t-fbdev`'s and `t-dsp`'s: every claim
//! below is about *this* card's answers - no gamma table, no vblank, and a
//! kernel-mode copy that has to fault politely - and a host's real `i915` or
//! `virtio-gpu` card answers its own. The one thing a hosted run could measure,
//! that an unowned pointer is `EFAULT`, is Linux's answer too but is not what
//! this file is pinning; the pin is that *this* kernel reaches that answer for
//! every request in its table instead of resetting. So a host run says so and
//! claims nothing, rather than reporting a different card's answers under this
//! program's name.

use std::ffi::c_void;
use std::os::fd::AsRawFd;

use mpos_user_tests::{Report, host_run};
use rustix::fs::OFlags;
use rustix::io::Errno;

/// `_IOWR('d', nr, size)` as libdrm builds it: the direction pair at bits 30..31,
/// the argument's size at 16..31, the family byte at 8..15, the request number
/// below that. Identical on both machines - an ioctl request word is not a
/// syscall number (`t-fbdev` spells its whole family the same way).
const fn ioc_d(nr: u32, size: u32) -> u64 {
    ((3u64) << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | (nr as u64)
}

const NR_VERSION: u32 = 0x00;
const NR_GET_CAP: u32 = 0x0c;
const NR_GETRESOURCES: u32 = 0xa0;
const NR_GETCRTC: u32 = 0xa1;
const NR_MODE_ATOMIC: u32 = 0xbc;
const NR_CREATE_BLOB: u32 = 0xbd;
/// The queries this card *refuses*: the legacy gamma pair, which it has no LUT
/// for, and the CRTC-sequence pair, which it has no vblank for.
const NR_MODE_GETGAMMA: u32 = 0xa4;
const NR_MODE_SETGAMMA: u32 = 0xa5;
const NR_CRTC_GET_SEQUENCE: u32 = 0x3b;
const NR_CRTC_QUEUE_SEQUENCE: u32 = 0x3c;

/// `DRM_CAP_DUMB_BUFFER`, which this card answers 1 for.
const CAP_DUMB_BUFFER: u64 = 1;

/// The poison musl's allocator leaves in a freed block, and the value that came
/// back as a pointer the first time this killed a machine.
const POISON: u64 = 0xdead_1111;
/// A mapped-looking address no process holds: low enough to be inside the guard
/// the loader leaves below a program's first mapping.
const UNMAPPED: u64 = 0x1000;

// --- the records, spelled as `drm.h` lays them out --------------------------
//
// Every one is handed over at the size the kernel's own ioctl table names for
// its request (`src/drivers/drm/`, the `(size, writes)` pairs), because the
// handler writes the whole record - a shorter buffer is not a short copy, it is
// a write past the caller's allocation.

/// `struct drm_get_cap`: a capability word in, its value out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GetCap {
    capability: u64,
    value: u64,
}

/// `struct drm_mode_card_res` - the two-call enumeration: four id arrays and
/// the four counts that size them, then the mode limits.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

/// `struct drm_mode_create_blob`: bytes read *from* the caller, an id out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CreateBlob {
    data_ptr: u64,
    length: u32,
    blob_id: u32,
}

/// `struct drm_mode_atomic` - the request a compositor makes sixty times a
/// second, and the exact shape that killed the machine: a stale `objs_ptr` in a
/// reused struct.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeAtomic {
    flags: u32,
    count_objs: u32,
    objs_ptr: u64,
    count_props_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    reserved: u64,
    req_seq: u64,
}

/// `struct drm_version`: the three numbers, then three (length, pointer) pairs
/// whose strings the caller allocates. `name_len` is a `__kernel_size_t`, which
/// is why it sits at 16 rather than at 12.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Version {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: u64,
    name: u64,
    date_len: u64,
    date: u64,
    desc_len: u64,
    desc: u64,
}

/// `struct drm_mode_crtc_lut`, at the 32 bytes this kernel's table names for the
/// legacy gamma pair. Only the two words at its head are a claim here; the tail
/// is carried so the handler's write has somewhere to land.
#[repr(C)]
#[derive(Clone, Copy)]
struct CrtcLut {
    crtc_id: u32,
    gamma_size: u32,
    _tail: [u8; 24],
}

// The two records with a carried tail are zeroed rather than derived: `Default`
// for an array is not written for every length, and a request's neutral state is
// "every field the program does not name is zero" - which is also what the
// kernel would write over.
impl Default for CrtcLut {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

/// `struct drm_crtc_get_sequence`: the question about a vblank clock this card
/// does not have.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CrtcSequence {
    crtc_id: u32,
    active: u32,
    sequence: u64,
    sequence_ns: u64,
}

/// `struct drm_mode_crtc`, all 104 bytes of it: the CRTC's own state readback,
/// whose `gamma_size` is what tells a caller the gamma pair is not worth asking
/// about. The trailing `drm_mode_modeinfo` is the card's current mode, which no
/// claim here reads - it is carried for the size, not spelled out.
#[repr(C)]
#[derive(Clone, Copy)]
struct ModeCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    _mode: [u8; 68],
}

/// As `CrtcLut`'s: zeroed, because the request's neutral state is zeroes and the
/// array's length is the struct's, not something a derive answers for.
impl Default for ModeCrtc {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

// The layout claims, as a *build* claim rather than a printed one: a drifting
// record is a compile failure, not a silently misread word or a line in a log
// nobody diffs. The offsets are the ones the no-std version wrote down by hand.
const _: () = assert!(core::mem::size_of::<GetCap>() == 16);
const _: () = assert!(core::mem::offset_of!(GetCap, value) == 8);
const _: () = assert!(core::mem::size_of::<CardRes>() == 64);
const _: () = assert!(core::mem::offset_of!(CardRes, crtc_id_ptr) == 8);
const _: () = assert!(core::mem::offset_of!(CardRes, count_fbs) == 32);
const _: () = assert!(core::mem::offset_of!(CardRes, count_crtcs) == 36);
const _: () = assert!(core::mem::size_of::<CreateBlob>() == 16);
const _: () = assert!(core::mem::offset_of!(CreateBlob, data_ptr) == 0);
const _: () = assert!(core::mem::offset_of!(CreateBlob, length) == 8);
const _: () = assert!(core::mem::size_of::<ModeAtomic>() == 56);
const _: () = assert!(core::mem::offset_of!(ModeAtomic, count_objs) == 4);
const _: () = assert!(core::mem::offset_of!(ModeAtomic, objs_ptr) == 8);
const _: () = assert!(core::mem::offset_of!(ModeAtomic, count_props_ptr) == 16);
const _: () = assert!(core::mem::offset_of!(ModeAtomic, props_ptr) == 24);
const _: () = assert!(core::mem::offset_of!(ModeAtomic, prop_values_ptr) == 32);
const _: () = assert!(core::mem::size_of::<Version>() == 64);
const _: () = assert!(core::mem::offset_of!(Version, name_len) == 16);
const _: () = assert!(core::mem::offset_of!(Version, name) == 24);
const _: () = assert!(core::mem::size_of::<CrtcLut>() == 32);
const _: () = assert!(core::mem::offset_of!(CrtcLut, crtc_id) == 0);
const _: () = assert!(core::mem::offset_of!(CrtcLut, gamma_size) == 4);
const _: () = assert!(core::mem::size_of::<CrtcSequence>() == 24);
const _: () = assert!(core::mem::offset_of!(CrtcSequence, crtc_id) == 0);
const _: () = assert!(core::mem::size_of::<ModeCrtc>() == 104);
const _: () = assert!(core::mem::offset_of!(ModeCrtc, crtc_id) == 12);
const _: () = assert!(core::mem::offset_of!(ModeCrtc, gamma_size) == 28);

/// One `ioctl`, answering the negative errno a raw syscall hands back.
///
/// Through `libc::syscall` rather than `libc::ioctl`, for `t-fbdev`'s reason:
/// `ioctl`'s request parameter is `c_int` in the musl bindings and `c_ulong` in
/// the gnu ones, while a request is an `unsigned long` word the kernel decodes.
/// The number comes from `libc::SYS_ioctl`, the maintained per-machine table -
/// which is the whole reason this program builds and runs on the second machine
/// instead of only on the first.
///
/// # Safety
/// `arg` must be null, an address the caller owns, or an address deliberately
/// not owned - which is this program's subject - and `request`'s size word must
/// match what that address can hold.
unsafe fn drm_ioctl(fd: i32, request: u64, arg: *mut c_void) -> isize {
    let rc = unsafe { libc::syscall(libc::SYS_ioctl, fd as libc::c_long, request, arg) };
    if rc < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        rc as isize
    }
}

/// The errno a refusal answers, as the negative value the door hands back.
fn refused(err: Errno) -> isize {
    -(err.raw_os_error() as isize)
}

/// One claim that a request was refused with a particular errno.
fn check_refused(r: &mut Report, name: &str, got: isize, want: Errno) {
    if got == refused(want) {
        r.check(name, true);
    } else {
        r.check(
            &format!("{name} (got {got}, want {})", refused(want)),
            false,
        );
    }
}

/// One claim that a request succeeded.
fn check_ok(r: &mut Report, name: &str, got: isize) {
    if got >= 0 {
        r.check(name, true);
    } else {
        r.check(&format!("{name} (got {got})"), false);
    }
}

fn main() {
    println!("t-drm-args");
    let mut r = Report::new();

    // A hosted run measures a different card, so it names that and stops: see
    // the header. This is `t-pty`'s `mint` rule - a run that cannot build the
    // fixture makes no claims rather than making the host's ones.
    if host_run() {
        println!(
            "note: not made on a host - every claim below is about this kernel's card \
             (no gamma table, no vblank, and a kernel-mode copy that answers EFAULT), \
             and a host's own /dev/dri/card0 answers its own"
        );
        r.finish();
    }

    let card = match rustix::fs::open("/dev/dri/card0", OFlags::RDWR, rustix::fs::Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("/dev/dri/card0 opens ({e:?})"), false);
            r.finish();
        }
    };
    let card = card.as_raw_fd();
    r.check("/dev/dri/card0 opens", true);

    // --- the crtc id, asked for rather than restated ------------------------
    //
    // The sizing call first: four null pointers, whose only answer is the four
    // counts. Then the id, read out of an array this program owns. The no-std
    // version wrote `34` because that is what the first machine's card has; the
    // number is a constant of the kernel, but a test that is *given* its fixture
    // is a test that cannot go stale on the machine that serves it.
    let mut res = CardRes::default();
    let got = unsafe {
        drm_ioctl(
            card,
            ioc_d(NR_GETRESOURCES, 64),
            (&raw mut res) as *mut c_void,
        )
    };
    check_ok(&mut r, "GETRESOURCES answers the sizing call", got);
    r.check(
        &format!("the card reports one crtc (got {})", res.count_crtcs),
        res.count_crtcs == 1,
    );
    let mut crtc_ids = [0u32; 4];
    let mut res = CardRes {
        count_crtcs: crtc_ids.len() as u32,
        crtc_id_ptr: crtc_ids.as_mut_ptr() as u64,
        ..CardRes::default()
    };
    let got = unsafe {
        drm_ioctl(
            card,
            ioc_d(NR_GETRESOURCES, 64),
            (&raw mut res) as *mut c_void,
        )
    };
    check_ok(&mut r, "GETRESOURCES fills the crtc array", got);
    let crtc_id = crtc_ids[0];
    r.check(
        &format!("and names a crtc (got {crtc_id})"),
        crtc_id >= 1 && res.count_crtcs >= 1,
    );
    // The rest of the program uses the id it was given. A card with no crtc has
    // nothing left to ask about, and saying so beats a run of refusals that are
    // only refusals because the fixture was missing.
    if crtc_id == 0 {
        r.check("the card has a crtc to ask about", false);
        r.finish();
    }

    // --- the good path, so a green run below means "refused that one request"
    // and not "the card answers nothing at all" ---
    let mut cap = GetCap {
        capability: CAP_DUMB_BUFFER,
        value: 0,
    };
    let got = unsafe { drm_ioctl(card, ioc_d(NR_GET_CAP, 16), (&raw mut cap) as *mut c_void) };
    check_ok(&mut r, "a valid GET_CAP succeeds", got);
    r.check(
        &format!("and answers the cap (got {})", cap.value),
        cap.value == 1,
    );

    // No argument at all.
    let got = unsafe { drm_ioctl(card, ioc_d(NR_GET_CAP, 16), core::ptr::null_mut()) };
    check_refused(
        &mut r,
        "a null GET_CAP argument is EFAULT",
        got,
        Errno::FAULT,
    );

    // A poison pointer: freed memory whose contents came back as an address.
    let got = unsafe { drm_ioctl(card, ioc_d(NR_GET_CAP, 16), POISON as *mut c_void) };
    check_refused(
        &mut r,
        "a poisoned GET_CAP argument is EFAULT",
        got,
        Errno::FAULT,
    );

    // Mapped-looking but not the caller's.
    let got = unsafe { drm_ioctl(card, ioc_d(NR_GETRESOURCES, 64), UNMAPPED as *mut c_void) };
    check_refused(
        &mut r,
        "an unmapped GETRESOURCES argument is EFAULT",
        got,
        Errno::FAULT,
    );

    // A valid struct whose *nested* array pointer is not the caller's: the
    // two-call enumeration shape, asked for one crtc id at an address that is
    // not memory. The struct itself is fine, so only the array can be refused.
    let mut nested = CardRes {
        count_crtcs: 1,
        crtc_id_ptr: POISON,
        ..CardRes::default()
    };
    let got = unsafe {
        drm_ioctl(
            card,
            ioc_d(NR_GETRESOURCES, 64),
            (&raw mut nested) as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "GETRESOURCES with a poisoned crtc_ptr is EFAULT",
        got,
        Errno::FAULT,
    );

    // `CREATE_BLOB`: the bytes are read *from* the caller, so an address that is
    // not the caller's fails the same way - and a kernel that reads it anyway
    // walks off into a page that is not there.
    let mut blob = CreateBlob {
        data_ptr: POISON,
        length: 68, // a mode
        blob_id: 0,
    };
    let got = unsafe {
        drm_ioctl(
            card,
            ioc_d(NR_CREATE_BLOB, 16),
            (&raw mut blob) as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "CREATE_BLOB from a poisoned address is EFAULT",
        got,
        Errno::FAULT,
    );

    // `MODE_ATOMIC`: the object array is read before anything is applied.
    let mut atomic = ModeAtomic {
        count_objs: 1,
        objs_ptr: POISON,
        count_props_ptr: POISON + 8,
        props_ptr: POISON + 16,
        prop_values_ptr: POISON + 24,
        ..ModeAtomic::default()
    };
    let got = unsafe {
        drm_ioctl(
            card,
            ioc_d(NR_MODE_ATOMIC, 56),
            (&raw mut atomic) as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "MODE_ATOMIC with a poisoned objs_ptr is EFAULT",
        got,
        Errno::FAULT,
    );

    // `VERSION`: the sizing call brings a length and no room, and a call that
    // brings room that is not its own must not be copied into.
    let mut version = Version {
        name_len: 4,
        name: POISON,
        ..Version::default()
    };
    let got = unsafe {
        drm_ioctl(
            card,
            ioc_d(NR_VERSION, 64),
            (&raw mut version) as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "VERSION writing a name to a poisoned address is EFAULT",
        got,
        Errno::FAULT,
    );

    // --- the requests this card *refuses*, and the argument check in front ---
    //
    // These are not "unimplemented": each one is in the dispatch table with a
    // reason, and the reason is that this card has no hardware behind it - no
    // gamma table for the legacy pair, no vertical blank for the sequence pair.
    // Before they were named they fell through to the catch-all, which meant the
    // same errno for a different reason and, worse, no argument check at all:
    // Linux's ioctl layer copies every DRM struct in before a handler can refuse
    // it, so a bad pointer must hear EFAULT.
    let mut lut = CrtcLut {
        crtc_id,
        gamma_size: 0, // the caller asks for none
        ..CrtcLut::default()
    };
    let got = unsafe {
        drm_ioctl(
            card,
            ioc_d(NR_MODE_GETGAMMA, 32),
            (&raw mut lut) as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "GETGAMMA on a CRTC with no gamma table is EINVAL",
        got,
        Errno::INVAL,
    );
    let got = unsafe {
        drm_ioctl(
            card,
            ioc_d(NR_MODE_SETGAMMA, 32),
            (&raw mut lut) as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "and SETGAMMA is refused the same way",
        got,
        Errno::INVAL,
    );
    let got = unsafe { drm_ioctl(card, ioc_d(NR_MODE_GETGAMMA, 32), POISON as *mut c_void) };
    check_refused(
        &mut r,
        "a poisoned GETGAMMA argument is EFAULT before the refusal",
        got,
        Errno::FAULT,
    );

    // The CRTC-sequence pair: ENOTTY, the answer §7.1 recorded for the other
    // vblank door. What changes here is the argument check, which is what the
    // poisoned case pins.
    let mut seq = CrtcSequence {
        crtc_id,
        ..CrtcSequence::default()
    };
    let got = unsafe {
        drm_ioctl(
            card,
            ioc_d(NR_CRTC_GET_SEQUENCE, 24),
            (&raw mut seq) as *mut c_void,
        )
    };
    check_refused(
        &mut r,
        "CRTC_GET_SEQUENCE on a card with no vblank is ENOTTY",
        got,
        Errno::NOTTY,
    );
    let got = unsafe {
        drm_ioctl(
            card,
            ioc_d(NR_CRTC_QUEUE_SEQUENCE, 24),
            (&raw mut seq) as *mut c_void,
        )
    };
    check_refused(&mut r, "and CRTC_QUEUE_SEQUENCE with it", got, Errno::NOTTY);
    let got = unsafe { drm_ioctl(card, ioc_d(NR_CRTC_GET_SEQUENCE, 24), POISON as *mut c_void) };
    check_refused(
        &mut r,
        "a poisoned CRTC_GET_SEQUENCE argument is EFAULT, not ENOTTY",
        got,
        Errno::FAULT,
    );

    // `GETCRTC` writes `gamma_size` rather than leaving it, which matters for
    // exactly the reason the mode's name does: the caller allocated the struct
    // with `malloc`, so a field the kernel skips is a number the caller reads as
    // garbage. Filled with a poison first, so a skipped field cannot pass by
    // being zero.
    let mut crtc = ModeCrtc {
        crtc_id,
        gamma_size: 0xffff_ffff,
        ..ModeCrtc::default()
    };
    let got = unsafe { drm_ioctl(card, ioc_d(NR_GETCRTC, 104), (&raw mut crtc) as *mut c_void) };
    check_ok(&mut r, "GETCRTC succeeds", got);
    r.check(
        &format!(
            "and reports a gamma_size of 0 rather than leaving the caller's bytes (got {})",
            crtc.gamma_size
        ),
        crtc.gamma_size == 0,
    );

    // The whole point: the card is still there after being fed all of that, and
    // answers a real request as it did before it.
    let mut cap = GetCap {
        capability: CAP_DUMB_BUFFER,
        value: 0,
    };
    let got = unsafe { drm_ioctl(card, ioc_d(NR_GET_CAP, 16), (&raw mut cap) as *mut c_void) };
    check_ok(&mut r, "the card still answers GET_CAP", got);
    r.check(
        &format!("and still with the same value (got {})", cap.value),
        cap.value == 1,
    );

    r.finish();
}
