//! `devpts`: the pseudo-terminal filesystem.
//!
//! Linux mounts this at `/dev/pts`, and the slave end of every pair minted
//! through `/dev/ptmx` appears here, named by its number: `posix_openpt`
//! mints, `TIOCGPTN` names the number, and `/dev/pts/<that number>` is the
//! terminal to open. The pair registry in [`super::pty`] is the storage -
//! this filesystem is the naming layer over it, and holds nothing itself.
//!
//! The virtual consoles are deliberately absent: their slaves already have
//! names - `/dev/tty1` and friends in `/dev` - and giving the same terminal a
//! second name would only make `ttyname(3)`'s walk find one of the two, at
//! random. A devpts that lists nothing it does not own is one that reads
//! exactly as it is.
//!
//! The numbers a slave is born with are the mount's: `mode=`, `uid=` and
//! `gid=` are measured on Linux 7.2.11 and applied as this machine's minting
//! defaults (`apply_mount_options`). A mount with no options births `0600`,
//! owner = the minting euid, group 0 - not the `0620 root:tty` an init script
//! names in its own `/etc/fstab`, which is why the distinction is worth
//! measuring rather than assuming.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::pty::PtySlave;
use super::{FileSystem, Inode, NodeKind, VfsError, VfsResult};

/// Apply a `mount(2)` devpts option string as the machine's minting defaults.
///
/// The numbers are measured on Linux 7.2.11 rather than assumed
/// (`build/scratch/devpts-probe/probe{2,3,4}.py`): `mode=` sets the slave
/// node's permission bits (octal, so `0620` and `620` are the same request),
/// `gid=` its group, `uid=` overrides the minting process's euid as owner,
/// and a mount with no options at all births `0600`, owner = minting euid,
/// group 0. A later mount replaces the defaults; pairs already minted keep
/// theirs, which is Linux's own single-instance behaviour for the same
/// reason (a pair's inode numbers are fixed when it is created).
///
/// Three of Linux's options are refused rather than ignored, because this
/// kernel cannot make them true: `newinstance` asks for a second devpts
/// instance where this machine has one `/dev/ptmx` and one pair registry,
/// `ptmxmode=` names a `/dev/pts/ptmx` this filesystem does not publish, and
/// `max=` bounds an instance's pair pool where this pool is the machine's
/// ([`super::pty::MAX`]). `EOPNOTSUPP` is the honest "present, cannot do
/// that"; `EINVAL` is what Linux measured for an option nobody recognises
/// (probe.py: `nonsense=1` failed the mount with rc 32).
pub(crate) fn apply_mount_options(data: &str) -> VfsResult<()> {
    let mut options = super::pty::DevPtsOptions::DEFAULT;
    for option in data.split(',') {
        // An empty field is not an option: `mount -o ,` names none, and a
        // trailing comma is how a generated option string often ends.
        if option.is_empty() {
            continue;
        }
        let (name, value) = match option.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (option, None),
        };
        match name {
            "mode" => options.mode = parse_mode(value)?,
            "uid" => options.uid = Some(parse_number(value)?),
            "gid" => options.gid = parse_number(value)?,
            "newinstance" | "ptmxmode" | "max" => return Err(VfsError::Unsupported),
            _ => return Err(VfsError::Invalid),
        }
    }
    super::pty::set_devpts_options(options);
    Ok(())
}

/// A `mode=` value, in octal as the option spells it (`0620`, `620`), with
/// the format bits left to the node's kind the way `chmod`'s own word is.
fn parse_mode(value: Option<&str>) -> VfsResult<u32> {
    let value = value.ok_or(VfsError::Invalid)?;
    u32::from_str_radix(value, 8)
        .map(|mode| mode & 0o7777)
        .map_err(|_| VfsError::Invalid)
}

/// A `uid=`/`gid=` value: Linux's `match_int` is decimal only, so `010` is
/// ten and not eight.
fn parse_number(value: Option<&str>) -> VfsResult<u32> {
    let value = value.ok_or(VfsError::Invalid)?;
    value.parse().map_err(|_| VfsError::Invalid)
}

/// The devpts root: the dynamic pairs' slaves, by number.
pub struct DevPts;

impl DevPts {
    pub fn new() -> Arc<DevPts> {
        Arc::new(DevPts)
    }
}

impl Default for DevPts {
    fn default() -> DevPts {
        DevPts
    }
}

impl FileSystem for DevPts {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::new(PtsRoot)
    }

    fn name(&self) -> &'static str {
        "devpts"
    }
}

/// The directory itself. Every lookup goes to the pair registry, so a slave
/// exists here exactly as long as its pair does - which is what makes the
/// `posix_openpt` → `TIOCGPTN` → `open("/dev/pts/N")` sequence meaningful.
struct PtsRoot;

impl Inode for PtsRoot {
    fn metadata(&self) -> super::Metadata {
        super::Metadata {
            ino: 1,
            kind: NodeKind::Dir,
            size: 0,
        }
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
        let number: u32 = name.parse().map_err(|_| super::VfsError::NoEntry)?;
        let pty = super::pty::lookup(number).ok_or(super::VfsError::NoEntry)?;
        Ok(PtySlave::new(pty) as Arc<dyn Inode>)
    }

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        Ok(super::pty::pairs()
            .iter()
            .filter(|pty| pty.slave_dev_id().0 != 4)
            .map(|pty| (alloc::format!("{}", pty.number()), NodeKind::CharDev))
            .collect())
    }
}

