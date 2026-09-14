//! Reading ustar archives: the initramfs format.
//!
//! A ustar archive is a run of 512-byte blocks. Each member starts with a
//! header block (name, size, type, all ASCII), followed by its data padded out
//! to a 512-byte boundary. The archive ends at the first block that is not a
//! header - usually two zero blocks, but reading defensively means an abrupt
//! end parses the same as a tidy one.
//!
//! Only what an initramfs needs is parsed: plain files and directories, with
//! paths taken from the ustar `name` field, which covers every path short of
//! 100 bytes without the pax or GNU long-name extensions. `mkiso.sh` builds
//! the archive with `tar --format=ustar` to keep that promise.

use alloc::format;
use alloc::sync::Arc;

use super::mount::Vfs;
use super::{Inode, NodeKind, VfsError, VfsResult};

pub const BLOCK: usize = 512;

/// What kind of member this is; anything else is skipped by the iterator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// A symbolic link. Its `data` is not contents: it is the path the link
    /// stands for, taken from the ustar `linkname` field.
    Symlink,
}

/// One member of the archive. `data` borrows the archive.
pub struct Entry<'a> {
    /// The member path, without the `./` some tars prefix or the trailing
    /// `/` directories carry.
    pub path: &'a str,
    pub kind: EntryKind,
    /// Empty for directories.
    pub data: &'a [u8],
}

/// An iterator over the members of an uncompressed ustar archive.
pub struct Entries<'a> {
    rest: &'a [u8],
}

/// Parse an archive. Malformed bytes are not an error case: the iterator
/// simply stops there, and the unpacker reports how much it got.
pub fn entries(archive: &[u8]) -> Entries<'_> {
    Entries { rest: archive }
}

impl<'a> Iterator for Entries<'a> {
    type Item = Entry<'a>;

    fn next(&mut self) -> Option<Entry<'a>> {
        let (header, _) = self.rest.as_chunks::<BLOCK>().0.split_first()?;
        // A header is recognized by its magic; anything else ends the archive.
        if header[257..263] != *b"ustar\0" {
            return None;
        }
        let kind = match header[156] {
            b'0' | b'\0' => EntryKind::File,
            b'5' => EntryKind::Dir,
            b'2' => EntryKind::Symlink,
            _ => {
                self.rest = &[];
                return None;
            }
        };
        let size = octal(&header[124..136])?;
        let name = str_field(&header[0..100])?;

        let data_len = size as usize;
        let padded = data_len.div_ceil(BLOCK) * BLOCK;
        if self.rest.len() < BLOCK + padded {
            // Truncated member: stop rather than hand out bytes that are not
            // all there.
            self.rest = &[];
            return None;
        }
        let data = &self.rest[BLOCK..BLOCK + data_len];
        self.rest = &self.rest[BLOCK + padded..];

        let path = name.strip_prefix("./").unwrap_or(name);
        let path = if kind == EntryKind::Dir {
            path.strip_suffix('/').unwrap_or(path)
        } else {
            path
        };
        // A link carries no contents: its target is the `linkname` field, and
        // `size` is zero. Reuse `data` so the unpacker needs no second case.
        let data = if kind == EntryKind::Symlink {
            str_field(&header[157..257])?.as_bytes()
        } else {
            data
        };
        Some(Entry { path, kind, data })
    }
}

/// One NUL-terminated ASCII field, trimmed of padding.
fn str_field(bytes: &[u8]) -> Option<&str> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).ok()
}

/// A padded octal field, e.g. file sizes and modes.
fn octal(bytes: &[u8]) -> Option<u64> {
    let text = str_field(bytes)?;
    u64::from_str_radix(text.trim(), 8).ok()
}

