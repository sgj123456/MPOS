//! The TOML files the build reads: `machine.toml`, `rootfs.toml` and
//! `initramfs.toml` - and `tests.toml`, whose shape is its own.
//!
//! The files use a small, fixed subset - scalars and one array per key, some of
//! them written across several lines - so this is a parser for that subset and
//! not a TOML library. What it does have is sections, and they are load-bearing:
//! `[default]` is the baseline both machines read, `[x86_64]` and `[aarch64]` are
//! the machines' own halves, and a key carries the name of the thing it belongs
//! to (`qemu.memory`, `system.hostname`, `rootfs.arch`). One file therefore
//! describes two machines without either machine's values being visible to the
//! other ([`parse_sectioned_toml`]). `tests.toml` uses no sections and is read
//! flat ([`parse_simple_toml`]).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::arch::Arch;

/// How `cargo xtask run` boots the whole machine: one word that decides the
/// QEMU wiring *and* the consoles the guest is told to use, because a machine
/// that shows its console in two places has to have agreed on that in both.
///
/// The guest half is a `console=` list and nothing else (see
/// [`RunMode::cmdline_consoles`]); the window and the serial line are the
/// launcher's, and no mode word ever reaches the kernel.
///
/// There are three of these because there are exactly two questions to answer,
/// and the third word used to hide that: does the machine have a window, and is
/// the framebuffer one of the guest's consoles. Every mode keeps the serial
/// line - `gui` reads it from a log file instead of the launcher's terminal -
/// because the mode that discarded it discarded the kernel log with it, and a
/// log nobody can read is not a reason to have a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Serial is the whole console, and the only one: `console=ttyS0` on the
    /// guest line, QEMU `-display none` with the serial on stdio.
    Terminal,
    /// The whole machine in the window: a window with every device behind it
    /// and the framebuffer as the guest's *only* console (`console=tty0`), so
    /// the text tty — boot log, virtual consoles, login prompt and shells —
    /// all shows there, and no session has to claim the card for the screen
    /// to say something.
    ///
    /// The guest line names no wire, so the kernel's `wire_bytes` drops serial
    /// output after the command line is read; the launcher still taps the port,
    /// into `build/serial.log` rather than stdio, because the UEFI loader's
    /// lines, the kernel banner that precedes the command-line parse and a
    /// panic's `force_serial` report reach nowhere else — the host terminal
    /// goes quiet, but it must not go deaf. A compositor is a runtime guest:
    /// root's `./start-weston.sh` claims the card when started from a window
    /// shell, and `^A 1`..`^A 6` (the kernel's own mux, not a QEMU escape)
    /// switches back to the text console while it holds the keyboard.
    Gui,
    /// The window plus a live copy out the serial line, so the same
    /// console shows in the window and the terminal at once: `console=ttyS0
    /// console=tty0`, QEMU `-display {backend}` with the serial on stdio. This
    /// is the default.
    Mirror,
}

impl RunMode {
    /// Parse the `qemu.mode` value / `--mode` argument. Case- and
    /// space-insensitive; anything unrecognised is `None` so the caller can
    /// report it rather than silently booting a different mode.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "terminal" => Some(Self::Terminal),
            "gui" => Some(Self::Gui),
            "mirror" => Some(Self::Mirror),
            _ => None,
        }
    }

    /// The same parse, but a word this crate does not have is an error rather
    /// than a `None` the caller might quietly default over.
    ///
    /// The default matters here: `weston` and `compositor` were mode words and
    /// are not any more, and a `machine.toml` still naming one would otherwise
    /// boot a mirror machine at someone who asked for a compositor.
    pub fn parse_strict(text: &str) -> Result<Self> {
        Self::parse(text).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown run mode {:?}; expected terminal, gui or mirror",
                text.trim()
            )
        })
    }

    /// The consoles this mode asks the guest to use, written the way Linux has
    /// always written them: `console=` words, read by `src/boot/console_list.rs`.
    ///
    /// This is the whole of what a mode tells the kernel. The order carries the
    /// meaning: Linux gives the log to every console on the list and treats the
    /// last as the preferred one, so a wire-then-framebuffer list means the window
    /// takes the console over once the framebuffer is up while the wire keeps a
    /// best-effort copy, the wire alone stays authoritative and lossless, and the
    /// framebuffer alone means the kernel never writes to the serial port at all.
    ///
    /// Three words, three lists: what separates `gui` from `terminal` is not a
    /// window the guest cannot see but which sink the guest's own console has.
    ///
    /// The serial port's name comes from the rootfs config rather than being
    /// spelled here: it is the port the launcher taps, and one machine has one
    /// serial port. (`gui` never reads it — its console is the screen.)
    pub fn cmdline_consoles(self, serial_port: &str) -> String {
        match self {
            Self::Terminal => format!("console={serial_port}"),
            Self::Gui => "console=tty0".to_string(),
            Self::Mirror => format!("console={serial_port} console=tty0"),
        }
    }

    /// Whether this mode wants a QEMU window at all. Only terminal does not;
    /// where the serial line goes on the host — stdio or a log file — is the
    /// launcher's own business, not a second predicate.
    pub fn has_window(self) -> bool {
        !matches!(self, Self::Terminal)
    }

    /// The word this mode is written down with: in the `qemu.mode` value, in
    /// the `build/console.mode` marker of what the image was last baked with, and
    /// in the `MPOS_XTASK_MODE` hand-off into the privileged build.
    ///
    /// The marker keeps its old name on purpose. It is a cache of what is
    /// already in the image, and a new filename reads as "nothing was baked",
    /// which sends `run` through a root refresh of the boot files - a mount this
    /// host cannot even do without the loop module - to say nothing changed.
    pub fn name(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Gui => "gui",
            Self::Mirror => "mirror",
        }
    }
}

#[derive(Debug)]
pub struct InitramfsConfig {
    pub mirror: String,
    pub branch: String,
    pub arch: String,
    pub repos: Vec<String>,
    pub base_packages: Vec<String>,
    pub extra_packages: Vec<String>,
}

/// The account the guest is logged in as, and what it may do.
///
/// An empty `name` means no account beyond root is created, which is what an
/// image that only ever runs as root wants.
#[derive(Debug)]
pub struct UserConfig {
    pub name: String,
    pub groups: Vec<String>,
    pub shell: String,
    /// Empty leaves the account passwordless. The console is a serial line
    /// with no keyboard to type a password on, and a login nobody can pass is
    /// not a login.
    pub password: String,
}

/// The machine `run` and `test` boot the image on.
#[derive(Debug)]
pub struct QemuConfig {
    pub memory: String,
    pub cpus: u32,
    /// The name the guest reports to QEMU's DHCP server. It is the same
    /// `hostname` the rootfs config writes into the image - read from there
    /// rather than named again here, because the one place it is *stored* is
    /// where the image is told it (`system.hostname`).
    pub hostname: String,
    /// Passed through to `qemu-system-x86_64` verbatim, after everything this
    /// crate computes. The escape hatch for the one option a run needs and
    /// this file does not name.
    pub extra_args: Vec<String>,
    /// Attach the vsock device (`-device virtio-vsock-pci,guest-cid=3`), so
    /// the guest's `socket(AF_VSOCK)` has a host to talk to. Opt-in because
    /// it needs the host's vhost kernel module (`/dev/vhost-vsock`) - a
    /// QEMU asked for the device on a host without the module refuses to
    /// start at all, and a boot that never starts is worse than a machine
    /// with no vsock.
    pub vsock: bool,
    /// Attach the sound card (`-audiodev none` + `virtio-sound-pci`), so the
    /// guest's `/dev/dsp` has a device to play through. The backend plays
    /// nowhere - the point is exercising the driver, not the speakers - but
    /// the card itself needs QEMU 8.2 or newer, which is why it is opt-in.
    pub audio: bool,
    /// The QEMU display backend for an interactive `run` (`-display`), so the
    /// framebuffer console has a window to show itself in. Absent means `sdl`;
    /// name another backend your QEMU was built with, or `none` to watch only
    /// the serial log - which then goes quiet once the console moves to the
    /// framebuffer. A `test` boot ignores this and always runs headless.
    ///
    /// The value is passed through verbatim, so a backend's own suboptions are
    /// written here too: `display = "egl-headless,gl=on"` asks for the GL
    /// context the GPU below needs, and is the whole of what a headless GL run
    /// has to say. A `,gl=on` on a backend QEMU was not built with fails at
    /// QEMU's own hand, which is the honest place for it.
    pub display: Option<String>,
    /// The GPU device (`-device`), so the machine can be given a card with the
    /// virgl 3D renderer behind it. `virtio-gpu-pci` is the 2D scanout every
    /// boot has used; `virtio-gpu-gl-pci` is the same device with the host's
    /// `virglrenderer` behind it, and it is the one thing that makes a
    /// compositor refusing a *software* EGL device able to run (`NOTES.md`
    /// §7.4).
    ///
    /// Named as QEMU names it rather than as a boolean, for the same reason
    /// `display` is: on this host the GL device is `virtio-gpu-gl-pci`, and
    /// `virtio-vga-gl` and a rutabaga/gfxstream variant sit beside it - a
    /// host with a different one is asked for it here, without a code change.
    /// The GL card is not mutually exclusive with a plain display: QEMU starts
    /// `-device virtio-gpu-gl-pci` against `-display sdl` and the guest simply
    /// gets a card whose 3D context nothing asked for.
    pub gpu: String,
    /// The word `qemu.mode` holds, kept as text: see
    /// [`QemuConfig::run_mode`] for when it becomes a [`RunMode`] and why a word
    /// this crate does not have may not become the default one.
    pub mode_text: Option<String>,
}

