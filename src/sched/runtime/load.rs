use super::BOOT_SLOT;
use super::INIT_PID;
use super::SCHED_ID;
use super::STARTED;
use super::USER_STACK_PAGES;
use super::child_exit::write_user_of_at;
use super::child_exit::{drop_retired, task_entry};
use super::child_mm::Mm;
use super::child_signal::current_identity;
use super::procfs::current_cwd;
use super::task::{
    Identity, Layout, Redirect, SpawnError, Stack, TIMER_SLOTS, Task, Tracing, current, new_layout,
    scheduler_loop, with_table,
};
use crate::abi::default_rlimits;
use crate::hal;
use crate::hal::AddressSpace;
use crate::mm::elf::{self, Image};
use crate::mm::vm::{MapFlags, Mapping};
use crate::mm::vma::{MappedRange, merge_ranges};
use crate::mm::{self, PAGE_SIZE, pframe};
use crate::sched::policy::{ARGS_BYTES_MAX, ARGV_MAX};
use crate::sched::policy::{Priority, TaskId};
use crate::sched::signal;
use crate::vfs::file::FdTable;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

const AUXV_WORDS: usize = 32; // 16 (type, value) pairs, AT_NULL included.

pub(crate) fn initial_stack_rsp(
    argv: &[String],
    env: &[String],
    exe: &str,
    stack_top: usize,
) -> Option<usize> {
    let arg_bytes: usize = argv.iter().map(|s| s.len() + 1).sum();
    let env_bytes: usize = env.iter().map(|s| s.len() + 1).sum();
    // The AT_EXECFN string: its own copy when the caller knows the path,
    // nothing extra when argv[0] can serve, a lone NUL when neither exists.
    let execfn_bytes: usize = if !exe.is_empty() {
        exe.len() + 1
    } else if argv.is_empty() {
        1
    } else {
        0
    };
    if argv.len() > ARGV_MAX || arg_bytes + env_bytes + execfn_bytes > ARGS_BYTES_MAX {
        return None;
    }
    let argc = argv.len();
    let envc = env.len();
    // argc, argv[0..argc] + NULL, envp[0..envc] + NULL, auxv.
    let words_bytes = (1 + argc + 1 + envc + 1 + AUXV_WORDS) * size_of::<usize>();
    // 16 bytes of random data for the AT_RANDOM auxv entry.
    let random_bytes = 16usize;
    let total = words_bytes + random_bytes + arg_bytes + env_bytes + execfn_bytes;
    let rsp = (stack_top - total) & !15;
    // The block has to land on the pages the loader maps. Everything below
    // them is demand-paged, and the fault that would claim one has nothing to
    // fault on yet: the program has not started. Linux bounds the block by
    // `ARG_MAX` and lets it span pages; this is the same shape at the size of
    // the stack a program starts with.
    (stack_top - rsp <= USER_STACK_PAGES * PAGE_SIZE).then_some(rsp)
}

/// Fill the initial stack page with the argument block [`initial_stack_rsp`]
/// laid out: random bytes and strings at the top, auxv, envp and argv
/// pointers and `argc` below them.
///
/// # Safety
///
/// The frame must back the pages between `loaded.stack_top` and the `user_rsp`
/// [`initial_stack_rsp`] computed from the same `argv` and `env` - the two are
/// one layout, and a block written against a different stack top lands in
/// whichever pages *that* layout mapped.
///
/// `exe` is the execution path the `AT_EXECFN` entry points at, the string a
/// libc derives `progname` from by scanning it for NUL. It must be
/// NUL-terminated somewhere in the block: an `argv`-empty spawn with no path
/// of its own falls back to an empty string, never to the random bytes, or
/// the scan walks off the stack.
pub(crate) fn write_arg_block(
    root: usize,
    user_rsp: usize,
    argv: &[String],
    env: &[String],
    exe: &str,
    loaded: &Loaded,
) {
    // The block is built here and copied to the user stack in one piece. It
    // may span pages - a *program* is not a pathname, and a completion helper
    // handing a subprocess a few kilobytes of script is an ordinary thing for
    // it to do - while the stack's pages are separate frames, so a writer that
    // reaches them through one page's worth of bytes can only place a block
    // that fits in one. The kernel's own memory has no such shape, and
    // [`write_user_of`] resolves a page at a time on the way across.
    //
    // The address space is named by its root rather than taken as "the
    // current one": a task being spawned is not running yet, so its stack is
    // not the stack the kernel is on.
    let total = loaded.stack_top - user_rsp;
    let mut block = alloc::vec![0u8; total];
    let base = user_rsp;

    let argc = argv.len();
    let mut off = 0usize;

    // argc
    block[off..off + 8].copy_from_slice(&(argc as u64).to_le_bytes());
    off += 8;

    // Random bytes at the very top of the stack (16 bytes, 16-byte aligned).
    let random_at = total - 16;
    let random_vaddr = base + random_at;
    // AT_RANDOM must be unique per exec: libc derives the `__stack_chk_guard`
    // (stack-smashing canary) from these bytes, so a constant here is a
    // predictable canary - the whole point of 8.3's stack protection is lost.
    // Draw them from the kernel ChaCha20 pool, freshly seeded, like `getrandom`.
    let mut random = [0u8; 16];
    crate::rand::fill(&mut random);
    block[random_at..random_at + 16].copy_from_slice(&random);

    // Strings, descending from just below the random bytes: argv first, then
    // the environment, then the AT_EXECFN path.
    let mut string_at = random_at;
    let mut argv0_vaddr = 0usize;
    for (i, arg) in argv.iter().enumerate() {
        string_at -= arg.len() + 1;
        block[string_at..string_at + arg.len()].copy_from_slice(arg.as_bytes());
        block[string_at + arg.len()] = 0;
        // argv[i] pointer: virtual address of the string.
        let vaddr = base + string_at;
        if i == 0 {
            argv0_vaddr = vaddr;
        }
        block[off..off + 8].copy_from_slice(&vaddr.to_le_bytes());
        off += 8;
    }
    // NULL argv terminator (zero-filled block).
    off += size_of::<usize>();

    for var in env.iter() {
        string_at -= var.len() + 1;
        block[string_at..string_at + var.len()].copy_from_slice(var.as_bytes());
        block[string_at + var.len()] = 0;
        let vaddr = base + string_at;
        block[off..off + 8].copy_from_slice(&vaddr.to_le_bytes());
        off += 8;
    }
    // NULL envp terminator (zero-filled block).
    off += size_of::<usize>();

    // The AT_EXECFN string: the path the program was executed through when
    // the caller knew it, argv[0] itself when it did not, and an empty
    // string when neither exists - the block must never point the libc's
    // progname scan at unterminated data.
    let execfn_vaddr = if !exe.is_empty() {
        string_at -= exe.len() + 1;
        block[string_at..string_at + exe.len()].copy_from_slice(exe.as_bytes());
        block[string_at + exe.len()] = 0;
        base + string_at
    } else if argv0_vaddr != 0 {
        argv0_vaddr
    } else {
        string_at -= 1;
        block[string_at] = 0;
        base + string_at
    };

    // Auxiliary vector entries.
    auxv(&mut block[off..], loaded, random_vaddr, execfn_vaddr);

    assert!(
        write_user_of_at(root, base, &block, false),
        "the argument block did not fit"
    );
}

