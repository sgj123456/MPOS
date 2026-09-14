#!/usr/bin/env python3
"""Build a FAT32 volume image, byte by byte, without mounting anything.

The obvious tools for this are `mkfs.vfat` plus `mtools`, and neither works
here: `mkfs.vfat` lays down an empty filesystem but cannot add files, and
filling it needs `mcopy` (not installed) or a loop mount (no privilege to
create the loop device). So the volume is built by hand - boot sector, FSInfo,
both FATs, and the directory tree - which is more bookkeeping than it is
difficulty and has no dependencies at all.

The result is what an EFI firmware expects to find on an EFI System
Partition, so it is written into one before being spliced into the disk image
by `mkdisk.sh`.

Usage: mkfat.py OUT SIZE_BYTES LABEL SRCDIR
"""

import os
import struct
import sys

SECTOR = 512
ATTR_DIRECTORY = 0x10
ATTR_LONG_NAME = 0x0F
FREE = 0x00000000
EOC = 0x0FFFFFFF

# Characters FAT forbids in a short name, plus the ones that would make a name
# ambiguous with the LFN entries around it.
ILLEGAL = set('"*+/:;<=>?[\\]|,') | {chr(c) for c in range(0x20)}


def split_name(name):
    """The `(base, ext)` halves of `name`, as FAT understands the split.

    The dot is the *last* one, and a name with no dot is all base - which is
    the opposite of what `rpartition` reports, since it hands the whole string
    back as the extension.
    """
    head, dot, tail = name.upper().rpartition('.')
    return (head, tail) if dot else (name.upper(), '')


def short_name(name, taken):
    """An 8.3 name for `name`, unique within `taken`: 8 base bytes then 3
    extension bytes, both space-padded, which is the 11 bytes that go on disk.

    Whatever cannot be represented is dropped rather than guessed at: the
    long-name entry next to it carries the real name, and this is the handle
    every tool that refuses to read LFNs falls back to.
    """
    keep = lambda s: ''.join(c for c in s if c not in ILLEGAL and ord(c) < 128)
    base, ext = split_name(name)
    base = keep(base)[:8] or 'FILE'
    ext = keep(ext)[:3]
    form = lambda b: b.ljust(8) + ext.ljust(3)

    candidate = form(base)
    if candidate not in taken:
        taken.add(candidate)
        return candidate

    # Collide: replace the tail of the base with ~N until it is free. This is
    # the same scheme Windows uses, and the only one anything else expects.
    for n in range(1, 1000):
        tail = '~' + str(n)
        stem = base[: max(1, 8 - len(tail))] + tail
        candidate = form(stem)
        if candidate not in taken:
            taken.add(candidate)
            return candidate
    raise SystemExit(f'mkfat: ran out of short names for {name!r}')


def lfn_checksum(short11):
    """The checksum a long-name entry must carry to belong to its short name."""
    total = 0
    for byte in short11:
        total = (((total & 1) << 7) + (total >> 1) + byte) & 0xFF
    return total


def is_plain_83(name):
    """Whether `name` is already its own 8.3 name, with no case to preserve."""
    if name != name.upper() or name.startswith('.'):
        return False
    head, dot, tail = name.rpartition('.')
    # `rpartition` on a name with no dot puts the whole thing in `tail`, and a
    # name that is nothing but an extension is not an 8.3 name either.
    base, ext = (head, tail) if dot else (name, '')
    return 0 < len(base) <= 8 and len(ext) <= 3