/// Unpack an archive into `vfs`'s root and return how many members landed.
///
/// Parent directories are created as needed, so an archive need not mention
/// them. A directory named twice is fine; a file named twice is a broken
/// archive and stops the unpack.
pub fn unpack_into(vfs: &Vfs, archive: &[u8]) -> VfsResult<usize> {
    let mut count = 0;
    for entry in entries(archive) {
        let path = format!("/{}", entry.path);
        match entry.kind {
            EntryKind::Dir => {
                ensure_dir(vfs, &path)?;
            }
            EntryKind::File => {
                let (parent_path, name) = split_last(&path);
                let parent = ensure_dir(vfs, parent_path)?;
                if parent.lookup(name).is_ok() {
                    return Err(VfsError::Exists);
                }
                let file = parent.create(name, NodeKind::File)?;
                file.write_at(0, entry.data)?;
            }
            EntryKind::Symlink => {
                let (parent_path, name) = split_last(&path);
                let parent = ensure_dir(vfs, parent_path)?;
                if parent.lookup(name).is_ok() {
                    return Err(VfsError::Exists);
                }
                let target = core::str::from_utf8(entry.data).map_err(|_| VfsError::Invalid)?;
                parent.symlink(name, target)?;
            }
        }
        count += 1;
    }
    Ok(count)
}

/// The directory named by `path`, created along with its parents if missing.
fn ensure_dir(vfs: &Vfs, path: &str) -> VfsResult<Arc<dyn Inode>> {
    match vfs.resolve(path) {
        Ok(node) => {
            if node.metadata().kind == NodeKind::Dir {
                Ok(node)
            } else {
                Err(VfsError::NotDir)
            }
        }
        Err(VfsError::NoEntry) => {
            let (parent_path, name) = split_last(path);
            let parent = ensure_dir(vfs, parent_path)?;
            if parent.lookup(name).is_ok() {
                // Someone else made it a file, or the archive lists a file and
                // a directory under one name. Either way, refuse.
                return Err(VfsError::Exists);
            }
            parent.create(name, NodeKind::Dir)
        }
        Err(e) => Err(e),
    }
}

