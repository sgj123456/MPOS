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

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::pty::PtySlave;
use super::{FileSystem, Inode, NodeKind, VfsResult};

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
    use super::*;

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
        assert!(
            !listed.iter().any(|(name, _)| name == "1"),
            "the virtual consoles have their own names in /dev"
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
}