/// Write the Linux auxiliary vector into `buf` as (type, value) u64 pairs.
///
/// The ids are the Linux `AT_*` numbers, so a Linux-built libc reads the
/// vector without translation: program headers for TLS setup, page size and
/// clock tick for `sysconf`, the identity fields for `getuid`-style calls,
/// and 16 random bytes at `random_vaddr` for the stack guard. `execfn` is
/// the virtual address of `argv[0]`'s string, what `AT_EXECFN` points at.
///
/// Every address in here is a *placed* one, which is the whole reason the vector
/// is written from [`Loaded`] rather than from the parsed image: `AT_PHDR` and
/// `AT_ENTRY` are how a libc and an interpreter find the program they were
/// handed, and a program moved from the addresses it was linked with is exactly
/// the case where the two differ. `AT_BASE` is the one entry a dynamically linked
/// program cannot do without - it is how the interpreter finds *itself*, since
/// nothing told it where it was put - and zero means the program is static.
fn auxv(buf: &mut [u8], loaded: &Loaded, random_vaddr: usize, execfn: usize) {
    let mut off = 0usize;

    // Helper: write one auxv entry (type, value) and advance.
    macro_rules! entry {
        ($typ:expr, $val:expr) => {{
            let t = $typ as u64;
            let v = $val as u64;
            buf[off..off + 8].copy_from_slice(&t.to_le_bytes());
            buf[off + 8..off + 16].copy_from_slice(&v.to_le_bytes());
            off += 16;
        }};
    }

    entry!(3, loaded.at_phdr); // AT_PHDR
    entry!(4, loaded.phent); // AT_PHENT
    entry!(5, loaded.phnum); // AT_PHNUM
    entry!(6, PAGE_SIZE); // AT_PAGESZ
    entry!(7, loaded.base); // AT_BASE (0 = no dynamic linker)
    entry!(9, loaded.at_entry); // AT_ENTRY
    // The ids are the ones the new image will run with, which after a set-uid
    // exec are not the ones the caller had. libc reads them here rather than
    // asking, and a program that found the wrong answer would take the wrong
    // path through its own privilege handling - musl's `issetugid` is this
    // entry.
    let id = current_identity();
    entry!(11, id.ruid); // AT_UID
    entry!(12, id.euid); // AT_EUID
    entry!(13, id.rgid); // AT_GID
    entry!(14, id.egid); // AT_EGID
    entry!(16, 0usize); // AT_HWCAP
    entry!(17, 100usize); // AT_CLKTCK
    // AT_SECURE: the exec raised an effective id, so libc has to take its
    // hardened path - no `LD_PRELOAD`, no `LD_LIBRARY_PATH`, sanitised locale.
    // Linux computes it the same way, as "did the ids come out different".
    let secure = id.euid != id.ruid || id.egid != id.rgid;
    entry!(23, secure as usize); // AT_SECURE
    entry!(25, random_vaddr); // AT_RANDOM
    entry!(31, execfn); // AT_EXECFN
    entry!(0, 0usize); // AT_NULL
    let _ = off;
}

