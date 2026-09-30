//! The two TOML files the build reads: `initramfs.toml` and `rootfs.toml`.
//!
//! The files use a small, fixed subset - scalars and one array per key, some of
//! them written across several lines - so this is a parser for that subset and
//! not a TOML library. Section headers are skipped: the keys this crate reads
//! are unique across the sections the files use.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

/// How `cargo xtask run` boots the whole machine: one word that decides the
/// QEMU wiring *and* the consoles the guest is told to use, because a machine
/// that shows its console in two places has to have agreed on that in both.
///
/// The guest half is a `console=` list and nothing else (see
/// [`RunMode::cmdline_consoles`]); the window and the serial line are the
/// launcher's, and no mode word ever reaches the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Serial is the whole console, and the only one: `console=ttyS0` on the
    /// guest line, QEMU `-display none` with the serial on stdio.
    Terminal,
    /// The framebuffer window only: `console=tty0`, QEMU `-display {backend}`
    /// with the serial line discarded.
    Gui,
    /// The window plus a best-effort copy out the serial line, so the same
    /// console shows in the window and the terminal at once: `console=ttyS0
    /// console=tty0`, QEMU `-display {backend}` with the serial on stdio. This
    /// is the default.
    Mirror,
    /// The window a rootful Wayland compositor paints, with the kernel console
    /// kept off the framebuffer: `console=ttyS0`, QEMU `-display {backend}`
    /// with the serial on stdio.
    ///
    /// Named for the compositor that runs here, because that is what the mode is
    /// for. The guest is told only where its log goes; `/dev/fb0` and the
    /// scanout reach the compositor through the DRM master it takes at runtime,
    /// not through anything this mode bakes.
    Weston,
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
            "weston" | "compositor" => Some(Self::Weston),
            _ => None,
        }
    }

    /// The consoles this mode asks the guest to use, written the way Linux has
    /// always written them: `console=` words, read by `src/boot/console_list.rs`.
    ///
    /// This is the whole of what a mode tells the kernel. The order carries the
    /// meaning: Linux gives the log to every console on the list and treats the
    /// last as the preferred one, so a wire-then-framebuffer list means the window
    /// takes the console over once the framebuffer is up while the wire keeps a
    /// best-effort copy, and the wire alone stays authoritative and lossless.
    ///
    /// The serial port's name comes from the rootfs config rather than being
    /// spelled here: it is the same name the image's getty runs on, and one
    /// machine has one serial port.
    pub fn cmdline_consoles(self, serial_port: &str) -> String {
        match self {
            Self::Terminal | Self::Weston => format!("console={serial_port}"),
            Self::Gui => "console=tty0".to_string(),
            Self::Mirror => format!("console={serial_port} console=tty0"),
        }
    }

    /// Whether this mode wants a QEMU window at all. Only terminal does not.
    pub fn has_window(self) -> bool {
        !matches!(self, Self::Terminal)
    }

    /// Whether this mode routes the serial line to the user's terminal (stdio),
    /// as opposed to leaving it attached to nothing. Gui drops it.
    pub fn serial_to_stdio(self) -> bool {
        matches!(self, Self::Terminal | Self::Mirror | Self::Weston)
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
            Self::Weston => "weston",
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
    pub display: Option<String>,
    /// The standing mode for `run`, overridable per invocation with `--mode`.
    /// Defaults to [`RunMode::Mirror`] - the window with a
    /// best-effort serial copy - which is what `run` did before the modes split.
    pub mode: RunMode,
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
    pub fn load(machine_path: &Path, rootfs_path: &Path) -> Result<Self> {
        let mut values = match std::fs::read_to_string(machine_path) {
            Ok(content) => parse_simple_toml(&content)?,
            Err(_) => HashMap::new(),
        };
        let hostname = std::fs::read_to_string(rootfs_path)
            .ok()
            .and_then(|content| parse_simple_toml(&content).ok())
            .and_then(|mut image| image.remove("hostname"))
            .unwrap_or_else(default_hostname);
        Ok(Self::from_values(&mut values, hostname))
    }

    /// Both files resolved from the repository root, which is what every caller
    /// wants: `machine.toml` for the machine, `rootfs.toml` for the name.
    pub fn load_default() -> Result<Self> {
        let root = crate::utils::project_root()?;
        Self::load(&root.join("machine.toml"), &root.join("rootfs.toml"))
    }

    fn from_values(values: &mut HashMap<String, String>, hostname: String) -> Self {
        Self {
            // `MEM` still wins: it is what a one-off `MEM=2048 cargo xtask
            // run` reaches for, and the file is the standing answer.
            memory: std::env::var("MEM").unwrap_or_else(|_| {
                values
                    .remove("memory")
                    .unwrap_or_else(|| "1024".to_string())
            }),
            cpus: values
                .remove("cpus")
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(1),
            hostname,
            extra_args: take_list(values, "extra_args", &[]),
            vsock: take_flag(values, "vsock"),
            audio: take_flag(values, "audio"),
            display: values
                .remove("display")
                .filter(|text| !text.trim().is_empty()),
            // A name this crate does not recognise is not an error at load -
            // `test` never boots a window and must not fail on a typo it ignores
            // - so it falls back to the default here. `run --mode` reports an
            // unknown word directly when it parses the flag.
            mode: values
                .remove("mode")
                .and_then(|text| RunMode::parse(&text))
                .unwrap_or(RunMode::Mirror),
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
/// The selection is a pure function of the file and the flags, which is what
/// makes it host-testable - and it is the one piece of this a mistake would make
/// invisible, because a sweep that runs fewer tests than it should prints a
/// smaller number and passes.
#[derive(Debug, Default, Clone)]
pub struct TestConfig {
    /// Groups a plain run adds to the ungrouped tests.
    default: Vec<String>,
    /// `(group, tests)` by group name, so the report is stable and a run cannot
    /// depend on a hash map's iteration order.
    groups: Vec<(String, Vec<String>)>,
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

    /// Whether `test` runs under `selection`.
    pub fn runs(&self, test: &str, selection: &[String]) -> bool {
        let groups = self.groups_of(test);
        groups.is_empty()
            || groups
                .iter()
                .any(|group| selection.iter().any(|asked| asked == group))
    }

    /// The groups `test` is in, by name.
    pub fn groups_of<'a>(&'a self, test: &str) -> Vec<&'a str> {
        self.groups
            .iter()
            .filter(|(_, tests)| tests.iter().any(|name| name == test))
            .map(|(group, _)| group.as_str())
            .collect()
    }

    /// Every group the file defines, for a message that lists the choices.
    pub fn group_names(&self) -> Vec<&str> {
        self.groups.iter().map(|(name, _)| name.as_str()).collect()
    }
}

/// Read a `key = true`-style boolean. Anything but an explicit `true` -
/// absent, empty, any other word - is false: a device that needs the host's
/// cooperation defaults to off, and the file says so in one word.
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
            base_packages: parse_package_list(
                values
                    .remove("base_packages")
                    .unwrap_or_else(|| "alpine-base,e2fsprogs,dosfstools".to_string()),
            ),
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
            ("gui", RunMode::Gui, "console=tty0"),
            // Mirror is two consoles with the framebuffer preferred, which is
            // what the word has always meant: Linux writes the log to every
            // console on the list and the last is the preferred one, so mirror
            // is a list and needs no word of its own.
            ("mirror", RunMode::Mirror, "console=ttyS0 console=tty0"),
            // Weston keeps the log on the wire alone, exactly as terminal does.
            // What makes it a compositor boot is a session claiming the card at
            // runtime, and that is not a fact a command line can carry.
            ("weston", RunMode::Weston, "console=ttyS0"),
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
        // Only terminal drops the window; only gui drops the serial line.
        assert!(!RunMode::Terminal.has_window());
        assert!(RunMode::Mirror.has_window() && RunMode::Gui.has_window());
        assert!(RunMode::Terminal.serial_to_stdio());
        assert!(RunMode::Mirror.serial_to_stdio());
        assert!(!RunMode::Gui.serial_to_stdio());
        // And the list has to agree with that wiring: the one mode that discards
        // the serial line is the one mode whose list does not name it. A mode
        // that says one thing to QEMU and another to the kernel is the failure
        // this whole pairing exists to prevent.
        for mode in [
            RunMode::Terminal,
            RunMode::Gui,
            RunMode::Mirror,
            RunMode::Weston,
        ] {
            assert_eq!(
                mode.serial_to_stdio(),
                mode.cmdline_consoles("ttyS0").contains("ttyS0"),
                "{} disagrees with itself about the serial line",
                mode.name()
            );
        }
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

        // The machine half is all there...
        for key in ["memory", "cpus", "extra_args", "vsock", "audio", "mode"] {
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
        assert!(config.runs("t-openat", &config.selection(&[], false)));
        assert!(!config.runs("t-dns", &config.selection(&[], false)));
        assert!(config.runs("t-dns", &config.selection(&["external".into()], false)));
        // A test in two groups runs when *either* is asked for.
        let both = groups("external = [\"t-dns\"]\nslow = [\"t-dns\", \"t-pipecost\"]\n");
        assert!(both.runs("t-dns", &both.selection(&["slow".into()], false)));
        assert!(!both.runs("t-pipecost", &both.selection(&["external".into()], false)));
        assert_eq!(both.groups_of("t-dns"), vec!["external", "slow"]);
    }

    #[test]
    fn the_flags_replace_or_extend_the_files_default() {
        let config =
            groups("default = [\"slow\"]\nslow = [\"t-pipecost\"]\nexternal = [\"t-dns\"]\n");
        // No flag: the file's `default`.
        assert!(config.runs("t-pipecost", &config.selection(&[], false)));
        assert!(!config.runs("t-dns", &config.selection(&[], false)));
        // A named group *replaces* it rather than adding to it - otherwise a
        // second `--groups` on the command line would be unable to narrow.
        assert!(!config.runs("t-pipecost", &config.selection(&["external".into()], false)));
        assert!(config.runs("t-dns", &config.selection(&["external".into()], false)));
        // Every group, for the run that wants everything.
        let all = config.selection(&[], true);
        assert!(config.runs("t-dns", &all) && config.runs("t-pipecost", &all));
        assert!(config.runs("t-openat", &all), "the ungrouped ones run too");
    }

    #[test]
    fn a_missing_file_runs_everything() {
        // The dangerous default is the other one: a sweep that silently skips
        // programs because a file went missing reports success for a machine it
        // never checked.
        let config = TestConfig::load(Path::new("/nonexistent/tests.toml")).unwrap();
        assert!(config.runs("t-openat", &config.selection(&[], false)));
        assert!(config.group_names().is_empty());
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
                config.groups_of(test).contains(&"external"),
                "{test} reaches outside the machine and is not in `external`"
            );
            assert!(!config.runs(test, &default), "{test} would run by default");
        }
        // The three groups are named, so a reader can ask for them.
        for group in ["diagnostic", "external", "slow"] {
            assert!(config.group_names().contains(&group), "no `{group}` group");
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

        let config = QemuConfig::load(&machine, &image).unwrap();
        assert_eq!(config.memory, "512");
        assert_eq!(config.cpus, 2);
        assert_eq!(config.hostname, "probe");
        // A missing machine file is the default machine, not an error: `test`
        // booting must not need either file to exist.
        let absent = QemuConfig::load(&dir.join("nowhere.toml"), &dir.join("nowhere.toml"));
        assert_eq!(absent.unwrap().hostname, "mpos");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_pre_rename_spellings_still_read_as_the_same_modes() {
        // `compositor` was the mode's name before the option became `--mode`, and
        // an image baked then records it in `build/console.mode`. Reading that as
        // "an unknown mode, so default to mirror" would tell `run` the image
        // carries the wrong consoles and send it through a root rebuild of the
        // boot files for no reason - so the old word must land on the same
        // variant.
        assert_eq!(RunMode::parse("compositor"), Some(RunMode::Weston));
        let mut values = HashMap::new();
        values.insert("mode".to_string(), "weston".to_string());
        let config = QemuConfig::from_values(&mut values, "mpos".to_string());
        assert_eq!(config.mode, RunMode::Weston);
        assert!(values.is_empty(), "the `mode` key was not consumed");
    }

    #[test]
    fn an_absent_mode_config_defaults_to_mirror() {
        // The `[qemu]` block may not name `mode` at all; the default must be
        // the pre-split behaviour (window + serial mirror), not an error.
        let mut values = HashMap::new();
        let config = QemuConfig::from_values(&mut values, "mpos".to_string());
        assert_eq!(config.mode, RunMode::Mirror);
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
        let config = QemuConfig::from_values(&mut values, "mpos".to_string());
        assert_eq!(config.cpus, 4);
        assert_eq!(config.extra_args, vec!["-enable-kvm", "-cpu host"]);
        assert_eq!(config.hostname, "mpos");
    }

    #[test]
    fn a_processor_count_that_is_not_a_number_is_one_processor() {
        // The alternative is a build that stops on a typo in a field whose
        // wrong value is also its default.
        let mut values = parse_simple_toml("cpus = \"four\"\n").unwrap();
        assert_eq!(QemuConfig::from_values(&mut values, "mpos".into()).cpus, 1);
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