#[derive(Debug)]
pub struct RootfsConfig {
    pub mirror: String,
    pub branch: String,
    pub arch: String,
    pub repos: Vec<String>,
    pub filesystem: String,
    pub root_device: String,
    pub serial_port: String,
    pub image_size: String,
    pub esp_size: String,
    pub mkfs_options: String,
    pub hostname: String,
    pub timezone: String,
    pub locale: String,
    /// The nameserver written into the guest's `/etc/resolv.conf`, over the
    /// stock `10.0.2.3` the `rootfs/` overlay ships. Unset, QEMU's user-mode
    /// network answers `10.0.2.3` itself - by forwarding to whatever the
    /// *host's* resolv.conf names, which on a systemd-resolved host is the
    /// `127.0.0.53` loopback stub, and which no QEMU option can redirect (its
    /// `dns=` netdev key renames the address slirp answers at; it does not
    /// name an upstream, which is why the build never passes it). A server
    /// named here is asked directly instead: slirp carries the guest's UDP to
    /// any address the host can reach, which is the one way a build has of
    /// choosing the guest's upstream.
    pub dns: Option<String>,
    /// Empty leaves root passwordless, which is the default a serial console
    /// with no keyboard needs. Anything else is set through `chpasswd`.
    pub root_password: String,
    pub base_packages: Vec<String>,
    pub extra_packages: Vec<String>,
    pub user: UserConfig,
}

impl InitramfsConfig {
    /// The archive's contents for `arch`, from the `[default]` baseline with that
    /// machine's own section overlaid over it
    /// ([`parse_sectioned_toml`]).
    pub fn load(path: &Path, arch: Arch) -> Result<Self> {
        let content = std::fs::read_to_string(path).with_context(|| {
            format!(
                "Failed to read the initramfs config from {}",
                path.display()
            )
        })?;
        let mut values = parse_sectioned_toml(&content, arch)
            .with_context(|| format!("in {}", path.display()))?;

        Ok(Self {
            mirror: values
                .remove("initramfs.mirror")
                .unwrap_or_else(|| "https://mirrors.tuna.tsinghua.edu.cn/alpine".to_string()),
            branch: values
                .remove("initramfs.branch")
                .unwrap_or_else(|| "latest-stable".to_string()),
            arch: values
                .remove("initramfs.arch")
                .unwrap_or_else(|| arch.name().to_string()),
            repos: take_list(&mut values, "initramfs.repos", &["main", "community"]),
            base_packages: parse_package_list(
                values
                    .remove("initramfs.base_packages")
                    .unwrap_or_else(|| "busybox,alpine-baselayout,apk-tools".to_string()),
            ),
            extra_packages: parse_package_list(
                values
                    .remove("initramfs.extra_packages")
                    .unwrap_or_default(),
            ),
        })
    }
}

impl QemuConfig {
    /// The machine `run` and `test` boot on: the one machine's half of
    /// `machine.toml`, plus the image's own hostname.
    ///
    /// The machine half lives in its own file because `rootfs.toml`'s mtime is
    /// what decides whether the Alpine tree is reinstalled
    /// (`rootfs::needs_full_install`), and a memory size has no business
    /// triggering one. What is read from `rootfs.toml` is one key - `system.hostname`,
    /// which is also the name QEMU's DHCP server is told - and it is read rather
    /// than repeated because the guest's name is written into the image, so that is
    /// where it lives. One fact, one home, two readers.
    ///
    /// A missing file is the defaults rather than an error: `test` builds no
    /// image, and a kernel test run must not need the files that describe one.
    /// A missing `machine.toml` therefore gets the default machine, and a missing
    /// `rootfs.toml` the default name. A file that is *there* and wrong is not
    /// covered by either: its parse error names the key.
    pub fn load(machine_path: &Path, rootfs_path: &Path, arch: Arch) -> Result<Self> {
        let mut values = match std::fs::read_to_string(machine_path) {
            Ok(content) => parse_sectioned_toml(&content, arch)
                .with_context(|| format!("in {}", machine_path.display()))?,
            Err(_) => HashMap::new(),
        };
        let hostname = std::fs::read_to_string(rootfs_path)
            .ok()
            .and_then(|content| parse_sectioned_toml(&content, arch).ok())
            .and_then(|mut image| image.remove("system.hostname"))
            .unwrap_or_else(default_hostname);
        Ok(Self::from_values(&mut values, hostname))
    }

    /// Both files resolved from the repository root, which is what every caller
    /// wants: `machine.toml` for the machine, `rootfs.toml` for the name.
    pub fn load_default(arch: Arch) -> Result<Self> {
        let root = crate::utils::project_root()?;
        Self::load(
            &root.join(crate::paths::MACHINE_CONFIG),
            &root.join(crate::paths::ROOTFS_CONFIG),
            arch,
        )
    }

    /// The values one machine's half of `machine.toml` carries, keyed by their
    /// dotted names.
    ///
    /// `arch` is not a parameter because which machine this is was decided when
    /// the file was read, where `[default]` and that machine's own section were
    /// overlaid into one map ([`parse_sectioned_toml`]). Every key here is
    /// therefore already that machine's final word: a value written under
    /// `[aarch64]` replaced the baseline's for the second machine, and was never
    /// visible to the first.
    fn from_values(values: &mut HashMap<String, String>, hostname: String) -> Self {
        // `MEM` still wins over everything: it is what a one-off `MEM=2048 cargo
        // xtask run` reaches for, and the file is the standing answer.
        let memory = std::env::var("MEM")
            .ok()
            .or_else(|| values.remove("qemu.memory"));
        Self {
            memory: memory.unwrap_or_else(|| "1024".to_string()),
            cpus: values
                .remove("qemu.cpus")
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(1),
            hostname,
            extra_args: take_list(values, "qemu.extra_args", &[]),
            vsock: take_flag(values, "qemu.vsock"),
            audio: take_flag(values, "qemu.audio"),
            display: values
                .remove("qemu.display")
                .filter(|text| !text.trim().is_empty()),
            // The 2D scanout is the default because it is what every boot
            // before this key had, and because it is the one that needs nothing
            // of the host. A blank value is the same as an absent one: a key
            // written and left empty is a mistake, not a request for a device
            // named "".
            gpu: values
                .remove("qemu.gpu")
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "virtio-gpu-pci".to_string()),
            // The word is stored rather than resolved here, because the load is
            // shared with `test` — which boots no window at all and must not fail
            // on a mode name it never uses. The mode is resolved at the two places
            // that actually boot a window, in [`Self::run_mode`].
            mode_text: values.remove("qemu.mode"),
        }
    }

    /// The standing mode for `run`, overridable per invocation with `--mode`.
    ///
    /// An absent or blank key is [`RunMode::Mirror`] — the machine `run` booted
    /// before the modes were split — while a key naming a word this crate does not
    /// have is an error. That difference is the whole reason the text is kept:
    /// `weston` was a mode word and is not one any more, and a `machine.toml` still
    /// naming it would otherwise be defaulted into a mirror boot at someone who
    /// asked for a compositor. The consoles the two modes bake differ (`console=`
    /// with and without `tty0`), so the wrong default is not a cosmetic one.
    pub fn run_mode(&self) -> Result<RunMode> {
        match self.mode_text.as_deref().map(str::trim) {
            None | Some("") => Ok(RunMode::Mirror),
            Some(text) => RunMode::parse_strict(text),
        }
    }
}

