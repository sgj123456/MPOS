//! t-pty - one test for the pseudo-terminal pair, the thing a login runs on
//! when it is not on the machine's own console.
//!
//! Pinned here: opening `/dev/ptmx` mints a *new* pair and hands back its
//! master; `TIOCGPTN` on the master names the number the slave appears under
//! in `/dev/pts`; `TIOCSPTLCK` with zero is `unlockpt`, and before it a
//! master still answers. The master is a byte channel, not a terminal - its
//! `TCGETS` answers ENOTTY, the slave's succeeds. Bytes written at the master
//! are typed at the slave's line discipline (canonical mode, so `hi\r` reads
//! back as `hi\n`), and what the slave prints is read at the master.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PTMX: [u8; 10] = *b"/dev/ptmx\0";

static mut MASTER_TO_SLAVE: [u8; 8] = [0; 8];
static mut SLAVE_TO_MASTER: [u8; 8] = [0; 8];
static mut ECHO: [u8; 16] = [0; 16];
static mut PATH: [u8; 20] = [0; 20];
static mut SLAVE_LINE: [u8; 8] = [0; 8];

/// `/dev/pts/<n>` spelled into `PATH` from the number `TIOCGPTN` answered.
fn slave_path(mut n: usize) -> &'static [u8] {
    const PREFIX: [u8; 9] = *b"/dev/pts/";
    unsafe {
        let at = PREFIX.len();
        PATH[..at].copy_from_slice(&PREFIX);
        let mut digits = [0u8; 10];
        let mut len = 0;
        loop {
            digits[len] = b'0' + (n % 10) as u8;
            n /= 10;
            len += 1;
            if n == 0 {
                break;
            }
        }
        for (i, &d) in digits[..len].iter().rev().enumerate() {
            PATH[at + i] = d;
        }
        PATH[at + len] = 0;
        core::slice::from_raw_parts(core::ptr::addr_of!(PATH).cast(), at + len + 1)
    }
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-pty\n");
    let mut failed = false;

    let master = lin::openat(lin::AT_FDCWD, &PTMX, lin::O_RDWR, 0);
    lin::check_true(
        &mut failed,
        b"/dev/ptmx opens",
        master >= 0,
    );
    let master = master as i32;

    // The number the slave will carry: at least past the six virtual
    // consoles, whose numbers are minted at boot.
    let mut n: u32 = 0;
    let r = lin::ioctl(master, lin::TIOCGPTN, &raw mut n as usize);
    lin::check_eq(&mut failed, b"TIOCGPTN answers the pair number", r, 0);
    lin::check_true(
        &mut failed,
        b"the pair number is past the virtual consoles",
        n > 6,
    );

    // `grantpt`/`unlockpt`: the lock clears, and a locked pair would refuse
    // its slave with EIO - the check that makes the sequence meaningful.
    let zero: i32 = 0;
    let r = lin::ioctl(master, lin::TIOCSPTLCK, &raw const zero as usize);
    lin::check_eq(&mut failed, b"TIOCSPTLCK(0) unlocks the pair", r, 0);

    let path = slave_path(n as usize);
    lin::puts(b"openat path: "); lin::puts(path); lin::puts(b"\n");
    let slave = lin::openat(lin::AT_FDCWD, path, lin::O_RDWR, 0);
    lin::puts(b"slave fd = "); lin::puts_isize(slave); lin::puts(b"\n");
    lin::check_true(&mut failed, b"/dev/pts/N opens", slave >= 0);
    let slave = slave as i32;

    // The master is a wire, not a terminal; the slave is one.
    let mut termios = [0u8; 64];
    let r = lin::ioctl(master, lin::TCGETS, &raw mut termios as usize);
    lin::check_eq(
        &mut failed,
        b"TCGETS on the master answers ENOTTY",
        r,
        lin::ENOTTY,
    );
    let r = lin::ioctl(slave, lin::TCGETS, &raw mut termios as usize);
    lin::check_eq(&mut failed, b"TCGETS on the slave succeeds", r, 0);

    // Master to slave: what is typed is what the line discipline hands the
    // reader, canonical mode and ICRNL included.
    let r = lin::write(master, b"hi\r");
    lin::check_eq(&mut failed, b"the master write is taken whole", r, 3);
    let got = lin::read(slave, unsafe { &mut *core::ptr::addr_of_mut!(SLAVE_LINE) });
    lin::check_eq(&mut failed, b"the slave read is a whole line", got, 3);
    unsafe {
        let line: [u8; 3] = core::ptr::read(core::ptr::addr_of!(SLAVE_LINE).cast());
        lin::check_true(
            &mut failed,
            b"CR became LF on the way through",
            &line == b"hi\n",
        );
    }

    // Slave to master: what the slave prints is what the master reads. The
    // echo of the typed line is in the master's queue ahead of it, so that
    // goes down the drain first.
    let got = lin::read(master, unsafe { &mut *core::ptr::addr_of_mut!(ECHO) });
    lin::check_eq(&mut failed, b"the typed line's echo reached the master", got, 3);
    let r = lin::write(slave, b"out\n");
    lin::check_eq(&mut failed, b"the slave write is taken whole", r, 4);
    let got = lin::read(master, unsafe { &mut *core::ptr::addr_of_mut!(SLAVE_TO_MASTER) });
    lin::check_eq(&mut failed, b"the master read sees the slave's bytes", got, 4);
    unsafe {
        let line: [u8; 4] = core::ptr::read(core::ptr::addr_of!(SLAVE_TO_MASTER).cast());
        lin::check_true(
            &mut failed,
            b"the bytes are the ones printed",
            &line == b"out\n",
        );
    }

    let r = lin::close(slave);
    lin::check_eq(&mut failed, b"the slave closes", r, 0);
    let r = lin::close(master);
    lin::check_eq(&mut failed, b"the master closes", r, 0);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-pty: panicked\n")
}