/// Where the pieces of a loaded program ended up, which is what the initial
/// stack has to describe to the libc that reads it.
///
/// Every address here is a *placed* one: the image's own `entry` and `phdr_addr`
/// are offsets from where it was put, and a libc told the offset instead of the
/// address walks off the end of its own memory looking for its program headers.
/// That is the whole reason this structure exists next to [`Layout`] rather than
/// as a few more arguments - the summing happens once, here, and cannot be
/// forgotten in the four places that read them afterwards.
pub(crate) struct Loaded {
    /// The address control is transferred to. With an interpreter this is the
    /// interpreter's entry point rather than the program's: the interpreter
    /// is what finishes setting the process up, and it finds the program
    /// through `AT_ENTRY`.
    pub(crate) entry: usize,
    /// What `AT_ENTRY` says: the *program's* entry, wherever the program
    /// landed - which is not the address the process starts at when there is an
    /// interpreter, and is the same address when there is not.
    pub(crate) at_entry: usize,
    /// What `AT_PHDR` says: the program header table, wherever it landed. The
    /// libc's TLS setup reads this and walks `phnum` entries of `phent` bytes.
    pub(crate) at_phdr: usize,
    pub(crate) phnum: usize,
    pub(crate) phent: usize,
    /// The interpreter's load address, reported on the initial stack as
    /// `AT_BASE`. Zero when the program is static, which is what the ABI uses
    /// to mean "there is no interpreter".
    pub(crate) base: usize,
    /// The stack top this program was laid out against, repeated here so that
    /// the argument block and the pages under it agree without a third value
    /// having to be threaded between them.
    pub(crate) stack_top: usize,
    /// The first address above the image, where the heap begins.
    pub(crate) brk: usize,
    /// What the loader put where, folded into the runs `/proc/<pid>/maps`
    /// prints: the program's segments, the interpreter's, and the stack. See
    /// [`MappedRange`] for why these are not VMAs.
    pub(crate) ranges: Vec<MappedRange>,
    /// Every frame that was mapped for this process, for the reaper to give
    /// back when it exits.
    pub(crate) frames: Vec<usize>,
}

/// Map every page of `image` into `space` at `base` - file bytes, the zero
/// tail of each segment past them - and record the frames as it goes.
///
/// `base` is the load address: the interpreter's is [`INTERP_BASE`], and a
/// position-independent program's is the address its layout drew. A fixed one
/// keeps zero, because the addresses in its own instructions are the ones it
/// expects to run at.
fn map_image(space: &mut AddressSpace, image: &Image<'_>, base: usize, frames: &mut Vec<usize>) {
    for segment in &image.segments {
        let vaddr = base + segment.vaddr;
        let first = pframe::align_down(vaddr, PAGE_SIZE);
        let limit = pframe::align_up(vaddr + segment.memsz, PAGE_SIZE);
        for page in (first..limit).step_by(PAGE_SIZE) {
            let frame = mm::alloc_frames(0).expect("out of frames for a user page");
            frames.push(frame);
            // Fill through the direct map, not through the mapping: the space
            // being loaded is not the active one, and the direct map is part
            // of the shared kernel half, reachable on whatever CR3 is loaded.
            let dest = unsafe {
                core::slice::from_raw_parts_mut(mm::phys_to_virt(frame) as *mut u8, PAGE_SIZE)
            };
            dest.fill(0);
            // The slice of the file this page carries; everything beyond it up
            // to `memsz` stays zero, which is what .bss is.
            let file_end = vaddr + segment.data.len();
            let from = page.max(vaddr);
            let to = (page + PAGE_SIZE).min(file_end);
            if from < to {
                dest[from - page..to - page]
                    .copy_from_slice(&segment.data[from - vaddr..to - vaddr]);
            }
            let mut flags = MapFlags::READ | MapFlags::USER;
            flags.insert(segment.flags & (MapFlags::WRITE | MapFlags::EXEC));
            space.map(Mapping {
                virt: page,
                phys: frame,
                length: PAGE_SIZE,
                flags,
            });
        }
    }
}

/// Apply `image`'s R_X86_64_RELATIVE relocations for a load address of `base`.
///
/// Only the program gets this treatment; an interpreter relocates itself from
/// `AT_BASE` before it does anything else, and a second pass here would be
/// doing a libc's job with a different idea of where it starts.
///
/// Both halves of the record move with the load address, and the second one is
/// easy to miss: `r_offset` says *where* to write, in the addresses the image
/// was linked with, while `r_addend` is the *pointer* to write - an address
/// too. The relocation's whole meaning is "this word is an address, and here is
/// the one it should have been", so placing the image somewhere else has to
/// move the value as well as the slot. A static-pie loaded at a bias with only
/// `r_offset` shifted ends up with a table of pointers into the layout it was
/// linked for, and the first dereference of one is a fault in the middle of the
/// program's own data.
fn apply_rela(space: &AddressSpace, image: &Image<'_>, base: usize) {
    for &(offset, addend) in &image.rela {
        let addr = base + offset;
        if let Some(phys) = space.phys_for_virt(addr) {
            let ptr = (mm::phys_to_virt(phys & !(PAGE_SIZE - 1)) + (addr % PAGE_SIZE)) as *mut u64;
            unsafe {
                ptr.write((addend + base as i64) as u64);
            }
        }
    }
}

