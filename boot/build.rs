//! Puts the kernel *inside* the loader, which is what lets the boot volume
//! carry one file where it used to carry two.
//!
//! Linux's `bzImage` is the shape being copied, and it is worth being exact
//! about what that shape is: there is still a stub, and there is still a kernel
//! that is not a firmware application. The difference is only where the kernel
//! travels - as *data inside the stub's own image* rather than as a second file
//! the stub has to open. The stub decompresses it and loads it exactly as it
//! loaded a file before; the loader below this script is unchanged in every
//! other respect.
//!
//! This has to be a build script rather than an `include_bytes!` of a path
//! written in the source, because the kernel *is* the input that moves. A path
//! spelled in `lib.rs` would go on naming a kernel cargo has no reason to make
//! this crate depend on: rebuilding the kernel alone would leave the loader
//! holding the previous one, and nothing would say so. A build script can
//! declare the two dependencies the rebuild really has - the variable that
//! names the kernel, and the file's own bytes.
//!
//! The compression uses the *same crate* the loader decompresses with
//! (`miniz_oxide`, in both `[dependencies]` and `[build-dependencies]`): one
//! end's compressor and another project's decompressor is a pair that drifts,
//! and what it drifts into is a boot that dies on a stream it cannot read.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// The compressed payload, as a file in `OUT_DIR` that `lib.rs` `include_bytes!`s.
///
/// A file and not a value in an environment variable: the kernel does not fit
/// in one, and cargo's answer to "the bytes changed" is a file it can stat.
const PAYLOAD: &str = "kernel.deflate";

/// DEFLATE level, 0-10 (10 is miniz's own "uber" setting).
///
/// Paid once per build of the loader and never again - the loader does not
/// re-compress anything - so the trade is all on this side, and the only cost
/// of a slow level is a slower build. 9 is the best of the ordinary levels.
const LEVEL: u8 = 9;

fn main() {
    // Both dependencies, because either one moving has to rebuild this crate:
    // the variable names which kernel a boot is for, and the file behind it is
    // the bytes. Watching only the variable would miss every rebuild of the
    // kernel, which is the one that happens most.
    println!("cargo:rerun-if-env-changed=MPOS_KERNEL_ELF");

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let target = env::var("TARGET").expect("cargo sets TARGET");
    let profile = env::var("PROFILE").expect("cargo sets PROFILE");
    let payload = out.join(PAYLOAD);

    let kernel = kernel_path(&out, &target, &profile).filter(|path| path.exists());
    let Some(kernel) = kernel else {
        // No kernel to carry. The crate still compiles and links - a loader
        // that cannot be *built* on a tree with no kernel is a lint or a
        // `cargo check` that fails for a reason that is not in the code - but
        // it refuses to boot (`run` checks the payload is not empty), so the
        // mistake is a named refusal rather than a firmware message about a
        // file that is not there.
        fs::write(&payload, b"").expect("Failed to write the empty payload");
        println!(
            "cargo:warning=the loader is being built with no kernel to carry: \
             build the kernel first (`cargo xtask build`) or name it with \
             MPOS_KERNEL_ELF; this loader will refuse to boot"
        );
        return;
    };

    println!("cargo:rerun-if-changed={}", kernel.display());
    let raw = fs::read(&kernel)
        .unwrap_or_else(|e| panic!("Failed to read the kernel at {}: {e}", kernel.display()));
    let packed = miniz_oxide::deflate::compress_to_vec(&raw, LEVEL);
    fs::write(&payload, &packed)
        .unwrap_or_else(|e| panic!("Failed to write {}: {e}", payload.display()));
    // The one place the compression is measured, and the only place that says
    // *which* kernel the loader is carrying. Both matter: the ratio is what a
    // boot pays, and the source is the fact this whole build script exists to
    // get right - a loader carrying the wrong kernel is a boot that runs
    // yesterday's machine, and the symptom (a boot that hangs, or one that runs
    // the production build's tests) does not name the cause.
    println!(
        "cargo:warning=kernel payload: {} KiB -> {} KiB ({}%) from {} ({})",
        raw.len() / 1024,
        packed.len() / 1024,
        packed.len() * 100 / raw.len().max(1),
        kernel.display(),
        if env::var_os("MPOS_KERNEL_ELF").is_some() {
            "MPOS_KERNEL_ELF"
        } else {
            "derived from TARGET"
        }
    );
}

/// The kernel image this build of the loader is to carry.
///
/// `MPOS_KERNEL_ELF` wins, and `xtask` always sets it: it is the caller that
/// knows *which* kernel a boot is for - the ordinary one or the self-test one -
/// and handing over the path is what makes the loader carry the same kernel the
/// boot will run rather than whichever one happens to be in the target
/// directory. The derivation below is the fallback for building this crate on
/// its own, on a tree whose kernel has already been built.
fn kernel_path(out: &Path, target: &str, profile: &str) -> Option<PathBuf> {
    if let Some(named) = env::var_os("MPOS_KERNEL_ELF") {
        return Some(PathBuf::from(named));
    }
    // The loader is built for `<arch>-unknown-uefi` and the kernel for
    // `<arch>-unknown-none`: one machine, two targets as far as rustc is
    // concerned, and that one suffix is the whole of the mapping between them.
    let kernel_target = target.replace("-unknown-uefi", "-unknown-none");
    if kernel_target == target {
        return None;
    }
    // Cargo's build-script output sits somewhere under
    // `<target dir>/<target>/<profile>/build/…/out`, and how many levels sit
    // between `out` and `<target dir>` is cargo's business rather than this
    // script's - it has been two shapes already (`build/<crate>-<hash>/out` and
    // `build/<crate>/<hash>/out`). Counting them is what this did first, and
    // the off-by-one failed *silently*: the derived path simply did not exist,
    // so the script reported "no kernel to carry" on a tree that had one. The
    // directory is found by name instead - the first ancestor called `<target>`
    // is this build's target directory's child, whatever the depth.
    let triple_dir = out
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == target))?;
    let target_dir = triple_dir.parent()?;
    Some(target_dir.join(kernel_target).join(profile).join("mpos"))
}
