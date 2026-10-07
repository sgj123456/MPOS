//! The two TOML files the build reads: `initramfs.toml` and `rootfs.toml`.
//!
//! The files use a small, fixed subset - scalars and one array per key, some of
//! them written across several lines - so this is a parser for that subset and
//! not a TOML library. Section headers are skipped: the keys this crate reads
//! are unique across the sections the files use.

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
    /// Parse the `[qemu] mode` value / `--mode` argument. Case- and
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

    /// The word this mode is written down with: in the `[qemu] mode` value, in
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
    /// where the image is told it (`[system] hostname`).
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
    /// compositor refusing a *software* EGL device able to run (`PLAN.md`
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
    /// The word `[qemu] mode` holds, kept as text: see
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
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).with_context(|| {
            format!(
                "Failed to read the initramfs config from {}",
                path.display()
            )
        })?;
        let mut values = parse_simple_toml(&content)?;

        Ok(Self {
            mirror: values
                .remove("mirror")
                .unwrap_or_else(|| "https://mirrors.tuna.tsinghua.edu.cn/alpine".to_string()),
            branch: values
                .remove("branch")
                .unwrap_or_else(|| "latest-stable".to_string()),
            arch: values
                .remove("arch")
                .unwrap_or_else(|| "x86_64".to_string()),
            repos: take_list(&mut values, "repos", &["main", "community"]),
            base_packages: parse_package_list(
                values
                    .remove("base_packages")
                    .unwrap_or_else(|| "busybox,alpine-baselayout,apk-tools".to_string()),
            ),
            extra_packages: parse_package_list(values.remove("extra_packages").unwrap_or_default()),
        })
    }
}

impl QemuConfig {
    /// The machine `run` and `test` boot on: `[qemu]` in `machine.toml`, plus
    /// the image's own hostname.
    ///
    /// The machine half lives in its own file because `rootfs.toml`'s mtime is
    /// what decides whether the Alpine tree is reinstalled
    /// (`rootfs::needs_full_install`), and a memory size has no business
    /// triggering one. What is read from `rootfs.toml` is one key - `[system]
    /// hostname`, which is also the name QEMU's DHCP server is told - and it is
    /// read rather than repeated because the guest's name is written into the
    /// image, so that is where it lives. One fact, one home, two readers.
    ///
    /// A missing file is the defaults rather than an error: `test` builds no
    /// image, and a kernel test run must not need the files that describe one.
    /// A missing `machine.toml` therefore gets the default machine, and a
    /// missing `rootfs.toml` the default name.
    pub fn load(machine_path: &Path, rootfs_path: &Path, arch: Arch) -> Result<Self> {
        let mut values = match std::fs::read_to_string(machine_path) {
            Ok(content) => parse_simple_toml(&content)?,
            Err(_) => HashMap::new(),
        };
        let hostname = std::fs::read_to_string(rootfs_path)
            .ok()
            .and_then(|content| parse_simple_toml(&content).ok())
            .and_then(|mut image| image.remove("hostname"))
            .unwrap_or_else(default_hostname);
        Ok(Self::from_values(&mut values, hostname, arch))
    }

    /// Both files resolved from the repository root, which is what every caller
    /// wants: `machine.toml` for the machine, `rootfs.toml` for the name.
    pub fn load_default(arch: Arch) -> Result<Self> {
        let root = crate::utils::project_root()?;
        Self::load(&root.join("machine.toml"), &root.join("rootfs.toml"), arch)
    }