/// Which user test programs the self-test sweep runs, and which it leaves out.
///
/// The file is `tests.toml` at the repository root and its shape is one array
/// per group - `external = ["t-dns", ...]` - plus `default`, naming the groups a
/// plain run adds to the ungrouped tests. A test in *no* group always runs: the
/// default sweep is "everything that is a check of this machine", so leaving one
/// out has to be a deliberate act with a reason written next to it.
///
/// One kind of group names a **machine** rather than a reason ([`MACHINE_GROUPS`]):
/// `aarch64 = [...]` lists the tests the second machine cannot serve yet. Such a
/// group is invisible to every other machine's sweep, so a test in it is
/// ungrouped as far as x86_64 is concerned and stays in the default sweep there -
/// which is the whole point of saying it this way. A list that took the socket
/// suite out of the x86_64 gate to describe an aarch64 gap would be a gate that
/// stopped testing the machine it boots.
///
/// The selection is a pure function of the file, the flags and the machine,
/// which is what makes it host-testable - and it is the one piece of this a
/// mistake would make invisible, because a sweep that runs fewer tests than it
/// should prints a smaller number and passes.
#[derive(Debug, Default, Clone)]
pub struct TestConfig {
    /// Groups a plain run adds to the ungrouped tests.
    default: Vec<String>,
    /// `(group, tests)` by group name, so the report is stable and a run cannot
    /// depend on a hash map's iteration order.
    groups: Vec<(String, Vec<String>)>,
}

/// The machine a sweep is selecting for.
///
/// A group named after a machine is that machine's: the tests in it are the ones
/// it cannot serve yet, and only its own sweep sees the group at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Machine {
    X86_64,
    Aarch64,
}

/// The groups that name a machine. A test in one of these runs on every machine
/// *but* the one it names.
const MACHINE_GROUPS: [(&str, Machine); 1] = [("aarch64", Machine::Aarch64)];

impl Machine {
    /// Whether `group` is this machine's own - a name that says "not me".
    fn owns(self, group: &str) -> bool {
        MACHINE_GROUPS
            .iter()
            .any(|(name, machine)| *name == group && *machine == self)
    }
}

/// Whether a sweep for `machine` can see a group named `group`. A group that
/// names no machine is a reason, and every machine sees it; a group that names
/// one is visible only to that machine.
fn visible(group: &str, machine: Machine) -> bool {
    !MACHINE_GROUPS.iter().any(|(name, _)| *name == group) || machine.owns(group)
}

impl TestConfig {
    /// Read the file. A missing one is **no exclusions**, not an error.
    ///
    /// The opposite default is the dangerous one: a sweep that silently skips
    /// programs because a file it does not strictly need went missing is how a
    /// suite comes to report success for a machine it never checked.
    pub fn load(path: &Path) -> Result<Self> {
        let Ok(content) = std::fs::read_to_string(path) else {
            return Ok(Self::default());
        };
        let mut values = parse_simple_toml(&content)?;
        let default = take_list(&mut values, "default", &[]);
        let mut groups: Vec<(String, Vec<String>)> = values
            .into_iter()
            .map(|(name, tests)| (name, parse_package_list(tests)))
            .collect();
        groups.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Self { default, groups })
    }

    /// The groups a run will use: an explicit list replaces the file's
    /// `default`, `--all-groups` is every group the file defines, and neither
    /// means the `default`.
    pub fn selection(&self, requested: &[String], all: bool) -> Vec<String> {
        if all {
            return self.groups.iter().map(|(name, _)| name.clone()).collect();
        }
        if requested.is_empty() {
            return self.default.clone();
        }
        requested.to_vec()
    }

    /// The names in `requested` that this file does not define.
    pub fn unknown(&self, requested: &[String]) -> Vec<String> {
        requested
            .iter()
            .filter(|name| !self.groups.iter().any(|(group, _)| group == *name))
            .cloned()
            .collect()
    }

    /// The names the file lists that no built program answers to - a group that
    /// has outlived a test, or a typo in one, either of which would otherwise
    /// leave that test running unclassified for ever.
    pub fn unmatched(&self, built: &[String]) -> Vec<String> {
        self.groups
            .iter()
            .flat_map(|(_, tests)| tests.iter())
            .filter(|test| !built.iter().any(|name| name == *test))
            .cloned()
            .collect()
    }

    /// The `(group, test)` pairs that are in a group `machine` owns - the tests
    /// left out because *this* machine cannot serve them.
    ///
    /// This is what a machine's own arm validates its payload against: a name in
    /// its group that the payload does not carry would otherwise be a test
    /// quietly left out of both sweeps, which is the one failure this file exists
    /// to prevent.
    pub fn machine_tests(&self, machine: Machine) -> Vec<(&str, &str)> {
        self.groups
            .iter()
            .filter(|(group, _)| machine.owns(group))
            .flat_map(|(group, tests)| {
                tests
                    .iter()
                    .map(move |test| (group.as_str(), test.as_str()))
            })
            .collect()
    }

    /// Whether `test` runs under `selection` on `machine`.
    pub fn runs(&self, test: &str, selection: &[String], machine: Machine) -> bool {
        let groups = self.groups_of(test, machine);
        groups.is_empty()
            || groups
                .iter()
                .any(|group| selection.iter().any(|asked| asked == group))
    }

    /// The groups `test` is in, by name, as `machine`'s sweep sees them.
    pub fn groups_of<'a>(&'a self, test: &str, machine: Machine) -> Vec<&'a str> {
        self.groups
            .iter()
            .filter(|(group, _)| visible(group, machine))
            .filter(|(_, tests)| tests.iter().any(|name| name == test))
            .map(|(group, _)| group.as_str())
            .collect()
    }

    /// Every group the file defines that `machine`'s sweep can see, for a report
    /// that lists what was left out group by group.
    pub fn group_names(&self, machine: Machine) -> Vec<&str> {
        self.groups
            .iter()
            .filter(|(group, _)| visible(group, machine))
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

/// Takes a `true`/`false` key, answering `false` when it is not there. A key
/// written and left empty is not a `true`: the value has to say so.
fn take_flag(values: &mut HashMap<String, String>, key: &str) -> bool {
    values
        .remove(key)
        .is_some_and(|text| text.trim().eq_ignore_ascii_case("true"))
}

fn default_hostname() -> String {
    "mpos".to_string()
}

impl UserConfig {
    fn from_values(values: &mut HashMap<String, String>) -> Self {
        Self {
            name: values
                .remove("user.name")
                .unwrap_or_default()
                .trim()
                .to_string(),
            groups: take_list(values, "user.groups", &["wheel"]),
            shell: values
                .remove("user.shell")
                .unwrap_or_else(|| "/bin/sh".to_string()),
            password: values.remove("user.password").unwrap_or_default(),
        }
    }
}

impl RootfsConfig {
    /// The image's contents for `arch`: the `[default]` baseline with that
    /// machine's own section overlaid over it ([`parse_sectioned_toml`]).
    ///
    /// `arch` picks which section is overlaid; it is not checked against the
    /// `rootfs.arch` key, because that key is what *apk* is asked for and the
    /// caller that boots the machine is the authority on which machine it is.
    pub fn load(path: &Path, arch: Arch) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read the rootfs config from {}", path.display()))?;
        let mut values = parse_sectioned_toml(&content, arch)
            .with_context(|| format!("in {}", path.display()))?;
        // Taken once, because two structs of the answer want it: `system.hostname`
        // and the name QEMU's DHCP server is told.
        let hostname = values
            .remove("system.hostname")
            .unwrap_or_else(default_hostname);

        Ok(Self {
            mirror: values
                .remove("rootfs.mirror")
                .unwrap_or_else(|| "https://mirrors.tuna.tsinghua.edu.cn/alpine".to_string()),
            branch: values
                .remove("rootfs.branch")
                .unwrap_or_else(|| "latest-stable".to_string()),
            arch: values
                .remove("rootfs.arch")
                .unwrap_or_else(|| arch.name().to_string()),
            repos: take_list(&mut values, "rootfs.repos", &["main", "community"]),
            filesystem: values
                .remove("rootfs.filesystem")
                .unwrap_or_else(|| "ext2".to_string()),
            root_device: values
                .remove("rootfs.root_device")
                .unwrap_or_else(|| "/dev/vda2".to_string()),
            serial_port: values
                .remove("rootfs.serial_port")
                .unwrap_or_else(|| "ttyS0".to_string()),
            image_size: values
                .remove("rootfs.image_size")
                .unwrap_or_else(|| "2G".to_string()),
            esp_size: values
                .remove("rootfs.esp_size")
                .unwrap_or_else(|| "128M".to_string()),
            // What the driver can honour, spelled as `mke2fs -O` features:
            // no journal (nothing here replays one, and a crash would leave
            // RECOVER set and the image read-only until a Linux fsck ran) and
            // no metadata checksums (every write would leave a stale one).
            mkfs_options: values
                .remove("rootfs.mkfs_options")
                .unwrap_or_else(|| "^has_journal,^metadata_csum".to_string()),
            hostname,
            timezone: values
                .remove("system.timezone")
                .unwrap_or_else(|| "UTC".to_string()),
            locale: values
                .remove("system.locale")
                .unwrap_or_else(|| "en_US.UTF-8".to_string()),
            dns: values
                .remove("system.dns")
                .filter(|text| !text.trim().is_empty()),
            root_password: values.remove("system.root_password").unwrap_or_default(),
            // The shared core of both userlands (`rootfs::SHARED_BASE_PACKAGES`)
            // is *added* rather than merely defaulted: the file's two machine
            // sections name each machine's own half, and a package that belongs
            // in the base on both machines must land on both even when a section
            // forgets it - which is exactly the drift (§13.19) this fold makes
            // impossible. A name listed twice installs once; apk dedupes its own
            // arguments.
            base_packages: {
                let mut packages = crate::rootfs::SHARED_BASE_PACKAGES
                    .iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>();
                packages.extend(parse_package_list(
                    values.remove("rootfs.base_packages").unwrap_or_default(),
                ));
                packages
            },
            extra_packages: parse_package_list(
                values.remove("rootfs.extra_packages").unwrap_or_else(|| {
                    "openrc,alpine-release,ssl_client,ca-certificates,apk-tools".to_string()
                }),
            ),
            user: UserConfig::from_values(&mut values),
        })
    }
}

