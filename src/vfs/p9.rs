//! 9P2000.u, the protocol, and the filesystem it mounts as over virtio.
//!
//! A 9p share is a directory tree the *host* keeps: QEMU serves one from a
//! path it was handed on its command line, and the guest reaches in with a
//! wire protocol - version, attach, walk, open, read, write, stat - that
//! Plan 9 spelled out decades before anyone put it on a virtqueue. The
//! `virtio-drivers` crate moves the bytes; this module is everything else:
//! the message encoding and parsing, the fid table, and the mapping onto
//! the [`Inode`] contract, which is the same mapping every other filesystem
//! in this tree already answers.
//!
//! The dialect is **9P2000.u**, and that is measured rather than chosen
//! (`NOTES.md` §13.29): QEMU 11.1.1's virtfs answers `"unknown"` to a plain
//! `9P2000` `Tversion` and `9P2000.u` to this one, so the plain dialect this
//! client used to ask for is one no modern server serves. `.u` is 9P2000 plus
//! the numeric ids (attach's `n_uname`, a stat's `n_uid`/`n_gid`/`n_muid`) and
//! the `extension` string a stat carries; every other message is unchanged.
//! What the dialect family still costs is named rather than hidden: symlinks and
//! hard links have no message to build from ([`Inode::symlink`]/[`Inode::link`]
//! answer `Unsupported`, which is what Linux's v9fs answers for them on a 9P2000
//! server), and `rename(2)` has no atomic form - it answers `EXDEV`, the same
//! answer a cross-filesystem rename gives, so a caller falls back to the
//! copy-and-delete every cross-device move already needs. Both are messages
//! 9P2000*L* adds and this dialect does not have.
//!
//! # Fids and write-through
//!
//! A fid is a handle the server keeps for a path the client walked to.
//! Fid zero belongs to the root of the share, attached at mount and held
//! for the mount's life; every other fid is minted by a walk and retired
//! by a clunk when the inode that owned it drops. The numbers only ever
//! count up - a fid is never reused, which is what makes a *failed* walk
//! cheap: the number it would have used was never anyone else's, so
//! nothing has to be given back.
//!
//! I/O is synchronous, like the disk's: every `write_at` is a `Twrite`
//! answered before the call returns, and there is no cache to write back
//! because nothing was ever cached. A file's size is the server's answer,
//! asked fresh on every `metadata` - the same "every edge is fresh" rule
//! the ext2/ext4 driver lives by.

use alloc::string::String;
use alloc::vec::Vec;

use crate::vfs::{NodeKind, VfsError};

// The device-dependent half's imports, gated with that half: on the host this
// module is the wire format and its parsers only, so the types the *filesystem*
// is built from would be dead weight there.
#[cfg(target_os = "none")]
use crate::vfs::{FileSystem, Inode, Metadata, VfsResult};
#[cfg(target_os = "none")]
use alloc::sync::{Arc, Weak};
#[cfg(target_os = "none")]
use core::cell::RefCell;
#[cfg(target_os = "none")]
use critical_section::Mutex;

// ---------------------------------------------------------------------------
// The wire: message types, and the encoder/decoder helpers. Pure code,
// testable on the host, which is why it sits above the device-dependent
// half and takes and returns plain bytes.
// ---------------------------------------------------------------------------

/// The dialect this client speaks.
///
/// **9P2000.u**, and that is measured rather than preferred (`NOTES.md` §13.29):
/// QEMU 11.1.1's virtfs answers `"unknown"` to a plain `9P2000` `Tversion` and
/// `9P2000.u` to this one, so the plain dialect the client used to ask for is
/// one no modern server serves. `.u` is `9P2000` plus the numeric ids
/// (`n_uname` on attach, `n_uid`/`n_gid`/`n_muid` in a stat) and the `extension`
/// string a stat carries; everything else on the wire - walk, open, read, write,
/// clunk, remove - is the same message with the same encoding.
const DIALECT: &str = "9P2000.u";

const T_VERSION: u8 = 100;
const R_VERSION: u8 = 101;
const T_ATTACH: u8 = 104;
const R_ATTACH: u8 = 105;
const R_ERROR: u8 = 107;
const T_WALK: u8 = 110;
const R_WALK: u8 = 111;
const T_OPEN: u8 = 112;
const R_OPEN: u8 = 113;
const T_CREATE: u8 = 114;
const R_CREATE: u8 = 115;
const T_READ: u8 = 116;
const R_READ: u8 = 117;
const T_WRITE: u8 = 118;
const R_WRITE: u8 = 119;
const T_CLUNK: u8 = 120;
const R_CLUNK: u8 = 121;
const T_REMOVE: u8 = 122;
const R_REMOVE: u8 = 123;
const T_STAT: u8 = 124;
const R_STAT: u8 = 125;
const T_WSTAT: u8 = 126;
const R_WSTAT: u8 = 127;

/// A `qid`'s type byte, as the dialect spells the interesting bits.
const QT_DIR: u8 = 0x80;

/// `stat.mode`'s directory bit - the high bit of the 9P2000 permission
/// word, where Plan 9 keeps "this is a directory" alongside rwxrwxrwx.
const DM_DIR: u32 = 0x8000_0000;

/// `Topen` modes, in the wire's own numbering.
const O_READ: u8 = 0;
const O_RDWR: u8 = 2;

/// A file's facts as one `Rstat` carries them, decoded.
///
/// `pub` and not `pub(crate)` because [`parse_stat`] hands it out, and
/// `parse_stat` is `pub` so the fuzz target can drive it
/// (`fuzz/fuzz_targets/p9_stat.rs`). Its fields stay private: what callers - and
/// the fuzzer - need is that a record is placed inside the buffer it came in,
/// not the individual words.
pub struct Stat {
    kind: NodeKind,
    /// The qid's path - the identity the *server* keeps for the file, which
    /// is the closest thing a 9p share has to an inode number.
    path: u64,
    /// The permission bits the low nine carry: mode, not the DM word.
    perm: u32,
    atime: u32,
    mtime: u32,
    size: u64,
    name: String,
}

