//! `/proc/<pid>/fdinfo/<n>`: what one descriptor of one process is doing.
//!
//! `/proc/<pid>/fd` answers *which* descriptors a process has and what each one
//! names. This file answers the questions a `readlink` cannot: where the
//! descriptor is positioned, what flags it carries, which mount it is on, and -
//! for the kinds that have more to say - what their own state is. It is a
//! debugging and introspection door (`lsof` reads it for the position and the
//! flags, `udev`'s workers are watched through it, a program that has inherited
//! a descriptor with `SCM_RIGHTS` asks it whether anything is still queued), and
//! it is the only door for most of those facts: `F_GETFL` needs the descriptor
//! in the asking process, and a process **cannot** ask it about a descriptor it
//! does not hold, which is exactly the case this file exists for.
//!
//! ## The measured shape
//!
//! Linux renders it from `seq_file`: four generic lines every kind has, then
//! whatever the node's own `show_fdinfo` adds. Measured on Linux 7.2.14
//! (`build/scratch/fdinfo/{raw,pertype,radix,cloexec,inorel,mnt}.c`):
//!
//! ```text
//! pos:	<decimal>
//! flags:	<octal>
//! mnt_id:	<decimal>
//! ino:	<decimal>
//! <per-kind lines, none here>
//! ```
//!
//! The first three separators are tabs and the fourth line is the last of the
//! generic block; a per-kind block starts on the next line. Each of the four
//! generic lines is its own radix, and getting one wrong is the kind of defect a
//! tool does not notice until it prints a number in the wrong base:
//!
//! * `pos` is **decimal** (`4294967313` for an offset of `2^32 + 17`, not
//!   `200000011`).
//! * `flags` is **octal** and is `F_GETFL | (FD_CLOEXEC ? O_CLOEXEC : 0)`, not
//!   `F_GETFL` alone: `open(O_RDONLY|O_CLOEXEC)` reports `02100000` where
//!   `F_GETFL` answers `0100000`, and `F_SETFD(0)` takes the bit back out. So
//!   the line follows `f_flags`, which is what an open *and* `F_SETFD` write on
//!   Linux, and both halves are needed.
//! * `mnt_id` is **decimal**: the **reused 32-bit** id `/proc/self/mountinfo`
//!   prints in its first column - *not* `statx`'s `stx_mnt_id`/
//!   `STATX_MNT_ID_UNIQUE`, which measured `2147491621` where this column
//!   printed `634` for the same file. The two are different number spaces on
//!   purpose (`docs/notes/` §3.13's mount-id match), and this file is the one
//!   that takes the mountinfo one: a tool correlating `fdinfo` with
//!   `mountinfo` compares this column against that file's first field.
//! * `ino` is **decimal** and is the inode's own number - the same one `fstat`
//!   reports as `st_ino` for every kind measured, anonymous inodes included.
//!
//! ## What an anonymous descriptor reports
//!
//! Three of the four lines are the same for every descriptor, and the fourth is
//! the interesting one. An anonymous inode - an `epoll`, an `eventfd`, a pipe
//! end, a socket, a `memfd` - has no mount, so Linux reports a **pseudo** id
//! there: measured, `19` for the `anon_inodefs` descriptors the five
//! `anon_inode:[...]` kinds share, `18` for `pipefs`, `12` for `sockfs`, and
//! **`0`** for a `memfd`, whose `shmem` inode is not a mount at all. None of
//! those ids appears in `/proc/self/mountinfo`, which is why they are spelled
//! here as the constants Linux measures rather than looked up in the table: a
//! reader that compared them against the mount table would find nothing, and
//! Linux's own answer is that they are not mounts.
//!
//! ## Why this is a module and not a `match` in procfs
//!
//! The four generic lines are pure - a position, a flag word, two numbers - and
//! the per-kind lines are not, but the *rendering* is where the radix and the
//! separator live, and those are what a host test can check without a machine.
//! So the pure half is here, taking its facts as arguments, and `procfs` calls
//! it with the live description's answers. The per-kind half is
//! [`crate::vfs::Inode::fdinfo_lines`], asked of the node the descriptor opens.

use alloc::string::String;

use super::file::File;

/// The mount id Linux reports for the `anon_inodefs` descriptors - `epoll`,
/// `eventfd`, `signalfd`, `timerfd` and `inotify`.
///
/// Measured on Linux 7.2.14 (`build/scratch/fdinfo/rel.c`): all five of the
/// `anon_inode:[...]` kinds answer the same id because they share the one
/// anonymous filesystem, and a `memfd` answers a **different** number (see
/// [`MEMFD_MNT_ID`]) because it is `shmem` and not `anon_inodefs`.
pub const ANON_INODE_MNT_ID: u64 = 19;

/// The mount id Linux reports for a pipe end - `pipefs`'s own pseudo number.
pub const PIPE_MNT_ID: u64 = 18;