/// Takes one array-valued key, answering `default` when it is not there.
fn take_list(values: &mut HashMap<String, String>, key: &str, default: &[&str]) -> Vec<String> {
    match values.remove(key) {
        Some(list) => parse_package_list(list),
        None => default.iter().map(|item| item.to_string()).collect(),
    }
}

/// One `key = value` from a config file, tagged with the `[section]` it was
/// written under - `None` for a key that precedes every header.
struct Entry {
    section: Option<String>,
    key: String,
    value: String,
}

/// Reads every `key = value` in a file, in the order the file wrote them,
/// joining an array written across several lines into one value and dropping
/// comments whether they own the whole line or trail a value - because a `#`
/// inside one of the package strings would otherwise be read as one.
fn scan(content: &str) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut section: Option<String> = None;
    let mut lines = content.lines();

    while let Some(raw) = lines.next() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            // A header owns every key until the next one. What the name *means*
            // is the resolver's business, not the scanner's.
            section = Some(
                line.trim_start_matches('[')
                    .trim_end_matches(']')
                    .trim()
                    .to_string(),
            );
            continue;
        }

        let Some(pos) = line.find('=') else {
            continue;
        };
        let key = line[..pos].trim().to_string();
        let mut value = line[pos + 1..].trim().to_string();

        // An array that starts on this line and does not close on it runs on
        // until the bracket that closes it. `rootfs.toml` writes its package
        // list that way.
        while value.starts_with('[') && !brackets_balanced(&value) {
            let Some(next) = lines.next() else { break };
            value.push(' ');
            value.push_str(strip_comment(next).trim());
        }

        let value = if value.starts_with('[') && value.ends_with(']') {
            value[1..value.len() - 1].to_string()
        } else {
            unquote(&value)
        };
        entries.push(Entry {
            section: section.clone(),
            key,
            value,
        });
    }

    Ok(entries)
}

/// The flat view of a file that uses no sections: `tests.toml`, whose keys are
/// test groups rather than a machine's values.
fn parse_simple_toml(content: &str) -> Result<HashMap<String, String>> {
    Ok(scan(content)?
        .into_iter()
        .map(|entry| (entry.key, entry.value))
        .collect())
}

/// The machine sections a config file may carry. `default` is the baseline both
/// machines read; these two name the machines themselves.
const MACHINE_SECTIONS: [&str; 2] = ["x86_64", "aarch64"];

/// The keys `arch` reads from a sectioned config file: the `[default]` baseline
/// with that machine's own section overlaid on it.
///
/// One file describes both machines because most of each of these files is about
/// the *build* rather than about a machine - the mirror, the repositories, the
/// timezone and the filesystem are one answer - so those keys are written once
/// under `[default]`. What differs is written once under the machine's own name,
/// and a key the baseline does not carry at all is that machine's alone. Either
/// way a value is written once, where it is read.
///
/// Two mistakes are refused rather than skipped. A key before any header is a
/// forgotten section, and a header that is not one of the three is a leftover of
/// the flat format these files used to wear (`[qemu]`, `[system]`): both would
/// drop their keys in silence, and a machine booted on defaults because a header
/// moved is the shape of wrong answer this parser exists to prevent. A refusal
/// is a build error instead, naming the key.
fn parse_sectioned_toml(content: &str, arch: Arch) -> Result<HashMap<String, String>> {
    let mut baseline: HashMap<String, String> = HashMap::new();
    let mut own: HashMap<String, String> = HashMap::new();

    for entry in scan(content)? {
        let Some(section) = entry.section else {
            anyhow::bail!(
                "the key `{}` is written before any section; every key in this file \
                 belongs under [default], [x86_64] or [aarch64]",
                entry.key
            );
        };
        if section == "default" {
            baseline.insert(entry.key, entry.value);
        } else if section == arch.name() {
            own.insert(entry.key, entry.value);
        } else if !MACHINE_SECTIONS.contains(&section.as_str()) {
            anyhow::bail!(
                "unknown section `[{section}]`; this file takes [default] (the baseline \
                 both machines read), [x86_64] and [aarch64]"
            );
        }
        // The other machine's section is this file's other half, not a mistake:
        // read past it.
    }

    // The machine's own value wins over the baseline's, wherever in the file the
    // two are written.
    baseline.extend(own);
    Ok(baseline)
}

fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (index, ch) in line.char_indices() {
        match ch {
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            ch if Some(ch) == quote => quote = None,
            '#' if quote.is_none() => return &line[..index],
            _ => {}
        }
    }
    line
}