/// Decode one `statu` record out of a directory read or an `Rstat`.
///
/// The record is its own length word followed by that many bytes, which is
/// how the readdir stream can be walked without knowing anything about
/// what follows. A buffer that ends mid-record is a broken server; `None`
/// is the answer, and the caller treats it as damage rather than
/// truncating the listing silently.
///
/// The layout is `.u`'s `statu`: `type[2] dev[4] qid[13] mode[4] atime[4]
/// mtime[4] length[8] name[s] uid[s] gid[s] muid[s] extension[s] n_uid[4]
/// n_gid[4] n_muid[4]`. This reader stops after `name`, the last field it uses -
/// the numeric ids and the `extension` are the server's business - and `used` is
/// the whole record either way, so nothing here depends on them. It is `pub`
/// because it is the wire half's one parser and the fuzz target drives it
/// directly (`fuzz/fuzz_targets/p9_stat.rs`).
pub fn parse_stat(bytes: &[u8]) -> Option<(Stat, usize)> {
    let mut at = 0usize;
    let total = take_u16(bytes, &mut at)? as usize;
    let record = bytes.get(at..at.checked_add(total)?)?;
    at += total;

    // Inside the record: type[2] dev[4] qid[13] mode[4] atime[4]
    // mtime[4] length[8] name[s] uid[s] gid[s] muid[s] extension[s]
    // n_uid[4] n_gid[4] n_muid[4]. Only the fields up to `name` are read.
    let mut in_rec = 0usize;
    let _type = take_u16(record, &mut in_rec)?;
    let _dev = take_u32(record, &mut in_rec)?;
    let qid_type = *record.get(in_rec)?;
    // The qid is type[1] version[4] path[8]; the path is the file's
    // server-side identity.
    let mut qid_at = in_rec + 5;
    let qid_path = take_u64(record, &mut qid_at)?;
    in_rec += 13;
    let mode = take_u32(record, &mut in_rec)?;
    let atime = take_u32(record, &mut in_rec)?;
    let mtime = take_u32(record, &mut in_rec)?;
    let size = take_u64(record, &mut in_rec)?;
    let name = take_str(record, &mut in_rec)?;

    Some((
        Stat {
            kind: if mode & DM_DIR != 0 || qid_type & QT_DIR != 0 {
                NodeKind::Dir
            } else {
                NodeKind::File
            },
            path: qid_path,
            perm: mode & 0o777,
            atime,
            mtime,
            size,
            name,
        },
        at,
    ))
}

/// The 9P2000 wire has no errno. A server that fails says so in prose -
/// `ename`, the string an `Rerror` carries - and the only honest thing a
/// client can do is map the phrases it expects onto the failures it has
/// seen, and answer `EIO` for the prose it has not. Linux's v9fs does the
/// same, phrase for phrase, because the phrases are the protocol.
fn error_of(ename: &str) -> VfsError {
    if ename.contains("No such file") || ename.contains("not found") {
        return VfsError::NoEntry;
    }
    if ename.contains("exists") || ename.contains("Exist") {
        return VfsError::Exists;
    }
    if ename.contains("not empty") || ename.contains("Not empty") {
        return VfsError::NotEmpty;
    }
    if ename.contains("Is a directory") {
        return VfsError::IsDir;
    }
    if ename.contains("Not a directory") {
        return VfsError::NotDir;
    }
    if ename.contains("Read-only") || ename.contains("read only") {
        return VfsError::ReadOnly;
    }
    if ename.contains("No space") || ename.contains("too large") {
        return VfsError::NoSpace;
    }
    if ename.contains("Permission") || ename.contains("denied") {
        return VfsError::Access;
    }
    if ename.contains("too long") {
        return VfsError::Invalid;
    }
    // Corrupt is EIO - the answer Linux gives for damage it can see but
    // not classify, which is what unknown prose from a server is.
    VfsError::Corrupt
}

/// Read a little-endian `u16` at `*at`, advancing it.
fn take_u16(bytes: &[u8], at: &mut usize) -> Option<u16> {
    let raw = bytes.get(*at..at.checked_add(2)?)?;
    *at += 2;
    Some(u16::from_le_bytes([raw[0], raw[1]]))
}

fn take_u32(bytes: &[u8], at: &mut usize) -> Option<u32> {
    let raw = bytes.get(*at..at.checked_add(4)?)?;
    *at += 4;
    Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn take_u64(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let raw = bytes.get(*at..at.checked_add(8)?)?;
    *at += 8;
    let mut word = [0u8; 8];
    word.copy_from_slice(raw);
    Some(u64::from_le_bytes(word))
}

/// Read one counted string at `*at`, advancing past it.
fn take_str(bytes: &[u8], at: &mut usize) -> Option<String> {
    let len = take_u16(bytes, at)? as usize;
    let raw = bytes.get(*at..at.checked_add(len)?)?;
    *at += len;
    Some(String::from_utf8_lossy(raw).into_owned())
}

/// Append a counted string.
fn put_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s);
}

