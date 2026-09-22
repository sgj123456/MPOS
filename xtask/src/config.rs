//! The two TOML files the build reads: `initramfs.toml` and `rootfs.toml`.
//!
//! The files use a small, fixed subset - scalars and one array per key, some of
//! them written across several lines - so this is a parser for that subset and
//! not a TOML library. Section headers are skipped: the keys this crate reads
//! are unique across the sections the files use.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

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
    /// `hostname` the rootfs config writes into the image: one machine has one
    /// name, and the file has one key for it.
    pub hostname: String,
    /// Passed through to `qemu-system-x86_64` verbatim, after everything this
    /// crate computes. The escape hatch for the one option a run needs and
    /// this file does not name.
    pub extra_args: Vec<String>,
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
    /// The machine `run` and `test` boot on, read out of `rootfs.toml`.
    ///
    /// A missing file is the defaults rather than an error: `test` builds no
    /// image, and a kernel test run must not need the file that describes one.
    pub fn load(path: &Path) -> Result<Self> {
        let Ok(content) = std::fs::read_to_string(path) else {
            return Ok(Self::from_values(&mut HashMap::new(), default_hostname()));
        };
        let mut values = parse_simple_toml(&content)?;
        let hostname = values.remove("hostname").unwrap_or_else(default_hostname);
        Ok(Self::from_values(&mut values, hostname))
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
        }
    }
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