/// The interpreter an image names, read from the filesystem - `None` when it
/// names none.
///
/// A dynamically linked program does not run by itself: the kernel maps the
/// loader it names, hands over `AT_BASE`, `AT_PHDR` and `AT_ENTRY` on the
/// initial stack, and starts the loader instead of the program. Both of the
/// ways a program starts need that - the `execve` that replaces one process
/// with another, and the kernel's own spawn, which starts init and the shell
/// the milestones test with - so the lookup lives here rather than in either
/// of them.
///
/// The bytes come back unparsed because the parsed [`Image`] borrows from
/// them; the caller keeps the bytes alive for as long as it holds the image.
/// The path comes back beside them because `/proc/<pid>/maps` names the
/// interpreter's segments with it, and this is the one place that resolved it.
pub(crate) fn interpreter_bytes(
    image: &Image<'_>,
) -> Result<Option<(Vec<u8>, String)>, SpawnError> {
    let Some(path) = &image.interp else {
        return Ok(None);
    };
    let path = core::str::from_utf8(path).map_err(|_| SpawnError::NotExecutable)?;
    // A missing or unreadable interpreter reads as "the file was not found":
    // the program itself was fine, and what could not be followed is the path
    // it names.
    // The loader is resolved through the *calling task's* root, not the
    // machine's: a dynamically-linked program exec'd inside a `chroot` finds its
    // `PT_INTERP` there. A boot spawn has no chroot, so `current_root` falls back
    // to the machine root and the resolution is exactly as global as it was.
    crate::vfs::read_file_opt(path, Some(current_cwd()))
        .map(|bytes| Some((bytes, String::from(path))))
        .map_err(|_| SpawnError::NoInterpreter)
}

/// Lay out a program - and its interpreter, when it has one - in `space`, map
/// the user stack, and report where everything landed.
///
/// The interpreter is loaded first and at [`INTERP_BASE`]; the program follows
/// at `layout.bias` plus the addresses its own headers name, which is zero for a
/// fixed-address image and a drawn address for a position-independent one.
/// Nothing here tries to link the two together beyond handing over `AT_BASE`,
/// `AT_PHDR` and `AT_ENTRY` on the initial stack: resolving symbols, mapping
/// shared libraries and jumping to the program's entry is the interpreter's
/// whole purpose, and a kernel that did any of it would be guessing at a
/// contract it is not party to.
///
/// That last sentence also decides who relocates. An image with an interpreter
/// is relocated *by* it - `AT_PHDR` is enough for the loader to find the address
/// it was loaded at, which is why the value handed over is the placed one rather
/// than the linked one - so the kernel's own relocation pass runs only for a
/// static image, where nobody else will ever do the job. Doing it for both would
/// add the load bias twice to every pointer the program reads out of its own
/// data.
pub(crate) fn load_user(
    space: &mut AddressSpace,
    image: &Image<'_>,
    interp: Option<&Image<'_>>,
    layout: &Layout,
    program_name: &str,
    interp_name: Option<&str>,
) -> Loaded {
    let mut frames = Vec::new();
    let mut ranges = Vec::new();

    let base = match interp {
        Some(interp) => {
            map_image(space, interp, layout.interp_bias, &mut frames);
            segment_ranges(interp, layout.interp_bias, interp_name, &mut ranges);
            layout.interp_bias
        }
        None => 0,
    };

    map_image(space, image, layout.bias, &mut frames);
    segment_ranges(image, layout.bias, Some(program_name), &mut ranges);
    if interp.is_none() {
        apply_rela(space, image, layout.bias);
    }

    for index in 0..USER_STACK_PAGES {
        let frame = mm::alloc_frames(0).expect("out of frames for the user stack");
        frames.push(frame);
        let dest = unsafe {
            core::slice::from_raw_parts_mut(mm::phys_to_virt(frame) as *mut u8, PAGE_SIZE)
        };
        dest.fill(0);
        space.map(Mapping {
            // Index 0 is the highest page, the one `rsp` starts in and the
            // one the argument block is written into from the top down. The
            // block may reach past it, so the pages below are mapped here
            // too - it is the *stack* the loader lays out, not one page.
            virt: layout.stack_top - (index + 1) * PAGE_SIZE,
            phys: frame,
            length: PAGE_SIZE,
            flags: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
        });
    }
    // The stack is one line in `/proc/<pid>/maps`, and Linux brackets its name
    // because it is not a file: `[stack]`. What is reported is the range the
    // loader mapped, which is the same thing Linux's VMA starts as - the pages
    // a program grows below it are added to the same range as it takes them
    // (`Table::grow_stack`), so the line follows the stack down.
    ranges.push(MappedRange {
        start: layout.stack_top - USER_STACK_PAGES * PAGE_SIZE,
        end: layout.stack_top,
        prot: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
        name: Some(Arc::from("[stack]")),
    });

    // Control goes to the interpreter when there is one, and to the program
    // otherwise. The interpreter's own entry is a vaddr in a shared object,
    // so it only means anything once the base is added.
    let entry = match interp {
        Some(interp) => base + interp.entry,
        None => layout.bias + image.entry,
    };
    let brk_end = image
        .segments
        .iter()
        .map(|segment| segment.vaddr + segment.memsz)
        .max();

    Loaded {
        entry,
        at_entry: layout.bias + image.entry,
        at_phdr: layout.bias + image.phdr_addr,
        phnum: image.phnum,
        phent: image.phent,
        base,
        stack_top: layout.stack_top,
        // The heap starts on the page above the highest thing the image owns -
        // placed addresses, since a program that asked `brk` where it was and got
        // the address it was *linked* for would be told about a region that is
        // not its own.
        brk: brk_end.map_or(0, |end| pframe::align_up(layout.bias + end, PAGE_SIZE)),
        ranges: merge_ranges(ranges),
        frames,
    }
}