/// Append a `u32` in the wire's little-endian.
/// A `Tcreate` body: the fid to create in, the name, the permission word and the
/// open mode, followed by `9P2000.u`'s `extension[s]`.
///
/// The last field is the point of this being a function rather than three lines
/// inside the client: it is a field the *server* unmarshals, so a body that stops
/// short at `mode` is one the server cannot decode **at all** - not one it decodes
/// and refuses - which means the caller waits for an answer that never comes.
/// A missing field is therefore not a wrong answer to be checked for but a
/// *hang*, and the only way to catch one cheaply is to pin the bytes.
///
/// Measured, which is what makes the field load-bearing rather than decorative: a
/// guest creating a file through the host share wedged with
/// `qemu-system-x86_64: Failed to decode VirtFS request type 114` on the host's
/// side (114 being `Tcreate`) and no reply on the guest's, while every *read*
/// through the same share worked - which is why `t-9p`, whose subject was the
/// read half, stayed green.
fn create_body(dir_fid: u32, name: &str, perm: u32, mode: u8) -> Vec<u8> {
    let mut body = Vec::new();
    put_u32(&mut body, dir_fid);
    put_str(&mut body, name.as_bytes());
    put_u32(&mut body, perm);
    body.push(mode);
    put_str(&mut body, b"");
    body
}

fn put_u32(out: &mut Vec<u8>, word: u32) {
    out.extend_from_slice(&word.to_le_bytes());
}

// ---------------------------------------------------------------------------
// The device half: the filesystem instance, its fid table, and the calls
// that reach the host. Everything from here down needs the virtio device,
// so it is built for the machine alone - the wire half above stays
// host-testable.
// ---------------------------------------------------------------------------

#[cfg(target_os = "none")]
mod device {
    use super::*;

    /// The message size the mount asks for. The host may answer lower, and
    /// the answer is the number every later `read` and `write` chunks by -
    /// a request bigger than `msize` is the one thing the protocol refuses
    /// outright.
    const MSIZE: u32 = 8192;

    /// The head of a message: `size[4] type[1] tag[2]`.
    const HEADER: usize = 7;

    pub struct P9fs {
        dev: Arc<dyn crate::drivers::virtio::P9Device>,
        /// The message size the handshake settled on: ours, or the host's
        /// smaller answer.
        msize: Mutex<RefCell<u32>>,
        /// The next tag - a number to match an answer by. It only has to be
        /// unique among the requests in flight, and there is exactly one of
        /// those at a time (the transport posts and waits), but the answer
        /// is still checked against it: a reply that carries another tag
        /// is not this conversation's reply, and honouring it would be
        /// trusting the server to interleave for us.
        next_tag: Mutex<RefCell<u16>>,
        /// The next fid. Fid zero is the root's and is skipped.
        next_fid: Mutex<RefCell<u32>>,
        root: Mutex<RefCell<Option<Arc<P9Inode>>>>,
    }

    impl P9fs {
        /// Open the share the tag names: handshake, attach, and the root
        /// inode the mount table will hand out. The tag is matched against
        /// the devices the boot found, so `mount -t 9p hostshare /mnt` is
        /// answered by the device QEMU exported under that very tag - and
        /// an empty tag takes the only share there is, which is what the
        /// boot's own mount passes.
        pub fn mount(tag: &str) -> VfsResult<Arc<P9fs>> {
            let Some(dev) = crate::drivers::virtio::fs9p::by_tag(tag) else {
                return Err(VfsError::NoEntry);
            };

            let fs = Arc::new(P9fs {
                dev,
                msize: Mutex::new(RefCell::new(MSIZE)),
                next_tag: Mutex::new(RefCell::new(0)),
                next_fid: Mutex::new(RefCell::new(1)),
                root: Mutex::new(RefCell::new(None)),
            });

            fs.version()?;
            fs.attach()?;
            let root = Arc::new(P9Inode {
                fs: Arc::downgrade(&fs),
                fid: Mutex::new(RefCell::new(Some(0))),
                // The attach answer is the root's qid: type says directory,
                // which is the only fact about the root that is not asked
                // for afresh by whoever stats it.
                kind: NodeKind::Dir,
                open: Mutex::new(RefCell::new(None)),
                size: Mutex::new(RefCell::new(0)),
            });
            critical_section::with(|cs| {
                *fs.root.borrow(cs).borrow_mut() = Some(root);
            });
            Ok(fs)
        }

        /// The message size in force, for callers chunking by it.
        fn msize(&self) -> u32 {
            critical_section::with(|cs| *self.msize.borrow(cs).borrow())
        }

        /// One round trip: build the message around `body`, send it, and
        /// return the answer's bytes whole - header included, so the
        /// readers can pull fields at the offsets the protocol spells.
        fn call(&self, typ: u8, body: &[u8], expect: u8) -> VfsResult<Vec<u8>> {
            // A tag is a number to match an answer by. There is exactly one
            // request in flight at a time - the transport posts and waits -
            // so uniqueness would not matter; but the answer is still
            // checked against it, because a reply carrying another tag is
            // not this conversation's reply, and honouring it would be
            // trusting the server to interleave for us.
            let tag = critical_section::with(|cs| {
                let mut next = self.next_tag.borrow(cs).borrow_mut();
                let tag = *next;
                *next = next.wrapping_add(1).max(1);
                tag
            });

            let mut req = Vec::with_capacity(HEADER + body.len());
            req.extend_from_slice(&((HEADER + body.len()) as u32).to_le_bytes());
            req.push(typ);
            req.extend_from_slice(&tag.to_le_bytes());
            req.extend_from_slice(body);

            let mut resp = alloc::vec![0u8; self.msize() as usize];
            if !self.dev.request(&req, &mut resp) {
                return Err(VfsError::Corrupt);
            }

            if resp.len() < HEADER {
                return Err(VfsError::Corrupt);
            }
            let size = take_u32(&resp, &mut 0).ok_or(VfsError::Corrupt)? as usize;
            if size < HEADER || size > resp.len() {
                return Err(VfsError::Corrupt);
            }
            if resp[4] == R_ERROR {
                let mut at = HEADER;
                let ename = take_str(&resp[..size], &mut at).unwrap_or_default();
                return Err(error_of(&ename));
            }
            if resp[4] != expect {
                return Err(VfsError::Corrupt);
            }
            let answered = take_u16(&resp, &mut { HEADER - 2 });
            if answered != Some(tag) {
                // A well-formed reply about someone else's request. The
                // transport waits for the answer before returning, so this
                // cannot happen against an honest server - and a dishonest
                // one gets no bytes honoured.
                return Err(VfsError::Corrupt);
            }
            resp.truncate(size);
            Ok(resp)
        }