    /// The values `arch` reads: the plain keys, with that machine's own
    /// override winning.
    ///
    /// `machine.toml` carries one set of keys for the machine an image is booted
    /// on, and the two machines do not want the same values for all of them - the
    /// second is booted with less memory and never with a window, and its vsock
    /// device is conditional on the host. Rather than a second file (whose
    /// staleness would have to be tracked, and which would split "what the
    /// machine is booted on" across two homes) or a nested section (the parser is
    /// deliberately flat: keys are unique across the headers these files use), the
    /// machine's own value is written beside the shared one with the machine's
    /// name in front of it:
    ///
    /// ```toml
    /// [qemu]
    /// memory = "2048"
    /// aarch64_memory = "256"
    /// ```
    ///
    /// A key with no override is simply the shared one - which is the state a
    /// machine keeps where the two agree, so the file says only what actually
    /// differs.
    fn from_values(values: &mut HashMap<String, String>, hostname: String, arch: Arch) -> Self {
        // `MEM` still wins over everything: it is what a one-off `MEM=2048 cargo
        // xtask run` reaches for, and the file is the standing answer.
        let memory = std::env::var("MEM")
            .ok()
            .or_else(|| take_for(values, "memory", arch));
        Self {
            memory: memory.unwrap_or_else(|| "1024".to_string()),
            cpus: take_for(values, "cpus", arch)
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(1),
            hostname,
            extra_args: take_list_for(values, "extra_args", arch),
            vsock: take_flag_for(values, "vsock", arch),
            audio: take_flag_for(values, "audio", arch),
            display: take_for(values, "display", arch).filter(|text| !text.trim().is_empty()),
            // The 2D scanout is the default because it is what every boot
            // before this key had, and because it is the one that needs nothing
            // of the host. A blank value is the same as an absent one: a key
            // written and left empty is a mistake, not a request for a device
            // named "".
            gpu: take_for(values, "gpu", arch)
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "virtio-gpu-pci".to_string()),
            // The word is stored rather than resolved here, because the load is
            // shared with `test` — which boots no window at all and must not fail
            // on a mode name it never uses. The mode is resolved at the two places
            // that actually boot a window, in [`Self::run_mode`].
            mode_text: take_for(values, "mode", arch),
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

/// The prefix a machine's own `[qemu]` values carry.
///
/// The first machine's values *are* the plain keys, because the file was written
/// for it, so it has no prefix and every shared key keeps the meaning it already
/// had. The second machine writes `aarch64_memory` beside `memory` for the values
/// it does not share, which is what lets one `machine.toml` describe two machines
/// without a second file whose staleness would have to be tracked separately.
fn override_prefix(arch: Arch) -> &'static str {
    match arch {
        Arch::X86_64 => "",
        Arch::Aarch64 => "aarch64_",
    }
}

/// Take `key` for `arch`: its own override if the file has one, else the shared
/// key. Taking it here rather than in each caller is what makes "the machine's
/// value wins" one rule instead of one per key.
fn take_for(values: &mut HashMap<String, String>, key: &str, arch: Arch) -> Option<String> {
    let own = format!("{}{key}", override_prefix(arch));
    if own != key
        && let Some(value) = values.remove(&own)
    {
        return Some(value);
    }
    values.remove(key)
}

fn take_flag_for(values: &mut HashMap<String, String>, key: &str, arch: Arch) -> bool {
    take_for(values, key, arch).is_some_and(|text| text.trim().eq_ignore_ascii_case("true"))
}

fn take_list_for(values: &mut HashMap<String, String>, key: &str, arch: Arch) -> Vec<String> {
    take_for(values, key, arch)
        .map(parse_package_list)
        .unwrap_or_default()
}

fn default_hostname() -> String {
    "mpos".to_string()
}

impl UserConfig {
    fn from_values(values: &mut HashMap<String, String>) -> Self {
        Self {
            name: values.remove("name").unwrap_or_default().trim().to_string(),
            groups: take_list(values, "groups", &["wheel"]),
            shell: values
                .remove("shell")
                .unwrap_or_else(|| "/bin/sh".to_string()),
            password: values.remove("password").unwrap_or_default(),
        }
    }
}

impl RootfsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read the rootfs config from {}", path.display()))?;
        let mut values = parse_simple_toml(&content)?;
        // Taken once, because two structs of the answer want it: `[system]
        // hostname` and the name QEMU's DHCP server is told.
        let hostname = values.remove("hostname").unwrap_or_else(default_hostname);

