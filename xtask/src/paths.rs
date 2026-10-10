//! The one place the repository's image inputs are named.
//!
//! Every path here is *relative to the project root*: `rootfs.toml`, not
//! `root.join("rootfs.toml")`. A caller joins its own root - which it already
//! has, from `utils::project_root()` or a passed-in `&Path` - onto the name.
//! That keeps this module pure data (host-testable by construction) and keeps
//! the question "where does an install read its overlay from" answerable by
//! grepping one file.
//!
//! These names are where the build finds the trees and files that decide an
//! image's contents: the three sectioned tomls, each machine's overlay tree,
//! the shared root half. They were scattered across `disk`, `rootfs`,
//! `initramfs`, `config` and `sweep` as string literals, and a move of any of
//! them used to mean finding every copy - so the layout itself now lives here,
//! and the tests that pin the inventory (`rootfs.rs`, `config.rs`) ask this
//! module instead of restating a path.
//!
//! What deliberately stays out: anything under `build/`. Those are outputs,
//! named per machine in `arch::Arch::disk_image` and `initramfs::Machine`,
//! and moving them is a different change than moving an input.

/// The image's own half of the configuration: packages, partitions, hostname.
/// Its mtime is what marks an installed tree stale (`rootfs::needs_rebuild`).
pub(crate) const ROOTFS_CONFIG: &str = "assets/configs/rootfs.toml";

/// The machine's half: memory, cpus, devices, run mode. Host-side only, so
/// editing it never reinstalls (`config.rs`'s key-separation test).
pub(crate) const MACHINE_CONFIG: &str = "assets/configs/machine.toml";

/// Which test programs the sweep runs, and its groups.
pub(crate) const TESTS_CONFIG: &str = "assets/configs/tests.toml";

/// The early-userspace tree both machines' archives are built from, as one
/// sectioned file.
pub(crate) const INITRAMFS_CONFIG: &str = "assets/configs/initramfs.toml";

/// The root overlay the first machine merges last.
pub(crate) const ROOTFS_OVERLAY_X86_64: &str = "assets/rootfs/x86_64";

/// The second machine's root overlay: `etc/`, the mounts and runlevels its own
/// install writes. It carries no `/init` any more - both machines' disks answer
/// to the standard `/sbin/init`, whose applet farm the build lays
/// (`BUSYBOX_APPLETS`).
pub(crate) const ROOTFS_OVERLAY_AARCH64: &str = "assets/rootfs/aarch64";

/// The root overlay half *both* machines merge before their own (`CORE_OVERLAY`).
pub(crate) const ROOTFS_CORE: &str = "assets/rootfs/core";

/// The first machine's initramfs overlay: the half only its own early userspace
/// wants. Empty today, and the `/init` both machines run is *not* here - it is
/// the core's (`INITRAMFS_CORE`). A directory that does not exist merges to
/// nothing (`merge_overlay`'s documented answer), which is why an empty half
/// needs no special case anywhere.
pub(crate) const INITRAMFS_OVERLAY_X86_64: &str = "assets/initramfs/x86_64";

/// The second machine's initramfs overlay, merged after the core. Empty for the
/// same reason as the first machine's, and the same shape: a file only this
/// machine's early userspace wants is what would live here.
pub(crate) const INITRAMFS_OVERLAY_AARCH64: &str = "assets/initramfs/aarch64";

/// The early-userspace overlay half *both* machines merge before their own
/// (`initramfs.rs`'s `CORE_OVERLAY`), as `ROOTFS_CORE` is for the disk: the one
/// `/init` script the two machines run, and the tree's inventory beside it.
pub(crate) const INITRAMFS_CORE: &str = "assets/initramfs/core";

/// The busybox applet farm, one relative path per line: what a package's own
/// post-install would install, for the machine whose install cannot run it
/// (`rootfs::lay_applet_farm`).
pub(crate) const BUSYBOX_APPLETS: &str = "assets/configs/busybox-applets.txt";