        /// `Tversion`: agree the dialect and the message size. The host is
        /// allowed to answer with a smaller `msize` than was asked for, and
        /// that answer is the size every later message obeys.
        ///
        /// A server that answers any dialect but [`DIALECT`] is refused rather
        /// than parsed as if it were: plain `9P2000` lacks the fields `.u` adds,
        /// and `.L` is a different message set entirely.
        fn version(&self) -> VfsResult<()> {
            let mut body = Vec::new();
            put_u32(&mut body, MSIZE);
            put_str(&mut body, DIALECT.as_bytes());
            let answer = self.call(T_VERSION, &body, R_VERSION)?;
            let mut at = HEADER;
            let msize = take_u32(&answer, &mut at).ok_or(VfsError::Corrupt)?;
            let dialect = take_str(&answer, &mut at).ok_or(VfsError::Corrupt)?;
            if dialect != DIALECT {
                // Plain `9P2000`, or `.L` - whose message set is a different
                // protocol this client did not ask for and does not parse.
                return Err(VfsError::Unsupported);
            }
            critical_section::with(|cs| {
                *self.msize.borrow(cs).borrow_mut() = msize;
            });
            Ok(())
        }

        /// `Tattach` on fid zero, as the root of the share. The `uname` is
        /// a formality this host ignores; the `aname` is not - QEMU routes
        /// the attach by the export's tag, and the tag the device itself
        /// reported is the one the export answers to.
        ///
        /// The trailing `n_uname` is `.u`'s addition and is the numeric half of
        /// the same formality: `0` names the same principal `uname` does, and
        /// with the share exported `security_model=none` the host decides access
        /// from its own process identity, so no uid mapping is asked for.
        fn attach(&self) -> VfsResult<()> {
            let mut body = Vec::new();
            put_u32(&mut body, 0); // fid
            put_u32(&mut body, u32::MAX); // afid: NOFID
            put_str(&mut body, b"root");
            put_str(&mut body, self.dev.mount_tag().as_bytes());
            put_u32(&mut body, 0); // n_uname: the numeric half of uname
            self.call(T_ATTACH, &body, R_ATTACH)?;
            Ok(())
        }

        /// Mint the next fid. Fid zero belongs to the root and is never
        /// reissued; the count is monotonic, which is what makes a fid
        /// spent on a failed walk harmless - the number is never anyone
        /// else's, so there is nothing to hand back.
        fn alloc_fid(&self) -> VfsResult<u32> {
            critical_section::with(|cs| {
                let mut next = self.next_fid.borrow(cs).borrow_mut();
                let fid = *next;
                if fid == 0 {
                    return Err(VfsError::Corrupt);
                }
                *next = next.checked_add(1).ok_or(VfsError::Corrupt)?;
                Ok(fid)
            })
        }

        /// `Twalk` from `fid` along `names`, into a freshly minted fid.
        /// An empty `names` duplicates `fid` - the protocol's own fid copy,
        /// which is how the operations that consume a fid (open for a
        /// directory read, remove, create) work on a throwaway instead of
        /// the one an inode is keeping.
        fn walk(&self, fid: u32, names: &[&str]) -> VfsResult<(u32, u8)> {
            let newfid = self.alloc_fid()?;
            let mut body = Vec::new();
            put_u32(&mut body, fid);
            put_u32(&mut body, newfid);
            body.extend_from_slice(&(names.len() as u16).to_le_bytes());
            for name in names {
                put_str(&mut body, name.as_bytes());
            }
            let answer = self.call(T_WALK, &body, R_WALK)?;
            let mut at = HEADER;
            let walked = take_u16(&answer, &mut at).ok_or(VfsError::Corrupt)?;
            if walked as usize != names.len() {
                // A partial walk: the server moved partway and stopped. It
                // has also clunked the newfid in that case, so there is
                // nothing to give back - and the answer is "no such file",
                // which is what every caller of a failed walk means.
                return Err(VfsError::NoEntry);
            }
            // The last qid's type byte is the kind of thing arrived at.
            let mut qid_type = 0u8;
            for _ in 0..walked {
                qid_type = *answer.get(at).ok_or(VfsError::Corrupt)?;
                at += 13;
            }
            Ok((newfid, qid_type))
        }

        /// `Tclunk`: retire a fid. Best-effort by design - a clunk nobody
        /// can hear (an unresponsive device at teardown) leaks a server
        /// entry, and the machine is on its way down anyway.
        fn clunk(&self, fid: u32) {
            let mut body = Vec::new();
            put_u32(&mut body, fid);
            let _ = self.call(T_CLUNK, &body, R_CLUNK);
        }

        /// `Topen` a fid in `mode`, answering the iounit the server
        /// reported - its hint for the read size that will not be split.
        fn open(&self, fid: u32, mode: u8) -> VfsResult<u32> {
            let mut body = Vec::new();
            put_u32(&mut body, fid);
            body.push(mode);
            let answer = self.call(T_OPEN, &body, R_OPEN)?;
            let mut at = HEADER + 13; // the qid the answer leads with
            take_u32(&answer, &mut at).ok_or(VfsError::Corrupt)
        }