/// Record what one image's segments occupy, at the load address they were
/// placed at, so `/proc/<pid>/maps` can report them.
///
/// The page granularity and the permission word are `map_image`'s own: it
/// aligns each segment out to whole pages and maps them `READ | USER` plus
/// whatever the segment asked for. Anything else here would report a range
/// that is not the range a program can reach.
fn segment_ranges(
    image: &Image<'_>,
    base: usize,
    name: Option<&str>,
    ranges: &mut Vec<MappedRange>,
) {
    for segment in &image.segments {
        let vaddr = base + segment.vaddr;
        let mut prot = MapFlags::READ | MapFlags::USER;
        prot.insert(segment.flags & (MapFlags::WRITE | MapFlags::EXEC));
        ranges.push(MappedRange {
            start: pframe::align_down(vaddr, PAGE_SIZE),
            end: pframe::align_up(vaddr + segment.memsz, PAGE_SIZE),
            prot,
            // An empty name is no name: a kernel-started task's `exe` is empty
            // (there is no path it was run by), and a maps line with an empty
            // column is not the shape a reader parses.
            name: name.filter(|name| !name.is_empty()).map(Arc::from),
        });
    }
}
// The bytes and argv of the program a spawn actually runs.
pub fn resolve_script(
    bytes: Vec<u8>,
    mut argv: Vec<String>,
    start: Option<alloc::sync::Arc<dyn crate::vfs::Inode>>,
    exec_path: &str,
) -> Result<(Vec<u8>, Vec<String>), SpawnError> {
    let mut bytes = bytes;
    // The path the file in `bytes` was reached by - `execve`'s own argument for
    // the first file, and each interpreter's name for the next. A script is
    // handed to its interpreter by *this* path, not by the name the caller
    // chose for argv[0]: a `PATH` search finds `/usr/bin/mkfontdir` while the
    // shell still calls it through argv as plain `mkfontdir`, and an
    // interpreter told to open `mkfontdir` looks for it in the working
    // directory and fails on a file that is really elsewhere.
    let mut script_path = String::from(exec_path);
    let mut hops = 0;
    while let Some((interp, argument)) = crate::binfmt::shebang(&bytes) {
        hops += 1;
        if hops > crate::binfmt::SHEBANG_HOPS_MAX {
            // A chain of interpreters that leads back into itself.
            return Err(SpawnError::Loop);
        }
        // The interpreter is the new argv[0]; the resolved path stands in for
        // the name the script was invoked by, and the caller's own arguments
        // follow it.
        let mut rewritten: Vec<String> = Vec::with_capacity(argv.len() + 2);
        rewritten.push(interp.clone());
        if let Some(argument) = argument {
            rewritten.push(argument);
        }
        rewritten.push(core::mem::take(&mut script_path));
        rewritten.extend(argv.iter().skip(1).cloned());
        argv = rewritten;
        script_path = interp.clone();
        let read = match start.as_ref() {
            Some(dir) => crate::vfs::read_file_opt(&script_path, Some(dir.clone())),
            None => crate::vfs::read_file(&script_path),
        };
        bytes = read.map_err(|_| SpawnError::NoInterpreter)?;
    }
    Ok((bytes, argv))
}

pub fn spawn_init(bytes: &[u8], argv: &[String], env: &[String]) -> Result<TaskId, SpawnError> {
    let path = argv.first().cloned().unwrap_or_default();
    // The kernel's own first spawn resolves a shebang against the *machine's*
    // tree, not a task's. `resolve_at` jails an absolute path to
    // `process_root()`, and before init exists that is the boot task's - empty,
    // so it falls back to the global root, which is right for x86_64 (initramfs
    // root, no switch) but stale on this machine: pid 1 is resolved after the
    // root switch has replaced the global root, and a `#!/bin/sh` init would
    // look for its interpreter in the ramfs the switch has just deleted
    // (`/oldroot` in the kernel-pivot era `NOTES.md` §13.3 records) - `NoEntry`,
    // and an init that cannot be spawned ends the machine. The explicit start is
    // the switched tree itself.
    let start = if current() == TaskId(BOOT_SLOT) {
        Some(crate::vfs::global().root_inode())
    } else {
        None
    };
    let (bytes, argv) = resolve_script(bytes.to_vec(), argv.to_vec(), start, &path)?;
    let id = try_spawn_init(&bytes, &argv, env)?;
    assert_eq!(id, INIT_PID, "init did not land on pid 1");
    Ok(id)
}

