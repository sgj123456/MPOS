//! ls: the names in one directory, one per line, dots skipped.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process;

fn main() {
    let mut args = env::args_os();
    let _prog = args.next();
    let target = args.next().unwrap_or_else(|| ".".into());
    let target = Path::new(&target);

    if let Err(e) = run(target) {
        eprintln!("ls: {}: {}", target.display(), e);
        process::exit(1);
    }
}

fn run(target: &Path) -> io::Result<()> {
    let entries = fs::read_dir(target)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if is_dot(&name) {
            continue;
        }
        write_name(&mut out, &name)?;
    }

    out.flush()
}

/// 跳过 `.` 和 `..`
fn is_dot(name: &OsStr) -> bool {
    name == "." || name == ".."
}

/// 按字节写出名字，保证非 UTF-8 文件名也能正确输出。
#[cfg(unix)]
fn write_name<W: Write>(out: &mut W, name: &OsStr) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    out.write_all(name.as_bytes())?;
    out.write_all(b"\n")
}

#[cfg(not(unix))]
fn write_name<W: Write>(out: &mut W, name: &OsStr) -> io::Result<()> {
    write!(out, "{}\n", name.to_string_lossy())
}
