//! A PCI function's configuration space is as long as the *device* says.
//!
//! Linux publishes a function's `config` file at `dev->cfg_size`, and that
//! number is 256 or 4096. The standard space is mechanism 1's reach; the
//! 4096-byte extended space is what ECAM's one-page-per-function window can
//! address, and a function only gets it when it answers for it:
//!
//!   * a PCIe function, a host bridge and a PCI-X mode 2 bridge are the three
//!     kinds Linux asks (`pci_cfg_space_size`);
//!   * the question is the dword at 0x100: all ones means the machine cannot
//!     reach the extended space (a port-only machine reads that), and the
//!     standard header repeated at every 0x100 boundary means a bridge is
//!     aliasing its low 256 bytes (`pci_ext_cfg_is_aliased`);
//!   * everything else publishes 256 without a question.
//!
//! Measured on a current host first (`.workbuddy/probe_pci_ext.c`, as root):
//! the file's size is 4096 for PCIe functions and host bridges and 256 for an
//! IOMMU, an SMBus controller and an LPC bridge; a 4096-byte file answers at
//! 0x100, 0xfff and 0xff8 (short at the end), and 0x1000 is EOF; a 256-byte
//! file is EOF at 0x100. The same probe found Linux's *other* shape, which
//! this program does not claim: a caller without `CAP_SYS_ADMIN` in the initial
//! user namespace has every config file clamped to its first 64 bytes.
//!
//! The gate's machine carries one PCIe function by construction - an
//! `nvme` controller with no drive, because every device q35 attaches by
//! itself is conventional PCI (measured: none of the fourteen carries
//! capability id 0x10) and a bare `pcie-root-port` answers its 0x100 with the
//! standard header repeated, the alias this program refuses below. That
//! function's 4096 bytes are what the claims ask about; the host-bridge half
//! of the rule is a unit test in `crates/pci`, where a synthetic device can
//! answer.

use mpos_user_tests::Report;
use std::fs::{self, File};
use std::os::unix::fs::FileExt;

const CFG_SIZE: u64 = 256;
const CFG_EXT_SIZE: u64 = 4096;

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Linux's `pci_find_capability` over the 256 bytes already read.
fn find_capability(cfg: &[u8], status: u16, header: u8, id: u8) -> Option<u8> {
    if status & 0x0010 == 0 {
        return None;
    }
    let mut pos = match header {
        0x00 | 0x01 => cfg[0x34] & 0xfc,
        0x02 => cfg[0x14] & 0xfc,
        _ => return None,
    };
    for _ in 0..48 {
        if pos == 0 {
            return None;
        }
        let cap = cfg[pos as usize];
        if cap == id {
            return Some(pos);
        }
        if cap == 0 {
            return None;
        }
        pos = cfg[pos as usize + 1] & 0xfc;
    }
    None
}

/// The `/proc/bus/pci` name of the function whose sysfs name is `0000:00:03.0`.
fn proc_path(sysname: &str) -> Option<String> {
    let mut parts = sysname.split(':');
    parts.next()?;
    let bus = parts.next()?;
    let rest = parts.next()?;
    let (dev, func) = rest.split_once('.')?;
    Some(format!("/proc/bus/pci/{bus}/{dev}.{func}"))
}