class Fat32:
    #: FAT32 is only defined for volumes of at least 65525 clusters. Anything
    #: smaller would be reported by a reader as FAT12/16 no matter what the
    #: boot sector claims, so the cluster size is picked to clear that floor
    #: with the smallest clusters that work - which is how Windows lays out a
    #: 100 MB EFI partition, and therefore what firmware is tested against.
    MIN_CLUSTERS = 65525

    def __init__(self, size, label):
        self.size = size
        self.label = label
        self.bps = SECTOR
        self.reserved = 32
        self.num_fats = 2
        self.root_cluster = 2
        self.total_sectors = size // SECTOR
        self.spc = self._pick_spc()
        self.fat_sectors = self._fat_sectors()
        self.data_start = self.reserved + self.num_fats * self.fat_sectors
        self.total_clusters = (self.total_sectors - self.data_start) // self.spc + 2
        if self.total_clusters < self.MIN_CLUSTERS:
            raise SystemExit(
                f'mkfat: {size} bytes cannot hold {self.MIN_CLUSTERS} clusters, '
                f'so it cannot be FAT32 - use a volume of at least '
                f'{self.MIN_CLUSTERS * SECTOR} bytes'
            )
        self.image = bytearray(size)
        self.fat = [FREE] * self.total_clusters
        self.next_free = 2
        self._init_volume()

    # ---- layout arithmetic ------------------------------------------------

    def _pick_spc(self):
        """The fewest sectors per cluster that still leaves room for FAT32.

        The FAT itself takes space out of the volume, so the test is run
        against a FAT sized for the resulting cluster count rather than the
        raw size - the difference is small but it is the difference between
        the last volume that works and the first one that does not.
        """
        for spc in (1, 2, 4, 8, 16, 32, 64):
            fat = 1
            while True:
                clusters = (
                    self.total_sectors - self.reserved - self.num_fats * fat
                ) // spc + 2
                needed = max(1, (clusters * 4 + SECTOR - 1) // SECTOR)
                if needed <= fat:
                    break
                fat = needed
            if clusters >= self.MIN_CLUSTERS:
                return spc
        return 64

    def _fat_sectors(self):
        """Size the FAT, which depends on its own size. Two passes converge.

        The formula is the one in Microsoft's FAT specification, and it is
        iterative by nature: a bigger FAT leaves fewer clusters to describe.
        """
        fat = 1
        while True:
            clusters = (self.total_sectors - self.reserved - self.num_fats * fat) // self.spc + 2
            needed = max(1, (clusters * 4 + SECTOR - 1) // SECTOR)
            if needed <= fat:
                return fat
            fat = needed

    def _cluster_bytes(self):
        return self.spc * self.bps

    def _cluster_offset(self, cluster):
        sector = self.data_start + (cluster - 2) * self.spc
        return sector * SECTOR

    # ---- volume construction ---------------------------------------------

    def _init_volume(self):
        # Sector 0 is the boot sector and sector 1 the FSInfo; sectors 6 and 7
        # are their backups, which the spec asks for and which cost two sectors
        # to get right.
        self._write_boot_sector(0)
        self._write_fsinfo(1)
        self._write_boot_sector(6)
        self._write_fsinfo(7)
        # Cluster 0 and 1 are the two reserved entries at the head of every
        # FAT: the media descriptor lives in the low byte of the first.
        self.fat[0] = 0x0FFFFFF8
        self.fat[1] = 0x0FFFFFFF
        # The root directory is a normal cluster in FAT32, just one nobody
        # names in a parent.
        self.fat[self.root_cluster] = EOC
        self.next_free = self.root_cluster + 1

    def _write_boot_sector(self, sector):
        boot = bytearray(SECTOR)
        boot[0:3] = b'\xeb\x58\x90'  # jmp +0x58 to the boot code, NOP
        boot[3:11] = b'mpos    '[:8]
        struct.pack_into('<H', boot, 11, self.bps)
        boot[13] = self.spc
        struct.pack_into('<H', boot, 14, self.reserved)
        boot[16] = self.num_fats
        struct.pack_into('<H', boot, 17, 0)  # no fixed root directory in FAT32
        struct.pack_into('<H', boot, 19, 0)  # nor a 16-bit sector count
        boot[21] = 0xF8  # fixed disk
        struct.pack_into('<H', boot, 22, 0)  # FAT32 keeps its size below
        struct.pack_into('<H', boot, 24, 63)  # sectors per track
        struct.pack_into('<H', boot, 26, 255)  # heads
        struct.pack_into('<I', boot, 28, 0)  # hidden sectors
        struct.pack_into('<I', boot, 32, self.total_sectors)
        struct.pack_into('<I', boot, 36, self.fat_sectors)
        struct.pack_into('<H', boot, 40, 0)  # mirror both FATs, no flags
        struct.pack_into('<H', boot, 42, 0)  # revision 0.0
        struct.pack_into('<I', boot, 44, self.root_cluster)
        struct.pack_into('<H', boot, 48, 1)  # FSInfo
        struct.pack_into('<H', boot, 50, 6)  # backup boot sector
        boot[64] = 0x80  # drive number
        boot[66] = 0x29  # extended boot signature: the four fields below exist
        struct.pack_into('<I', boot, 67, 0x4D505053)  # 'MPOS' as a volume id
        boot[71:82] = self.label.upper()[:11].ljust(11).encode('ascii')
        boot[82:90] = b'FAT32   '
        boot[SECTOR - 2 : SECTOR] = b'\x55\xaa'
        at = sector * SECTOR
        self.image[at : at + SECTOR] = boot

    def _write_fsinfo(self, sector, free=None, next_free=None):
        at = sector * SECTOR
        info = bytearray(SECTOR)
        struct.pack_into('<I', info, 0, 0x41615252)
        struct.pack_into('<I', info, 484, 0x61417272)
        # Both counts are unknown while the tree is being written, and the spec
        # lets a value of all-ones mean "ask the FAT instead". They are filled
        # in for real once the tree is complete, because a reader that trusts
        # them - some firmware does - would otherwise have to walk the table.
        struct.pack_into('<I', info, 488, 0xFFFFFFFF if free is None else free)
        struct.pack_into('<I', info, 492, 0xFFFFFFFF if next_free is None else next_free)
        struct.pack_into('<I', info, 508, 0xAA550000)
        self.image[at : at + SECTOR] = info

    # ---- clusters and chains ---------------------------------------------

    def _alloc(self):
        cluster = self.next_free
        while cluster < self.total_clusters and self.fat[cluster] != FREE:
            cluster += 1
        if cluster >= self.total_clusters:
            raise SystemExit('mkfat: the volume is full')
        self.next_free = cluster + 1
        self.fat[cluster] = EOC
        return cluster

    def _write_cluster(self, cluster, data):
        at = self._cluster_offset(cluster)
        self.image[at : at + len(data)] = data

    def _write_chain(self, data):
        """Lay `data` out across as many clusters as it needs, and link them.

        Returns the first cluster, or 0 for an empty file - a zero-length file
        owns nothing, and a reader that saw a cluster number there would
        follow a chain that is not its own.
        """
        if not data:
            return 0
        size = self._cluster_bytes()
        first = prev = None
        pos = 0
        while pos < len(data):
            cluster = self._alloc()
            if first is None:
                first = cluster
            else:
                self.fat[prev] = cluster
            prev = cluster
            self._write_cluster(cluster, data[pos : pos + size])
            pos += size
        return first

    # ---- directory entries ------------------------------------------------

    def _long_entries(self, name, short11):
        """The LFN entries for `name`, in the order they sit on disk.

        They run backwards - the entry carrying the last characters comes
        first, and the one with bit 6 set in its sequence number comes before
        the short entry - which is how a reader knows where the name starts
        without scanning to the end of the run.
        """
        encoded = name.encode('utf-16-le')
        units = [encoded[i : i + 2] for i in range(0, len(encoded), 2)]
        chunks = [units[i : i + 13] for i in range(0, len(units), 13)]
        check = lfn_checksum(short11)
        # Each entry carries 13 of the name's characters, but the two runs of
        # characters are not the same length as the runs of bytes they live
        # in: five, then six, then two. Taking them five at a time reads the
        # name correctly until the last pair, which comes out shifted.
        runs = ((1, 10, 0), (14, 12, 5), (28, 4, 11))

        out = []
        for index in range(len(chunks) - 1, -1, -1):
            chunk = chunks[index]
            # Slots the name does not reach are NUL-terminated once, then
            # padded with 0xFFFF; a reader stops at the first 0x0000. A full
            # chunk gets neither, and the slice below ignores the terminator
            # that the expression adds anyway.
            padded = chunk + [b'\x00\x00'] + [b'\xff\xff'] * (13 - len(chunk) - 1)

            entry = bytearray(32)
            seq = index + 1
            if index == len(chunks) - 1:
                seq |= 0x40  # last, and therefore also the logical one
            entry[0] = seq
            entry[11] = ATTR_LONG_NAME
            entry[12] = 0
            entry[13] = check
            entry[26:28] = b'\x00\x00'  # the first cluster of an LFN is always 0
            # The 13 characters are scattered across three runs so that the
            # entry's own metadata can sit in the middle of it.
            for offset, width, first in runs:
                entry[offset : offset + width] = b''.join(
                    padded[first + i] for i in range(width // 2)
                )
            out.append(bytes(entry))
        return out

    def _short_entry(self, short11, is_dir, first, size, attr=None):
        entry = bytearray(32)
        entry[0:8] = short11[:8].ljust(8).encode()
        entry[8:11] = short11[8:].ljust(3).encode()
        if attr is None:
            attr = ATTR_DIRECTORY if is_dir else 0x20
        entry[11] = attr
        entry[20:22] = struct.pack('<H', (first >> 16) & 0xFFFF)
        entry[26:28] = struct.pack('<H', first & 0xFFFF)
        struct.pack_into('<I', entry, 28, size)
        return bytes(entry)

    def _dirent(self, name, is_dir, first, size, taken):
        short11 = short_name(name, taken)
        out = b''
        if not is_plain_83(name):
            out += b''.join(self._long_entries(name, short11.encode()))
        out += self._short_entry(short11, is_dir, first, size)
        return out

    # ---- the tree ---------------------------------------------------------

    def add_tree(self, src):
        # A child of the root names cluster 0 as its parent, not the cluster
        # the root directory actually lives in. FAT32 gave the root a real
        # cluster, and every tool still writes and expects the placeholder:
        # the root is the one directory that cannot be reached by walking up.
        self._fill_dir(src, self.root_cluster, 0, is_root=True)

    def _fill_dir(self, src, cluster, parent, is_root):
        entries = b''
        if is_root:
            # A volume label is a directory entry like any other, with its own
            # attribute, and the one in the boot sector is only a copy of it.
            # fsck treats a boot-sector label with no entry behind it as a
            # mistake, and so does a user looking at the mounted volume.
            entries += self._short_entry(
                self.label.upper()[:11].ljust(11), False, 0, 0, attr=0x08
            )
        else:
            # Every directory but the root names itself and its parent in its
            # first two slots. Nothing walks up through them - the path
            # resolver here reads a table rather than a disk - but every tool
            # that does expects them, and their absence is what makes a
            # filesystem look corrupt.
            entries += self._short_entry('.' + ' ' * 10, True, cluster, 0)
            entries += self._short_entry('..' + ' ' * 9, True, parent, 0)

        taken = set()
        items = []
        for name in sorted(os.listdir(src)):
            path = os.path.join(src, name)
            if os.path.islink(path):
                raise SystemExit(f'mkfat: {path} is a symlink; FAT has none')
            if os.path.isdir(path):
                sub = self._alloc()
                # Depth first, so a child's own clusters are allocated before
                # the parent's entry that points at them is written.
                #
                # A child of the root records cluster 0 as its parent, not the
                # cluster the root directory actually lives in: the root is
                # the one directory nothing may walk up into, and 0 is the
                # placeholder every implementation writes and checks for.
                up = 0 if cluster == self.root_cluster else cluster
                self._fill_dir(path, sub, up, is_root=False)
                items.append((name, True, sub, 0))
            else:
                with open(path, 'rb') as handle:
                    data = handle.read()
                items.append((name, False, self._write_chain(data), len(data)))

        for name, is_dir, first, size in items:
            entries += self._dirent(name, is_dir, first, size, taken)
        if len(entries) > self._cluster_bytes():
            raise SystemExit(f'mkfat: {src} does not fit in one cluster')
        self._write_cluster(cluster, entries)

    # ---- output -----------------------------------------------------------

    def finish(self, out):
        free = sum(1 for cluster in self.fat[2:] if cluster == FREE)
        self._write_fsinfo(1, free, self.next_free)
        self._write_fsinfo(7, free, self.next_free)

        fat_bytes = bytearray(self.fat_sectors * SECTOR)
        for index, value in enumerate(self.fat):
            struct.pack_into('<I', fat_bytes, index * 4, value)
        for copy in range(self.num_fats):
            at = (self.reserved + copy * self.fat_sectors) * SECTOR
            self.image[at : at + len(fat_bytes)] = fat_bytes
        with open(out, 'wb') as handle:
            handle.write(self.image)
        used = sum(1 for c in self.fat[2:] if c != FREE)
        print(
            f'{out}: FAT32, {len(self.image) // 1024} KiB, '
            f'{self._cluster_bytes()} B clusters, {used} clusters in use',
            file=sys.stderr,
        )


def parse_size(text):
    units = {'k': 1024, 'm': 1024**2, 'g': 1024**3}
    if text and text[-1].lower() in units:
        return int(text[:-1]) * units[text[-1].lower()]
    return int(text)


def main(argv):
    if len(argv) != 5:
        raise SystemExit(__doc__)
    out, size, label, src = argv[1], parse_size(argv[2]), argv[3], argv[4]
    if not os.path.isdir(src):
        raise SystemExit(f'mkfat: {src} is not a directory')
    volume = Fat32(size, label)
    volume.add_tree(src)
    volume.finish(out)
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv))