/// Split `/a/b` into `/a` and `b`; `/a` into `/` and `a`.
fn split_last(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(0) => ("/", &path[1..]),
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("/", path),
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use alloc::{format, vec, vec::Vec};

    use super::{BLOCK, EntryKind, entries};
    use crate::vfs::mount::Vfs;
    use crate::vfs::ramfs::RamFs;
    use crate::vfs::{NodeKind, VfsError};

    /// Build a one-member archive the way tar would: header, data, padding.
    fn member(path: &str, typeflag: u8, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; BLOCK];
        header[..path.len()].copy_from_slice(path.as_bytes());
        let size = format!("{:011o}", data.len());
        header[124..124 + size.len()].copy_from_slice(size.as_bytes());
        header[156] = typeflag;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");

        let mut out = Vec::from(header.as_slice());
        out.extend_from_slice(data);
        let pad = data.len().div_ceil(BLOCK) * BLOCK - data.len();
        out.extend(core::iter::repeat_n(0u8, pad));
        out
    }

    /// A link member: the target lives in `linkname`, not in the data blocks.
    fn link_member(path: &str, target: &str) -> Vec<u8> {
        let mut header = [0u8; BLOCK];
        header[..path.len()].copy_from_slice(path.as_bytes());
        header[124..135].copy_from_slice(b"00000000000");
        header[156] = b'2';
        header[157..157 + target.len()].copy_from_slice(target.as_bytes());
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        Vec::from(header.as_slice())
    }

    fn tree() -> Vfs {
        let vfs = Vfs::new();
        vfs.mount_root(&*RamFs::new());
        vfs
    }

    #[test]
    fn files_and_dirs_parse_out_in_order() {
        let mut archive = member("etc/", b'5', b"");
        archive.extend(member("etc/motd", b'0', b"hello\n"));
        archive.extend(member("bin/", b'5', b""));
        archive.extend(member("bin/hello", b'0', b"echo hi\n"));

        let got: Vec<_> = entries(&archive)
            .map(|e| (e.path, e.kind, e.data))
            .collect();
        assert_eq!(
            got,
            vec![
                ("etc", EntryKind::Dir, b"".as_slice()),
                ("etc/motd", EntryKind::File, b"hello\n".as_slice()),
                ("bin", EntryKind::Dir, b"".as_slice()),
                ("bin/hello", EntryKind::File, b"echo hi\n".as_slice()),
            ]
        );
    }

    #[test]
    fn the_trailer_block_ends_the_archive() {
        let mut archive = member("f", b'0', b"x");
        archive.extend(vec![0u8; 2 * BLOCK]);
        assert_eq!(entries(&archive).count(), 1);
    }

    #[test]
    fn data_larger_than_one_block_survives_padding() {
        let payload = [0xa7u8; BLOCK * 2 + 3];
        let archive = member("big", b'0', &payload);
        let mut it = entries(&archive);
        assert_eq!(it.next().unwrap().data, payload.as_slice());
        assert!(it.next().is_none());
    }

    #[test]
    fn a_leading_dot_slash_is_dropped_and_junk_stops_the_parse() {
        let mut archive = member("./etc/motd", b'0', b"");
        // Not a header: the iterator must stop, not panic or loop.
        archive.extend_from_slice(&[0xffu8; BLOCK]);
        let got: Vec<_> = entries(&archive).map(|e| e.path).collect();
        assert_eq!(got, vec!["etc/motd"]);
    }

    #[test]
    fn unpack_creates_the_tree_it_is_given() {
        let mut archive = member("etc/", b'5', b"");
        archive.extend(member("etc/motd", b'0', b"hi\n"));

        let vfs = tree();
        assert_eq!(super::unpack_into(&vfs, &archive).unwrap(), 2);

        let motd = vfs.resolve("/etc/motd").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(motd.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"hi\n");
    }

    /// A member whose parent directories the archive never mentions still
    /// lands: the unpacker creates parents on demand.
    #[test]
    fn unpack_implies_parent_directories() {
        let archive = member("a/b/c", b'0', b"deep");
        let vfs = tree();
        assert_eq!(super::unpack_into(&vfs, &archive).unwrap(), 1);
        let c = vfs.resolve("/a/b/c").unwrap();
        assert_eq!(c.metadata().kind, NodeKind::File);
    }

    /// Directories repeat in real archives; naming one twice must work.
    #[test]
    fn unpack_is_idempotent_for_directories() {
        let mut archive = member("d/", b'5', b"");
        archive.extend(member("d/", b'5', b""));
        let vfs = tree();
        assert_eq!(super::unpack_into(&vfs, &archive).unwrap(), 2);
    }

    /// Two files under one name is a broken archive; the unpack refuses
    /// rather than silently picking one.
    #[test]
    fn unpack_refuses_a_doubled_file() {
        let mut archive = member("f", b'0', b"one");
        archive.extend(member("f", b'0', b"two"));
        let vfs = tree();
        assert_eq!(
            super::unpack_into(&vfs, &archive).err(),
            Some(VfsError::Exists)
        );
    }

    /// A link member's target is the `linkname` field, never the data blocks -
    /// which for a link are empty. Unpacking one has to produce something
    /// resolution will follow, or a busybox `/bin` arrives as an empty file.
    #[test]
    fn unpack_creates_links_that_resolution_follows() {
        let mut archive = member("bin/busybox", b'0', b"busybox");
        archive.extend(link_member("bin/ls", "/bin/busybox"));
        archive.extend(link_member("bin/sh", "busybox"));
        let vfs = tree();
        assert_eq!(super::unpack_into(&vfs, &archive).unwrap(), 3);

        let busybox = vfs.resolve("/bin/busybox").unwrap();
        assert_eq!(busybox.metadata().kind, NodeKind::File);
        for name in ["ls", "sh"] {
            assert!(
                alloc::sync::Arc::ptr_eq(&vfs.resolve(&format!("/bin/{name}")).unwrap(), &busybox),
                "/bin/{name} is not a working link to busybox"
            );
        }
    }
}