        /// `Tread` at an absolute offset. One round trip's worth at most,
        /// so a caller with a big buffer loops - the offset is what makes
        /// the pieces line up again.
        fn read_once(&self, fid: u32, offset: u64, out: &mut [u8]) -> VfsResult<usize> {
            let room = out.len().min(self.msize() as usize - 11);
            let mut body = Vec::new();
            put_u32(&mut body, fid);
            body.extend_from_slice(&offset.to_le_bytes());
            put_u32(&mut body, room as u32);
            let answer = self.call(T_READ, &body, R_READ)?;
            let mut at = HEADER;
            let count = take_u32(&answer, &mut at).ok_or(VfsError::Corrupt)? as usize;
            let end = at.checked_add(count).ok_or(VfsError::Corrupt)?;
            let data = answer.get(at..end).ok_or(VfsError::Corrupt)?;
            out[..count].copy_from_slice(data);
            Ok(count)
        }

        /// `Twrite` at an absolute offset, chunked to the message size.
        /// Answers how many bytes the server took - a short answer is a
        /// full disk or a full-sized refusal, and the caller decides
        /// whether to continue.
        fn write(&self, fid: u32, offset: u64, data: &[u8]) -> VfsResult<usize> {
            let limit = (self.msize() - 23) as usize;
            let mut done = 0usize;
            loop {
                let piece = &data[done..];
                let piece = &piece[..piece.len().min(limit)];
                let mut body = Vec::new();
                put_u32(&mut body, fid);
                body.extend_from_slice(&(offset + done as u64).to_le_bytes());
                put_u32(&mut body, piece.len() as u32);
                body.extend_from_slice(piece);
                let answer = self.call(T_WRITE, &body, R_WRITE)?;
                let mut at = HEADER;
                let count = take_u32(&answer, &mut at).ok_or(VfsError::Corrupt)? as usize;
                done += count;
                if count < piece.len() || done == data.len() {
                    return Ok(done);
                }
            }
        }

        /// `Tstat` a fid.
        fn stat(&self, fid: u32) -> VfsResult<Stat> {
            let mut body = Vec::new();
            put_u32(&mut body, fid);
            let answer = self.call(T_STAT, &body, R_STAT)?;
            // Rstat leads with a u16 count and then the stat itself.
            let mut at = HEADER;
            let _count = take_u16(&answer, &mut at).ok_or(VfsError::Corrupt)?;
            let (stat, _) = parse_stat(&answer[at..]).ok_or(VfsError::Corrupt)?;
            Ok(stat)
        }

        /// `Twstat`: change what a caller asked to change and mark the rest
        /// "leave alone" - every field has a dont-care spelling, which is
        /// what keeps a `truncate` from also rewriting the times.
        ///
        /// The record is `.u`'s `statu`: the plain `stat` fields followed by
        /// `extension[s]` and the three numeric ids, each with its own dont-care
        /// (`""` for the string, `u32::MAX` for the ids). They have to be present
        /// even when nothing is being changed, because the server parses the
        /// record whole.
        fn wstat(
            &self,
            fid: u32,
            mode: Option<u32>,
            atime: Option<u32>,
            mtime: Option<u32>,
            length: Option<u64>,
        ) -> VfsResult<()> {
            let dont_care = |field: &mut Vec<u8>, value: Option<u32>| {
                put_u32(field, value.unwrap_or(u32::MAX));
            };
            let mut record = Vec::new();
            record.extend_from_slice(&u16::MAX.to_le_bytes()); // type
            put_u32(&mut record, u32::MAX); // dev
            record.extend(&[u8::MAX; 13]); // qid
            dont_care(&mut record, mode);
            dont_care(&mut record, atime);
            dont_care(&mut record, mtime);
            record.extend_from_slice(&length.unwrap_or(u64::MAX).to_le_bytes());
            put_str(&mut record, b""); // name: keep
            put_str(&mut record, b""); // uid
            put_str(&mut record, b""); // gid
            put_str(&mut record, b""); // muid
            put_str(&mut record, b""); // extension: none
            put_u32(&mut record, u32::MAX); // n_uid: keep
            put_u32(&mut record, u32::MAX); // n_gid
            put_u32(&mut record, u32::MAX); // n_muid

            let mut body = Vec::new();
            put_u32(&mut body, fid);
            body.extend_from_slice(&(record.len() as u16).to_le_bytes());
            body.extend_from_slice(&record);
            self.call(T_WSTAT, &body, R_WSTAT)?;
            Ok(())
        }

        /// `Tcreate` in a directory reached by `dir_fid`: the fid - which
        /// must be a throwaway, because the server turns it into the new
        /// file's - becomes an open handle on what was made. The permission
        /// word carries the DM_DIR bit for a directory, exactly as on the
        /// wire.
        fn create(&self, dir_fid: u32, name: &str, perm: u32, mode: u8) -> VfsResult<(u8, u32)> {
            let body = create_body(dir_fid, name, perm, mode);
            let answer = self.call(T_CREATE, &body, R_CREATE)?;
            let qid_type = *answer.get(HEADER).ok_or(VfsError::Corrupt)?;
            let iounit = take_u32(&answer, &mut { HEADER + 13 }).ok_or(VfsError::Corrupt)?;
            Ok((qid_type, iounit))
        }

        /// `Tremove`: unlink by fid. The server retires the fid whether or
        /// not the removal took, so the caller treats the fid as spent the
        /// moment the call returns - success or error alike.
        fn remove(&self, fid: u32) -> VfsResult<()> {
            let mut body = Vec::new();
            put_u32(&mut body, fid);
            self.call(T_REMOVE, &body, R_REMOVE)?;
            Ok(())
        }

