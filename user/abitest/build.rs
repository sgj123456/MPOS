//! Link every test as a static ET_EXEC at a fixed base: the kernel's ELF
//! loader accepts exactly that shape and refuses position-independent or
//! dynamically linked images. `x86_64-unknown-none` already links with
//! rust-lld and no libc start files; these two args place the image where the
//! loader expects one and keep dead sections out of the ~60 small binaries.

fn main() {
    println!("cargo:rustc-link-arg=--image-base=0x400000");
    println!("cargo:rustc-link-arg=--gc-sections");
}
