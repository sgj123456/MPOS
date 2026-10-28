//! The keyring syscalls: `add_key(2)`, `request_key(2)` and `keyctl(2)`.
//!
//! This module is the ABI surface only - the argument shapes, the user-memory
//! copies and the errno mapping. The model (keys, keyrings, permissions,
//! quota, search) lives in [`crate::sched::keys`], where process state belongs;
//! the measured shapes behind every answer are that module's header and
//! `docs/notes/` §6.30.
//!
//! Two copy rules are Linux's and are not obvious from a signature:
//!
//! - `keyctl(READ)` answers the whole payload length even when the caller's
//!   buffer is smaller, and copies *nothing* in that case - so a caller learns
//!   the size by asking with a short buffer (measured: 5 into a 10-byte key
//!   returned 10 and copied nothing).
//! - `keyctl(DESCRIBE)` and `keyctl(READ)` both accept a NULL buffer, which is
//!   the size probe: they answer the length and copy nothing.

use alloc::string::String;
use alloc::vec::Vec;

use super::nr;
use crate::err;
use crate::sched::keys::{self, Caller, ReadData};
use crate::syscall::ARG_COUNT;
use crate::syscall::{buffer_ok, cstr_arg};

/// `keyctl`'s command numbers. Linux's `KEYCTL_*` values are an append-only
/// ABI, so these are its numbers, not a local ordering.
const KEYCTL_GET_KEYRING_ID: usize = 0;
const KEYCTL_JOIN_SESSION_KEYRING: usize = 1;
const KEYCTL_UPDATE: usize = 2;
const KEYCTL_REVOKE: usize = 3;
const KEYCTL_SETPERM: usize = 4;
const KEYCTL_CHOWN: usize = 5;
const KEYCTL_DESCRIBE: usize = 6;
const KEYCTL_CLEAR: usize = 7;
const KEYCTL_LINK: usize = 8;
const KEYCTL_UNLINK: usize = 9;
const KEYCTL_SEARCH: usize = 10;
const KEYCTL_READ: usize = 11;
const KEYCTL_INVALIDATE: usize = 21;

/// Read a key's type or description. A string past the kernel's path bound is
/// `EINVAL` here rather than `ENAMETOOLONG`: Linux's key code rejects an
/// over-long description with `EINVAL` (measured at 4 096 bytes), and the
/// string never reaches the filesystem layer whose errno that would be.
fn key_string(ptr: usize) -> Result<String, isize> {
    match cstr_arg(ptr, true) {
        Ok(string) => {
            if string.len() > keys::MAX_DESC {
                Err(err::EINVAL)
            } else {
                Ok(string)
            }
        }
        Err(err::ENAMETOOLONG) => Err(err::EINVAL),
        Err(other) => Err(other),
    }
}

/// Copy a payload the caller names: NULL with a length is `EFAULT` (measured),
/// an unreadable range is `EFAULT`, and one past the payload ceiling is
/// `EINVAL`.
/// `add_key`'s payload argument, whose rules are the *type's*: measured,
/// `"user"` requires at least one byte (`EINVAL` for a zero-length payload even
/// with a real buffer) while `"keyring"` takes exactly none (`EINVAL` for
/// three). A bad pointer is `EFAULT` for either, and an unknown type is
/// `ENODEV` before the payload is even looked at.
fn payload_arg(ty: &str, ptr: usize, len: usize) -> Result<Vec<u8>, isize> {
    match ty {
        "keyring" => {
            if len != 0 {
                return Err(err::EINVAL);
            }
            Ok(Vec::new())
        }
        "user" => payload_bytes(ptr, len),
        _ => Err(err::ENODEV),
    }
}

/// A non-empty user payload: length zero is `EINVAL` (measured for `add_key`
/// and `keyctl(UPDATE)`), a bad pointer `EFAULT`, and the size ceiling
/// `EINVAL`.
fn payload_bytes(ptr: usize, len: usize) -> Result<Vec<u8>, isize> {
    if len == 0 {
        return Err(err::EINVAL);
    }
    if ptr == 0 || !buffer_ok(ptr, len, false, true) {
        return Err(err::EFAULT);
    }
    if len > keys::MAX_PAYLOAD {
        return Err(err::EINVAL);
    }
    let mut bytes = alloc::vec![0u8; len];
    if !crate::sched::read_user(ptr, &mut bytes) {
        return Err(err::EFAULT);
    }
    Ok(bytes)
}

/// The `keyctl` copy rules: answer the length, copy only when the caller's
/// buffer can hold it all, and treat a NULL buffer as a size probe.
fn copy_out(ptr: usize, buflen: usize, bytes: &[u8]) -> isize {
    let len = bytes.len();
    if ptr == 0 || buflen < len {
        return len as isize;
    }
    if !buffer_ok(ptr, len, true, true) || !crate::sched::write_user(ptr, bytes) {
        return err::EFAULT;
    }
    len as isize
}

/// `add_key(2)`: `(type, description, payload, plen, ringid)`.
pub(super) fn handle_add_key(args: [usize; ARG_COUNT]) -> isize {
    let ty = match key_string(args[0]) {
        Ok(ty) => ty,
        Err(error) => return error,
    };
    let desc = match key_string(args[1]) {
        Ok(desc) => desc,
        Err(error) => return error,
    };
    let payload = match payload_arg(&ty, args[2], args[3]) {
        Ok(payload) => payload,
        Err(error) => return error,
    };
    let caller = Caller::current();
    match keys::add_key(&caller, &ty, &desc, &payload, args[4] as i32) {
        Ok(serial) => serial as isize,
        Err(error) => error,
    }
}

