//! Fill a FAT32 image from a directory tree, with the build's own FAT32 writer.
//!
//! A second entry point rather than a subcommand of `xtask`, because
//! `Commands` is `Copy` and threaded through the whole dispatch - a variant
//! carrying two paths would ripple through nine call sites to serve one
//! developer tool. This binary reaches the same module by path, so there is one
//! FAT32 writer and not two.
//!
//! Why it exists at all: the *installed* disk image is the only place a newer
//! kernel can be measured. `cargo xtask run` needs root to refresh that image,
//! the loader embeds the kernel, and `mtools` is not a dependency this build
//! assumes - so refreshing the ESP had no privileged-free path. That gap has
//! cost this repository a whole counterfactual once (`docs/notes/`
//! `07-chromium-analysis.md` §7.9v: a fixed kernel measured twice as broken,
//! because the image still carried the old one).
//!
//! Usage: `fat-populate <image.fat> <dir>`

#[path = "../fat.rs"]
mod fat;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let (Some(image), Some(dir)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: fat-populate <image.fat> <dir>");
    };
    fat::populate(std::path::Path::new(&image), std::path::Path::new(&dir))
}