        /// `Tread` a directory's entries, by a throwaway fid. One `Topen`
        /// and a stream of reads until the server answers empty, which is
        /// the protocol's whole notion of "end of directory".
        fn read_dir(&self, dir_fid: u32) -> VfsResult<Vec<(String, NodeKind)>> {
            let (fid, _) = self.walk(dir_fid, &[])?;
            let result = (|| {
                self.open(fid, O_READ)?;
                let mut entries = Vec::new();
                let mut offset = 0u64;
                loop {
                    let mut chunk = alloc::vec![0u8; self.msize() as usize - HEADER];
                    let got = self.read_once(fid, offset, &mut chunk)?;
                    if got == 0 {
                        return Ok(entries);
                    }
                    offset += got as u64;
                    let mut at = 0;
                    while at < got {
                        let Some((stat, used)) = parse_stat(&chunk[at..got]) else {
                            // A record the parser cannot place is damage:
                            // report the entries that parsed rather than
                            // pretending the listing was whole.
                            return Ok(entries);
                        };
                        entries.push((stat.name, stat.kind));
                        at += used;
                    }
                }
            })();
            self.clunk(fid);
            result
        }
    }

    // -- the inode ----------------------------------------------------------

    /// One file or directory of the share, reached by the fid a walk left.
    pub struct P9Inode {
        fs: Weak<P9fs>,
        fid: Mutex<RefCell<Option<u32>>>,
        kind: NodeKind,
        /// The open state: `Some((mode, iounit))` once a `Topen` succeeded
        /// on the fid. A file opened for reading that is asked to write
        /// answers `ReadOnly` rather than re-opening - the server turned
        /// the fid into a read-only handle, and a second open on it is an
        /// error the protocol has no way to take back.
        open: Mutex<RefCell<Option<(u8, u32)>>>,
        /// The size the last `stat` reported. Kept so `metadata` can answer
        /// without a round trip when the filesystem is gone, and updated by
        /// writes so a `stat` right after one does not have to ask.
        size: Mutex<RefCell<u64>>,
    }

    impl P9Inode {
        fn new(fs: Weak<P9fs>, fid: u32, kind: NodeKind) -> Arc<Self> {
            Arc::new(P9Inode {
                fs,
                fid: Mutex::new(RefCell::new(Some(fid))),
                kind,
                open: Mutex::new(RefCell::new(None)),
                size: Mutex::new(RefCell::new(0)),
            })
        }

        /// The live filesystem, or the error for a node that outlived it -
        /// an fd held past an unmount, which on Linux reads as EIO too.
        fn up(&self) -> VfsResult<Arc<P9fs>> {
            self.fs.upgrade().ok_or(VfsError::Corrupt)
        }

        /// The fid this node holds, spent if it has been clunked.
        fn fid(&self) -> VfsResult<u32> {
            critical_section::with(|cs| self.fid.borrow(cs).borrow().ok_or(VfsError::BadFd))
        }

        /// Open the fid if it is not open, in the mode the caller needs.
        fn ensure_open(&self, fs: &P9fs, for_write: bool) -> VfsResult<u32> {
            let state = critical_section::with(|cs| *self.open.borrow(cs).borrow());
            if let Some((mode, iounit)) = state {
                if for_write && mode == O_READ {
                    return Err(VfsError::ReadOnly);
                }
                return Ok(iounit);
            }
            let mode = if for_write { O_RDWR } else { O_READ };
            let iounit = fs.open(self.fid()?, mode)?;
            critical_section::with(|cs| {
                *self.open.borrow(cs).borrow_mut() = Some((mode, iounit));
            });
            Ok(iounit)
        }

        /// Refresh the cached size and answer it, along with the rest of
        /// the facts a `stat` wants. The server is asked every time: this
        /// kernel's filesystems are write-through and cache no metadata,
        /// because a `stat` that answers from yesterday is a `stat` that
        /// lies about what the host wrote since.
        fn fresh_stat(&self, fs: &P9fs) -> Stat {
            let fid = self.fid().ok();
            let stat = fid.and_then(|fid| fs.stat(fid).ok());
            match stat {
                Some(stat) => {
                    critical_section::with(|cs| {
                        *self.size.borrow(cs).borrow_mut() = stat.size;
                    });
                    stat
                }
                None => {
                    let size = critical_section::with(|cs| *self.size.borrow(cs).borrow());
                    Stat {
                        kind: self.kind,
                        path: 0,
                        perm: 0,
                        atime: 0,
                        mtime: 0,
                        size,
                        name: String::new(),
                    }
                }
            }
        }
    }

    impl Drop for P9Inode {
        fn drop(&mut self) {
            // The root's fid belongs to the mount, not to this node: it is
            // clunked when the filesystem itself goes, by nothing at all -
            // the share lives as long as the device does, and QEMU reclaims
            // every fid the moment the guest resets. Retiring it here would
            // tear the root out from under the mount table, which holds
            // this very node.
            let fid = critical_section::with(|cs| self.fid.borrow(cs).borrow_mut().take());
            if let Some(fid) = fid
                && fid != 0
                && let Some(fs) = self.fs.upgrade()
            {
                fs.clunk(fid);
            }
        }
    }

    impl Inode for P9Inode {
        fn metadata(&self) -> Metadata {
            let Ok(fs) = self.up() else {
                let size = critical_section::with(|cs| *self.size.borrow(cs).borrow());
                return Metadata {
                    ino: 0,
                    kind: self.kind,
                    size,
                };
            };
            let stat = self.fresh_stat(&fs);
            Metadata {
                // The qid path is the identity the server itself keeps;
                // 0 is the placeholder for a node nobody could ask about.
                ino: stat.path,
                kind: stat.kind,
                size: stat.size,
            }
        }

        fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
            let fs = self.up()?;
            if self.kind != NodeKind::Dir {
                return Err(VfsError::NotDir);
            }
            let (fid, qid_type) = fs.walk(self.fid()?, &[name])?;
            let kind = if qid_type & QT_DIR != 0 {
                NodeKind::Dir
            } else {
                NodeKind::File
            };
            Ok(P9Inode::new(self.fs.clone(), fid, kind))
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
            let fs = self.up()?;
            if self.kind == NodeKind::Dir {
                // Reading a directory's bytes is not what a 9p share offers:
                // the entries come out through `read_dir`, as the VFS
                // contract wants. `IsDir` is the standard refusal.
                return Err(VfsError::IsDir);
            }
            self.ensure_open(&fs, false)?;
            let mut done = 0usize;
            while done < buf.len() {
                let got = fs.read_once(self.fid()?, offset + done as u64, &mut buf[done..])?;
                if got == 0 {
                    break;
                }
                done += got;
            }
            Ok(done)
        }

        fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
            let fs = self.up()?;
            if self.kind == NodeKind::Dir {
                return Err(VfsError::IsDir);
            }
            self.ensure_open(&fs, true)?;
            let written = fs.write(self.fid()?, offset, buf)?;
            // The size the next `stat` reports should include what this
            // write just put on the far end - a writer that appends and
            // stats should not see the size from before its write.
            critical_section::with(|cs| {
                let mut size = self.size.borrow(cs).borrow_mut();
                *size = (*size).max(offset + written as u64);
            });
            Ok(written)
        }

        fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
            let fs = self.up()?;
            if self.kind != NodeKind::Dir {
                return Err(VfsError::NotDir);
            }
            fs.read_dir(self.fid()?)
        }

        /// Create a name here, with the number a device node would carry - a
        /// number this dialect cannot send, so one asked for is refused rather
        /// than made without it (`docs/uapi/` §5.1 item 71).
        fn create(
            &self,
            name: &str,
            kind: NodeKind,
            rdev: Option<(u32, u32)>,
        ) -> VfsResult<Arc<dyn Inode>> {
            let fs = self.up()?;
            let (perm, mode) = match kind {
                NodeKind::File => (0o644, O_RDWR),
                NodeKind::Dir => (DM_DIR | 0o777, O_READ),
                // The dialect has no message for these, and a driver that
                // answered one would be a driver inventing a protocol.
                _ => return Err(VfsError::Unsupported),
            };
            if rdev.is_some() {
                return Err(VfsError::Unsupported);
            }
            // A throwaway of the directory, because Tcreate turns the fid
            // it is given into the new file's.
            let (dir_clone, _) = fs.walk(self.fid()?, &[])?;
            let made = fs.create(dir_clone, name, perm, mode);
            match made {
                Ok((qid_type, iounit)) => {
                    let kind = if perm & DM_DIR != 0 || qid_type & QT_DIR != 0 {
                        NodeKind::Dir
                    } else {
                        NodeKind::File
                    };
                    let node = P9Inode::new(self.fs.clone(), dir_clone, kind);
                    critical_section::with(|cs| {
                        *node.open.borrow(cs).borrow_mut() = Some((mode, iounit));
                    });
                    Ok(node)
                }
                Err(e) => {
                    // The create failed; the server left the clone clunked
                    // or unusable, and either way it is nobody's now.
                    Err(e)
                }
            }
        }

        fn unlink(&self, name: &str) -> VfsResult<()> {
            let fs = self.up()?;
            // Walk to the entry, then remove by the walked fid. Tremove
            // retires the fid, so the throwaway never comes back.
            let (fid, _) = fs.walk(self.fid()?, &[name])?;
            fs.remove(fid)
        }

        fn rmdir(&self, name: &str) -> VfsResult<()> {
            // One request for both: the server enforces "empty or refused",
            // which is the whole of what a separate rmdir path would add.
            self.unlink(name)
        }

        fn truncate(&self, len: u64) -> VfsResult<()> {
            let fs = self.up()?;
            fs.wstat(self.fid()?, None, None, None, Some(len))
        }

        fn set_mode(&self, mode: u32) {
            // The permission bits only - the DM_DIR bit is not a caller's
            // to change, and a mode word that carried it in would ask the
            // server to turn a file into a directory. The trait's shape has
            // no room for a refusal to travel back, so a node whose
            // filesystem is gone - or whose fid is spent - keeps silent,
            // which is what a `chmod` nobody can hear does.
            let Ok(fs) = self.up() else { return };
            let Ok(fid) = self.fid() else { return };
            let _ = fs.wstat(fid, Some(mode & 0o777), None, None, None);
        }

        fn keeps_modes(&self) -> bool {
            true
        }

        fn set_times(&self, atime: u64, mtime: u64) -> VfsResult<()> {
            let fs = self.up()?;
            fs.wstat(
                self.fid()?,
                None,
                Some(atime as u32),
                Some(mtime as u32),
                None,
            )
        }

        fn mtime(&self) -> u64 {
            let Ok(fs) = self.up() else { return 0 };
            self.fresh_stat(&fs).mtime as u64
        }

        fn atime(&self) -> u64 {
            let Ok(fs) = self.up() else { return 0 };
            self.fresh_stat(&fs).atime as u64
        }

        fn mode(&self) -> u32 {
            let Ok(fs) = self.up() else { return 0 };
            self.fresh_stat(&fs).perm
        }

        fn seekable(&self) -> bool {
            // Both kinds of node are: files at their offsets, directories
            // by their entry stream - the VFS reads those whole, not by
            // position, but a caller probing with lseek gets "yes" either
            // way, as a 9p share has no kind that cannot be positioned.
            true
        }
    }

    impl FileSystem for P9fs {
        fn root(&self) -> Arc<dyn Inode> {
            critical_section::with(|cs| {
                let root = self.root.borrow(cs).borrow();
                root.clone().expect("the root exists from mount on")
            })
        }

        fn name(&self) -> &'static str {
            "9p"
        }
    }
}

#[cfg(target_os = "none")]
pub use device::{P9Inode, P9fs};

/// The mount entry point the `mount(2)` dispatch and the boot both call:
/// open the share the tag names as a filesystem. The tag matches what the
/// boot probed, so the call is answered from the device table rather than
/// a second probe - and an unknown tag is `NoEntry`, the errno a mount
/// source that does not exist has always carried.
#[cfg(target_os = "none")]
pub fn mount(tag: &str) -> VfsResult<Arc<P9fs>> {
    device::P9fs::mount(tag)
}