/// How many terminal pairs are open, for `/proc/sys/kernel/pty/nr`.
pub fn open_count() -> usize {
    super::pty::open_count()
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    /// The lock every test that reads or writes the mount options takes first.
    ///
    /// The options are the module's one global (`pty::DEVPTS_OPTIONS`), and
    /// `cargo test` runs these on several threads: two save/apply/restore
    /// sequences interleaved make "a mount publishes the numbers its options
    /// name" false for reasons that have nothing to do with the parser - the
    /// sibling test's numbers were published between the assert and the
    /// restore. The lock is module-local because no test outside this module
    /// touches the options.
    fn options_lock() -> std::sync::MutexGuard<'static, ()> {
        static OPTIONS: std::sync::Mutex<()> = std::sync::Mutex::new(());
        OPTIONS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn the_tree_lists_dynamic_pairs_and_nothing_else() {
        let pty = crate::vfs::pty::allocate(800, (136, 800), 0o666);
        let _ = pty; // minted; devpts reads the registry
        let fs = DevPts::new();
        let listed = fs.root().read_dir().unwrap();
        assert!(
            listed
                .iter()
                .any(|(name, kind)| { name == "800" && *kind == NodeKind::CharDev }),
            "the dynamic pair is listed by its number: {listed:?}"
        );
        // The virtual consoles are not here: they are /dev/ttyN, and a
        // second name for the same terminal would break ttyname(3)'s walk.
        // The question is by *device*, not by name: the first dynamically
        // minted pair legitimately holds the number 1, and only the console's
        // own device (major 4) must never appear under this tree.
        assert!(
            !listed.iter().any(|(name, _)| {
                name.parse::<u32>()
                    .ok()
                    .and_then(crate::vfs::pty::lookup)
                    .is_some_and(|pair| pair.slave_dev_id().0 == 4)
            }),
            "the virtual consoles have their own names in /dev: {listed:?}"
        );
    }

    #[test]
    fn a_lookup_by_number_hands_back_a_terminal() {
        crate::vfs::pty::allocate(801, (136, 801), 0o666);
        let slave = DevPts::new().root().lookup("801").unwrap();
        assert_eq!(slave.metadata().kind, NodeKind::CharDev);
        assert!(slave.as_tty().is_some(), "the slave is a real terminal");
        assert_eq!(slave.rdev(), Some((136, 801)));
        assert!(DevPts::new().root().lookup("99999").is_err());
        assert!(DevPts::new().root().lookup("pts").is_err());
    }

    #[test]
    fn the_filesystem_introduces_itself_as_devpts() {
        assert_eq!(DevPts::new().name(), "devpts");
    }

    /// The measured numbers, not the ones a comment used to claim: Linux
    /// 7.2.11 births an option-less slave `0600`, and the init script's
    /// `0620 root:tty` is the mount's own request.
    #[test]
    fn a_mount_publishes_the_numbers_its_options_name() {
        let _guard = options_lock();
        let saved = crate::vfs::pty::devpts_options();
        apply_mount_options("mode=0620,gid=5").unwrap();
        let options = crate::vfs::pty::devpts_options();
        assert_eq!(options.mode, 0o620);
        assert_eq!(options.gid, 5);
        assert_eq!(options.uid, None, "no uid= names the minting process");

        apply_mount_options("mode=0640,uid=1000,gid=5").unwrap();
        let options = crate::vfs::pty::devpts_options();
        assert_eq!(
            (options.mode, options.uid, options.gid),
            (0o640, Some(1000), 5)
        );

        // A mount with no options asks for Linux's defaults, not for whatever
        // the previous mount published.
        apply_mount_options("").unwrap();
        let options = crate::vfs::pty::devpts_options();
        assert_eq!((options.mode, options.uid, options.gid), (0o600, None, 0));

        crate::vfs::pty::set_devpts_options(saved);
    }

    #[test]
    fn an_option_this_kernel_cannot_make_true_is_refused() {
        // `mode=0620,` below is accepted, so this test publishes numbers too:
        // it takes the lock and puts the old ones back, or it would leave the
        // options of whichever test ran before it changed.
        let _guard = options_lock();
        let saved = crate::vfs::pty::devpts_options();
        // Measured on Linux 7.2.11: an unrecognised option fails the mount
        // with EINVAL rather than being ignored.
        assert_eq!(
            apply_mount_options("nonsense=1").err(),
            Some(VfsError::Invalid)
        );
        assert_eq!(
            apply_mount_options("mode=zz").err(),
            Some(VfsError::Invalid)
        );
        assert_eq!(apply_mount_options("uid=").err(), Some(VfsError::Invalid));
        assert_eq!(apply_mount_options("mode=0620,").err(), None);
        // Present in Linux, but each names something this kernel does not have.
        for refused in ["newinstance", "ptmxmode=0666", "max=2"] {
            assert_eq!(
                apply_mount_options(refused).err(),
                Some(VfsError::Unsupported),
                "{refused} must be refused, not silently dropped"
            );
        }
        crate::vfs::pty::set_devpts_options(saved);
    }

    /// The pair carries the numbers its mount published - and they survive a
    /// new `PtySlave`, which is what makes a `chown` from `login` stick.
    #[test]
    fn a_dynamic_pair_is_born_with_the_mounts_numbers() {
        let _guard = options_lock();
        let saved = crate::vfs::pty::devpts_options();
        apply_mount_options("mode=0640,gid=5").unwrap();
        let pty = crate::vfs::pty::allocate_dynamic().unwrap();
        let slave = PtySlave::new(pty.clone());
        assert_eq!(slave.mode(), 0o640);
        assert_eq!(slave.owner().1, 5);
        assert!(slave.keeps_modes(), "the slave's numbers are the pair's");

        // The write side lands on the pair, so the next lookup sees it.
        slave.set_owner(1000, 5);
        let again = PtySlave::new(pty);
        assert_eq!(again.owner(), (1000, 5));

        crate::vfs::pty::set_devpts_options(saved);
    }
}