/// The mount id Linux reports for a socket - `sockfs`'s own pseudo number.
///
/// Measured for an `AF_UNIX` socketpair and an `AF_INET` socket alike: the id
/// is the *filesystem*'s, so the family does not change it.
pub const SOCKET_MNT_ID: u64 = 12;

/// The mount id Linux reports for a `memfd`.
///
/// It is zero, and that is not an accident of measurement: a `memfd`'s inode is
/// a `shmem` inode with no mount behind it at all, so Linux has no id to
/// report. A caller that treats `mnt_id: 0` as "not on a mount" is reading the
/// file correctly.
pub const MEMFD_MNT_ID: u64 = 0;

/// Render the four lines every descriptor has, at the state the caller hands in.
///
/// `flags` is the *open* flag word as `fcntl(F_GETFL)` answers it; the
/// `O_CLOEXEC` half is added here from `cloexec` because the two halves have
/// different owners - see [`super::file::O_CLOEXEC`]'s doc for the measurement.
///
/// The text is byte-for-byte Linux's, which for one descriptor is four lines and
/// no blank between them:
///
/// ```text
/// pos:\t<pos>\n
/// flags:\t0<flags octal>\n
/// mnt_id:\t<mnt>\n
/// ino:\t<ino>\n
/// ```
///
/// The `0` on the flags line is a **literal** prefix, not padding: Linux's
/// format string is `"flags:\t0%o\n"` (`fs/proc/fd.c`), so the line is a `0`
/// followed by the octal digits. That is why `O_RDWR` prints as `02` and a zero
/// word as `00` rather than as `2` and `0` - a renderer that used `%o` alone
/// would print a different line for every value whose octal form has no leading
/// zero, which is most of them. Both forms are measured
/// (`build/scratch/fdinfo/raw.c`: a pipe end's `flags:\t00\n`, a socket's
/// `flags:\t02\n`, a regular file's `flags:\t0100000\n`).
///
/// A function of its arguments alone so a host test can spell a table of
/// descriptors and check the rendering, and so `procfs` owns only the question
/// of *where* the four numbers come from.
pub fn generic_lines(pos: u64, flags: u32, cloexec: bool, mnt_id: u64, ino: u64) -> String {
    let flags = flags | if cloexec { super::file::O_CLOEXEC } else { 0 };
    alloc::format!("pos:\t{pos}\nflags:\t0{flags:o}\nmnt_id:\t{mnt_id}\nino:\t{ino}\n")
}

/// The mount id to report for a descriptor whose node is not on a mount.
///
/// The three pseudo ids and the `memfd` zero, chosen by the *kind* of the node
/// and never looked up in the mount table: a pipe's `18` is `pipefs`'s number
/// and no mount in the table carries it. `None` means "this node's own mount",
/// which the caller resolves from the mount table because only a real path has
/// one.
///
/// The order of the questions is the kind's, because a node is one kind: a
/// `memfd` is a `memfd` before it is anything else, and the four
/// `anon_inodefs` kinds share a number so the distinction between them is not
/// needed here.
pub fn pseudo_mount_id(file: &File) -> Option<u64> {
    let inode = file.inode();
    if inode.as_eventfd().is_some()
        || inode.as_epoll().is_some()
        || inode.as_timerfd().is_some()
        || inode.as_inotify().is_some()
        || inode.as_pidfd().is_some()
        || inode.as_signalfd().is_some()
        || inode.as_fanotify().is_some()
        || is_io_uring(&inode)
    {
        return Some(ANON_INODE_MNT_ID);
    }
    if is_memfd(&inode) {
        return Some(MEMFD_MNT_ID);
    }
    // A `pipe()` end is `pipefs`; a `socketpair(2)` end is **`sockfs`**, and the
    // two are different node types here even though both are two-way channels:
    // `pipe()` mints a `PipeEnd` and `socketpair(AF_UNIX, SOCK_STREAM)` mints a
    // `DuplexEnd`. Measured on Linux 7.2.14 (`build/scratch/fdinfo/rel.c`): a
    // pipe end answered `mnt_id: 18` and a socketpair end `12`, so grouping the
    // two under one number is wrong for whichever loses. This was a live bug the
    // guest pin caught - the classifier put `as_duplex` in with `as_pipe` and
    // the socketpair claim went red on the machine while every host test was
    // green, because the host has no `socketpair` to classify.
    if inode.as_pipe().is_some() {
        return Some(PIPE_MNT_ID);
    }
    if inode.as_duplex().is_some() || inode.socket_handle() != 0 {
        return Some(SOCKET_MNT_ID);
    }
    None
}

/// Whether this node is a `memfd`, on a build that has the type.
///
/// `Inode::as_memfd` is gated with the `memfd` module because the module needs a
/// page allocator the host build has no business carrying, so the question is
/// asked through a helper the host answers `false` to - the host has no `memfd`
/// to find, which is the honest answer rather than a missing one.
#[cfg(target_os = "none")]
fn is_memfd(inode: &alloc::sync::Arc<dyn crate::vfs::Inode>) -> bool {
    inode.as_memfd().is_some()
}