        Ok(Self {
            mirror: values
                .remove("mirror")
                .unwrap_or_else(|| "https://mirrors.tuna.tsinghua.edu.cn/alpine".to_string()),
            branch: values
                .remove("branch")
                .unwrap_or_else(|| "latest-stable".to_string()),
            arch: values
                .remove("arch")
                .unwrap_or_else(|| "x86_64".to_string()),
            repos: take_list(&mut values, "repos", &["main", "community"]),
            filesystem: values
                .remove("filesystem")
                .unwrap_or_else(|| "ext2".to_string()),
            root_device: values
                .remove("root_device")
                .unwrap_or_else(|| "/dev/vda2".to_string()),
            serial_port: values
                .remove("serial_port")
                .unwrap_or_else(|| "ttyS0".to_string()),
            image_size: values
                .remove("image_size")
                .unwrap_or_else(|| "2G".to_string()),
            esp_size: values
                .remove("esp_size")
                .unwrap_or_else(|| "128M".to_string()),
            // What the driver can honour, spelled as `mke2fs -O` features:
            // no journal (nothing here replays one, and a crash would leave
            // RECOVER set and the image read-only until a Linux fsck ran) and
            // no metadata checksums (every write would leave a stale one).
            mkfs_options: values
                .remove("mkfs_options")
                .unwrap_or_else(|| "^has_journal,^metadata_csum".to_string()),
            hostname: hostname.clone(),
            timezone: values
                .remove("timezone")
                .unwrap_or_else(|| "UTC".to_string()),
            locale: values
                .remove("locale")
                .unwrap_or_else(|| "en_US.UTF-8".to_string()),
            dns: values.remove("dns").filter(|text| !text.trim().is_empty()),
            root_password: values.remove("root_password").unwrap_or_default(),
            // The shared core of both userlands (`rootfs::SHARED_BASE_PACKAGES`)
            // is *added* rather than merely defaulted: the two tomls name their
            // machine's own half, and a package that belongs in the base on both
            // machines must land on both even when one file forgets it - which
            // is exactly the drift (§13.19) this fold makes impossible. A name
            // listed twice installs once; apk dedupes its own arguments.
            base_packages: {
                let mut packages = crate::rootfs::SHARED_BASE_PACKAGES
                    .iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>();
                packages.extend(parse_package_list(
                    values.remove("base_packages").unwrap_or_default(),
                ));
                packages
            },
            extra_packages: parse_package_list(values.remove("extra_packages").unwrap_or_else(
                || "openrc,alpine-release,ssl_client,ca-certificates,apk-tools".to_string(),
            )),
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

/// Reads `key = value` pairs, joining an array written across several lines
/// into one value. Comments are dropped whether they own the whole line or
/// trail a value, because a `#` inside one of the package strings would
/// otherwise be read as one.
fn parse_simple_toml(content: &str) -> Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    let mut lines = content.lines();

    while let Some(raw) = lines.next() {
        let line = strip_comment(raw).trim();
        if line.is_empty() || line.starts_with('[') {
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

        if value.starts_with('[') && value.ends_with(']') {
            values.insert(key, value[1..value.len() - 1].to_string());
        } else {
            values.insert(key, unquote(&value));
        }
    }

    Ok(values)
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

    /// One `machine.toml`, two machines: a value the second does not share is
    /// written with its own name in front, wins over the shared one, and is not
    /// visible to the first - which is what keeps the second machine's
    /// differences in the file that describes machines instead of in the code
    /// that boots them.
    #[test]
    fn a_machines_own_value_wins_and_the_other_machine_never_sees_it() {
        let mut values = parse_simple_toml(
            "memory = \"2048\"\naarch64_memory = \"256\"\naudio = false\naarch64_audio = true\n",
        )
        .unwrap();
        let x86 = QemuConfig::from_values(&mut values.clone(), "probe".to_string(), Arch::X86_64);
        let aarch64 = QemuConfig::from_values(&mut values, "probe".to_string(), Arch::Aarch64);

        assert_eq!(x86.memory, "2048", "the shared key is the first machine's");
        assert_eq!(aarch64.memory, "256", "the second machine's own value wins");
        assert!(!x86.audio, "the second machine's key is not the first's");
        assert!(aarch64.audio);

        // A key with no override is the shared one on both - so the file says
        // only what actually differs.
        assert_eq!(x86.gpu, aarch64.gpu);
        assert_eq!(x86.cpus, aarch64.cpus);
    }

    #[test]
    fn the_machine_and_the_image_keep_their_own_keys() {
        // The repository's own two files, read through the parser the build
        // uses. This is the pin for the split's whole purpose: a machine key
        // that drifts back into `rootfs.toml` starts charging a full package
        // install for a memory size, and nothing else in the tree would notice.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask sits under the repository root");
        let machine = std::fs::read_to_string(root.join("machine.toml"))
            .expect("machine.toml is the machine half of the configuration");
        let image = std::fs::read_to_string(root.join("rootfs.toml")).expect("rootfs.toml");
        let machine_keys = parse_simple_toml(&machine).unwrap();
        let image_keys = parse_simple_toml(&image).unwrap();

        // The second machine's own values, which exist only here. They are
        // required rather than merely allowed: a file that lost them would still
        // boot - the shared keys would answer - but it would boot the first
        // machine's memory and window, which is the drift this test is about
        // (`aarch64_memory = "256"` against the 2048 above).
        for key in [
            "aarch64_memory",
            "aarch64_display",
            "aarch64_vsock",
            "aarch64_audio",
            "aarch64_extra_args",
        ] {
            assert!(machine_keys.contains_key(key), "machine.toml lost {key}");
            assert!(
                !image_keys.contains_key(key),
                "{key} drifted into rootfs.toml, whose mtime reinstalls Alpine"
            );
        }

        // The machine half is all there...
        for key in [
            "memory",
            "cpus",
            "extra_args",
            "vsock",
            "audio",
            "gpu",
            "mode",
        ] {
            assert!(machine_keys.contains_key(key), "machine.toml lost {key}");
        }
        // ...and none of it is in the file whose mtime decides whether Alpine
        // is reinstalled.
        for key in [
            "memory",
            "cpus",
            "extra_args",
            "vsock",
            "audio",
            "display",
            "gpu",
            "mode",
        ] {
            assert!(
                !image_keys.contains_key(key),
                "rootfs.toml names {key}: editing it would reinstall the image"
            );
        }
        // The two facts the machine half reads *through* stay where the image
        // is told them: its own name, and the serial port the console and the
        // getty share.
        assert!(
            image_keys.contains_key("hostname"),
            "the name lives with the image"
        );
        assert!(image_keys.contains_key("serial_port"));
        // And the disk geometry stays there on purpose - changing it re-makes
        // the partitions, which is a rebuild this file should ask for.
        assert!(image_keys.contains_key("image_size") && image_keys.contains_key("esp_size"));
    }

    #[test]
    fn both_machines_base_on_the_shared_core_list() {
        // The pin for §13.19's package convergence: `SHARED_BASE_PACKAGES` is
        // folded into *every* machine's base by the loader itself, so neither
        // toml can forget it and no install path can skip it. What each file
        // names are its own hardware facts - and the two files must not
        // re-name what the core already carries, or the drift returns under a
        // different spelling.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask sits under the repository root");
        let x86 = RootfsConfig::load(&root.join("rootfs.toml")).unwrap();
        let arm = RootfsConfig::load(&root.join("rootfs-aarch64.toml")).unwrap();

        for config in [&x86, &arm] {
            for shared in crate::rootfs::SHARED_BASE_PACKAGES {
                assert!(
                    config.base_packages.iter().any(|p| p == shared),
                    "the base lost {shared} - the fold stopped folding"
                );
            }
        }
        // A name listed twice installs once (apk dedupes its arguments), but
        // the tomls stay honest about who owns what: the core's names appear
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
                "{shared} is named by the core list, not repeated in rootfs-aarch64.toml"
            );
        }
        // The halves that stayed per-machine, with their reasons written in
        // the files themselves: e2fsprogs/dosfstools belong to the guest that
        // makes filesystems; the second machine adds nothing to the core.
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
        let config = TestConfig::load(&root.join("tests.toml")).unwrap();

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
        // Two files, one fact between them: `hostname` is stored where it is
        // written into the image and read here, so the guest's own name and the
        // name QEMU's DHCP server is told cannot drift apart.
        let dir = std::env::temp_dir().join(format!("mpos-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let machine = dir.join("machine.toml");
        let image = dir.join("rootfs.toml");
        std::fs::write(&machine, "[qemu]\nmemory = \"512\"\ncpus = 2\n").unwrap();
        std::fs::write(&image, "[system]\nhostname = \"probe\"\n").unwrap();

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
            values.insert("mode".to_string(), word.to_string());
            let config = QemuConfig::from_values(&mut values, "mpos".to_string(), Arch::X86_64);
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
            &root.join("machine.toml"),
            &root.join("rootfs.toml"),
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
        // The `[qemu]` block may not name `mode` at all; the default must be
        // the pre-split behaviour (window + serial mirror), not an error.
        let mut values = HashMap::new();
        let config = QemuConfig::from_values(&mut values, "mpos".to_string(), Arch::X86_64);
        assert_eq!(config.run_mode().unwrap(), RunMode::Mirror);
        // A blank value is the same absence, the way a blank `display` is.
        values.insert("mode".to_string(), "   ".to_string());
        let config = QemuConfig::from_values(&mut values, "mpos".to_string(), Arch::X86_64);
        assert_eq!(config.run_mode().unwrap(), RunMode::Mirror);
    }

    #[test]
    fn an_absent_gpu_defaults_to_the_two_dimensional_card() {
        // Every boot before this key had the plain scanout, so an absent `gpu`
        // must keep exactly that machine - and the GL card is opt-in, because
        // it asks the host for a renderer the plain one does not.
        let mut values = HashMap::new();
        let config = QemuConfig::from_values(&mut values, "mpos".to_string(), Arch::X86_64);
        assert_eq!(config.gpu, "virtio-gpu-pci");

        // A key written and left empty is a mistake, not a request for a device
        // whose QEMU name is "" - which QEMU would refuse at its own hand, a
        // boot later and a layer away from the typo.
        let mut values = HashMap::new();
        values.insert("gpu".to_string(), "   ".to_string());
        let config = QemuConfig::from_values(&mut values, "mpos".to_string(), Arch::X86_64);
        assert_eq!(config.gpu, "virtio-gpu-pci");

        // A named card is taken as written, and the key is consumed: a value
        // left in the map is a key this parser does not know, which nothing
        // downstream would ever notice.
        let mut values = HashMap::new();
        values.insert("gpu".to_string(), " virtio-gpu-gl-pci ".to_string());
        let config = QemuConfig::from_values(&mut values, "mpos".to_string(), Arch::X86_64);
        assert_eq!(config.gpu, "virtio-gpu-gl-pci");
        assert!(values.is_empty(), "the `gpu` key was not consumed");
    }

    #[test]
    fn joins_an_array_written_across_lines() {
        // `rootfs.toml` writes `extra_packages` this way. A line-by-line reader
        // that only accepts an array closed on its own line stores the opening
        // bracket as the package list, and apk is then asked for a package
        // literally named `[`.
        let values =
            parse_simple_toml("extra_packages = [\n    \"openrc\",\n    \"curl\",\n]\n").unwrap();
        assert_eq!(
            parse_package_list(values["extra_packages"].clone()),
            vec!["openrc", "curl"]
        );
    }

    #[test]
    fn reads_a_single_line_array() {
        let values = parse_simple_toml("base_packages = [\"busybox\", \"apk-tools\"]\n").unwrap();
        assert_eq!(
            parse_package_list(values["base_packages"].clone()),
            vec!["busybox", "apk-tools"]
        );
    }

    #[test]
    fn drops_comments_but_not_hashes_inside_quotes() {
        let values =
            parse_simple_toml("mirror = \"https://example.invalid/#frag\" # trailing\n").unwrap();
        assert_eq!(values["mirror"], "https://example.invalid/#frag");
    }

    #[test]
    fn unquotes_scalars_and_skips_section_headers() {
        let values = parse_simple_toml("[rootfs]\narch = 'x86_64'\n\n# a comment\n").unwrap();
        assert_eq!(values["arch"], "x86_64");
        assert!(!values.contains_key("[rootfs]"));
    }

    #[test]
    fn reads_the_user_the_image_logs_in_as() {
        let mut values = parse_simple_toml(
            "[user]\nname = \"mpos\"\ngroups = [\"wheel\", \"audio\"]\nshell = \"/bin/fish\"\npassword = \"hunter2\"\n",
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
        let mut values = parse_simple_toml(
            "memory = \"2048\"\ncpus = 4\nextra_args = [\"-enable-kvm\", \"-cpu host\"]\n",
        )
        .unwrap();
        let config = QemuConfig::from_values(&mut values, "mpos".to_string(), Arch::X86_64);
        assert_eq!(config.cpus, 4);
        assert_eq!(config.extra_args, vec!["-enable-kvm", "-cpu host"]);
        assert_eq!(config.hostname, "mpos");
    }

    #[test]
    fn a_processor_count_that_is_not_a_number_is_one_processor() {
        // The alternative is a build that stops on a typo in a field whose
        // wrong value is also its default.
        let mut values = parse_simple_toml("cpus = \"four\"\n").unwrap();
        assert_eq!(
            QemuConfig::from_values(&mut values, "mpos".into(), Arch::X86_64).cpus,
            1
        );
    }

    #[test]
    fn reads_the_disk_and_repository_sections() {
        let mut values =
            parse_simple_toml("esp_size = \"256M\"\nmkfs_options = \"\"\nrepos = [\"main\"]\n")
                .unwrap();
        assert_eq!(
            take_list(&mut values, "repos", &["main", "community"]),
            vec!["main"]
        );
        assert_eq!(values["esp_size"], "256M");
        assert_eq!(values["mkfs_options"], "");
    }

    #[test]
    fn defaults_are_the_ones_the_image_ships_with() {
        // Everything optional, absent: the values a bare file gets are the ones
        // the repository's own `rootfs.toml` spells out, so deleting a line
        // cannot silently change the image.
        let mut values = parse_simple_toml("[rootfs]\n").unwrap();
        assert_eq!(
            take_list(&mut values, "repos", &["main", "community"]),
            vec!["main", "community"]
        );
        assert_eq!(take_list(&mut values, "groups", &["wheel"]), vec!["wheel"]);
        assert!(take_list(&mut values, "extra_args", &[]).is_empty());
    }
}