/// [`spawn_init`] without the pid assertion, so the loader can be exercised
/// before the slot layout is taken as given.
pub fn try_spawn_init(bytes: &[u8], argv: &[String], env: &[String]) -> Result<TaskId, SpawnError> {
    assert!(
        STARTED.load(Ordering::Relaxed),
        "spawn_init() was called before sched::init()"
    );
    let image = elf::parse(bytes).ok_or(SpawnError::NotExecutable)?;
    // Whether init has an interpreter depends on which init it is. The kernel
    // used to start its own program, statically linked, when the initramfs was
    // the root and no filesystem was up to read a loader from. The machine now
    // hands over to the root filesystem's init - and on this userland that is
    // a link to busybox, which is dynamically linked. Both cases are the same
    // call: an image that names no interpreter reads none.
    let interp_file = interpreter_bytes(&image)?;
    let interp = match &interp_file {
        Some((bytes, _)) => Some(elf::parse(bytes).ok_or(SpawnError::NoInterpreter)?),
        None => None,
    };
    let interp_name = interp_file.as_ref().map(|(_, path)| path.as_str());
    // The interpreter first, because the layout has to know about it: the two
    // are stacked against each other rather than drawn independently, so that no
    // draw can drop one on top of the other.
    let layout = new_layout(&image, interp.as_ref());
    let user_rsp =
        initial_stack_rsp(argv, env, "", layout.stack_top).ok_or(SpawnError::ArgsTooBig)?;

    with_table(|t| {
        let slot = INIT_PID.0;
        assert!(
            t.tasks[slot].is_none(),
            "init's slot {slot} is already taken"
        );
        drop_retired(INIT_PID);
        let stack = Stack::new();
        let mut space = AddressSpace::new();
        let loaded = load_user(
            &mut space,
            &image,
            interp.as_ref(),
            &layout,
            "",
            interp_name,
        );
        write_arg_block(space.root(), user_rsp, argv, env, "", &loaded);

        let root = space.root();

        // Safety: as in `create`, the stack was just allocated and nothing else
        // can reach it, so the bootstrap state below its top is ours to write.
        let context = unsafe { hal::bootstrap(stack.top(), task_entry, slot, true) };

        t.tasks[slot] = Some(Task {
            context,
            // A fresh process starts with no semaphore adjustments: the list is
            // per-process and is *not* inherited by a plain `fork` (measured,
            // `docs/notes/` §6.14); a `clone(CLONE_SYSVSEM)` child shares the
            // parent's and gets it copied in by the clone path afterwards.
            sem_undo: None,
            fpu_live: false,
            priority: Priority::NORMAL,
            stack: Some(stack),
            exited: false,
            body: None,
            space: Some(space),
            user: true,
            mm: Mm::with_eager(TaskId(slot), root, loaded.frames, loaded.ranges),
            released: false,
            tgid: TaskId(slot),
            user_entry: Some((loaded.entry, user_rsp)),
            user_resume: None,
            signals: 0,
            siginfo: [signal::SigInfo::ZERO; signal::SIGINFO_SLOTS],
            rtq: signal::RtQueue::new(),
            blocked: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds: Arc::new(FdTable::with_standard(crate::vfs::tty::global())),
            fs: crate::sched::FsStruct::new(crate::abi::DEFAULT_UMASK),
            fs_ns: None,
            parent: None,
            // Init is nobody's child, so its own kind is unobservable; it is
            // what a `fork` child looks like, which is what its own are.
            exit_signal: signal::SIGCHLD,
            // init leads the first process group, so a command started
            // before any job control exists still has a group to land in.
            pgid: INIT_PID,
            // ...and the first session, of which it is the leader: getty's
            // `setsid` splits away from it, and the orphan test reads the
            // difference between init's session and a getty's.
            session: INIT_PID,
            ctty: None,
            // Init starts as root, like everything the kernel itself spawns.
            identity: Identity::default(),
            // ...and in the machine's own namespace, the one a boot begins in.
            // The machine's one UTS namespace (`crates/uts`'s `init` keeps the
            // first). The id is minted here because the pool is the kernel's;
            // only the first call's is worn, and a number nothing wears is the
            // price of a leaf that cannot mint its own.
            uts: crate::uts::init(crate::vfs::ns::mint()),
            // The machine's own PID namespace: the one a boot begins in, whose
            // numbers are the task slots themselves. A task created here is
            // registered nowhere - the init namespace's `task_of` is the
            // identity - so this is the field pair every task starts with and
            // nothing observable changes until something unshares.
            pid_ns: super::root_ns(),
            pid_ns_children: super::root_ns(),
            // The machine's own time namespace, in both slots, the same way:
            // the first program the kernel starts reads the machine's clocks
            // and its children would be born under them.
            time_ns: crate::vfs::ns::machine_time_ns(),
            time_ns_children: crate::vfs::ns::machine_time_ns(),
            // The machine's own user namespace, the identity map: `init` reads
            // every id as the number it is stored under.
            user_ns: crate::vfs::ns::machine_user_ns(),
            cgroup_ns: None,
            // The machine's own network namespace, which is `None`: every socket
            // call resolves to the stack the boot built.
            net_ns: None,
            // And the machine's own IPC namespace: pid 1's tables are the
            // machine's until it unshares.
            ipc_ns: None,
            pdeathsig: 0,
            subreaper: false,
            no_new_privs: false,
            rlimits: default_rlimits(),
            seccomp_strict: false,
            seccomp_filters: Vec::new(),
            oom_score_adj: 0,
            oom_score_adj_min: 0,
            nice: 0,
            brk_base: loaded.brk,
            brk_pages: 0,
            mmap_cursor: layout.mmap_top,
            stack_top: loaded.stack_top,
            exe: alloc::string::String::new(),
            // The one record of how init was started: `/proc/1/cmdline` and
            // `ps` both read it back, and an empty one reads as a process
            // that has no name at all.
            argv: alloc::sync::Arc::new(argv.to_vec()),
            // Init's environment, likewise the one record: `/proc/1/environ`
            // is what a service manager asks before deciding whether it was
            // started with the variables it expects.
            env: alloc::sync::Arc::new(env.to_vec()),
            child_tid: 0,
            tls: 0,
            itimers: [None; 3],
            timers: [None; TIMER_SLOTS],
            trace: Tracing::default(),
        });
        t.sched.push_ready(INIT_PID, Priority::NORMAL);
        Ok(INIT_PID)
    })
}