#[cfg(not(target_os = "none"))]
fn is_memfd(_inode: &alloc::sync::Arc<dyn crate::vfs::Inode>) -> bool {
    false
}

/// The same split for the `io_uring` ring, whose module is gated for the same
/// reason (it hands out physical pages).
#[cfg(target_os = "none")]
fn is_io_uring(inode: &alloc::sync::Arc<dyn crate::vfs::Inode>) -> bool {
    inode.as_io_uring().is_some()
}

#[cfg(not(target_os = "none"))]
fn is_io_uring(_inode: &alloc::sync::Arc<dyn crate::vfs::Inode>) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four lines, byte for byte, in Linux's radixes.
    ///
    /// The measured table (`build/scratch/fdinfo/raw.c`) for a regular file
    /// seeked to 7: `pos:\t7\nflags:\t0100000\nmnt_id:\t634\nino:\t27001799\n`.
    /// The octal `flags` and the decimal `pos` are the two a wrong `format!`
    /// would swap, so the claim is made on the whole string rather than on a
    /// substring a shared prefix would satisfy.
    #[test]
    fn the_generic_block_is_four_lines_in_linux_radixes() {
        let text = generic_lines(7, 0o100_000, false, 634, 27_001_799);
        assert_eq!(
            text,
            "pos:\t7\nflags:\t0100000\nmnt_id:\t634\nino:\t27001799\n"
        );
    }

    /// `flags:` is `F_GETFL` **or** `O_CLOEXEC`, and the bit comes from
    /// `FD_CLOEXEC` rather than from the open's word.
    ///
    /// Measured (`build/scratch/fdinfo/cloexec.c`): `open(O_RDONLY|O_CLOEXEC)`
    /// answers `02100000` where `F_GETFL` answers `0100000`, and a later
    /// `F_SETFD(0)` takes the bit back out. A renderer that printed `F_GETFL`
    /// alone loses the half that tells a tool the descriptor will not survive
    /// an `exec`.
    #[test]
    fn the_flags_line_carries_o_cloexec_from_the_descriptor_flag() {
        let plain = generic_lines(0, 0o100_000, false, 1, 2);
        let cloexec = generic_lines(0, 0o100_000, true, 1, 2);
        assert!(plain.contains("flags:\t0100000\n"), "{plain}");
        assert!(cloexec.contains("flags:\t02100000\n"), "{cloexec}");
    }

    /// A large position is printed in decimal, not hex.
    ///
    /// `2^32 + 17` reads `4294967313`; as hex it would be `100000011`, which a
    /// tool that parses it as a number would take for a different offset
    /// entirely - and a >4 GiB file is exactly where this shows up.
    #[test]
    fn a_position_past_four_gibibytes_is_decimal() {
        let text = generic_lines((1 << 32) + 17, 0, false, 1, 2);
        assert!(text.starts_with("pos:\t4294967313\n"), "{text}");
    }

    /// A `pipe()` end and a `socketpair()` end report **different** pseudo
    /// ids, and they are different node types here.
    ///
    /// Measured on Linux 7.2.14 (`build/scratch/fdinfo/rel.c`): a pipe end
    /// answers `mnt_id: 18` (`pipefs`) and a socketpair end `12` (`sockfs`),
    /// because they are different filesystems and not two spellings of one
    /// channel. This kernel mints a `PipeEnd` for `pipe()` and a `DuplexEnd`
    /// for `socketpair(AF_UNIX, SOCK_STREAM)`, so a classifier that puts the
    /// two `as_*` answers in one arm is wrong for one of them - which is what
    /// the guest pin caught (the host has no `socketpair` to classify).
    #[test]
    fn a_pipe_end_and_a_socketpair_end_get_different_pseudo_ids() {
        use super::super::file::File;
        use super::super::pipe::{self, DuplexEnd};

        let (read_end, write_end) = pipe::pair();
        assert_eq!(
            pseudo_mount_id(&File::new(read_end)),
            Some(PIPE_MNT_ID),
            "a pipe() end is pipefs"
        );
        assert_eq!(pseudo_mount_id(&File::new(write_end)), Some(PIPE_MNT_ID));

        let (a, _b) = DuplexEnd::pair(Default::default());
        assert_eq!(
            pseudo_mount_id(&File::new(a)),
            Some(SOCKET_MNT_ID),
            "a socketpair(2) end is sockfs, not pipefs"
        );
    }

    /// The `memfd` id is zero and the three pseudo ids are Linux's measured
    /// ones; none of them is a mount in the table.
    #[test]
    fn the_pseudo_mount_ids_are_the_measured_ones() {
        assert_eq!(ANON_INODE_MNT_ID, 19);
        assert_eq!(PIPE_MNT_ID, 18);
        assert_eq!(SOCKET_MNT_ID, 12);
        assert_eq!(MEMFD_MNT_ID, 0);
    }
}