fn main() {
    let mut r = Report::new();
    let dir = "/sys/bus/pci/devices";
    let mut names: Vec<String> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => {
            println!("skip: this machine publishes no /sys/bus/pci/devices");
            return;
        }
    };
    names.sort();
    if names.is_empty() {
        println!("skip: this machine has no PCI functions");
        return;
    }

    // A host run as an ordinary user is clamped to 64 bytes by Linux, which is
    // a different rule from this program's subject; the payload runs as root.
    let first = format!("{dir}/{}/config", names[0]);
    let mut probe = [0u8; 256];
    if File::open(&first)
        .and_then(|file| file.read_at(&mut probe, 0))
        .unwrap_or(0)
        != 256
    {
        println!("skip: this machine's configuration space is not readable through this path");
        return;
    }

    let mut every_size = true;
    let mut any_conventional = false;
    let mut any_extended = false;
    let mut any_pcie = false;
    let mut pcie_all_extended = true;
    let mut conventional_eof = true;
    let mut extended_real = true;
    let mut extended_end = true;
    let mut chains = true;
    let mut proc_agrees = true;

    for name in &names {
        let path = format!("{dir}/{name}/config");
        let Ok(file) = File::open(&path) else {
            every_size = false;
            continue;
        };
        let Ok(meta) = file.metadata() else {
            every_size = false;
            continue;
        };
        let mut cfg = [0u8; 256];
        if file.read_at(&mut cfg, 0).unwrap_or(0) != 256 {
            every_size = false;
            continue;
        }
        let status = u16_at(&cfg, 0x06);
        let header = cfg[0x0e] & 0x7f;
        let class =
            (u32::from(cfg[0x0b]) << 16) | (u32::from(cfg[0x0a]) << 8) | u32::from(cfg[0x09]);

        let pcie = find_capability(&cfg, status, header, 0x10).is_some();
        let host_bridge = (class >> 8) == 0x0600;
        let pcix_mode_2 = match find_capability(&cfg, status, header, 0x07) {
            Some(pos) if u64::from(pos) + 8 <= CFG_SIZE => {
                let mut word = [0u8; 4];
                file.read_at(&mut word, u64::from(pos) + 4).unwrap_or(0) == 4
                    && u32::from_le_bytes(word) & 0x3 != 0
            }
            _ => false,
        };

        // The kinds Linux asks can publish either length; a PCIe function on a
        // window machine publishes 4096, and everything else is 256 without a
        // question. (A host bridge or PCI-X bridge whose 0x100 read fails
        // stays 256, which is why those two admit both answers.)
        if pcie {
            any_pcie = true;
            if meta.len() != CFG_EXT_SIZE {
                pcie_all_extended = false;
                every_size = false;
            }
        } else if !host_bridge && !pcix_mode_2 && meta.len() != CFG_SIZE {
            every_size = false;
        }

        match meta.len() {
            CFG_SIZE => {
                any_conventional = true;
                let mut one = [0u8; 1];
                if file.read_at(&mut one, 0x100).unwrap_or(1) != 0 {
                    conventional_eof = false;
                }
            }
            CFG_EXT_SIZE => {
                any_extended = true;
                // The answer at 0x100 is a real extended header (or zero, a
                // device with no extended capability), not a read that never
                // landed, and the extended space is not the standard header
                // repeated - the two conditions Linux's probe applies.
                let mut word = [0u8; 4];
                if file.read_at(&mut word, 0x100).unwrap_or(0) != 4 {
                    extended_real = false;
                } else if u32::from_le_bytes(word) == u32::MAX {
                    extended_real = false;
                }
                let header = u32_at(&cfg, 0);
                let mut at = 0x100u64;
                while at < CFG_EXT_SIZE {
                    let mut word = [0u8; 4];
                    if file.read_at(&mut word, at).unwrap_or(0) != 4 {
                        extended_real = false;
                        break;
                    }
                    if u32::from_le_bytes(word) != header {
                        break;
                    }
                    at += CFG_SIZE;
                }
                if at >= CFG_EXT_SIZE {
                    extended_real = false;
                }

                // The last byte is reachable, the first past the end is EOF,
                // and a read that crosses the end stops at it.
                let mut one = [0u8; 1];
                if file.read_at(&mut one, 0xfff).unwrap_or(0) != 1
                    || file.read_at(&mut one, 0x1000).unwrap_or(1) != 0
                {
                    extended_end = false;
                }
                let mut tail = [0u8; 16];
                if file.read_at(&mut tail, 0xff8).unwrap_or(0) != 8 {
                    extended_end = false;
                }

                // An extended capability chain, when the device has one, is
                // dword-aligned headers inside 0x100..0x1000, version 1, each
                // pointing forward, and id 0 ends it.
                let mut at = 0x100u64;
                for _ in 0..32 {
                    let mut word = [0u8; 4];
                    if file.read_at(&mut word, at).unwrap_or(0) != 4 {
                        chains = false;
                        break;
                    }
                    let header = u32::from_le_bytes(word);
                    let id = header & 0xffff;
                    if id == 0 {
                        break;
                    }
                    if (header >> 16) & 0xf != 1 {
                        chains = false;
                        break;
                    }
                    let next = u64::from((header >> 20) & 0xfff) * 4;
                    if next == 0 {
                        break;
                    }
                    if next < 0x100 || next >= CFG_EXT_SIZE || next <= at {
                        chains = false;
                        break;
                    }
                    at = next;
                }

                // The other tree publishes the same device: same length, same
                // extended bytes.
                let Some(proc) = proc_path(name) else {
                    proc_agrees = false;
                    continue;
                };
                let Ok(proc_file) = File::open(&proc) else {
                    proc_agrees = false;
                    continue;
                };
                if proc_file.metadata().map(|m| m.len()).unwrap_or(0) != CFG_EXT_SIZE {
                    proc_agrees = false;
                    continue;
                }
                let mut here = [0u8; 64];
                let mut there = [0u8; 64];
                if file.read_at(&mut here, 0x100).unwrap_or(0) != 64
                    || proc_file.read_at(&mut there, 0x100).unwrap_or(0) != 64
                    || here != there
                {
                    proc_agrees = false;
                }
            }
            _ => every_size = false,
        }
    }

    r.check(
        &format!(
            "{names_len} config attributes are as long as their device says, 256 or 4096",
            names_len = names.len()
        ),
        every_size,
    );
    if any_conventional {
        r.check(
            "a conventional function's configuration space ends at 256 bytes",
            conventional_eof,
        );
    } else {
        println!("skip: this machine has no conventional PCI function");
    }
    if any_extended {
        if any_pcie {
            r.check(
                "a PCIe function publishes the 4096-byte extended space",
                pcie_all_extended,
            );
        } else {
            println!("skip: this machine has extended space but no PCIe function");
        }
        r.check(
            "the extended space is a real answer and not the standard header repeated",
            extended_real,
        );
        r.check(
            "an extended function's space answers at 0xfff and is EOF at 0x1000",
            extended_end,
        );
        r.check(
            "the extended capability chain is one, or the device has none",
            chains,
        );
        r.check(
            "/proc/bus/pci publishes the same length and the same extended bytes",
            proc_agrees,
        );
    } else {
        println!("skip: this machine has no function with extended space");
    }

    r.finish();
}
