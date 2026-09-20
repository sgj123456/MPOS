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
    pub base_packages: Vec<String>,
    pub extra_packages: Vec<String>,
}

#[derive(Debug)]
pub struct RootfsConfig {
    pub mirror: String,
    pub branch: String,
    pub arch: String,
    pub filesystem: String,
    pub root_device: String,
    pub serial_port: String,
    pub image_size: String,
    pub base_packages: Vec<String>,
    pub extra_packages: Vec<String>,
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
            base_packages: parse_package_list(
                values
                    .remove("base_packages")
                    .unwrap_or_else(|| "busybox,alpine-baselayout,apk-tools".to_string()),
            ),
            extra_packages: parse_package_list(values.remove("extra_packages").unwrap_or_default()),
        })
    }
}

impl RootfsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read the rootfs config from {}", path.display()))?;
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
            base_packages: parse_package_list(
                values
                    .remove("base_packages")
                    .unwrap_or_else(|| "alpine-base,e2fsprogs,dosfstools".to_string()),
            ),
            extra_packages: parse_package_list(
                values.remove("extra_packages").unwrap_or_else(|| {
                    "openrc,alpine-release,ssl_client,ca-certificates,apk-tools,fastfetch,strace,gcc,gcompat,curl"
                        .to_string()
                }),
            ),
        })
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
}