/// [`spawn_user`] with arguments and a parent: the process form the `spawn`
/// syscall uses. `argv` is the full argument vector, `argv[0]` being the
/// program name as POSIX frames it - it does not have to match `bytes`' path.
/// `env` is the environment vector, laid out on the child's stack the way the
/// Linux ABI does: `envp` follows the argv NULL, and is what a libc's
/// `environ` and Rust's `env::var` read through. `exe` is the path `bytes`
/// was read from, and is what `/proc/self/exe`
/// reads through; spawn sites that know it pass it, kernel-internal starts
/// that have no name for their image pass an empty string.
///
/// The parent link is what `wait` needs: when the task exits, its parent is
/// woken and reads the exit code.
pub fn try_spawn_user(
    bytes: &[u8],
    priority: Priority,
    parent: TaskId,
    argv: &[String],
    env: &[String],
    redirects: &[Redirect],
    exe: &str,
) -> Result<TaskId, SpawnError> {
    assert!(
        STARTED.load(Ordering::Relaxed),
        "spawn_user() was called before sched::init()"
    );
    let image = elf::parse(bytes).ok_or(SpawnError::NotExecutable)?;
    // The interpreter, found the same way `execve` finds it. This used to pass
    // none, on the assumption that only `execve` starts dynamic programs -
    // which held only while every program the kernel started itself was static.
    // Alpine's userland is dynamically linked, so the shell the milestones hand
    // the machine to is one of them, and a dynamic program started at its own
    // entry point runs before its relocations are applied: the first call
    // through the GOT jumps to nothing.
    let interp_file = interpreter_bytes(&image)?;
    let interp = match &interp_file {
        Some((bytes, _)) => Some(elf::parse(bytes).ok_or(SpawnError::NoInterpreter)?),
        None => None,
    };
    let interp_name = interp_file.as_ref().map(|(_, path)| path.as_str());
    let layout = new_layout(&image, interp.as_ref());
    let user_rsp =
        initial_stack_rsp(argv, env, exe, layout.stack_top).ok_or(SpawnError::ArgsTooBig)?;

    with_table(|t| {
        let slot = t.alloc_slot();
        drop_retired(TaskId(slot));
        // Its namespace is where its starter's children go, by reference, and
        // it is registered there and at every level above - which is what lets
        // the starter `wait` for it and find it in `/proc` under its own
        // number. In a machine that never unshares this is the init namespace
        // and the call is the identity.
        let child_pid_ns = alloc::sync::Arc::clone(&t.task(parent).pid_ns_children);
        super::adopt_pid(TaskId(slot), &child_pid_ns);
        // ...and the clocks the same way: the program is born in the namespace
        // its starter was pointing its children at, in both slots, and sealing
        // it is the freeze that stops the offsets being written from here on.
        let child_time_ns = alloc::sync::Arc::clone(&t.task(parent).time_ns_children);
        child_time_ns.freeze();
        let stack = Stack::new();
        let mut space = AddressSpace::new();
        let loaded = load_user(
            &mut space,
            &image,
            interp.as_ref(),
            &layout,
            exe,
            interp_name,
        );
        // Read before the space moves into the task: a thread switches to
        // this root, and the group is what remembers it.
        let root = space.root();

        // The argument block is written last, into the stack this task will
        // land on. It is placed from its *top* down - the strings grow
        // towards the program's entry point - and it may span pages, so the
        // write resolves the stack's frames as it crosses them rather than
        // assuming the block lives in one.
        write_arg_block(space.root(), user_rsp, argv, env, exe, &loaded);

        // Safety: as in `create`, the stack was just allocated and nothing else
        // can reach it, so the bootstrap state below its top is ours to write.
        let context = unsafe { hal::bootstrap(stack.top(), task_entry, slot, true) };

        // A process's descriptors are its own from the first instruction: the
        // standard three over the console, and nothing of anyone else's - not
        // even the starter's, which is why a pipe end has to be handed over
        // explicitly. What it closes is then its business, not the shell's.
        let fds = Arc::new(FdTable::with_standard(crate::vfs::tty::global()));
        // The child's table is fresh, so it inherits no descriptor the starter
        // happened to be holding - a pipe's write end in the starter's own
        // table, say. That is what makes a pipeline's end-of-file arrive: the
        // reader sees the writers close because nobody else ever had one.
        for &(from, to) in redirects {
            let file = match t.task(parent).fds.clone_at(from) {
                Ok(file) => file,
                Err(_) => return Err(SpawnError::BadFd),
            };
            fds.install_at(file, to).map_err(|_| SpawnError::BadFd)?;
        }

        t.tasks[slot] = Some(Task {
            context,
            // A fresh process starts with no semaphore adjustments: the list is
            // per-process and is *not* inherited by a plain `fork` (measured,
            // `docs/notes/` §6.14); a `clone(CLONE_SYSVSEM)` child shares the
            // parent's and gets it copied in by the clone path afterwards.
            sem_undo: None,
            fpu_live: false,
            priority,
            stack: Some(stack),
            exited: false,
            body: None,
            space: Some(space),
            user: true,
            mm: Mm::with_eager(TaskId(slot), root, loaded.frames, loaded.ranges),
            released: false,
            tgid: TaskId(slot),
            user_entry: Some((loaded.entry, user_rsp)),
            user_resume: None,
            signals: 0,
            siginfo: [signal::SigInfo::ZERO; signal::SIGINFO_SLOTS],
            rtq: signal::RtQueue::new(),
            blocked: 0,
            dispositions: signal::Actions::default(),
            alt_stack: None,
            fds,
            // A new program, not a thread: it gets its own handle copied from
            // the starter's, so what it `chdir`s to does not move the process
            // that launched it.
            fs: t.task(parent).fs.copy(),
            fs_ns: t.task(parent).fs_ns.clone(),
            parent: Some(parent),
            // A program the kernel starts is collected by `wait` the way a
            // `fork` child is, so it is born to report `SIGCHLD`.
            exit_signal: signal::SIGCHLD,
            pgid: t.task(parent).pgid,
            session: t.task(parent).session,
            ctty: t.task(parent).ctty,
            identity: t.task(parent).identity.clone(),
            // Inherited, not copied: a program the kernel starts shares the
            // namespace of whatever started it until it unshares, which is the
            // rule `fork` follows for the same reason.
            uts: Arc::clone(&t.task(parent).uts),
            pid_ns: alloc::sync::Arc::clone(&child_pid_ns),
            pid_ns_children: child_pid_ns,
            time_ns: alloc::sync::Arc::clone(&child_time_ns),
            time_ns_children: child_time_ns,
            // A user namespace has no children's slot, so there is one value to
            // inherit rather than two: the child reads its ids through the
            // namespace of whatever started it until it unshares.
            user_ns: alloc::sync::Arc::clone(&t.task(parent).user_ns),
            cgroup_ns: None,
            // The starter's network namespace, by reference: a program the kernel
            // starts shares the stack of whatever started it, which is the rule
            // `fork` follows.
            net_ns: t.task(parent).net_ns,
            // The starter's IPC namespace, by reference: the objects a program
            // is started with are the tables it was started from, which is the
            // rule `fork` follows too.
            ipc_ns: t.task(parent).ipc_ns.clone(),
            // `execve` keeps the setting - measured - so the new image starts
            // with whatever the process had asked for.
            pdeathsig: t.task(parent).pdeathsig,
            // ...and the subreaper flag too: measured, `execve` preserves it.
            subreaper: t.task(parent).subreaper,
            // `no_new_privs` is one-way and survives `exec` - the whole point of
            // it is that the image cannot have its rights back.
            no_new_privs: t.task(parent).no_new_privs,
            oom_score_adj: t.task(parent).oom_score_adj,
            oom_score_adj_min: t.task(parent).oom_score_adj_min,
            nice: t.task(parent).nice,
            rlimits: t.task(parent).rlimits,
            seccomp_strict: t.task(parent).seccomp_strict,
            // A filter is inherited, not shared: the child gets its own copy of
            // the list, so a filter installed *after* the fork does not appear
            // in the child, and the parent cannot add to the child's.
            seccomp_filters: t.task(parent).seccomp_filters.clone(),
            brk_base: loaded.brk,
            brk_pages: 0,
            mmap_cursor: layout.mmap_top,
            stack_top: loaded.stack_top,
            exe: alloc::string::String::from(exe),
            argv: alloc::sync::Arc::new(argv.to_vec()),
            env: alloc::sync::Arc::new(env.to_vec()),
            child_tid: 0,
            tls: 0,
            itimers: [None; 3],
            timers: [None; TIMER_SLOTS],
            trace: Tracing::default(),
        });
        let id = TaskId(slot);
        t.sched.push_ready(id, priority);
        // A parent waiting for a child wakes here, in the same critical
        // section that made the condition true.
        if t.tasks.get(parent.0).and_then(Option::as_ref).is_some() {
            t.sched.wake(parent);
        }
        // The console's interrupt belongs to the program now, not to the shell
        // that started it: a shell which has spawned a command is waiting, and
        // a Ctrl-C while it waits is aimed at the command.
        let tty = crate::vfs::tty::global();
        // A new task joins the foreground as part of its group, which is the
        // group it inherited - so for the ordinary case this changes nothing
        // and only records that the group is the thing that matters.
        if tty.is_foreground(parent) {
            let pgid = t.task(id).pgid;
            tty.set_foreground(Some(pgid));
        }
        Ok(id)
    })
}

