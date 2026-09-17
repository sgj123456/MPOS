#!/usr/bin/env python3
"""Extract an initramfs cpio archive into a directory.

Alpine's mkinitfs writes a gzipped cpio archive, and the kernel's own
initramfs unpacker reads ustar - so the archive is unpacked here, on the host,
and repacked as ustar by the caller. Everything a root filesystem holds is
kept: directories, files, symlinks, permissions. Device nodes are dropped:
the kernel builds its own `/dev` (that is what `mount -t devfs` is for), so a
static node in the archive would only be a second, staler answer.

Usage: cpio-extract.py <archive.cpio[.gz]> <destdir>
"""

import gzip
import os
import stat
import sys

MAGIC = b"070701"
MAGIC_CRC = b"070702"
HEADER = 110
#: A file's contents are padded so the next header starts on a four-byte
#: boundary; so are names.
ALIGN = 4


def read_archive(path):
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rb") as f:
        return f.read()


def field(header, index):
    start = 6 + index * 8
    return int(header[start:start + 8], 16)


def extract(blob, dest):
    offset = 0
    count = 0
    while offset + HEADER <= len(blob):
        header = blob[offset:offset + HEADER]
        if header[:6] not in (MAGIC, MAGIC_CRC):
            raise SystemExit(f"cpio: bad magic at {offset}: {header[:6]!r}")
        mode = field(header, 1)
        filesize = field(header, 6)
        namesize = field(header, 11)
        name_start = offset + HEADER
        name = blob[name_start:name_start + namesize - 1].decode("utf-8", "surrogateescape")
        # The archive's end is an entry named TRAILER!!! - zero padding follows
        # it, and reading that as a header is what "bad magic" was.
        if name == "TRAILER!!!":
            break
        data_start = name_start + namesize
        data_start += (-data_start) % ALIGN
        data = blob[data_start:data_start + filesize]
        offset = data_start + filesize
        offset += (-offset) % ALIGN

        # A leading slash would escape the destination; the archive is read as
        # paths relative to it, which is what cpio means. mkinitfs's archive
        # starts with the root directory itself, spelled ".", which lands on
        # the destination and is fine.
        dest_abs = os.path.abspath(dest)
        target = os.path.normpath(os.path.join(dest_abs, name.lstrip("/")))
        if target != dest_abs and not target.startswith(dest_abs + os.sep):
            raise SystemExit(f"cpio: {name} escapes the destination")

        kind = mode & 0o170000
        if kind == stat.S_IFDIR:
            os.makedirs(target, exist_ok=True)
            os.chmod(target, mode & 0o7777)
        elif kind == stat.S_IFLNK:
            link = data.decode("utf-8", "surrogateescape")
            if os.path.lexists(target):
                os.remove(target)
            os.symlink(link, target)
        elif kind == stat.S_IFREG:
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with open(target, "wb") as out:
                out.write(data)
            os.chmod(target, mode & 0o7777)
        else:
            # Character and block devices: the kernel's devfs is the only
            # source of device nodes here.
            continue
        count += 1
    return count


def main():
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    archive, dest = sys.argv[1], sys.argv[2]
    os.makedirs(dest, exist_ok=True)
    blob = read_archive(archive)
    count = extract(blob, dest)
    print(f"cpio: {count} members extracted into {dest}")


if __name__ == "__main__":
    main()