/// `request_key(2)`: `(type, description, callout_info, dest_ringid)`. The
/// callout string is parsed so a bad pointer is `EFAULT` like any other
/// argument, but this kernel has no upcall to hand it to - a miss is `ENOKEY`
/// and the gap is named in `crate::sched::keys`'s header.
pub(super) fn handle_request_key(args: [usize; ARG_COUNT]) -> isize {
    let ty = match key_string(args[0]) {
        Ok(ty) => ty,
        Err(error) => return error,
    };
    let desc = match key_string(args[1]) {
        Ok(desc) => desc,
        Err(error) => return error,
    };
    let callout = if args[2] == 0 {
        None
    } else {
        match key_string(args[2]) {
            Ok(callout) => Some(callout),
            Err(error) => return error,
        }
    };
    let caller = Caller::current();
    match keys::request_key(&caller, &ty, &desc, callout.as_deref(), args[3] as i32) {
        Ok(serial) => serial as isize,
        Err(error) => error,
    }
}

/// `keyctl(2)`: `(cmd, arg2, arg3, arg4, arg5)`.
pub(super) fn handle_keyctl(args: [usize; ARG_COUNT]) -> isize {
    let caller = Caller::current();
    match args[0] {
        KEYCTL_GET_KEYRING_ID => {
            match keys::get_keyring_id(&caller, args[1] as i32, args[2] != 0) {
                Ok(serial) => serial as isize,
                Err(error) => error,
            }
        }
        KEYCTL_JOIN_SESSION_KEYRING => {
            let name = if args[1] == 0 {
                None
            } else {
                match key_string(args[1]) {
                    Ok(name) => Some(name),
                    Err(error) => return error,
                }
            };
            match keys::join_session_keyring(&caller, name.as_deref()) {
                Ok(serial) => serial as isize,
                Err(error) => error,
            }
        }
        KEYCTL_UPDATE => {
            let payload = match payload_bytes(args[2], args[3]) {
                Ok(payload) => payload,
                Err(error) => return error,
            };
            match keys::keyctl_update(&caller, args[1] as u32, &payload) {
                Ok(()) => 0,
                Err(error) => error,
            }
        }
        KEYCTL_REVOKE => match keys::keyctl_revoke(&caller, args[1] as u32) {
            Ok(()) => 0,
            Err(error) => error,
        },
        KEYCTL_SETPERM => match keys::keyctl_setperm(&caller, args[1] as u32, args[2] as u32) {
            Ok(()) => 0,
            Err(error) => error,
        },
        KEYCTL_CHOWN => {
            match keys::keyctl_chown(&caller, args[1] as u32, args[2] as u32, args[3] as u32) {
                Ok(()) => 0,
                Err(error) => error,
            }
        }
        KEYCTL_DESCRIBE => match keys::keyctl_describe(&caller, args[1] as u32) {
            Ok(text) => {
                // Measured: the answer is NUL-terminated and its length counts
                // the NUL (30 for a 29-byte description).
                let mut bytes = text.into_bytes();
                bytes.push(0);
                copy_out(args[2], args[3], &bytes)
            }
            Err(error) => error,
        },
        KEYCTL_CLEAR => match keys::keyctl_clear(&caller, args[1] as i32) {
            Ok(()) => 0,
            Err(error) => error,
        },
        KEYCTL_LINK => match keys::keyctl_link(&caller, args[1] as u32, args[2] as i32) {
            Ok(()) => 0,
            Err(error) => error,
        },
        KEYCTL_UNLINK => match keys::keyctl_unlink(&caller, args[1] as u32, args[2] as i32) {
            Ok(()) => 0,
            Err(error) => error,
        },
        KEYCTL_SEARCH => {
            let ty = match key_string(args[2]) {
                Ok(ty) => ty,
                Err(error) => return error,
            };
            let desc = match key_string(args[3]) {
                Ok(desc) => desc,
                Err(error) => return error,
            };
            match keys::keyctl_search(&caller, args[1] as i32, &ty, &desc, args[4] as i32) {
                Ok(serial) => serial as isize,
                Err(error) => error,
            }
        }
        KEYCTL_READ => match keys::keyctl_read(&caller, args[1] as u32) {
            Ok(data) => {
                let bytes = data.as_wire();
                let _: ReadData = data;
                copy_out(args[2], args[3], &bytes)
            }
            Err(error) => error,
        },
        KEYCTL_INVALIDATE => match keys::keyctl_invalidate(&caller, args[1] as u32) {
            Ok(()) => 0,
            Err(error) => error,
        },
        // Every other command is present in the interface and refused: Linux
        // answers `EOPNOTSUPP` for a command number it does not know too
        // (measured with command 999), so the unknown-command answer and the
        // unimplemented-command answer are the same one.
        _ => err::EOPNOTSUPP,
    }
}

/// The syscall *numbers* only exist per machine, so the dispatch names them
/// here rather than sprinkling `cfg` over the handlers.
#[allow(dead_code)]
pub(super) const NUMBERS: [u64; 3] = [nr::ADD_KEY, nr::REQUEST_KEY, nr::KEYCTL];