pub fn init() {
    assert!(
        !STARTED.load(Ordering::Relaxed),
        "the scheduler is already running"
    );

    with_table(|t| {
        t.tasks[BOOT_SLOT] = Some(Task::adopted());
        t.current = TaskId(BOOT_SLOT);

        // The scheduler is a task like any other except that its body never
        // returns and it is entered by switching to it rather than by being
        // picked. It starts with interrupts off and stays that way: the only
        // place it enables them is the idle halt.
        //
        // Its slot is named rather than allocated, because it must never take
        // the slot init is going to be born in.
        let id = t.create_at(SCHED_ID.0, scheduler_loop, Priority::CRITICAL, false);
        assert_eq!(
            id, SCHED_ID,
            "the scheduler did not land in its reserved slot"
        );
    });

    STARTED.store(true, Ordering::Relaxed);
}

/// Start a new task running `body` at `priority`. It runs the next time the
/// scheduler gets a turn, not before this returns.
pub fn spawn(body: impl FnOnce() + 'static, priority: Priority) -> TaskId {
    assert!(
        STARTED.load(Ordering::Relaxed),
        "spawn() was called before sched::init()"
    );
    with_table(|t| {
        let id = t.create(body, priority, true);
        t.sched.push_ready(id, priority);
        id
    })
}

/// Start a task in ring 3, running the ELF64 image `bytes` at `priority`.
///
/// The image is parsed first, then its pages and a stack are mapped into a
/// fresh address space, so the task's first dispatch goes straight to user
/// mode. The program runs the next time the scheduler gets a turn, not before
/// this returns.
///
/// Nothing about the image is trusted past [`elf::parse`]: a segment that
/// reaches into the kernel half is refused there, and every page placed here comes from frames this code allocates, never from the file's claims.
pub fn spawn_user(bytes: &[u8], priority: Priority) -> TaskId {
    try_spawn_user(bytes, priority, current(), &[], &[], &[], "")
        .expect("spawn_user: not an ELF64 executable mpos can load")
}
