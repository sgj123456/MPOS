//! ls: the names in one directory, one per line, dots skipped.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const BUF_MAX: usize = 4096;
const MAX_NAMES: usize = 64;
const NAME_MAX: usize = 64;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(argc: usize, argv: *const *const u8) -> ! {
    let args = lin::Args { argc, argv };
    let target = if argc > 1 { args.get(1) } else { b"." as &[u8] };

    let mut names: [[u8; NAME_MAX]; MAX_NAMES] = [[0; NAME_MAX]; MAX_NAMES];
    let mut lengths: [usize; MAX_NAMES] = [0; MAX_NAMES];
    let mut count = 0usize;

    if let Some(n) = read_dir(target, &mut names, &mut lengths, &mut count) {
        if n < 0 {
            let _ = lin::write_all(2, b"ls: ");
            let _ = lin::write_all(2, target);
            let _ = lin::write_all(2, b": cannot list\n");
            lin::exit_group(1);
        }
        for i in 0..count {
            let name = &names[i][..lengths[i]];
            if name == b".".as_slice() || name == b"..".as_slice() {
                continue;
            }
            if lin::write_all(1, name) < 0 || lin::write_all(1, b"\n") < 0 {
                lin::exit_group(1);
            }
        }
        lin::exit_group(0);
    } else {
        let _ = lin::write_all(2, b"ls: ");
        let _ = lin::write_all(2, target);
        let _ = lin::write_all(2, b": cannot list\n");
        lin::exit_group(1);
    }
}

/// Read the contents of `path` into fixed-size `names` / `lengths` slots.
/// Returns `Some(0)` on success, `Some(n)` for a negative errno otherwise,
/// `None` if the directory could not be opened.
fn read_dir(
    path: &[u8],
    names: &mut [[u8; NAME_MAX]; MAX_NAMES],
    lengths: &mut [usize; MAX_NAMES],
    count: &mut usize,
) -> Option<isize> {
    let fd = lin::openat(lin::AT_FDCWD, path, lin::O_RDONLY | lin::O_DIRECTORY, 0);
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; BUF_MAX];
    let mut pos = 0usize;
    let mut rc = 0isize;
    loop {
        let n = lin::getdents64(fd as i32, &mut buf);
        if n < 0 {
            rc = n;
            break;
        }
        if n == 0 {
            break;
        }
        let slice = &buf[..n as usize];
        while pos < slice.len() {
            let Some((entry, reclen)) = lin::parse_dirent64(&slice[pos..]) else {
                break;
            };
            pos += reclen;
            if *count >= MAX_NAMES {
                continue;
            }
            let take = entry.name.len().min(NAME_MAX - 1);
            names[*count][..take].copy_from_slice(&entry.name[..take]);
            names[*count][take] = 0;
            lengths[*count] = take;
            *count += 1;
        }
    }
    let _ = lin::close(fd as i32);
    Some(rc)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"ls: panicked\n")
}