// ---------------------------------------------------------------------------
// Host tests: the wire half is pure, and its parsers deserve the same
// proving every other decodable record in this tree gets.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    /// One well-formed `statu` record, in the layout `Rstat` delivers: a u16
    /// size, then type, dev, qid, mode, times, length, the four strings, the
    /// `extension` and the three numeric ids `.u` appends.
    fn stat_bytes(mode: u32, length: u64, name: &str) -> Vec<u8> {
        let mut record = Vec::new();
        record.extend_from_slice(&0u16.to_le_bytes()); // type
        record.extend_from_slice(&0u32.to_le_bytes()); // dev
        record.extend(&[0u8; 13]); // qid
        put_u32(&mut record, mode);
        put_u32(&mut record, 1); // atime
        put_u32(&mut record, 2); // mtime
        record.extend_from_slice(&length.to_le_bytes());
        put_str(&mut record, name.as_bytes());
        put_str(&mut record, b"root");
        put_str(&mut record, b"root");
        put_str(&mut record, b"root");
        // `.u`'s tail: the parser stops before it, but a real record carries it,
        // so the fixture has to as well or the tests would pin a shape no server
        // sends.
        put_str(&mut record, b"");
        put_u32(&mut record, 0); // n_uid
        put_u32(&mut record, 0); // n_gid
        put_u32(&mut record, 0); // n_muid

        let mut out = Vec::new();
        out.extend_from_slice(&(record.len() as u16).to_le_bytes());
        out.extend_from_slice(&record);
        out
    }

    #[test]
    fn a_statu_records_numeric_tail_is_stepped_over_rather_than_parsed() {
        // `.u`'s `extension` and ids are the server's business; the point of the
        // test is that a record carrying them is consumed *whole*, so a readdir
        // stream of such records walks to its end instead of stalling on the
        // first one.
        let bytes = stat_bytes(0o644, 7, "tail");
        let (stat, used) = parse_stat(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(stat.name, "tail");
        assert_eq!(stat.size, 7);

        // And the reader does not *depend* on the tail: a record whose length
        // word covers only the plain fields - what a plain-9P2000 server sends -
        // parses just the same, which is what lets this client step over either
        // shape. Fourteen bytes come off (the empty `extension`, 2, and the
        // three ids, 12) and the leading length word is rewritten to match.
        const TAIL: usize = 2 + 4 + 4 + 4;
        let mut short = bytes[..bytes.len() - TAIL].to_vec();
        let total = (short.len() - 2) as u16;
        short[..2].copy_from_slice(&total.to_le_bytes());
        let (stat, used) = parse_stat(&short).unwrap();
        assert_eq!(used, short.len());
        assert_eq!(stat.name, "tail");
        assert_eq!(stat.size, 7);
    }

    #[test]
    fn a_file_stat_parses_with_its_size_and_name() {
        let bytes = stat_bytes(0o644, 4096, "seed.txt");
        let (stat, used) = parse_stat(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(stat.kind, NodeKind::File);
        assert_eq!(stat.size, 4096);
        assert_eq!(stat.name, "seed.txt");
        assert_eq!(stat.perm, 0o644);
        assert_eq!(stat.atime, 1);
        assert_eq!(stat.mtime, 2);
    }

    #[test]
    fn a_directory_stat_carries_the_directory_bit() {
        let bytes = stat_bytes(DM_DIR | 0o755, 0, "etc");
        let (stat, _) = parse_stat(&bytes).unwrap();
        assert_eq!(stat.kind, NodeKind::Dir);
        // The permission bits read back without the kind bit mixed in.
        assert_eq!(stat.perm, 0o755);
    }

    #[test]
    fn a_truncated_stat_is_refused_rather_than_guessed() {
        let bytes = stat_bytes(0o644, 1, "half");
        assert!(parse_stat(&bytes[..bytes.len() - 3]).is_none());
    }

    /// `Tcreate`'s body, field for field, including the one it used to stop
    /// short of.
    ///
    /// A test that cannot be written as an assertion about *behaviour*, and the
    /// reason is the defect: the server's unmarshal reads the dialect's trailing
    /// `extension[s]`, so a body without it is not refused, it is *undecodable* -
    /// and an undecodable request is answered by silence, so the observable
    /// behaviour is a hang rather than a wrong result. The bytes are the only
    /// cheap pin there is.
    #[test]
    fn a_create_body_ends_with_the_dialects_extension() {
        let body = create_body(7, "scratch", 0o644, 0);

        let mut expected = Vec::new();
        expected.extend_from_slice(&7u32.to_le_bytes()); // fid
        expected.extend_from_slice(&7u16.to_le_bytes()); // name, as a 9P string
        expected.extend_from_slice(b"scratch");
        expected.extend_from_slice(&0o644u32.to_le_bytes()); // perm
        expected.push(0); // mode
        expected.extend_from_slice(&0u16.to_le_bytes()); // extension: empty, but *present*
        assert_eq!(body, expected);

        // And the field is a string like any other rather than a byte: a
        // non-empty extension would be length-prefixed the same way, which is
        // what makes "empty" a value of the field rather than its absence.
        assert_eq!(&body[body.len() - 2..], &[0, 0]);
    }

    #[test]
    fn the_prose_a_server_fails_with_maps_onto_the_errors_callers_know() {
        assert_eq!(error_of("No such file or directory"), VfsError::NoEntry);
        assert_eq!(error_of("File exists"), VfsError::Exists);
        assert_eq!(error_of("Directory not empty"), VfsError::NotEmpty);
        assert_eq!(error_of("Read-only file system"), VfsError::ReadOnly);
        // Unknown prose is EIO, not a guess.
        assert_eq!(error_of("something novel"), VfsError::Corrupt);
    }
}