fn brackets_balanced(value: &str) -> bool {
    value.matches('[').count() <= value.matches(']').count()
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
        || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn parse_package_list(list: String) -> Vec<String> {
    list.split(',')
        .map(|item| unquote(item.trim()))
        .filter(|item| !item.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_modes_round_trip_and_name_their_console_list() {
        // The words the user types, the `console=` list the kernel reads, and the
        // name the marker keeps must all line up, because a mode that parses but
        // bakes the wrong consoles boots the machine in a mode nobody asked for.
        for (text, mode, consoles) in [
            ("terminal", RunMode::Terminal, "console=ttyS0"),
            // Gui is the framebuffer alone: the text console — log, VTs, login,
            // shells — lives in the window, and the guest is never told about
            // the wire, so the kernel's wire_bytes drop it. The launcher still
            // taps the port into build/serial.log for what cannot reach a
            // screen: the loader's lines and the pre-cmdline banner.
            ("gui", RunMode::Gui, "console=tty0"),
            // Mirror is two consoles with the framebuffer preferred, which is
            // what the word has always meant: Linux writes the log to every
            // console on the list and the last is the preferred one, so mirror
            // is a list and needs no word of its own.
            ("mirror", RunMode::Mirror, "console=ttyS0 console=tty0"),
        ] {
            assert_eq!(RunMode::parse(text), Some(mode));
            // Case- and space-insensitive: the name comes from a config file a
            // person edits by hand.
            assert_eq!(
                RunMode::parse(&format!("  {}  ", text.to_uppercase())),
                Some(mode)
            );
            assert_eq!(mode.cmdline_consoles("ttyS0"), consoles);
            assert_eq!(mode.name(), text);
        }
        assert_eq!(RunMode::parse("anything-else"), None);
        assert!(!RunMode::Terminal.has_window());
        assert!(RunMode::Gui.has_window() && RunMode::Mirror.has_window());
        // Three words, three lists: what a mode tells the guest is its own
        // answer, so no switch between modes can ride on an unchanged
        // `mpos.cmdline`. Gui in particular must leave the wire out of the
        // consoles it names — that omission is the whole of why the host
        // terminal goes quiet — and a future "let's also name ttyS0" here is
        // the change this pin is waiting for.
        let lists: Vec<_> = [RunMode::Terminal, RunMode::Gui, RunMode::Mirror]
            .iter()
            .map(|mode| mode.cmdline_consoles("ttyS0"))
            .collect();
        assert_eq!(
            lists.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "two modes baked the same consoles"
        );
        assert!(
            !lists[1].contains("ttyS0"),
            "gui names the wire, so its console would go to the terminal again"
        );
    }

    /// One file, two machines: a key under the machine's own section wins over
    /// the baseline's, is invisible to the other machine, and is written only
    /// where it differs - which is what keeps each machine's differences in the
    /// file that describes machines instead of in the code that boots them.
    #[test]
    fn a_machines_own_section_wins_and_the_other_machine_never_sees_it() {
        let content = "\
[default]
qemu.memory = \"2048\"
qemu.audio = false
qemu.gpu = \"virtio-gpu-pci\"
[aarch64]
qemu.memory = \"256\"
qemu.audio = true
";
        let mut x86 = parse_sectioned_toml(content, Arch::X86_64).unwrap();
        let mut aarch64 = parse_sectioned_toml(content, Arch::Aarch64).unwrap();

        assert_eq!(
            x86.remove("qemu.memory").as_deref(),
            Some("2048"),
            "the baseline is the first machine's"
        );
        assert_eq!(
            aarch64.remove("qemu.memory").as_deref(),
            Some("256"),
            "the second machine's own section wins"
        );
        assert_eq!(x86.remove("qemu.audio").as_deref(), Some("false"));
        assert_eq!(aarch64.remove("qemu.audio").as_deref(), Some("true"));
        // A key with no override is the baseline's on both, so the file says only
        // what actually differs.
        assert_eq!(x86.remove("qemu.gpu"), aarch64.remove("qemu.gpu"));
        assert!(
            x86.is_empty() && aarch64.is_empty(),
            "every key was consumed"
        );
    }

    /// The parser's two refusals, which are the same mistake seen twice: a file
    /// half-migrated from the flat format these files used to wear.
    ///
    /// A key that belongs to no section, and a header that is not one of the
    /// three, would both drop their keys in silence - which boots a machine on
    /// defaults, the one answer nobody can see. The refusal is what turns that
    /// into a build error naming the key.
    #[test]
    fn a_key_outside_a_section_and_an_unknown_section_are_both_refused() {
        let outside = parse_sectioned_toml("qemu.memory = \"512\"\n", Arch::X86_64)
            .expect_err("a key before any header must be refused")
            .to_string();
        assert!(outside.contains("qemu.memory"), "{outside}");

        let unknown = parse_sectioned_toml("[default]\n[qemu]\nmemory = \"512\"\n", Arch::X86_64)
            .expect_err("a header that is not one of the three must be refused")
            .to_string();
        assert!(unknown.contains("[qemu]"), "{unknown}");

        // The *other* machine's section is read past rather than refused: it is
        // this file's other half, which is what lets one file hold both. Each
        // machine reads its own value, and neither sees the other's.
        let both = "\
[default]
qemu.gpu = \"a\"
[x86_64]
qemu.gpu = \"b\"
[aarch64]
qemu.gpu = \"c\"
";
        assert_eq!(
            parse_sectioned_toml(both, Arch::X86_64).unwrap()["qemu.gpu"],
            "b"
        );
        assert_eq!(
            parse_sectioned_toml(both, Arch::Aarch64).unwrap()["qemu.gpu"],
            "c"
        );
    }

    /// The split's whole purpose, now that each kind of file is sectioned: the
    /// machine half is `machine.toml` and the image half is `rootfs.toml`, so a
    /// machine key that drifts back into the image file starts charging a full
    /// package install for a memory size.
    ///
    /// Both machines are asked, because the second one's half is where the drift
    /// would happen quietly - it is the half that overrides.
    #[test]
    fn the_machine_and_the_image_keep_their_own_keys() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask sits under the repository root");
        let machine = std::fs::read_to_string(root.join(crate::paths::MACHINE_CONFIG))
            .expect("machine.toml is the machine half of the configuration");
        let image =
            std::fs::read_to_string(root.join(crate::paths::ROOTFS_CONFIG)).expect("rootfs.toml");

        for arch in [Arch::X86_64, Arch::Aarch64] {
            let machine_keys = parse_sectioned_toml(&machine, arch).unwrap();
            let image_keys = parse_sectioned_toml(&image, arch).unwrap();

            // The machine half is all there...
            for key in [
                "qemu.memory",
                "qemu.cpus",
                "qemu.extra_args",
                "qemu.vsock",
                "qemu.audio",
                "qemu.gpu",
                "qemu.mode",
            ] {
                assert!(machine_keys.contains_key(key), "machine.toml lost {key}");
            }
            // ...and none of it is in the file whose mtime decides whether
            // Alpine is reinstalled.
            for key in machine_keys.keys() {
                assert!(
                    !image_keys.contains_key(key),
                    "rootfs.toml names {key}: editing it would reinstall the image"
                );
            }
            // The one fact the machine half reads *through* stays where the image
            // is told it: the guest's own name.
            assert!(
                image_keys.contains_key("system.hostname"),
                "the name lives with the image"
            );
        }
    }

    /// What the two machines actually read out of the shipped files.
    ///
    /// This is the pin the sectioned format had to earn: the prefixed keys were
    /// replaced by sections, and a migration that quietly changed a memory size, a
    /// serial port or a package list would pass every other test in this file
    /// while booting a different machine.
    #[test]
    fn the_shipped_files_answer_each_machine_its_own_values() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask sits under the repository root");
        let machine = std::fs::read_to_string(root.join(crate::paths::MACHINE_CONFIG)).unwrap();
        let x86_map = parse_sectioned_toml(&machine, Arch::X86_64).unwrap();
        let arm_map = parse_sectioned_toml(&machine, Arch::Aarch64).unwrap();

        // Read from the map for the memory, because `QemuConfig` lets a one-off
        // `MEM=` win over the file and this is a claim about the file.
        assert_eq!(x86_map["qemu.memory"], "2048");
        assert_eq!(arm_map["qemu.memory"], "256");
        let x86_qemu = QemuConfig::from_values(&mut x86_map.clone(), "probe".into());
        let arm_qemu = QemuConfig::from_values(&mut arm_map.clone(), "probe".into());
        // The first machine's window backend is the *absent* key, which is what
        // makes `run` use `sdl`; the second's is `none`, and it is a value
        // because that machine has no window at all.
        assert_eq!(x86_qemu.display, None);
        assert_eq!(arm_qemu.display.as_deref(), Some("none"));
        // `-vga none` is an x86-only option a `-machine virt` boot refuses, so
        // the second machine's half overrides the list with an empty one - which
        // is a value, while an absent key would inherit the baseline's.
        assert_eq!(x86_qemu.extra_args, vec!["-vga", "none"]);
        assert!(arm_qemu.extra_args.is_empty());
        // The devices the two share are written once, and the second machine's
        // extra condition on vsock is the wiring's rather than a key's.
        assert_eq!(x86_qemu.vsock, arm_qemu.vsock);
        assert_eq!(x86_qemu.audio, arm_qemu.audio);
        assert_eq!(x86_qemu.gpu, "virtio-gpu-pci");

        let x86 =
            RootfsConfig::load(&root.join(crate::paths::ROOTFS_CONFIG), Arch::X86_64).unwrap();
        let arm =
            RootfsConfig::load(&root.join(crate::paths::ROOTFS_CONFIG), Arch::Aarch64).unwrap();
        // The package architecture, the serial port and the root device are
        // machine facts, and each half names its own.
        assert_eq!(
            (x86.arch.as_str(), arm.arch.as_str()),
            ("x86_64", "aarch64")
        );
        assert_eq!(
            (x86.serial_port.as_str(), arm.serial_port.as_str()),
            ("ttyS0", "ttyAMA0")
        );
        // The root device is one value for both now, because both disks are the
        // same shape: a GPT whose second partition is the ext4 root. The second
        // machine's half stopped naming the whole disk when its image became a
        // partitioned one (`NOTES.md` §15.18), and a `root=` naming the whole disk
        // would send its initramfs script at a partition table instead of a
        // filesystem.
        assert_eq!(
            (x86.root_device.as_str(), arm.root_device.as_str()),
            ("/dev/vda2", "/dev/vda2")
        );
        // The desktop's base packages are not the second machine's, and the
        // second machine's extra list is not the desktop's.
        assert!(x86.base_packages.iter().any(|p| p == "e2fsprogs"));
        assert!(!arm.base_packages.iter().any(|p| p == "e2fsprogs"));
        assert!(x86.extra_packages.iter().any(|p| p == "weston"));
        assert!(!arm.extra_packages.iter().any(|p| p == "weston"));
        assert!(arm.extra_packages.iter().any(|p| p == "strace"));
        // The account is the first machine's: the second logs in as root alone,
        // which is what emptying `user.name` in its half means.
        assert_eq!(x86.user.name, "mpos");
        assert!(
            arm.user.name.is_empty(),
            "the second machine has no account beyond root"
        );
        // What the two share is written once and read by both.
        assert_eq!(x86.hostname, arm.hostname);
        assert_eq!(x86.timezone, arm.timezone);
        assert_eq!(x86.dns, arm.dns);
        assert_eq!(x86.filesystem, arm.filesystem);
        assert_eq!(x86.repos, arm.repos);

        let x86 = InitramfsConfig::load(&root.join(crate::paths::INITRAMFS_CONFIG), Arch::X86_64)
            .unwrap();
        let arm = InitramfsConfig::load(&root.join(crate::paths::INITRAMFS_CONFIG), Arch::Aarch64)
            .unwrap();
        assert_eq!(
            (x86.arch.as_str(), arm.arch.as_str()),
            ("x86_64", "aarch64")
        );
        assert_eq!(x86.mirror, arm.mirror);
        assert_eq!(x86.repos, arm.repos);
    }

    #[test]
    fn both_machines_base_on_the_shared_core_list() {
        // The pin for §13.19's package convergence: `SHARED_BASE_PACKAGES` is
        // folded into *every* machine's base by the loader itself, so neither
        // half can forget it and no install path can skip it. What each half
        // names are its own hardware facts - and a half must not re-name what
        // the core already carries, or the drift returns under a different
        // spelling.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask sits under the repository root");
        let x86 =
            RootfsConfig::load(&root.join(crate::paths::ROOTFS_CONFIG), Arch::X86_64).unwrap();
        let arm =
            RootfsConfig::load(&root.join(crate::paths::ROOTFS_CONFIG), Arch::Aarch64).unwrap();

        for config in [&x86, &arm] {
            for shared in crate::rootfs::SHARED_BASE_PACKAGES {
                assert!(
                    config.base_packages.iter().any(|p| p == shared),
                    "the base lost {shared} - the fold stopped folding"
                );
            }
        }
        // A name listed twice installs once (apk dedupes its arguments), but the
        // halves stay honest about who owns what: the core's names appear
        // exactly once, from the fold alone.
        for shared in crate::rootfs::SHARED_BASE_PACKAGES {
            assert_eq!(
                x86.base_packages
                    .iter()
                    .filter(|p| p.as_str() == *shared)
                    .count(),
                1,
                "{shared} is named by the core list, not repeated in rootfs.toml"
            );
            assert_eq!(
                arm.base_packages
                    .iter()
                    .filter(|p| p.as_str() == *shared)
                    .count(),
                1,
                "{shared} is named by the core list, not repeated in [aarch64]"
            );
        }
        // The halves that stayed per-machine, with their reasons written in the
        // file itself: e2fsprogs/dosfstools belong to the guest that makes
        // filesystems; the second machine adds nothing to the core.
        assert!(x86.base_packages.iter().any(|p| p == "e2fsprogs"));
        assert!(!arm.base_packages.iter().any(|p| p == "e2fsprogs"));
        // And openrc left the x86 extras when it entered the core - one name,
        // one place, both machines.
        assert!(!x86.extra_packages.iter().any(|p| p == "openrc"));
    }

    /// The machine a plain `cargo xtask test` boots: the one every selection
    /// test that is not about the machine rule is about.
    const X86: Machine = Machine::X86_64;

    fn groups(text: &str) -> TestConfig {
        let values = parse_simple_toml(text).unwrap();
        let mut values = values;
        let default = take_list(&mut values, "default", &[]);
        let mut groups: Vec<(String, Vec<String>)> = values
            .into_iter()
            .map(|(name, tests)| (name, parse_package_list(tests)))
            .collect();
        groups.sort_by(|a, b| a.0.cmp(&b.0));
        TestConfig { default, groups }
    }

    #[test]
    fn a_test_in_no_group_runs_and_a_grouped_one_does_not() {
        // The whole contract in one line: being named in a group is what takes a
        // test out of the default sweep, and naming a group puts it back.
        let config = groups("default = []\nexternal = [\"t-dns\", \"t-udp\"]\n");
        assert!(config.runs("t-openat", &config.selection(&[], false), X86));
        assert!(!config.runs("t-dns", &config.selection(&[], false), X86));
        assert!(config.runs("t-dns", &config.selection(&["external".into()], false), X86));
        // A test in two groups runs when *either* is asked for.
        let both = groups("external = [\"t-dns\"]\nslow = [\"t-dns\", \"t-pipecost\"]\n");
        assert!(both.runs("t-dns", &both.selection(&["slow".into()], false), X86));
        assert!(!both.runs(
            "t-pipecost",
            &both.selection(&["external".into()], false),
            X86
        ));
        assert_eq!(both.groups_of("t-dns", X86), vec!["external", "slow"]);
    }

    /// A group named after a machine says "not that machine", and says nothing
    /// about any other one.
    ///
    /// This is the rule that lets the second machine's gaps be written down
    /// without taking the tests out of the first machine's gate: the same test
    /// has to be *left out* on aarch64 and *staged by default* on x86_64, from
    /// one file.
    #[test]
    fn a_group_named_after_a_machine_is_only_that_machines() {
        let config = groups("aarch64 = [\"t-socket\"]\nexternal = [\"t-udp\"]\n");
        // The ungrouped test runs everywhere, as always.
        assert!(config.runs("t-openat", &config.selection(&[], false), X86));
        assert!(config.runs("t-openat", &config.selection(&[], false), Machine::Aarch64));
        // The aarch64 group is invisible to x86_64, so its member is ungrouped
        // there - in the default sweep, exactly as it was before the group
        // existed. Visible to aarch64, which leaves it out.
        assert!(config.runs("t-socket", &config.selection(&[], false), X86));
        assert!(!config.runs("t-socket", &config.selection(&[], false), Machine::Aarch64));
        assert!(config.groups_of("t-socket", X86).is_empty());
        assert_eq!(
            config.groups_of("t-socket", Machine::Aarch64),
            vec!["aarch64"]
        );
        // A reason-group is every machine's, so the internet stays out of both.
        assert!(!config.runs("t-udp", &config.selection(&[], false), X86));
        assert!(!config.runs("t-udp", &config.selection(&[], false), Machine::Aarch64));
        // The reports see different group lists, which is what makes each one's
        // "left out" list readable.
        assert_eq!(config.group_names(X86), vec!["external"]);
        assert_eq!(
            config.group_names(Machine::Aarch64),
            vec!["aarch64", "external"]
        );
        // And a machine's own arm can ask what it is supposed to be missing.
        assert_eq!(
            config.machine_tests(Machine::Aarch64),
            vec![("aarch64", "t-socket")]
        );
        assert!(config.machine_tests(X86).is_empty());
    }

    #[test]
    fn the_flags_replace_or_extend_the_files_default() {
        let config =
            groups("default = [\"slow\"]\nslow = [\"t-pipecost\"]\nexternal = [\"t-dns\"]\n");
        // No flag: the file's `default`.
        assert!(config.runs("t-pipecost", &config.selection(&[], false), X86));
        assert!(!config.runs("t-dns", &config.selection(&[], false), X86));
        // A named group *replaces* it rather than adding to it - otherwise a
        // second `--groups` on the command line would be unable to narrow.
        assert!(!config.runs(
            "t-pipecost",
            &config.selection(&["external".into()], false),
            X86
        ));
        assert!(config.runs("t-dns", &config.selection(&["external".into()], false), X86));
        // Every group, for the run that wants everything.
        let all = config.selection(&[], true);
        assert!(config.runs("t-dns", &all, X86) && config.runs("t-pipecost", &all, X86));
        assert!(
            config.runs("t-openat", &all, X86),
            "the ungrouped ones run too"
        );
    }

    #[test]
    fn a_missing_file_runs_everything() {
        // The dangerous default is the other one: a sweep that silently skips
        // programs because a file went missing reports success for a machine it
        // never checked.
        let config = TestConfig::load(Path::new("/nonexistent/tests.toml")).unwrap();
        assert!(config.runs("t-openat", &config.selection(&[], false), X86));
        assert!(config.group_names(X86).is_empty());
    }

    #[test]
    fn a_name_nothing_answers_to_is_reported() {
        let config = groups("external = [\"t-dns\", \"t-gone\"]\n");
        assert_eq!(
            config.unmatched(&["t-dns".to_string(), "t-openat".to_string()]),
            vec!["t-gone"]
        );
        assert!(config.unknown(&["nope".to_string()]).len() == 1);
        assert!(config.unknown(&["external".to_string()]).is_empty());
    }

    #[test]
    fn the_shipped_file_keeps_the_internet_out_of_the_default_sweep() {
        // The repository's own `tests.toml`, through the parser the build uses.
        // This is the pin for the sweep's determinism: every test that reaches
        // outside the machine has to be in a group, and the default sweep has to
        // add none, or a red gate stops meaning "the kernel".
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask sits under the repository root");
        let config = TestConfig::load(&root.join(crate::paths::TESTS_CONFIG)).unwrap();

        let default = config.selection(&[], false);
        assert!(
            default.is_empty(),
            "the default sweep adds {default:?}: a plain `cargo xtask test` is \
             supposed to run the ungrouped tests and nothing else"
        );
        for test in ["t-dns", "t-stdresolve", "t-udp", "t-tcp-close"] {
            assert!(
                config.groups_of(test, X86).contains(&"external"),
                "{test} reaches outside the machine and is not in `external`"
            );
            assert!(
                !config.runs(test, &default, X86),
                "{test} would run by default"
            );
        }
        // The three reason-groups are named, so a reader can ask for them.
        for group in ["diagnostic", "external", "slow"] {
            assert!(
                config.group_names(X86).contains(&group),
                "no `{group}` group"
            );
        }
        // The machine group is the second machine's. Two things have to hold or
        // it is not what it looks like: x86_64 must not see the group at all
        // (which is what keeps its members in the default sweep there), and
        // every name in it must really be left out on the machine it names. The
        // loop is deliberately about *whatever the file lists*: the socket
        // family was in this group until the second machine got a stack, and the
        // spawn family until its harness got a `/bin`, and a pin that named those
        // tests would have had to be edited on each of those days - which is the
        // day it should be shrinking. Empty is a legitimate state, so there is no
        // "the group names nothing" assertion here: the group's *presence* is
        // what this checks, and the day a call cannot be served is the day a name
        // comes back.
        assert!(config.group_names(Machine::Aarch64).contains(&"aarch64"));
        assert!(!config.group_names(X86).contains(&"aarch64"));
        for (group, test) in &config.machine_tests(Machine::Aarch64) {
            assert_eq!(*group, "aarch64");
            let on_x86 = config.groups_of(test, X86);
            assert!(
                !on_x86.contains(&"aarch64"),
                "{test} is named as an aarch64 gap and left out of x86_64's sweep with it"
            );
            // A name whose *only* group is this machine's runs in x86_64's
            // default sweep - the property a single global list would have
            // destroyed. One that is also in `external` or `slow` is left out
            // there for that reason instead, so the two cases are told apart
            // rather than asserted away.
            if on_x86.is_empty() {
                assert!(
                    config.runs(test, &default, X86),
                    "{test} stopped running in x86_64's default sweep"
                );
            }
            assert!(
                !config.runs(test, &default, Machine::Aarch64),
                "{test} is named as an aarch64 gap and would still run there"
            );
        }
    }

    #[test]
    fn the_machine_config_reads_its_name_from_the_image() {
        // Two files, one fact between them: `system.hostname` is stored where it
        // is written into the image and read here, so the guest's own name and
        // the name QEMU's DHCP server is told cannot drift apart.
        let dir = std::env::temp_dir().join(format!("mpos-config-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(crate::paths::ROOTFS_CONFIG).parent().unwrap()).unwrap();
        let machine = dir.join(crate::paths::MACHINE_CONFIG);
        let image = dir.join(crate::paths::ROOTFS_CONFIG);
        std::fs::write(
            &machine,
            "[default]\nqemu.memory = \"512\"\nqemu.cpus = 2\n",
        )
        .unwrap();
        std::fs::write(&image, "[default]\nsystem.hostname = \"probe\"\n").unwrap();

        let config = QemuConfig::load(&machine, &image, Arch::X86_64).unwrap();
        assert_eq!(config.memory, "512");
        assert_eq!(config.cpus, 2);
        assert_eq!(config.hostname, "probe");
        // A missing machine file is the default machine, not an error: `test`
        // booting must not need either file to exist.
        let absent = QemuConfig::load(
            &dir.join("nowhere.toml"),
            &dir.join("nowhere.toml"),
            Arch::X86_64,
        );
        assert_eq!(absent.unwrap().hostname, "mpos");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_mode_word_that_was_retired_is_an_error_not_a_default() {
        // `weston` and `compositor` named a mode and now name nothing. What must
        // not happen is the leftover word in someone's `machine.toml` being
        // *defaulted* — a defaulted load would bake
        // `console=ttyS0 console=tty0` for a person who asked for a compositor
        // and never notice, which is the one shape of wrong answer this file's
        // whole `run_mode` split exists to refuse.
        assert_eq!(RunMode::parse("weston"), None);
        assert_eq!(RunMode::parse("compositor"), None);
        for word in ["weston", "compositor"] {
            let mut values = HashMap::new();
            values.insert("qemu.mode".to_string(), word.to_string());
            let config = QemuConfig::from_values(&mut values, "mpos".to_string());
            assert!(values.is_empty(), "the `mode` key was not consumed");
            let error = config.run_mode().unwrap_err().to_string();
            assert!(
                error.contains(word) && error.contains("expected terminal, gui or mirror"),
                "{error}"
            );
        }
        // The repository's own machine file is a machine this xtask can boot.
        // Cheap to assert, and it is exactly the file that carried the retired word.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask sits under the repository root");
        let machine = QemuConfig::load(
            &root.join(crate::paths::MACHINE_CONFIG),
            &root.join(crate::paths::ROOTFS_CONFIG),
            Arch::X86_64,
        )
        .expect("machine.toml parses");
        assert_eq!(
            machine.run_mode().unwrap(),
            RunMode::Gui,
            "the standing machine `cargo xtask run` boots"
        );
    }

    #[test]
    fn an_absent_mode_config_defaults_to_mirror() {
        // `[default]` may not name `qemu.mode` at all; the default must be
        // the pre-split behaviour (window + serial mirror), not an error.
        let mut values = HashMap::new();
        let config = QemuConfig::from_values(&mut values, "mpos".to_string());
        assert_eq!(config.run_mode().unwrap(), RunMode::Mirror);
        // A blank value is the same absence, the way a blank `display` is.
        values.insert("qemu.mode".to_string(), "   ".to_string());
        let config = QemuConfig::from_values(&mut values, "mpos".to_string());
        assert_eq!(config.run_mode().unwrap(), RunMode::Mirror);
    }

    #[test]
    fn an_absent_gpu_defaults_to_the_two_dimensional_card() {
        // Every boot before this key had the plain scanout, so an absent `gpu`
        // must keep exactly that machine - and the GL card is opt-in, because
        // it asks the host for a renderer the plain one does not.
        let mut values = HashMap::new();
        let config = QemuConfig::from_values(&mut values, "mpos".to_string());
        assert_eq!(config.gpu, "virtio-gpu-pci");

        // A key written and left empty is a mistake, not a request for a device
        // whose QEMU name is "" - which QEMU would refuse at its own hand, a
        // boot later and a layer away from the typo.
        let mut values = HashMap::new();
        values.insert("qemu.gpu".to_string(), "   ".to_string());
        let config = QemuConfig::from_values(&mut values, "mpos".to_string());
        assert_eq!(config.gpu, "virtio-gpu-pci");

        // A named card is taken as written, and the key is consumed: a value
        // left in the map is a key this parser does not know, which nothing
        // downstream would ever notice.
        let mut values = HashMap::new();
        values.insert("qemu.gpu".to_string(), " virtio-gpu-gl-pci ".to_string());
        let config = QemuConfig::from_values(&mut values, "mpos".to_string());
        assert_eq!(config.gpu, "virtio-gpu-gl-pci");
        assert!(values.is_empty(), "the `gpu` key was not consumed");
    }

    #[test]
    fn joins_an_array_written_across_lines() {
        // `rootfs.toml` writes `extra_packages` this way. A line-by-line reader
        // that only accepts an array closed on its own line stores the opening
        // bracket as the package list, and apk is then asked for a package
        // literally named `[`.
        let values = parse_sectioned_toml(
            "[default]\nrootfs.extra_packages = [\n    \"openrc\",\n    \"curl\",\n]\n",
            Arch::X86_64,
        )
        .unwrap();
        assert_eq!(
            parse_package_list(values["rootfs.extra_packages"].clone()),
            vec!["openrc", "curl"]
        );
    }

    #[test]
    fn reads_a_single_line_array() {
        let values = parse_sectioned_toml(
            "[default]\nrootfs.base_packages = [\"busybox\", \"apk-tools\"]\n",
            Arch::X86_64,
        )
        .unwrap();
        assert_eq!(
            parse_package_list(values["rootfs.base_packages"].clone()),
            vec!["busybox", "apk-tools"]
        );
    }

    #[test]
    fn drops_comments_but_not_hashes_inside_quotes() {
        let values = parse_sectioned_toml(
            "[default]\nrootfs.mirror = \"https://example.invalid/#frag\" # trailing\n",
            Arch::X86_64,
        )
        .unwrap();
        assert_eq!(values["rootfs.mirror"], "https://example.invalid/#frag");
    }

    /// `tests.toml` is read flat: it has no sections, and its keys are test
    /// groups. A `[...]` heading in such a file is decoration, which is what this
    /// pins - the flat view keeps every key it finds and stores the header as
    /// nothing.
    #[test]
    fn the_flat_view_unquotes_scalars_and_keeps_every_key() {
        let values = parse_simple_toml("[rootfs]\narch = 'x86_64'\n\n# a comment\n").unwrap();
        assert_eq!(values["arch"], "x86_64");
        assert!(!values.contains_key("[rootfs]"));
    }

    #[test]
    fn reads_the_user_the_image_logs_in_as() {
        let mut values = parse_sectioned_toml(
            "[default]\nuser.name = \"mpos\"\nuser.groups = [\"wheel\", \"audio\"]\nuser.shell = \"/bin/fish\"\nuser.password = \"hunter2\"\n",
            Arch::X86_64,
        )
        .unwrap();
        let user = UserConfig::from_values(&mut values);
        assert_eq!(user.name, "mpos");
        assert_eq!(user.groups, vec!["wheel", "audio"]);
        assert_eq!(user.shell, "/bin/fish");
        assert_eq!(user.password, "hunter2");
    }

    #[test]
    fn a_config_with_no_user_creates_no_account() {
        let mut values = HashMap::new();
        let user = UserConfig::from_values(&mut values);
        assert!(user.name.is_empty(), "an empty name is the whole answer");
        // The defaults still describe a usable account: `wheel` is what
        // /etc/doas.conf grants escalation to, and /bin/sh is always there.
        assert_eq!(user.groups, vec!["wheel"]);
        assert_eq!(user.shell, "/bin/sh");
    }

    #[test]
    fn reads_the_machine_the_image_boots_on() {
        let mut values = parse_sectioned_toml(
            "[default]\nqemu.memory = \"2048\"\nqemu.cpus = 4\nqemu.extra_args = [\"-enable-kvm\", \"-cpu host\"]\n",
            Arch::X86_64,
        )
        .unwrap();
        let config = QemuConfig::from_values(&mut values, "mpos".to_string());
        assert_eq!(config.cpus, 4);
        assert_eq!(config.extra_args, vec!["-enable-kvm", "-cpu host"]);
        assert_eq!(config.hostname, "mpos");
    }

    #[test]
    fn a_processor_count_that_is_not_a_number_is_one_processor() {
        // The alternative is a build that stops on a typo in a field whose
        // wrong value is also its default.
        let mut values =
            parse_sectioned_toml("[default]\nqemu.cpus = \"four\"\n", Arch::X86_64).unwrap();
        assert_eq!(QemuConfig::from_values(&mut values, "mpos".into()).cpus, 1);
    }

    #[test]
    fn reads_the_disk_and_repository_sections() {
        let mut values = parse_sectioned_toml(
            "[default]\nrootfs.esp_size = \"256M\"\nrootfs.mkfs_options = \"\"\nrootfs.repos = [\"main\"]\n",
            Arch::X86_64,
        )
        .unwrap();
        assert_eq!(
            take_list(&mut values, "rootfs.repos", &["main", "community"]),
            vec!["main"]
        );
        assert_eq!(values["rootfs.esp_size"], "256M");
        assert_eq!(values["rootfs.mkfs_options"], "");
    }

    #[test]
    fn defaults_are_the_ones_the_image_ships_with() {
        // Everything optional, absent: the values a bare file gets are the ones
        // the repository's own `rootfs.toml` spells out, so deleting a line
        // cannot silently change the image.
        let mut values = parse_sectioned_toml("[default]\n", Arch::X86_64).unwrap();
        assert_eq!(
            take_list(&mut values, "rootfs.repos", &["main", "community"]),
            vec!["main", "community"]
        );
        assert_eq!(
            take_list(&mut values, "user.groups", &["wheel"]),
            vec!["wheel"]
        );
        assert!(take_list(&mut values, "qemu.extra_args", &[]).is_empty());
    }
}
