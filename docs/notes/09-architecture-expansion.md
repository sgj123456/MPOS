# §9 Phase 9: Architecture Expansion (Far Future)

Part of the split `docs/notes/` evidence library — routing for every `§N.M` pointer is in
[index.md](index.md). Section numbers are unchanged from the original `NOTES.md`.

## Phase 9: Architecture Expansion (Far Future)

### 9.1 aarch64 Complete Boot

Current status: **stages 1a-1f, 2, 5a, 5b, 6a-6h landed, and `hal` compiles for
aarch64: the machine boots the way the x86_64 one does - the kernel owns its own
address space, two kernel tasks suspend and resume each other, the machine reads
its own device tree and reaches the firmware's thirty-two virtio-MMIO transports
through the kernel half, EL0 runs with an address space a process can live in
(protect, share, copy on write, take back, and its tables returned), the kernel
answers a *running* process's questions of that space (6c), reads a real wall
epoch off the PL031 (6d), turns on user SIMD, masks a line and waits on an
interrupt (6e) - and with the `hal` seam split into six baseline concerns plus
three optional capabilities, aarch64 implements the six it has and
`crate::hal` compiles for it.**
`cargo xtask boot-aarch64`
builds the kernel (`aarch64-unknown-none`), the loader
(`aarch64-unknown-uefi`), lays out an ESP with `EFI/BOOT/BOOTAA64.EFI` and boots
it under AAVMF; the loader reads `mpos.elf` off the volume, applies its
`R_AARCH64_RELATIVE` relocations against `KERNEL_VIRT_BASE`, builds four-level
tables from the firmware's own memory map and branches into the higher half at
**EL1**; then the kernel brings the frame allocator up from the record and
rebuilds the address space from `mm::vm::kernel_mappings`, which is the same
plan x86_64's `install` consumes. Measured, from one of three consecutive runs
that print every marker - the two numbers that move between runs, `47944` and
`47946` frames, differ by one page of the firmware's own noise, and every number
below is identical across those runs, the two frame addresses stage 6b prints
included:

```
mpos-boot: kernel image: 98 KiB
mpos-boot: initramfs: 11360 KiB
mpos-boot: RAM: 248 MiB
mpos-boot: device tree: 1052672 bytes at phys 0x44699000
mpos-boot: kernel: 104 KiB at phys 0x447da000, virt 0xffffffff80000000, entry 0xffffffff80011618
mpos-boot: mapped 496 MiB of physical memory, identity and through the direct map at 0xffff800000000000
mpos-boot: device window 0x8000000..0xc000000 mapped at its own address and at 0xffffffff40000000, Device and not executable
mpos-boot: page tables: 325 pages of them
mpos 0.1.0 (aarch64) booting
console: PL011 at 0xffffffff41000000
image: text 74 KiB, rodata 20 KiB, data 8 KiB (102 KiB total)
handoff: mpos-boot 0.1.0, record at 0x456ba020, image phys 0x447da000 -> virt 0xffffffff80000000, direct map at 0xffff800000000000
memory: 14 regions, 187 MiB the allocator may use
cmdline: "console=ttyAMA0"
exception level: EL1
devices: the firmware's tree, 1052672 bytes at 0xffff800044699000
devices: memory 0x40000000..0x50000000 (256 MiB), every region the record calls usable is inside it
devices: PL011 at 0x9000000, GIC at 0x8000000/0x8010000, timer on PPI 14 (INTID 30)
devices: 32 virtio-mmio windows, all inside the 0x8000000..0xc000000 device window
M4_DEVICES_OK
pmm: 5 zones, 47944 frames (187 MiB of 187 MiB usable)
interrupts: GICv2 (distributor 0xffffffff40000000), vector table at 0xffffffff8000e000, timer on INTID 30
page tables: 165 frames (660 KiB), TTBR0 0x434e6000, TTBR1 0x434e6000 (the kernel's own, read back from the registers)
plan: 7 mappings, all 63626 pages translate as the plan says (image r-x / r-- / rw-, direct map rw-)
direct map: 0xffff800043458000 -> 0x43458000, rw and it stores
device window: PL011 0xffffffff41000000 -> 0x9000000, Device, through the kernel half at 0xffffffff40000000
no low half: 0x447da000 is unreachable, and so is the device window at 0x8000000 that used to live there
M3_TABLES_OK
virtio: 32 transports, every one answering virtio 1.0 through the kernel half; 1 carries a device
virtio: the first is at 0xa003e00, device 4, vendor 0x554d4551
M5_VIRTIO_OK
timer: CNTFRQ_EL0 62500000 Hz, tick at 250 Hz (250000 counts apart)
M1_TIMER_OK: 25 interrupts in 100 ms (~25 at 250 Hz), 109192656 ns of counter
contexts: two kernel tasks alternating
ABABABABABABAM2_CONTEXT_OK: 13 hand-overs, both directions, every callee-saved register and v31 back where its task left it
space: 4 pages mapped, walked in order and read back through the MMU; two protected, one copy-on-write and resolved into a frame of its own (0x43476000) with the parent's 0x4346a000 left alone; two shared between spaces and written through one of them; two unmapped and given back
space: dropping an active space gave back all 4 frames its tables took, and the page it mapped not among them
M6_AS_OK
paging: active/kernel roots told apart; a named-root walk user-gated and unmapped-refused; uaccess read+write vs unmapped vs the kernel half; the copy-on-write marker the CPU cannot see; a u32 written through a space's root and read back from its frame; leaving to the kernel and re-installing
M6_PAGING_OK
wallclock: PL031 reached through the kernel half; boot anchored to a live UTC epoch; the counter read-only, so write_epoch refuses; the software anchor moves on set_boot_epoch
M6_WALL_OK
cpu: CPACR.FPEN full access after enable_user_simd; a distributor line enabled then masked and read back; idle() woken by the clock and returning with interrupts off
M6_CPU_OK
tls: set_thread_pointer(0xdeadbeef00001234) read back through thread_pointer(); cleared to 0
M6_TLS_OK
EL0: the kernel was on SP_EL1 (SPSel 1)
EL0: root 0x43479000 active, program at 0x200000 (40 bytes), stack at 0x210000, answers at 0x220000
EL0: the program wrote 0xaaaa, called a number no system call has, read 0xffffffffffffffda back and left with status 7
M6_EL0_OK
```

The `devices:` lines are stage 5a's *check*: every number was read out of the
firmware's tree and compared against the constant the kernel is compiled with for
that device - the PL011's base, both GIC frames, the timer's line, and the RAM the
boot record calls usable. A tree is not trusted to be right; it asks whether this
kernel's idea of the machine still is. The `187 MiB`/`47944 frames` against the
last stage's `191 MiB`/`48954` are 3.66 MiB of this harness's own device, 264 KiB
of loader page tables and 16 KiB of kernel that stage 6b's check added (text 62 ->
74 KiB, rodata 16 -> 20: the failure lines are most of it), all named rather than
folded into "the tree costs a megabyte": the tree is 1052672 bytes for 7160 bytes
of structure and 569 of strings, and the loader copies what the tree says it is.

Stage 5b is the two lines between `M3_TABLES_OK` and `M5_VIRTIO_OK`, and it is
the first time this machine has *used* a number the firmware gave it rather than
checking one. The device window - PL011 and GIC included - moved out of `TTBR0`
and into the kernel half at `0xffffffff40000000`, where x86_64's `mm::MMIO_BASE`
already was, so the low half holds nothing: the `no low half:` check asks about
the device window's *physical* address as well as the image's. `M5_VIRTIO_OK`
then reads four identity registers out of each of the tree's thirty-two
`virtio,mmio` windows through that alias, which is what makes the tree
actionable: a driver has somewhere to go. Three of the four registers are
load-bearing. **The magic and version are checked** - `0x74726976` and 2, the
only layout this kernel's driver crate reads - and **version is a choice the
harness has to make**: measured, `virt`'s transports answer version **1**, the
legacy register map, unless QEMU is told `-global virtio-mmio.force-legacy=false`,
now in the command line beside `acpi=off` and for the same kind of reason. And
**the tree cannot say which window carries a device**: QEMU publishes all
thirty-two nodes whether or not anything is attached (measured by decompiling the
tree with and without `-device virtio-rng-device` - the two differ only in the
firmware's own random seeds), and the device does not land in the first of them;
it answers at `0xa003e00`, the *last*. So a driver must read each transport,
which is the whole reason the check reads them.

Stage 6a is the last four lines of the log, and it is a change of kind rather
than of degree: everything above ran at EL1, and this is the first time this
kernel has been *below* itself. What makes EL0 different is where the address
space comes from - a program's code and stack are in the half `TTBR0_EL1` names,
the kernel's in `TTBR1_EL1`'s, and the two registers are what this stage first
tells apart. Until now both pointed at the *kernel's* root, which worked only
because stage 5b had left the low half empty; a user space is not an arrangement
imposed on the kernel, it is the state the kernel was already in being handed to
somebody else. The check is built the way stage 1c's and stage 2's were - the
smallest program that can be wrong in every way this mechanism can, and nothing
that can only be wrong for a reason the mechanism does not have yet. That program
is forty bytes: it puts the answer page's address in a *callee-saved* register,
writes its own value through it, calls a number no system call has, writes the
answer back through the same register, and leaves through `exit_group`. The
verdict is read out of a page the *program* wrote, because a trap is only
observable by the code that answers it: the kernel reads back `0xaaaa` (the value
the program wrote before it called anything) and `0xffffffffffffffda`
(`-ENOSYS`, the value the kernel put in `x0` and the program read back).
Nothing on the kernel side can fake either number.

**One measurement came out of that and changed the gate, and it is a difference
between the two machines rather than a bug.** `ELR_EL1` for a Supervisor Call
already holds the address of the instruction *after* the `svc`, so a gate that
steps over it - the shape the x86_64 twin suggests, where `SA_RESTART` subtracts
the two bytes back - skips an instruction the program never got to run. The first
run did exactly that: the `svc` is at `0x200010` and the trap reported `pc`
`0x200014`, so the program's next instruction (the store of the kernel's answer)
was stepped over and the page came back zero where the gate's answer should have
been. The gate does not advance `pc` at all now, and that is written down beside
it.

Stage 6b is the two `space:` lines before `M6_AS_OK`, and it is the difference
between an address space a program can be *put into* and one it can *live in*.
6a had no way to change a space once it existed, and that is most of what a
`fork`, an `mmap` and a `munmap` are made of: taking a mapping back, changing
one's permissions in place, walking what is there, and the copy-on-write half
that lets two processes share a frame until one writes it. All of it lands in
`paging.rs`'s `UserSpace`, and the encoding does not change - the file now writes
*and erases*. The signatures are deliberately `crate::hal::AddressSpaceOps`'
rather than the type's own, because writing them to the trait's shapes is what
will make the impl delegation when `hal` reaches this machine; the trait itself
cannot be implemented yet, since `crate::hal` is x86_64-only until all six of its
concern traits exist.

The resolved frame, `0x43476000`, is a frame the *child* space took when it first
wrote a page it was sharing with the parent, and the parent's frame,
`0x4346a000`, is printed beside it because "the child copied" and "the parent was
left alone" are two claims and only one is about the child. Four frames is what a
space's tables cost here (a root and three levels down to the page), and dropping
an *active* space gives all four back and not the page it mapped: measured as an
exact count, because both ways of getting it wrong are off by a specific number,
and a leak would otherwise look like a machine that simply works.

The check reads every answer back through three doors: the space's *own* tables
for what the kernel intends, a store to a user address with that space switched
in for what the MMU enforces, and the frame's own bytes through the direct map
for what actually happened. 6a's own measurement, where a table the kernel had
written was not the table the CPU walked, is the same lesson one register down.
It also has to be able to *fail*, so all three mistakes it exists to catch were
made on purpose before this stage was accepted: a `resolve_cow` with the copy
removed, a `Drop` that leaked its tables, and a copy-on-write page marked without
the read-only bit. Each stopped the boot with its own `M6_AS_FAILED` line naming
the claim that broke - and the first run, on correct code, failed on the check's
own arithmetic instead (three pages left where it had counted two), which is why
the count it prints is a count rather than a list of addresses.

The `plan:` line is the strongest claim either machine's boot makes about its
own page tables: every mapping `mm::vm::kernel_mappings` names is walked page by
page and asked to translate to the frame and permissions the plan gave it - so a
disagreement is a failure whichever side is wrong, and the direct map coming back
`rw` is the same plan that gives x86_64's `rw`.

The `-kernel` bring-up of 1b-1d (a fixed physical address with the MMU off, the
smallest thing that proved the toolchain, the linker script and the machine body
agreed) is **deleted**, not left beside this: stage 1e boots from the ESP through
AAVMF - the same boot path, not a shortcut past it - the `aarch64-unknown-none`
block is `relocation-model=pic` + `-pie` like the x86_64 one, the linker script has
no base address and no `.stack` (the stack moved to one the loader allocates), and
`src/board/aarch64.rs` is an `extern "C" fn _start(x0)` with no assembly stub. What
is left of the instrument is one entry in `AGENTS.md`'s trap table saying not to
bring it back; `src/lib.rs`'s header carries the stage ledger, the record of which
module un-gates when.

This section used to say "kernel ELF compiles but boot flow incomplete", which
was false in both halves. Nothing compiled for aarch64 - V-8 (§11.1.8) removed
the `[target.aarch64-unknown-none]` block that named a linker script which had
never existed - and "incomplete" hid the real shape of the work, which is not
one boot sequence but a second ABI. Measured: the x86-specific surface is
*concentrated* (port I/O in seven files, no ACPI consumer, no IOAPIC, no HPET,
no i8042), but `src/syscall/linux/` is 11028 lines and `src/drivers/` 14481, and
that is the long tail, not the machine layer. `src/` also carried only 17
`target_arch` gates, so everything else was gated on `target_os = "none"` alone
- true for both bare-metal targets - and tried to compile for aarch64 and could
not.

So the order below is the measured one. Each stage is its own commit and is
verified by the means named; none of them claims a green from compilation alone.

**Why stage 2's user-entry half is at stage 6.** The table used to put
`UserFrame` (`x0..x30`/`pc`/`pstate`) and the `eret` that enters EL0 in stage 2
with the kernel switch. They moved, and the reason is not tidiness: every field
of that struct gets its order and its meaning from `user_regs_struct`, which is
x86_64-only until stage 6, so a frame written at stage 2 would be a struct
nothing could check against anything - and its `eret` sequence has no user
address space to enter until stage 6 builds one. Writing them early would have
produced exactly the shape this port refuses: code that looks finished and has
never been run. What stage 2 keeps is the half that *is* checkable at this stage,
and it is checked - the boot prints `M2_CONTEXT_OK`. **Stage 6a is where both
arrived, and neither was written before it could be run**: the frame's field
order is the ABI's own `user_pt_regs`, and the `eret` enters a space that the
same stage builds and the same boot walks. The aarch64 numbers are not the
x86_64 ones - `exit_group` is 94 here against 231 there - so a `nr.rs` written
at stage 6a would still have been a table nothing could check against; the one
number the gate answers is spelled beside it and named as such.

| # | Stage | Verified by |
|---|---|---|
| 1a | Build plumbing: `rust-toolchain.toml` targets, `src/arch/aarch64/linker.ld`, `[target.aarch64-unknown-none]`, the gate list and the stage ledger | **landed**: `cargo build --target aarch64-unknown-none` links; the x86 gate stayed green |
| 1b | The machine body: PL011 at `0x0900_0000`, the `_start` stub, the banner | **landed**: `qemu-system-aarch64 -M virt -kernel` printed the banner - the bring-up instrument 1e deletes, named as such at the time |
| 1c | Exception vectors (`VBAR_EL1`), GICv2, the generic timer (`CNTPCT_EL0`/`CNTP_CVAL_EL0`) | **landed**: `M1_TIMER_OK: 25 interrupts in 100 ms (~25 at 250 Hz)`, three runs of three, and the check's failure branch names the timer's, the distributor's and the CPU interface's registers rather than hanging |
| 1d | 4-level page tables, `TTBR0_EL1`/`TTBR1_EL1`, `TCR_EL1`/`MAIR_EL1`/`SCTLR_EL1` | **landed**: `page tables: 12 frames (48 KiB)`, `M3_TABLES_OK` (image `r-x`/`r--`/`rw-` at both its addresses, direct map written and read back, null page absent), and the clock still counts to 25 with the MMU on. **What this row cost first, and is now a machine-wide rule:** the bring-up carried the page-table arena as a `static` array with a zero initializer and no writer — which lands it in `.rodata`, read-only on a machine whose page tables enforce it, and cost 128 KiB of `.rodata`; it was deleted with the `-kernel` path. Stage 2's task stack was the same class twice over: its first `stp` took a data abort *with the fault address inside the kernel image*. The fix is not a section attribute — it is to take frames from the allocator, which is what the code wanted anyway. `-C link-arg=-zmax-page-size=0x1000` does not save you either; it belongs to the list of things that look like a fix for a wrongly-placed static and are not. |
| 1e | The aarch64 loader: `boot/src/vm/aarch64.rs` (the descriptor format, `MAIR`/`TCR`/`SCTLR`), `boot/src/main.rs`'s aarch64 `enter` and PL011, `boot/src/elf.rs` learns `EM_AARCH64`/`R_AARCH64_RELATIVE`, the kernel relinked as a higher-half PIE, `src/arch/aarch64/paging.rs` becomes a reader of the loader's tables, `cargo xtask boot-aarch64` | **landed**: AAVMF boots the real ESP image, the kernel reaches the higher half and prints `M3_TABLES_OK` (the loader's `TTBR0`/`TTBR1`, the image's W^X, the direct map written and read back, the PL011 device) and `M1_TIMER_OK: 25 interrupts in 100 ms`; the `-kernel` instrument is deleted |
| 1f | The kernel's own tables: `src/arch/aarch64/paging.rs` regains a builder, built from `mm::vm::kernel_mappings(info, sections)` out of frames from `mm::alloc_frames`; `mm::init` runs on aarch64; the boot switches `TTBR0_EL1`/`TTBR1_EL1` onto it | **landed**: `pmm: 5 zones, 49209 frames`, `page tables: 166 frames (664 KiB)`, `plan: 7 mappings, all 63616 pages translate as the plan says (image r-x / r-- / rw-, direct map rw-)`, `no identity map: 0x44bcf000 is unreachable, only the device window lives low`, then `M3_TABLES_OK` and `M1_TIMER_OK` |
| 2 | Kernel contexts: `src/arch/aarch64/context.rs` - `Context` with the callee-saved `x19..x28`/`fp`/`lr` on the task's own stack, `FpuState` (`q0..q31`, `FPCR`, `FPSR`), `bootstrap` and its trampoline, `switch` | **landed**: `M2_CONTEXT_OK: 13 hand-overs, both directions, every callee-saved register and v31 back where its task left it`, and the letters above it alternate `ABABAB` |
| 2b | *Not a stage* - the **user** frame (`x0..x30`/`pc`/`pstate`) and its `eret` entry moved to stage 6, where the ABI that gives those fields their order and their meaning is. See the note below the table | - |
| 3 | Un-gate `sched` (the TLS `MSR` write moves behind `hal`/`arch`) | `cargo build --release --target aarch64-unknown-none` with `sched` in - and see the correction below the table: this row is **not** one edit, and it waits on 4, 5 and 6 |
| 4 | Un-gate `console` and `vfs` (serial-only console first) | a ramfs root with the initramfs untarred |
| 4a | The first cut into the knot above `hal`: the serial console. The sink moves behind a new baseline `hal::Console` (`init`/`write_bytes`/`interrupt_driven`) with the interrupt-TX-ring behind an optional x86-only `hal::SerialRing` (`tx_space`/`tx_submit`/`tx_wait_now`/`tx_stop_waiting`), mirroring `Msi`/`FpuTrap`/`Segments`; `console.rs`'s framebuffer paint, ring-mirror and queue-and-park loop move verbatim into `#[cfg(target_arch="x86_64")]` helpers (inert elsewhere); aarch64 implements `Console` over the polled PL011 (CRLF at the sink, `interrupt_driven()`=`false` until stage 5); `console_list` recognises the aarch64 serial wire `ttyAMA0`; `console` and `print!`/`println!` un-gate to `target_os="none"` | **landed**: `cargo clippy --target aarch64-unknown-none` compiles `console` (the first subsystem above the machine layer to build for aarch64); `cargo xtask test`'s 142 user tests still exec through `console::write_bytes` and the parked/flood TX path (the behaviour-identity pin for the rewire); `cargo xtask boot-aarch64` still prints ten markers; host `console_list` tests pin `ttyAMA0` and `ttyAMA1`-unknown; depgraph holds at 12 pairs (`console -> hal` is one-way new, the `drivers`/`sched` names moved inside `#[cfg]` bodies and were already one-way) |
| 5a | The device tree: `crates/fdt` (the reader, host-tested), `Handoff.dtb_phys`/`dtb_len` appended after `regions`, the loader copying the firmware's blob out of the UEFI configuration table, and `arch::aarch64::check_devices` holding the tree up against the machine's own constants | **landed**: `cargo xtask boot-aarch64` prints `devices: PL011 at 0x9000000, GIC at 0x8000000/0x8010000, timer on PPI 14 (INTID 30)`, `devices: 32 virtio-mmio windows, all inside the 0x8000000..0xc000000 device window` and `M4_DEVICES_OK`, and the tree's memory node is compared with every region the record calls usable |
| 5b | The device window in the kernel half, and the tree's transports read back: `paging::DEVICE_PHYS`/`DEVICE_VIRT` split, `boot/src/vm.rs` mapping the window at both, `pl011`/`gic` compiled against the alias with their physical bases kept for the tree check, and `arch::aarch64::check_virtio` reading every `virtio,mmio` window's MagicValue/Version/DeviceID/VendorID | **landed**: `M3_TABLES_OK` now stands under `no low half: ... and so is the device window at 0x8000000 that used to live there`, and `M5_VIRTIO_OK` under `virtio: 32 transports, every one answering virtio 1.0 through the kernel half; 1 carries a device`, across three consecutive green runs |
| 5 | Drivers: virtio-MMIO found through the tree, PL011 as the serial device, GIC line routing | virtio-blk and virtio-net up - **the discovery half of this row is 5b and is done; what remains is the drivers themselves**, whose transport is `virtio_drivers::transport::mmio` reached at `paging::device_virt(phys, len)` |
| 6a | The entry to EL0: `paging::UserSpace` (a root of its own for `TTBR0_EL1`, user page mappings, the switch that installs it), `context::UserFrame` (`x0..x30`/`sp`/`pc`/`pstate`, in the ABI's own `user_pt_regs` order) with `enter_user` and `leave_for_kernel`, and the vector entry for a trap from a lower EL - which saves the whole register file and can *return*, unlike the fatal synchronous one | **landed**: `EL0: root 0x4346c000 active, program at 0x200000 (40 bytes), stack at 0x210000, answers at 0x220000`, `EL0: the program wrote 0xaaaa, called a number no system call has, read 0xffffffffffffffda back and left with status 7` and `M6_EL0_OK`, across four consecutive green runs |
| 6b | The address-space operations, in `crate::hal::AddressSpaceOps`' shapes: `map`/`try_map`/`unmap`/`protect`/`map_cow`/`map_shared`/`resolve_cow`/`walk_user_pages`/`set_cow_bit`/`set_cow_bit_quietly`/`flush_if_active`/`phys_for_virt` and the `Drop` built out of `unmap`; the copy-on-write marker in descriptor bit 55 (software-reserved there, `RES0` to the walker) with the read-only bit the CPU enforces set beside it, and `USER_END` as `TTBR0`'s own span rather than x86_64's bit-47 split | **landed**: `M6_AS_OK` under `space: 4 pages mapped, walked in order and read back through the MMU; ...` and `space: dropping an active space gave back all 4 frames its tables took`, across three consecutive green runs - and each of the three mistakes the check exists to catch (a resolve with no copy, a `Drop` that leaks its tables, a marker set without the read-only bit) was made on purpose and stopped the boot by name |
| 6c | The `hal::Paging` methods that need a *running* process, at module level so the `impl` is delegation: `active_root`/`kernel_root` (the `TTBR0`/`TTBR1` statics), `switch_to_root`/`switch_to_kernel` (over the private `load_root0`), `phys_for_virt_of`/`user_phys_for_virt_of` (over `walk_from`, the second gating on `USER`), `user_accessible`/`is_cow_page` (over the leaf walk + bit 55), `write_user_u32` (through a named root, no live switch), and `map_mmio` (asserts the fixed window covers the range - `virt` has no dynamic MMIO) | **landed**: `M6_PAGING_OK` under `paging: active/kernel roots told apart; ... the copy-on-write marker the CPU cannot see; a u32 written through a space's root and read back from its frame; leaving to the kernel and re-installing` - each claim read back against the hardware it names, and a kernel-half address rejected by `user_accessible` because a user pointer is a `TTBR0` address |
| 6d | The `hal::WallClock`: a PL031 driver reached through the kernel-half window (`0x0901_0000`), `boot_epoch` anchored to a read of the counter at init, `set_boot_epoch` moving the software anchor, `write_epoch` refusing (the counter is host-shadowed and read-only on QEMU's model, so there is no persistent register) | **landed**: `M6_WALL_OK` - and the design changed by measurement: the plan said this machine had no wall source, but reading `RTC_DR` at boot gave the host's real UTC epoch (`CR=1`, already counting) and a programmatic write did not stick, so `boot_epoch` reads it like x86 reads the CMOS and `write_epoch` answers `false` rather than pretending |
| 6e | The `hal::Cpu`/`hal::Interrupts` primitives the machine lacked: `enable_user_simd` (`CPACR_EL1.FPEN=0b11`, read-modify-write), `idle` (`daifclr;wfi;daifset`), `gic::disable` (clear `GICD_ICENABLER`) and `is_enabled`, the serial line's `SERIAL_INTID=33`, and the `Line -> INTID` mapper | **landed**: `M6_CPU_OK` - FPEN read back, the serial's distributor bit set then cleared and read back, and `idle` woken by an armed timer tick (28->29) returning with interrupts masked |
| 6f | `hal` compiles for aarch64: the arch-neutral split moves x86's optional capabilities (`Msi`, `FpuTrap`, `Segments`) out of the baseline traits, `Current` un-gates to `target_os = "none"`, and `arch/aarch64` writes `impl {Clock,WallClock,Cpu,Interrupts,Paging,Contexts,Platform} for Current` plus `AddressSpaceOps for UserSpace`, the ops traits, and a full-register `enter_frame` that `check_el0` and the trait `enter_user`/`resume_user` share | **landed**: `cargo clippy --target aarch64-unknown-none` compiles `hal` for aarch64 (the deliverable; the debug aarch64 *build* cannot link, so clippy is the check - `AGENTS.md` §3), boot-aarch64 still prints all ten markers, and the x86 gate + depgraph ratchet are unchanged. The `to_user_regs`/`from_user_regs` `user_pt_regs` field-order pin and the COW-fault resolver wait on stage 6's ABI caller and stage 3's scheduler - named below, not stubbed |
| 6g | The kernel's own ELF loader honours the machine it is given: `mm/elf.rs` reads `e_machine`, accepts `EM_X86_64`/`EM_AARCH64` (refuses any other, where it used to validate nothing), selects the arch's `R_*_RELATIVE` kind from the file rather than hardcoding `R_X86_64_RELATIVE`, and reports `Image.machine` so the per-arch `execve` caller can reject a foreign-but-known ISA. An aarch64 static-pie now has its relocations read instead of silently dropped | **landed**: host unit tests `an_aarch64_image_gets_its_relative_relocations_read`, `an_image_for_an_unknown_machine_is_refused`, `a_file_that_names_no_machine_is_refused`, `an_x86_64_image_reports_its_machine` pass (509 unit tests total), and `cargo xtask test`'s 142 user tests still exec real x86 binaries through this parser - so the x86 path is unchanged and the aarch64 path is pinned by a test that fails on the old type-8-only filter. `USER_LIMIT` is still x86_64's `1<<47` (aarch64's user span is `paging::USER_END`); taking it from the machine is a named follow-on, conservative for now because aarch64 statics place far below it |
| 6h | The `hal::Cpu` thread-pointer capability the scheduler needs to un-gate: `set_thread_pointer`/`thread_pointer` join `Cpu`, x86's `IA32_FS_BASE` `wrmsr` moves out of `sched`'s private helper into the x86 `Cpu` impl (behaviour identical), and aarch64 writes `TPIDR_EL0`. This is the first slice of row 6's "`TPIDR_EL0` TLS" - landed because `sched` could not name an x86 machine register and still compile here. The `Cloexec` inversion and the arch-neutral ABI-const leaf are the other two things standing between `sched` and the aarch64 build, and land beside it. | **landed**: `cargo clippy --target aarch64-unknown-none` compiles the `Cpu` impl with the two new methods; `M6_TLS_OK` under `tls: set_thread_pointer(0xdeadbeef00001234) read back through thread_pointer(); cleared to 0` (a sentinel the register must echo, then a clear the register must take - the write is not trusted until it reads back); the x86 behaviour-identity pin is that `cargo xtask test`'s 142 user tests still pass with `sched` now reaching FS_BASE through the forward, depgraph holds at 12 pairs (`sched` no longer names an `msr`), and boot-aarch64 now prints twelve markers |
| 6 | The rest of the aarch64 Linux ABI: `nr.rs` per arch (the numbers are the machine's - `exit_group` is 94 here and 231 on x86_64), signal frames, `TPIDR_EL0` TLS, `PTRACE_GETFPREGS`'s view of `FpuState`, the `Paging` methods that need a running process, and the `hal` impls those two complete | the `t-*` conformance programs run on aarch64 |
| 7 | aarch64 userspace and the image: `aarch64-unknown-linux-musl` builds, ESP layout for AAVMF, arch-aware `xtask` | `cargo xtask test` passes on aarch64 |
| 8 | The gate: `xtask test`/`run` grow an arch selection | a green aarch64 gate quoted with its summary line and exit code |

**The order below the machine layer is not the order of the table, and that was
measured rather than argued.** Row 3 reads as one edit - "un-gate `sched`: the
TLS `MSR` write moves behind `hal`/`arch`" - and it is not. Widening that gate
alone and building reports **50 errors, of which three are the `wrmsr`**; the
rest are 23 missing `vfs`, 9 missing `syscall`, 7 missing `hal`, 2 missing
`drivers` and a `println!`, because `sched` names all four directly. So `sched`
cannot be un-gated before 4 (`vfs`), 5 (`drivers`) and 6 (the ABI) have landed,
and `hal` - which `sched` wants for the TLS write and which everything else
wants for the machine - cannot compile for aarch64 until all six of its traits
exist, which needs stage 6's `Paging` user half and a `WallClock` this machine
has no driver for. The next work is therefore **5, not 3**, and the table's
numbering is the plan's order rather than the order it can be done in.

**And stage 5's drivers turned out to be on the same side of the knot**, which
is stage 5b's own measurement rather than a guess: un-gating `drivers` alone is
**190 errors** whose long tail is `crate::vfs` (eight imports, from the device
nodes, `uinput`, `rtc_dev` and the DRM card), `crate::uaccess`, `crate::sched`
and `mm::map_mmio`; un-gating `console` alone is **18**, and they are
`drivers::serial`, `drivers::video::render` and `sched`, so the console cannot
move without the drivers it writes through. The only cut in the knot is `hal`,
and the only thing standing between `hal` and compiling is the *user* half of the
machine - which is stage 6. That is why 6a and 6b came next rather than 5's
drivers, and why the drivers' turn is after the rest of 6 rather than before it:
the tree walk 5b landed is the half of "virtio-MMIO found through the tree" that
does not need `vfs`, and the transport that opens a device is the half that does.

**That cut has since landed, and the console side of the knot is now cut too.**
`hal` compiles for aarch64 (stage 6f), and stage 4a routed the console's sink behind
a new `hal::Console` baseline capability (its interrupt-TX-ring behind an optional
x86-only `hal::SerialRing`), cfg-partitioned the framebuffer/park arms, and un-gated
`console` + `println!` to aarch64 - so the 18 errors above are gone. The framebuffer,
ring and parking arms stay x86-only by `#[cfg]`. **Stage 4b then closed the one pin
4a left open** - a boot-observed `M4_CONSOLE_OK`. It did *not* need to relocate the
milestone checks: `arch::aarch64::boot` gained a `milestone: fn()` argument it calls
once the PL011 is up, and `board/aarch64.rs` hands in a function that writes through
`crate::console` (`init` + two `println!`) - so `arch` only invokes a pointer and the
`console` name stays on the `main` side of the seam (no `arch -> console` edge). It is
the first log bytes this machine sends through the console service rather than the
machine's own `pl011::Writer`, printed early (before `M4_DEVICES_OK`), so it is a
required marker and not a terminator - EL0's `M6_EL0_OK` stays last. `boot-aarch64`
now prints twelve markers (stage 6h's `M6_TLS_OK` joins the `M6_*_OK` capability family
between `M6_CPU_OK` and `M6_EL0_OK`). `cargo xtask run`/`test` still build and boot x86_64 only;
the next cut into the knot is a subsystem above `console` (`vfs`/`sched`/`drivers`),
each its own measured stage.

**Stage 5's terminal un-gate is DONE: `sched`+`vfs`+`uaccess` now compile for
aarch64** (`cargo clippy --target aarch64-unknown-none` is clean, and the x86 gate
is byte-identical - `cargo xtask test` still 142/142). Measured, not argued: the
knot is strongly connected, so the only observable test was flipping all three gates
and reading the compiler. The last blocker after the leaf/`MMAP_BASE`/lazy-FPU work
was the ptrace/stop register-snapshot subsystem (`group_stop`/`exec_trap`/`fork_trap`/
`park`/`trace_regs`, and the `[u64; uregs::COUNT]` field on `Tracing`). My first read
judged it a stage-6/7 dependency; a second, careful read of the *call graph* showed it
runs one-way into the gated x86 syscall/`proc.rs` + `#NM` layer - aarch64 has neither
the `svc` trap nor ptrace yet, so those functions have **no caller** on aarch64. Gating
the whole block to `target_arch = "x86_64"` (the field, the uregs-typed fns, and
`attach_new_child`/`fork_event`, whose core fork call-site takes the honest
no-tracer-equals-ready path) drops `sched -> syscall` to 0. This is not a faked
aarch64 ptrace: the functions are *absent*, not stubbed to return success, and the
uregs-free state (`Stop`, `task.trace`, core `wait`) stays for both machines. aarch64's
real stop/signal-frame/ptrace view still arrives with stage 6/7's `svc` gate +
signal-frame ABI - only now `sched`/`vfs`/`uaccess` build without it, so the syscall
stage lands on a machine whose scheduler already compiles.

**Superseded, and the later cut is the better answer** (§13.2). The gate above was an
interim: it made the edge 0 *on aarch64* while leaving it 1 on x86_64 and leaving `sched`
naming an architecture. What the block actually depended on was the *shape* of a register
file that only the x86 ABI table described. With the width moved to the machine
(`hal::Contexts::ABI_REG_COUNT`) and the `PTRACE_O_*`/`PTRACE_EVENT_*`/`SYSCALL_TRAP`
protocol words moved to the `abi` leaf beside the clone flags, the gate is gone, `sched`
names no architecture, and `sched -> syscall` is 0 on **both** machines - not only on the
one that never had a ptrace handler. The un-gated block still fakes nothing: a machine
whose syscall layer has no `PTRACE_ATTACH` has no tracer, so `attach_new_child` answers
"no" and the child simply goes ready.

**Stage 6A's prep cut is DONE: the whole `syscall` module now compiles for aarch64.**
Flipping `pub mod syscall`'s gate to include `aarch64` yields a clean
`cargo clippy --target aarch64-unknown-none` (warnings only) with the x86 gate
byte-identical (`cargo xtask test` still 142/142, host 511, boot-aarch64 twelve
markers, depgraph within ratchet). The 101 errors the first flip measured resolved
through ten `#[cfg(not(target_arch = "aarch64"))]` preps, each a provable no-op on
host (x86_64) and x86 bare metal where the predicate is simply true. What is deferred,
and why it is refused rather than faked: the socket arms and the `'K'`/`'V'`/`'E'`/`'U'`
and `SIOC`/`FBIO`/DRM ioctl families fall to `ENOTTY`/`ENOSYS` (no `net`/`drivers` on
this machine yet); `ptrace`/`rt_sigreturn`/`reboot`
dispatch arms fall to `_ => ENOSYS` (they rewrite the x86 return frame, which arrives
with the aarch64 signal-frame ABI; aarch64 has no `fork` number at all - a libc's
`fork()` is `clone(SIGCHLD)`); `process_vm_*` and `rt_sigtimedwait` were listed with
them until §13.2 un-gated both - neither rewrites a frame, and the lumping was the
stale-gate error this phase exists to find; `execve` and `clone`/`wait4` are now **served** on
aarch64 (stage 6D: both handlers were already arch-neutral through
`sched::exec_replace`/`sched::clone`, and only the x86 return-frame edges were split -
`UserFrame.pc/sp/x[0]` and the aarch64 `user_context` instead of `RegsFrame`), so a
program can `execve` a new image, `clone` a child, and `wait4` to reap it at EL0, pinned
by the `MPOS_EXEC_OK` and `MPOS_FORK_OK` markers; `arch_prctl` has no aarch64 number at all; the
seccomp *filter* path (reads the x86 RIP) and on-exit signal delivery in
`linux_dispatch` are gated to their no-signal identity (the ptrace entry/exit stops
were gated here too, until §13.2's register-ABI stage made the frame operations
machine ops - they now run on both machines and are no-ops wherever no tracer is
attached);
and `uname`'s machine field and the hostname/domainname were moved to per-arch and the
`uts` leaf so those always-live calls answer truthfully. The remaining stage-6A work is
the per-arch `nr.rs` (arm64 is a *different syscall set* - no `open`/`stat`/`fstat`/
`poll`/`select`/`fork` legacy numbers, and every shared number differs from x86) plus
routing `arch/aarch64/vectors.rs`'s `el0_sync` `svc` into `linux_dispatch` on the
aarch64 `UserFrame`, which is what makes the module *used* rather than merely built;
the gate stays x86-only until that lands so no commit ships with `syscall` compiling
but unreachable.

The per-arch `nr.rs` **has** since landed (prep 11): `nr.rs` selects a whole table by
`#[cfg(target_arch)]` via `include!` (so it stays the single file the `abitest` harness
names) - x86_64 keeps its body verbatim and its one-per-name test pins, aarch64 gets the
resolved asm-generic numbers, a different *set* rather than a renumber, with distinct
sentinels above any real number for the calls arm64 lacks so the shared `dispatch` arm
compiles yet can never be matched. With the gate flipped the whole module still compiles
clean. But routing `el0_sync`'s `svc` into `syscall::dispatch` was tried and **panicked
the boot**: `dispatch` opens on `current_is_seccomp_strict`, a scheduler read, and the
EL0 milestone runs on the boot stack *before* the scheduler has a current task ("there is
no task 63"). That is an ordering fact, not a defect to paper over. So the gate keeps its
honest inline `ENOSYS` refusal - `M6_EL0_OK` still pins the trap end-to-end by reading
`-ENOSYS` back - and the routing plus its runtime pin arrive *with* a task to answer
from: folded into the init-handoff stage (6E), which is exactly when the first real
userspace task exists, rather than a synthetic pre-scheduler probe. The `svc`->dispatch
edge is therefore no longer a standalone 6A item; the module is live and correctly
numbered, and the gate is opened to aarch64 as part of getting a program to run.


**6E reached a running aarch64 program.** `xtask/src/aarch64_init.rs` hand-encodes a
static `ET_EXEC`/`EM_AARCH64` `/init` whose whole program is `exit_group(0)` - four
instructions, every word pinned against `llvm-objdump` in a host test rather than a
cross-compiler at build time (which would make the artifact non-hermetic and add a
`clang`+`ld.lld` host prerequisite to the boot path) - and `build_esp` packs it ustar
as `\initramfs.tar`. `boot` now threads a `main`-side init handoff (`init_handoff`)
as a continuation that `el0_returned` runs after `M6_EL0_OK`: it starts the scheduler,
mounts a ramfs root, unpacks the archive, loads `/init` on pid 1 and runs it to
completion. Because `enter_frame` is terminal, control only comes back to EL1 inside
`el0_returned`, so the handoff is passed the direct-mapped initramfs pointer+length
read out of `BootInfo` *before* the address-space switch (re-reading the loader's raw
record pointer after `paging::install` faults - the identity address is not in the
kernel's tables). `cargo xtask boot-aarch64` now ends
`initramfs: 1 members unpacked` / `mpos: aarch64 machine handed to /init` / `M6_INIT_OK`
with no exception: the kernel booted AAVMF, ran a real userspace program at EL0, routed
its `exit_group` into the shared `syscall::dispatch` table, and reaped it. Thirteen
markers, x86 `cargo xtask test` still 142/142. The named deliverable `cargo xtask
run --arch aarch64` now exists too (interactive serial via `mon:stdio`, reusing the
one `aarch64_qemu` builder `boot-aarch64` uses); verified headless it streams the whole
boot to `M6_INIT_OK` on the terminal. An *interactive shell* is still gated on the
drivers stage (serial RX + the park/idle path below), not on this command.

**The idle/park switch is now sound (fixed the stage-6 boot fault).** The old follow-on
said parking to the idle task "faults on an uninstalled idle kernel stack." Measured, that
was wrong about the cause: the fault is `sched::Table::enter`'s `set_kernel_stack` step
emitting `msr SP_EL1, Xt`, which is **architecturally undefined while `SPSel` selects
`SP_EL1`** - and that is exactly how this kernel runs (`EL1h`, `SPSel`=1, confirmed by the
`SPSR_EL1.M` = `0b0101` the fault carried, `EC 0x00` at `ELR` inside `enter`, `far`=0). The
dedicated `MRS/MSR SP_ELx` can only reach the *non-selected* stack, so writing the one the
kernel is on traps as an undefined instruction. AArch64 has no separate `TSS.rsp0` to arm
here: the trap-landing stack *is* the current `sp`, and `context::switch` already sets it to
the incoming task's saved kernel `sp` (`mov sp, x2`) before that task is ever dropped to
EL0, so `set_kernel_stack` is both undefined and redundant on this machine. Making it a
no-op removes the fault (verified: the park that used to abort now switches boot→idle→boot
cleanly, and the boot reaches its terminal halt with no exception). An interactive shell is
still gated - but only on the genuine prerequisites, serial **RX** (stage 7) and a `/init`
that does not exit one-shot - not on this switch any more.

**Stage 7a's first cut is DONE: the PL011 receive half reads, pinned by a loopback
probe.** Every marker through 6E was about *transmit* - the boot talks, it does not
listen. `arch::aarch64::pl011` now has a non-blocking `read_byte` (polls `FR.RXFE`,
returns `Some`/`None`) and a `loopback_echoes` that sets the UART's `CR.LBE` (which ties
TX back to RX inside the PL011 and takes the wire output off) and checks that a written
probe byte comes back through `DR`. `check_serial_rx` runs between `check_cpu` and
`check_tls` and prints `M6_SERIAL_RX_OK`/`_FAILED`; `M6_SERIAL_RX_FAILED` joins the
`TERMINATORS` list (its `_OK` is not a terminator - the TLS/EL0/init checks come after).
**The rule `TERMINATORS` obeys, learned four times over** (`xtask/src/aarch64.rs`): the list holds
the *last* check's success marker and *every* check's failure marker — never the success marker of
a check that another check comes after. A terminator stops the run, so a success marker listed too
early kills the machine mid-boot and the log simply ends, which is indistinguishable from a hang.
The measured history: the list once ended at `M1_TIMER_OK`, which stage 2's checks come after
(killed mid-line); stage 5a added `M4_DEVICES_OK`, which the timer and context checks come after —
a boot that finished in 5.7 s instead of the 60 s deadline and stopped three lines into a machine
that was working; stage 6a is the first check to land *after* the last one, so `M2_CONTEXT_OK` had
to come **out** when `M6_EL0_OK` went in; stages 6b/6c/6d/6e/6h each sit before 6a's EL0 check, so
their `_FAILED` markers are terminators while their `_OK` markers are not; stage 6E's handoff runs
after the EL0 probe, so `M6_INIT_OK` came out when it went in; stage 7a's serial-receive check sits
in the same pre-EL0 slot as 6h; stage 7b's console-read check runs after the EL0 probe but before
the init wait, so only its `_FAILED` is a terminator; and stage 7c made the handoff actually run
`/init` (waiting on `is_running`, not the always-false `has_live_child` — pid 1 has no parent), so
the terminal proof became the init's `INIT_EXITED` and `M6_INIT_OK` left the list for good. Elapsed
time is the tell: a stall is 60 s, a wrong terminator is seconds.
Boot-aarch64 is now **fourteen** markers. Measured on QEMU's model: the loopback echoes
synchronously, so the check is deterministic with no terminal. What this lays and what
remains for a typed shell: the RX *primitive* exists and is proven; the next cut wires it
into the tty/console read so a userspace `read(0)` returns typed bytes, then puts a shell
(a static musl `ash`) into the aarch64 initramfs - the `aarch64-unknown-linux-musl` target
is installable here and Alpine aarch64 packages are fetchable, so there is no environment
wall, only stages left to walk.

**Stage 7b is DONE: the receive path is wired through the tty, not just the UART.**
`check_serial_rx` proved `pl011::read_byte` reads the FIFO inside `arch`; this cut moves
the read up behind the designed seam. `hal` gains an optional `ConsoleInput` capability
(`poll_input() -> Option<u8>`, `inject_input` for the bring-up hook), implemented for aarch64's
`Current` over `pl011` and *not* for x86_64 (its serial receiver is interrupt-driven and hands
bytes straight to the tty, so it has no polled input to expose - refuse-don't-fake, the
capability is simply absent). `board/aarch64.rs` runs `check_console_rx`: inject a line through
loopback, pull each byte back through `hal::console_poll_input`, hand it to
`vfs::tty::global().receive_byte` (the step `vcmux::route_input` does on x86_64, which is
x86-only here), and read it back through `tty.read` - proving a userspace-visible read of
typed input end to end, printed as `M6_CONSOLE_RX_OK`. It also spawns the production input
*pump* task (poll the wire, feed the tty, sleep when idle) that will carry a real shell's
`read(fd 0)`. boot-aarch64 is now **fifteen** markers. What remains for an interactive shell:
a `/init` that reads `fd 0` in a loop instead of exiting, and a static-musl `ash` in the
initramfs.

**Stage 7c is DONE - and it corrected a false stage-6E proof: the scheduler now
actually runs `/init`.** 6E recorded `M6_INIT_OK` as "a real userspace program ran at EL0
via the scheduler". Measured this session with an `el0_sync` svc trace: it had not. The
handoff waited `while has_live_child(pid)`, but `has_live_child` asks whether the init has
live *children* - pid 1 has none (no parent) - so it was false on the first check and the
boot task finished the wait without ever yielding a turn; the spawned `/init` never
executed. The only program that had run at EL0 was the pre-scheduler `check_el0` *probe*
(direct `enter_frame`, `started()==false`). Fix: a new `sched::is_running(id)` (the slot's
own liveness) and the handoff waits on that, so the init is scheduled, enters EL0, runs.
To make the proof observable from the console, the hand-encoded `/init` now does
`write(1, "MPOS_INIT_ON\n", 13)` (ten `mov`/`movk`/`svc` instructions, each verified by
disassembling the built image with `llvm-objdump`) before `exit_group`. boot-aarch64 then
shows in order `M6_INIT_OK` (handed to pid 1), `MPOS_INIT_ON` (the init's own syscall
reached the wire - the first time a scheduler-run userspace program produced console output
on this machine), and `INIT_EXITED` (its `exit_group` routed to `sched::exit`). `INIT_EXITED`
- not `M6_INIT_OK` (which now prints before the wait) - is the last `TERMINATOR`. This is the
objective's core - "boots the kernel to a working userspace" - now genuinely demonstrated
end to end. The `/init` still exits one-shot; a typed shell needs a read-loop `/init` + a
real shell binary (stage 7 continues), but the run-and-output path is proven.

**Stage 7d is DONE: a scheduler-run userspace program reads console input and echoes the
bytes it read back out.** The hand-encoded `/init` now does `read(0, buf, 64)`, then
`write(1, "R:", 2)` and `write(1, buf, n)` - the *bytes it just read* - then
`exit_group(0)`: twenty-three `mov`/`movz`/`movk`/`svc` words, all verified through the
`llvm-objdump` encode/disassemble round trip. Writing back what it read (not a fixed
string) is what makes the proof unambiguous: the tty's canonical echo of the queued input
also prints `echo-test`, so only the init's own output carries the `R:` prefix. The boot
queues a line (`echo-test\n`) on the tty before the handoff (the same receive path stage
7b wired; `cargo xtask run --arch aarch64` supplies it live from typed input), so the
init's `read` returns real console input. boot-aarch64 now shows, in order: `echo-test`
(canonical tty echo of the queued input), `M6_INIT_OK`, `R:echo-test` (the userspace
`read(0)` returned the console bytes and the userspace `write(1)` echoed them through
`syscall::dispatch`), and `INIT_EXITED`. That is the full interactive path - typed bytes
in at EL0 via a userspace `read` syscall, the same bytes back out via a userspace `write` -
demonstrated on the second machine. What remains for a typed *shell* rather than this
read/echo proof: a `/init` that loops read/echo (or busybox `ash`, which additionally
needs the aarch64 `execve`/`clone` return-frame path - the named stage-6D gap), then the
`drivers` disk root and `net`.

**Stage 7e is DONE: `cargo xtask run --arch aarch64` is an interactive read/echo REPL.**
The image is now two hand-encoded programs sharing one assembler (`aarch64_init.rs`):
the one-shot above (the headless `boot-aarch64` verdict's, which exits so the marker
poll stops on `INIT_EXITED`), and a **looping** `/init` whose body is the same
`read(0)`/`write(1,"R:")`/`write(1,buf,n)` with `exit_group`/`b .` replaced by a branch
back to the top - twenty words, the backward branch derived from the code length so it
cannot desync, pinned by a host test that decodes the final word's `imm26` and asserts it
is `-(20-1)`. `build_esp` takes the image to pack; `run --arch aarch64` packs the loop,
`boot-aarch64` keeps the one-shot. The loop is why this belongs to `run` and not the
verdict: with no typist its *second* `read(0)` parks - `read_wait`'s canonical
`UntilInput`, woken only when a line lands - so `INIT_EXITED` never comes and a
marker-poll would hang; at a terminal it is a REPL. Verified headlessly by booting the
ESP under `-serial stdio` (plain, not `mon:stdio`, which swallows a piped monitor mux)
and feeding `hello\nworld\n`: the log shows the tty's canonical echoes `hello`/`world`
*and* the loop's own `R:hello`/`R:world` after the initial `R:echo-test` - a
scheduler-run userspace program that read a typed line, echoed it, parked, and did it
again. That is the objective's "reaches init/shell on the serial console" in the
minimal genuine form: a persistent interactive userspace loop on the second machine.

**And the loop is now a shell (§13.8/§13.9).** The REPL stand-in is gone.
`cargo xtask run --arch aarch64` builds an in-tree `/init`
(`aarch64_init::build_shell_init`) that makes `/tmp` and `execve`s `/bin/sh`, stages the same
multi-call `/bin` the sweep uses (`aarch64_bin`, now six applets), and drops the machine at a
prompt on the serial console. Three things had to land with it and none is cosmetic: the `sh`
applet became a line-at-a-time reader with a prompt (`is_terminal`, so a piped script stays
silent and an interactive session does not wait for `Ctrl-D`); `cat` gained its file
arguments, because a `cat` that copies nothing but standard input does not fail on a
terminal - it *blocks* on it, which is how a shell's first real command turned into a hang;
and the second machine's `uaccess` learned what the first machine's had always known, that a
buffer the process has *mapped* is a valid one even before the tables describe it, with the
EL1 fault entry that the copy then needs (§13.9). Measured, with the shell's own lines fed to
it over the serial line:

```
mpos: init handing over to /bin/sh
#
sh: echo-test: not found        <- the line the handoff queued, run as a command
# echo hello-from-guest
hello-from-guest
# ls /etc
conf.d  doas.conf  fish  inittab  local.d  network  profile.d  resolv.conf  xdg
```

Before §13.9 that session was one line long: `sh: debug err Bad address (os error 14)`,
because the shell's first `read(0)` - into its `BufReader`'s heap buffer, allocated and
unread - was refused as `EFAULT`, and it exited. **The userland around that shell has since
arrived** (§13.12): the machine installs and boots a real Alpine `aarch64` tree -
`busybox` init on `/etc/inittab`, `openrc` running its runlevels, a `getty` printing
`localhost login:` - with `/proc` and `/sys` mounted (the kernel serves both now; the mount
factory's architecture gate named a deferral that was over), a dynamic loader that works
(the machine's `USER_LIMIT`, not x86_64's, so a shared library's `MAP_FIXED` segments map),
and its console tty published under its own serial name, `ttyAMA0`. `cargo xtask
alpine-aarch64` is the verdict. What was still x86_64's was the *shape* of the console: the
virtual consoles `tty1`..`tty4` needed the `vcmux` port, and **§13.16 landed it** - the mux is
both machines', so a getty on `tty1` prints where a login can read it.

**Named gap now closed on the way:** `USER_LIMIT` was the follow-on §9.1 wrote down here -
"still x86_64's `1<<47`, aarch64's user span is `paging::USER_END`; taking the limit from the
machine is a named follow-on" - and it was not conservative, it was wrong: `MMAP_BASE` on this
machine (`0xff00_0000_0000`) sits **above** `1<<47`, so the first dynamically linked shared
library any program loaded made `handle_mmap` answer `EINVAL` to its `MAP_FIXED` segments
(`openrc`'s `librc.so.1: Invalid argument`). `mm::elf::USER_LIMIT` is now the machine's, and
`syscall` reads that one rather than a private copy.

**Stage 6D (execve) is DONE: `execve` transfers a running aarch64 program to a new
image at EL0.** The handler's body was already arch-neutral - it reads the path and
`argv`/`envp` through `uaccess`, checks `X_OK`, resolves any `#!` interpreter, honours
set-uid, and calls `sched::exec_replace`, which returns `(entry, new_rsp)` exactly as
`spawn_init`'s loader does (the same `new_layout`/`initial_stack_rsp`/`write_arg_block`
the aarch64 boot already proves). Only the *return-frame patch* was x86: it rewrote
`RegsFrame.rcx`/`user_rsp` for `sysretq`. That patch is now `#[cfg]`-split - x86 keeps
`RegsFrame`, aarch64 rewrites the saved `UserFrame`'s `pc=entry`/`sp=new_rsp`/`x[0]=0`
that `el0_sync`'s `eret` restores - and the `EXECVE` dispatch arm lost its
`#[cfg(not(target_arch = "aarch64"))]` gate so `svc 221` reaches it. Pin: the headless
verdict's `/init` is now an exec *launcher* that echoes the queued line (so `R:echo-test`
still proves the read/write path) then `execve`s a second hand-encoded image `/t` that
writes `MPOS_EXEC_OK`; `boot-aarch64` asserts the literal `MPOS_EXEC_OK` between
`R:echo-test` and `INIT_EXITED` (`INIT_EXITED` is now `/t`'s own `exit_group`, exec
keeping pid 1). Two bugs found and fixed while landing it, both honest failures the pin
turned red on: the `EXECVE` dispatch arm was still gated out on aarch64 (`svc` fell to
`ENOSYS`), and the packed initramfs files had no exec bit, so `may_access(X_OK)` denied
even root - `boot-aarch64` now `chmod 0755` the staged files.

**Stage 6D (clone/wait4) is DONE: aarch64 can spawn and reap a child process.**
aarch64 has no `fork` syscall number (`nr::FORK` is an unreachable sentinel); a libc's
`fork()` is `clone(SIGCHLD)`, so the real path is `clone`. `handle_clone`/`handle_clone_impl`
were already arch-neutral through `abi::validate_clone_flags` + `sched::clone`/`spawn_copy`
(which build the child from a `UserFrame` and start it ready, with no tracer to attach), so
only two x86 edges needed splitting: `user_context` now reads the trap `UserFrame` directly
on aarch64 (copy it, clear `x0` for the child) instead of reconstructing one from x86
`RegsFrame` + `user_segments`, and the post-`clone` tracer announcement is `#[cfg]`-guarded.
The `CLONE` dispatch arm lost its gate; `wait4` was already served. Pin: the exec target
`/t`, after writing `MPOS_EXEC_OK`, does `clone(SIGCHLD)` and a `cbnz x0` splits parent from
child - the child writes `MPOS_FORK_OK` and exits, the parent `wait4`s so the child's line
lands before the machine halts, then exits (`INIT_EXITED`). `boot-aarch64` asserts the
literal `MPOS_FORK_OK` between `MPOS_EXEC_OK` and `INIT_EXITED`. So the wire now shows the
full chain - read, echo, `execve` to a second image, `clone` to a third process, `wait4` to
reap it - all at EL0 through `syscall::dispatch`. `CLONE_VM`-without-`CLONE_THREAD` (the
spawn shape) and the tidptr writes are honoured; `CLONE_THREAD`/TLS-threaded `clone` and the
ptrace stops remain gated as the honest `ENOSYS`/no-tracer answer. With execve+clone+wait
served, a real shell (busybox `ash`) needs only a static-musl aarch64 `/init`/`ash` in the
initramfs; the disk root (`drivers`), `net`, and `cargo xtask test --arch aarch64` remain.

**Aarch64 EL0 page-fault / demand paging - DONE; the gap it explained is closed.** The
`aarch64-unknown-linux-musl` target installs and `rustc -C target-feature=+crt-static -C
linker=rust-lld` produces a real *unmodified* statically-linked `ARM aarch64` ELF (4 `PT_LOAD`s
+ a `PT_TLS`). First run of one: it reached EL0 and took a **data abort** (`ESR_EL1` EC `0x24`,
`pc` in the mapped text, `FAR_EL1` a wild value) before `main`. Root cause, found by reading the
EL0 vector rather than guessed: `el0_sync` had **no fault path at all** - any non-SVC EL0 trap
halted with "EXCEPTION: from EL0, and not a system call". x86's `page_fault` answers the three
non-error faults - a `fork` COW write, an untouched anon page, and a stack push past the mapped
end - via the arch-neutral `crate::sched` resolvers (`resolve_cow_fault`/`resolve_anon_fault`/
`resolve_stack_growth`); aarch64 never called them, so a real binary's first unbacked page was
fatal. The pinned hand-encoded images (`memsz == filesz`, tiny stack) never touched an unbacked
page, so this was invisible until a real libc ran. Fix: `el0_sync` now, on a data/instruction
abort from EL0, runs the same three resolvers and returns to retry the faulting instruction; an
unresolvable fault delivers `SIGSEGV`'s default action (the task `exit(-11)`), since aarch64
signal-frame delivery is still the named later gap - death is the correct default, not a stub.
Re-measured: the same unmodified musl binary now runs to completion and prints its output
(`MPOS_MUSL_OK`) on the second machine. Pin committed: the headless `/init` now first does
`sub sp, #0x1000` + `str x19,[sp]` (a fault past the mapped stack) and writes `MPOS_FAULT_OK`
only if the store survives - `boot-aarch64` asserts `MPOS_FAULT_OK` before `R:echo-test`, so the
demand-paging handler is pinned hermetically without a cross-toolchain dependency. **Stage 8
static-musl image - DONE.** `xtask` now builds a *real* unmodified `aarch64-unknown-linux-musl`
helper (`rustc -C target-feature=+crt-static -C linker=rust-lld`; the target is a listed prereq of
`boot-aarch64`, symmetric with the x86 test path's musl prereq), packs it as `/musl`, and the exec
target's parent `execve`s it last - so `MPOS_MUSL_OK` (that binary's own `println!`) is a permanent
`boot-aarch64` assertion. The committed chain is now `MPOS_FAULT_OK -> R:echo-test -> MPOS_EXEC_OK
-> MPOS_FORK_OK -> MPOS_MUSL_OK -> INIT_EXITED`: demand paging, read/write, `execve`, `clone`/
`wait4`, and an unmodified musl/Rust ELF all run at EL0 - the project's central promise on the
second machine. **The `drivers` disk root is landed too** (`docs/notes/` §13.3, `M7_DISK_OK` +
`M7_ROOT_OK`): the MMIO virtio-blk is a registered kernel disk (`vda`, `/dev/vda`), `xtask`
lays the repository's `rootfs/` overlay and the init images on it, and the machine now boots
with `/` on that disk - the init is read from it and its whole file lifecycle
(create/rename/hard-link/symlink/ftruncate) runs on ext4. What remains is a fuller shell
(`ash`), `net`, and `cargo xtask test --arch aarch64`.

**Measured follow-on RESOLVED: a heap-using musl binary now runs to completion - the fault
was `SCTLR_EL1.A`, not the loader.** Extending the `/musl` helper to `vec![0u8; 256*1024]`,
filling and summing it, then printing the checksum, made the real binary take a Data Abort
(EC `0x24`) at `FAR 0x20171e` before printing. Two guesses written down while chasing it -
a `PT_LOAD` the real path tears down, and a bad heap/`brk` placement - are both refuted by
the evidence: a temporary diagnostic in `el0_sync` (since removed) that walks the faulting
VA's page table reported the page mapped `READ` (so it is not a translation fault and not
unmapped), and the fault's DFSC is `0x21`, an **Alignment fault**. Symbolizing the `pc` (built with symbols) put it inside musl **`memcpy`** at
`ldp x6, x7, [x1]` - a pair load that *requires 8-byte alignment*, here reading the format
string at `0x20171e` (low three bits `110`, not 8-aligned). Root cause: the kernel enabled
`SCTLR_EL1.A` (alignment-check enable, bit 1), which governs EL0 too. Linux boots `A=0`, so
real programs' unaligned accesses - which `memcpy` produces constantly - simply work; with
`A=1` an unmodified binary faults in `memcpy`. Fix: drop `A` from `SCTLR_ENABLE` **and**
`bic` it in `enable_mmu`'s asm - the `orr` can only *set* bits and AAVMF leaves `A` set, so
removing it from the OR-set was measured insufficient on its own. The committed `/musl` pin
is now that heap binary and `boot-aarch64` asserts its literal output `MPOS_MUSL_OK
sum=33423360` (= the correct `32640 * 1024` checksum, so a wrong number or any fault fails
the verdict - the pin is a claim the run backs, not a string that prints regardless). With a
real binary that allocates, does an unaligned `memcpy`, formats and prints now running to
completion, the concrete gate to a fuller shell is cleared: what remains is the Alpine
aarch64 `ash`/`busybox` binary in the initramfs (a fetch and wiring, not an environment
wall), the `drivers` disk root, `net`, and `cargo xtask test --arch aarch64`.



**And the profile matters: the aarch64 debug build does not link.** `cargo
build --target aarch64-unknown-none` fails with `relocation R_AARCH64_ABS64
cannot be used against local symbol` out of the *sysroot's* `liballoc`, which is
not built for a PIE; only `--release` links, and `--release` is what `xtask`
builds, so no gate has ever noticed. Row 3's verification command used to be the
bare one; it says `--release` now, and `AGENTS.md` §3 carries the same warning.

Named gaps the port carries rather than hides, each with its home:

- **aarch64 `drivers` and `net` are the next full stage, with their surface measured.**
  Flipping the `drivers` gate to include aarch64 compiles to **31 errors**, and they
  cluster entirely into the x86-only subsystems the drivers sit on: the `x86_64` crate
  (port I/O in `qemu.rs`/`pci.rs`/`serial.rs`), `uart_16550` (the x86 serial chip -
  aarch64's console is the polled PL011, already wired), `crate::mm::map_mmio` (x86 BAR
  windows), and `crate::vfs::vcmux` (the x86 virtual-console mux, deferred since stage 5),
  reached from `ahci`/`drm`/`video`/`virtio/input`. So the port is not a flip: it is the
  established method - gate those x86-only modules out on aarch64 (a no-op there and on
  x86/host, the stage-6A prep shape), then ADD the aarch64 path the crate lacks: a
  `virtio-drivers` **MMIO transport** bound over the device tree's `virtio,mmio` windows
  the boot already reaches through `paging::device_virt` (`M5_VIRTIO` reads their identity
  registers today), reusing the arch-neutral `HalImpl` (its `dma_alloc`/`mmio_phys_to_virt`/
  `share` are pure `crate::mm`), then `blk` (a disk root), `rng` (entropy), and `net` over
  the same transport, plus GIC line routing to turn the polled `Console` into `SerialRing`.
  First commit is the MMIO
  transport with the simplest device driven to a boot marker (**done**; so is the
  GIC line routing that turned the polled `Console` into a `SerialRing` - §13.7, whose
  queue is now the shared `drivers::tx_ring` both machines' UARTs drain). The architectural
  prerequisite found when starting it: a driver must reach a device's physical MMIO
  window, but `crate::drivers` naming `crate::arch` is a layering violation (the rule is
  every machine call above the machine layer goes through `crate::hal`), and aarch64's
  device-window mapping today lives in `arch::aarch64::paging::device_virt` with no `hal`
  seam. So the stage opens by adding a `hal` capability - "give me a kernel-virtual
  address for this physical MMIO range" (x86 answers it with `map_mmio`, aarch64 with the
  device window) - which is a design-record change to `ARCHITECTURE.md` §2/§3 in the same
  commit, before any MMIO device is driven. That seam is the true first unit of the
  drivers stage. **The seam is LANDED** (the drivers-stage first commit):
  `crate::hal::Paging::map_mmio_window(phys, len) -> Option<usize>` plus its forward, the
  x86 body `Some(crate::mm::map_mmio(..))` (bump-allocate one mapping per BAR), the
  aarch64 body `paging::device_virt(..)` (offset into the fixed device window); and
  `drivers::virtio::hal::HalImpl::mmio_phys_to_virt` now answers through it instead of
  naming `crate::mm::map_mmio` - which removes the last x86-only call from that module, so
  the `Hal` impl the aarch64 MMIO devices will build on is machine-neutral. The `drivers →
  hal` edge already exists (halt/ticks/`unmask`/MSI) and `hal → drivers` is zero, so this
  adds no mutual pair and moves no depgraph constant; it is the sanctioned route, not a new
  one. Pin: x86 `PciTransport` calls `mmio_phys_to_virt` at bring-up, so a wrong x86 body
  fails `cargo xtask test` (every virtio device); the aarch64 body is the same
  `device_virt` `M5_VIRTIO` already reads the transports through. **The `drivers` gate is
  now flipped to include aarch64 (the prep commit, landed):** every x86_64-only submodule -
  AHCI, DRM, PCI, serial, input, video, devices, qemu, rtc and the PCI virtio drivers - is
  `#[cfg(target_arch = "x86_64")]`-gated out there, the PCI half of `virtio::hal` moved
  behind a gated `mod pci_transport` (re-exported so the x86 callers' paths are unchanged),
  and `drivers::virtio::spawn_service` is gated too - so `crate::drivers` now compiles on
  both machines and on aarch64 only the machine-neutral `virtio::hal` is live, with no
  device driven there yet. **Stage 7's first driven device over MMIO - DONE (the MMIO
  commit):** `drivers::virtio::mmio` opens a device from a kernel-virtual header the
  caller hands it (discovery stays in `arch`: `check_rng` walks the tree for the
  `virtio,mmio` window whose `DeviceID` is 4 (`VIRTIO_ID_RNG`; measured on this
  machine - the `virtio-rng-device` QEMU attaches answers 4, vendor "QEMU"), maps it
  with `paging::device_virt`, and
  passes the address down - so the driver names neither `crate::fdt` nor `crate::boot`
  and adds no new edge). It builds `MmioTransport::new` over that header and
  `VirtIORng::<HalImpl, _>` on the transport, reusing the machine-neutral `Hal` whose DMA
  comes from the direct map, then `request_entropy`. The boot marker `M7_RNG_OK` asserts 32
  host-generated bytes actually came back (a short read is `M7_RNG_FAILED`, never a zeroed
  buffer reported as entropy). This is the second machine driving a real virtio device end
  to end - queue setup, feature negotiation and DMA over the memory-mapped transport -
  through the crate it already uses on x86_64. **The rng now has a stable device home and
  feeds the kernel's entropy (the seed commit):** `drivers::virtio::mmio` keeps a global
  `Arc<MmioRng>` the boot installs (`init_global`) from the window `check_rng` finds, and
  exposes `entropy()`; `check_rng` installs the device *then* reads it, so `M7_RNG_OK` also
  means the stream `rand` will seed from is live. `rand::seed` gained an aarch64 arm
  (`#[cfg(all(target_os = "none", target_arch = "aarch64"))]`) that XORs `mmio::entropy()`
  into the boot-instant mix - the same shape x86_64 uses with its `RDRAND`+PCI-rng, minus
  the CPU instruction - replacing what had been the fixed host string. `getrandom(2)` (nr
  278) and `/dev/urandom` were already served on aarch64 off `rand::fill`; they now draw a
  hardware-seeded ChaCha stream. A machine with no rng window falls back to clock-only
  (still unpredictable to a process on it), the honest refusal, never a fake entropy claim.
  **Stage 7's second driven device over MMIO - virtio-blk, DONE (the block commit):**
  `drivers::virtio::mmio::read_blocks` opens the tree's block window (`DeviceID` 2,
  `VIRTIO_ID_BLOCK`) as an `MmioTransport` and hands it to the crate's `VirtIOBlk`; the
  boot attaches a raw 1 MiB image whose every byte is `i as u8` over a `virtio-blk-device`
  MMIO transport, and `check_blk` reads sector 0 and *verifies* the pattern - so `M7_BLK_OK`
  means a real block read came back with the right bytes, not merely that a request
  returned. It also reads the device's reported capacity and checks it equals the 2048
  sectors of that image, pinning the geometry path (a configuration-space read distinct from
  the data read) so a transport that read a sector but mis-sized the device would fail here.
  It then **drives the write path**: `mmio::write_readback` hands a distinct
  pattern to sector 3, flushes, and reads it back through a fresh open, requiring
  the round trip to match - so `M7_BLK_OK` attests both the read and write virtqueues
  move data over MMIO (a mismatch reuses `M7_BLK_FAILED`, already a terminator). The
  filesystem mounted over it (the disk root) is the next step; this is the
  block driver working, both directions. **Stage 8's net transport over MMIO - DONE (the net
  commit):** `drivers::virtio::mmio::read_mac` opens the tree's net window
  (`DeviceID` 1, `VIRTIO_ID_NET`) as an `MmioTransport` and hands it to the crate's
  `VirtIONet`; xtask attaches a `virtio-net-device` on a user-mode backend and
  `check_net` reads the adapter's 6-byte MAC and asserts it is not all-zero, **and
  then drives the transmit virtqueue**: `mmio::tx_probe` builds a DMA-reachable TX
  buffer through the neutral `Hal`, places a 60-byte frame in it, and requires the
  device to accept it (`send` returns `Ok`). `M8_NET_OK` prints only after that, so
  it proves the multi-queue MMIO *data path* moves a buffer to the device over the
  rings - the same class of proof the block milestone made by reading a real sector
  back - not just a config-space MAC read. It still does not claim the frame left the
  machine: the smoltcp stack and socket syscalls over this device are the next cut.
  (A rejected frame reuses `M8_NET_FAILED`, already a terminator, so it aborts
  honestly rather than printing a false `M8_NET_OK`.)
  **How the smoltcp stage actually has to start (measured, not assumed).** An
  attempt to cfg-prep `net` for aarch64 (move `smoltcp` to the shared `none` dep
  block, flip the `net` gate) left only 8 residual errors - `crate::drivers::virtio::net::VirtioNet`
  (imported at `net/mod.rs:74`), `crate::drivers::virtio::vsock`, and four
  `crate::vfs::procfs` references - which looks trivially gatable. It is not:
  `VirtioNet` is not a peripheral in `net/mod.rs`, it *is* the smoltcp `Device` the
  whole stack is built on (interface construction, the poll loop, the `Rx/TxToken`
  impls), so gating it to x86 removes the spine and leaves nothing to compile. The
  correct first cut is therefore not a cfg-gate but an **abstraction**: introduce a
  device trait `net` depends on instead of the concrete PCI `VirtioNet`, give x86 the
  PCI impl and aarch64 an MMIO impl (the `drivers::virtio::mmio::MmioBlk`-style
  adapter, this time satisfying smoltcp's `Device`), and only then attach a device
  and route sockets. `vsock`/`procfs`-backed nodes stay cfg-gated (absent on
  aarch64, honest gaps). The attempt was reverted, and the run above is why the
  next commit was an abstraction rather than a gate. **That ladder is now complete
  (the net flip commit).** The seam is `drivers::virtio::NetDevice` (landed first),
  the MMIO adapter is `mmio::MmioNet` - the same shape as `MmioBlk`, a device that
  *stays open* so the receive ring it posts is the one the stack's frames arrive
  on - `check_net` opens it once and **publishes** it (`mmio::init_net_global`),
  `board/aarch64.rs`'s `init_net` hands it to `net::init` and calls
  `net::spawn_service()` (byte-for-byte the pair `board/x86_64.rs` boots with), and
  `net` is now gated by `target_os = "none"` with `mpos_net` declared by both
  machines. `M8_NET_STACK_OK` prints when that wiring reached both interfaces and
  the service task - a narrow claim on purpose, because the round trip is pinned by
  *programs*: the sweep now stages 10 of the 14 std-side programs and passes
  **10/10**, including a full TCP connect/accept/read-write round trip over the
  stack (`t-tcp`, `t-tcp-listen`), UDP over the loopback (`t-udp-loopback`), and an
  `AF_UNIX` name (`t-unix-rustix`) - the pin job 4 chose instead of a kernel-side
  ping hook.
  **The flip found three stale assumptions in shared code, all of the same shape
  ("`net` is x86_64's").** `close(2)`'s socket teardown (`close_last_socket`) and
  `unlink`'s `unix::unbind_node` were gated `not(target_arch = "aarch64")`;
  measured, the first was a *leak*: `t-socket` bound 34567 and exited, and the next
  program's bind of that port answered `EADDRINUSE` on a machine that had just
  booted, because a socket left in the stack's set keeps its port - `t-tcp` and
  `t-tcp-listen` both failed for it and both pass with the gate gone. The third was
  `ioctl`'s `ifreq_index_named`/`ifreq_flags`, whose aarch64 arms answered `None`
  (so every interface ioctl fell to `ENODEV`) because "no net stack to report link
  facts on this machine yet"; they now read the live `net::link_facts` on both
  machines. The harness had to learn the same lesson from the other side: `/tmp` is
  a **tmpfs** on the second machine now, exactly as the x86 `/init` mounts it, and
  that is not decoration - an ext4 directory (the disk root) cannot hold a socket
  node, so `t-unix-rustix`'s bind answered `EOPNOTSUPP` from
  `create(name, SOCK_FILE)` until the mount was there (measured both ways). What the
  `aarch64` group still names is harness-only: the two programs that run another
  program out of `/bin`.
  **Stage 8's input transport over MMIO - DONE (the input-transport commit):** the
  same MMIO path reaches a *fourth* device class. xtask attaches a
  `virtio-keyboard-device` (a self-contained virtio-input device with a built-in
  keyboard backend, so it presents its event queue and config headless with no
  external chardev); `check_input` finds the window by `DeviceID` 18
  (`VIRTIO_ID_INPUT`) and `mmio::probe_input` opens it as a `VirtIOInput`, requiring
  the transport to negotiate the queues **and read its configuration-space identity**
  (`ids()`), so the milestone proves the MMIO transport moves config bytes for a fourth
  device class - the same config-read step the net milestone took with its MAC.
  `M8_INPUT_OK`. Like `check_net`'s MAC read
  this proves the *transport reaches the class*, not that events flow: the evdev
  nodes, `/dev/input` registration and the input pump stay x86_64's (`drivers::virtio::input`
  is PCI), so `/dev/input` remains absent on aarch64 (a named gap in the `/dev` set),
  and a rejected/unfound input device reuses `M8_INPUT_FAILED` (a terminator) to
  abort honestly.
  **Stage 7's disk-root primitive - an ext4 mounted over the MMIO block - DONE (the
  fs commit):** `mmio` also implements `crate::blk::BlockDev` for the MMIO block
  (`MmioBlk`: sector read/write/flush + capacity through the neutral `Hal`), and
  `check_fs` opens every block window and takes the one `Ext4Fs::mount` accepts - so
  `M7_FS_OK` means the whole block-driver -> filesystem path reads over MMIO. xtask
  lays the ext4 image down with `mkfs.ext4 -d <dir>` (unprivileged, no mount) and
  `^has_journal,^metadata_csum` - the same two features the driver refuses on the
  x86_64 root - making e2fsprogs a `boot-aarch64` prereq. The two block devices are
  told apart by the *kernel's* own scan - `find_filesystem` reads the protective
  MBR, the GPT and the ext4 superblock, so a window that carries a filesystem is
  the machine's root and one that carries none is where the write path runs -
  rather than by a pattern the build packed (§15.34). `check_fs` then writes its
  own marker through the mount and reads it back (root -> create -> write_at ->
  read_at -> unlink on the `FileSystem`/`Inode` traits - no fd table or task
  needed at boot), so the check covers the full block-driver -> ext4 -> file path
  in both directions without depending on a file the image has to carry. What
  remains is pivoting to that filesystem
  as the root (§13.3, done), the smoltcp stack over the MMIO net device and
  its socket syscalls (§13.4, done), and then the aarch64 `/bin` and
  `cargo xtask test --arch aarch64` (§13.5/§13.6, done) - the console's own
  interrupt arriving with §13.7. (Debt to name: this boot-time mount reaches from the machine
  layer into `crate::vfs`; `depgraph` does not flag it - there is no `vfs -> arch`
  back-edge, so no new mutual pair - but the disk-root check is a candidate to move
  to the `main` wiring layer, where the initramfs root is already mounted.)

  **Stage 7's `/dev` - the device set a real init mounts - DONE (the devfs commit).**
  The prior *attempt* reached for `standard_devfs()` from an early boot `check_dev`
  and **KERNEL-PANICed in `vfs::tty::global()`** ("ttys exist only after
  `vfs::tty::init()`") - the console tty is registered during the init handoff, not
  at that milestone. The lesson was ordering, not a pty/`vcmux` wall: the `tty1..N`
  and `ptmx` nodes mint ordinary pty pairs (`vfs::pty::vc`), which are
  arch-independent, so the only x86-only references in `standard_devfs()` are the
  virtio-PCI `hwrng`/`hvc`/`dsp`/`fb0`, `/dev/input` + `uinput` and the DRM card.
  Those five registrations (and the `HwrngDev` adapter that wraps the PCI rng
  handle) are gated into `#[cfg(target_arch = "x86_64")]` blocks, `pub mod devices`
  is opened to aarch64 the way `virtio` was, and the `mount -t devtmpfs`/`devfs`
  dispatch arm in `syscall/linux/fs/file.rs` drops its `#[cfg(not(aarch64))]` - so
  a real init's `mount(2)` now builds `/dev` on this machine instead of getting
  `ENODEV`. The pin is `check_dev` moved into the **init handoff**, after
  `vfs::tty::init()`. It goes one step further than building the set: it creates
  a `dev` directory on the ramfs root, mounts `standard_devfs()` there, and then
  reads `/dev/urandom` and `/dev/hwrng` by *path* through the live mount table
  (`vfs::resolve -> read_at` - the same walk a userspace `open` makes), so the
  pin proves `/dev` is actually on the filesystem, not merely that the builder
  runs. This is the kernel-side mirror of Linux's `CONFIG_DEVTMPFS_MOUNT`: the
  machine that builds a devtmpfs also hangs it at `/dev`, so a userspace init
  finds the device set already mounted rather than having to make it. Both reads
  assert their full buffer - `/dev/urandom` (the pool `rand` seeded from the MMIO
  rng) 16 bytes, `/dev/hwrng` (the MMIO virtio-rng `check_rng` installed, reached
  through the same `fill`) 8 host-generated bytes. `M7_DEV_OK` is on the wire.
  `/dev` is then reached by *userspace*, not just the kernel: the `/musl`
  static-musl ELF that `boot-aarch64` already runs at EL0 now `open`s and
  `read_exact`s `/dev/urandom` and `open`s (write) `/dev/console` through libc file
  I/O, printing `MPOS_DEV_URANDOM_OK` / `MPOS_DEV_CONSOLE_OK`, and traverses it with
  `read_dir`/`metadata` (getdents64 + fstat), printing `MPOS_DEV_GETDENTS_OK` /
  `MPOS_DEV_STAT_OK` — all four asserted. It then runs a full **regular-file
  lifecycle** on the mounted root ramfs (`write` → `read` back → `metadata` →
  `remove_file` → confirm gone), printing `MPOS_FILE_OPS_OK` (also asserted) — the
  create/write/read/stat/unlink a shell and any init issue against the filesystem
  proper, not only against device nodes. A `getcwd`/`chdir` round-trip (`cd /dev`
  and back) prints `MPOS_CWD_OK` (also asserted) - the `pwd`/`cd` pair a shell uses
  at startup and on every directory change. A `symlink`/`readlink` round-trip
  (`ln -s /dev`, read the target back, remove) prints `MPOS_SYMLINK_OK` (asserted) -
  the SYMLINKAT/READLINK handlers a loader or shell completion performs. A
  `mkdir`/nested-file/`rmdir` round-trip (`create_dir /mdir`, write+read a file
  inside it, remove the file, remove the directory, confirm it is gone) prints
  `MPOS_MKDIR_OK` (asserted) - the MKDIRAT/UNLINKAT(+AT_REMOVEDIR) directory half an
  init uses to set up `/tmp`, `/var`, and the empty-directory gate (rmdir refusing a
  full one) held on this machine. A `rename`/`hard_link` round-trip (write `/mrn_a`,
  rename to `/mrn_b`, confirm the old name is gone and the body moved; hard-link
  `/mrn_c` to `/mrn_b`, then remove `/mrn_b` and confirm the bytes still read through
  `/mrn_c`) prints `MPOS_RENAME_LINK_OK` (asserted) - the RENAMEAT/LINKAT `mv`/`ln`
  pair, and the surviving read after the first name's unlink is the counterfactual that
  proves linkat shared one node rather than copying one (which is what rename does). An
  `ftruncate` round-trip (write ten bytes, `set_len(4)` then `set_len(0)`, each
  confirmed by the stat length) prints `MPOS_TRUNCATE_OK` (asserted) - the `: >file`
  and `truncate` length handler, so a trim that left stale bytes or a stale length
  would fail rather than pass. A **1 MiB heap buffer** (past musl's malloc threshold, so
  served by `mmap`) written one byte per page and read back prints `MPOS_MEMORY_OK`
  (asserted) - the anonymous/mmap demand-paging path proven from a running process, which
  is a *memory* handler rather than a filesystem one, closing the last facet of the
  "core fs/memory handlers" deliverable that was still only inferred from dispatch. An
  **fd-duplication** round-trip (`File::try_clone`, which is `fcntl(F_DUPFD_CLOEXEC)`,
  reading the file's bytes back through the duped descriptor) prints `MPOS_DUP_OK`
  (asserted) - the fd-table primitive behind a shell's `>` redirection and `|` pipes. A
  **struct-stat ABI** check (chmod `0o640`, then `st_mode` reads back `0o640`, the file
  reports `is_file()` with the right `len()`, `st_nlink`/`st_ino`/`st_blocks`/`st_size`
  read back at the aarch64 offsets, `/dev` reports `is_dir()`, and a symlink reports
  `is_symlink()` (`S_IFLNK` via `lstat`)) prints `MPOS_STAT_OK` (asserted) - pinning that `write_stat` lays the struct out at the
  **aarch64 asm-generic offsets**, not x86_64's. This fixed a real ABI bug: the previous
  x86-only offsets put `st_mode` where musl reads `st_nlink`, so every stat-based
  file-type/permission read on aarch64 returned another field's value (a regular file
  reported `st_mode == 1`) - a wrong-shape answer that no earlier marker exercised, because
  the file-lifecycle and `/dev` proofs read only `st_size` (offset shared by both ABIs) and
  existence. `abi.rs` `write_stat` now branches the field order on `target_arch`, x86_64
  byte-for-byte unchanged (the host `write_stat_lays_out_struct_stat_at_the_x86_64_offsets`
  test and the 142-suite `stat` users still cover it); aarch64 is pinned on the machine.
  This is the same `open`/`read`/`write` a shell issues against the device set - so the
  mounted `/dev` is proven usable from a running program, not merely resolvable
  kernel-side.
  x86's devfs is byte-for-byte unchanged (host 511, `cargo xtask test` 142/142),
  and `boot-aarch64` runs every marker through `INIT_EXITED` with `M7_DEV_OK`
  among them. The set is still the honest subset: no `/dev/input`/`uinput` (the
  virtio-PCI input bus and `uinput` are x86_64's), no `fb0`/`dri` (DRM is x86_64's),
  no `/dev/rtc` (CMOS is x86_64's) - each a named gap, not a stub. What still has
  to land for a typed shell is a *userspace* init that execs `ash` against the
  `/dev` the kernel now mounts. That init must exec a *static* shell binary: the
  kernel does not load a `PT_INTERP` dynamic interpreter (a measured fact -
  nothing in the exec path acts on `interp`), so Alpine's shared-musl busybox will
  not run; a static aarch64 busybox (cross-built or a static prebuilt) is the
  prerequisite. `/proc` is a further, separate blocker (it is gated behind the net
  and PCI subsystems - see the next note), not needed for `ash` to start.

  **`std::process::Command` spawn data-aborts on aarch64 - cause narrowed by a
  reproduced fault trace.** An
  attempt had the running musl helper call `std::process::Command::new("/leaf").output()`
  - the pipe + `dup2` + fork + `execve` + `waitpid` sequence a shell uses to run a
  command. The markers printed up to the spawn (`MPOS_DEV_*_OK`, `MPOS_DEV_STAT_OK`) and
  then the process died with a userspace **data abort** (`user fault ... ESR 0x9200004f`),
  not a clean errno. The change was reverted (no crash line shipped; boot-aarch64 stays
  clean). An earlier draft of this note blamed missing fork-COW - that was **wrong**:
  the aarch64 machine already carries the full COW machinery (`sched::fork` uses
  `map_cow`/`set_cow_bit`; `arch/aarch64/paging.rs` implements `map_cow`/`resolve_cow`/
  `is_cow_page`, and `el0_sync` already calls `sched::resolve_cow_fault`). So the abort is
  a *narrower, unresolved* fault on a specific address, not absent COW support.

  A throwaway re-run captured `el0_sync`'s own fault line, which pins the region rather
  than theorising about it: `user fault at 0x3fe1bf80 (ESR 0x9200004f, pc 0x276350)`.
  Decoded, `ESR`'s `DFSC` is `0x0f` (a **level-3 permission fault**) with `WnR` set (a
  **write**), and `pc` is inside the static-musl binary's own `clone` trampoline.

  A third re-run instrumented `handle_clone` itself and that corrects the two earlier
  drafts, which wrongly blamed an *unmapped* caller-supplied child stack. The request is
  `raw_flags=0x4111` - `CLONE_VM|CLONE_VFORK|SIGCHLD`, i.e. **vfork** - with
  `child_stack=0x3ffd05e0` *inside* the process's already-mapped stack window and
  `parent_sp=0x3ffcef90`; the faulting address is `0x3ffcef80`, exactly `parent_sp - 16`.
  So it is the **parent's own stack** page that faults, not the child and not an unmapped
  region - and the `MPOS_MEMORY_OK` 1 MiB `mmap` proof from the same run had already ruled
  out any general `mmap`/VMA breakage.

  The mechanism: `clone()` routes a `CLONE_VM`-without-`CLONE_THREAD` request to
  `spawn_copy` (the fork path), which does a full **copy-on-write** fork - it builds a
  private child space and read-protects the *parent's* writable pages via
  `set_cow_bit_quietly` (`runtime.rs:7480`) - and the design deliberately does **not**
  suspend the vfork parent (the doc at `runtime.rs:7994` assumes the `posix_spawn` caller
  waits on the child itself). A real vfork must *share* the address space and hold the
  parent until the child execs; here the child instead gets a private copy while the
  parent keeps running and immediately writes its now-read-protected stack just below
  `sp`. The earlier hypotheses are now eliminated with evidence. (1) COW-split *math*:
  `set_cow_bit_quietly` ORs `COW_BIT | AP_READ_ONLY` onto a user page whose `AP` is
  `AP_EL0_RW (0b01)`, yielding `AP_EL0_RO (0b11)` - EL0 keeps read access, only writes
  split (`paging.rs:92-98`, `1237`); `map_cow` maps the child copy `EL0_RO | COW_BIT`
  (`1106-1124`); the hand-encoded `fork` child exercises that and reaches EL0. (2) "parent
  `TTBR0` never reloaded after the child's turn": plain fork also runs the child first and
  works, so resume-reload functions. (3) a stack-growth / `stack_top` window bug: the
  `current_stack_range` value that looked far from `sp` is just the task's *kernel* stack
  (`runtime.rs:5958`), not the user window.

  The instrumented fault dump settles it: the fatal fault is a **read** (`is_write=false`,
  `ESR` DFSC `0x0f` = level-3 **permission**) at `sp - 16`, and the faulting leaf descriptor
  is `AP = 0b10` (**EL1 read-only, EL0 no access**) with `COW_BIT` set. That is a *symptom*,
  not a paging one-liner: a page carried into the child with `AP 0b10` never had the EL0
  (`USER`) bit, so the earlier "confirmed aarch64 paging fix" idea is wrong - the copied
  page faithfully reflects a non-user mapping the child should never have been reading in
  the first place. Why the vfork child touches a non-user page is the real cause: **`vfork`
  requires `CLONE_VM` sharing** - the child runs the parent's C code against the parent's
  live stack and pointers until it `execve`s - but `clone()` routes `CLONE_VM`-without-
  `CLONE_THREAD` to `spawn_copy`, which gives the child a *private* COW copy and does not
  suspend the parent (`runtime.rs:7407-7434`; comment at `7994`). Under a private copy the
  child's parent-relative pointers resolve against the wrong tables and land on a kernel
  page, faulting on read. So this is not a paging bug at all: it is **`CLONE_VM`/
  `CLONE_VFORK` fork semantics that are unimplemented** - a genuine multi-step feature in
  the shared fork/scheduler path (share the address space for `CLONE_VM`; suspend the parent
  on `CLONE_VFORK` until the child execs/exits), which x86 does exercise (its std forks, not
  vforks) and so the aarch64 gap is invisible there.

  Concrete implementation route (worked out, not yet executed): reuse the thread
  sharing mechanism rather than invent one - give the vfork child `space: None` and an
  `mm` whose `leader()` is the parent, so `space_owner` (runtime.rs:2134) resolves its
  address-space access to the parent's tables, and the parent alone owns/frees that space
  (no double-free when the child exits, since `Task.space` is `None`). Give the child its
  own `tgid`/pid so it stays a separate, `waitpid`-collectable process. This alone is NOT
  sufficient and NOT safe: with `CLONE_VFORK` the parent must be *suspended* until the
  child `execve`s or exits (otherwise both mutate the shared stack - the isolation break
  that makes a half-fix worse than the gap), which needs the scheduler to not `push_ready`
  the parent and a wake-parent hook in the child's `execve` and `exit`. On `execve` the
  child must detach: `execve` already builds a fresh address space for a process exec, so
  the child's `mm.leader` must be repointed to the child at that point (a thread never
  execs, so today's exec path may not exercise this case - it must be verified, not
  assumed). The gate is: a `MPOS_SPAWN_OK`-style proof from the running musl helper
  (std::process spawn returns), plus `cargo xtask test` 142 (x86 untouched - it forks, not
  vforks) and `boot-aarch64` green through `INIT_EXITED`. It was not attempted here only
  because that cross-subsystem work cannot be completed *and* shown correct on both gates
  within the budget, and a partially-correct version would break `fork` isolation or
  double-free a shared space.

  This is the named remaining blocker for running a *subcommand* (a shell's core) from
  `cargo xtask run --arch aarch64`, and is orthogonal to the static-busybox prerequisite
  above.

  Reading further confirms the COW path is not the bug: `resolve_cow`'s split leaf
  (`VALID | TABLE | attributes(flags)`, `paging.rs`) is byte-for-byte the same encoding
  `map_page` uses for a normal leaf, and `COW_BIT` is a software bit (55) the walk
  ignores - so the aarch64 paging COW primitives are internally consistent. The notable
  consequence is that aarch64 COW-*split* is **untested by the port itself**: the
  hand-encoded `clone(SIGCHLD)` milestone only proves a child reaches EL0 and issues a
  syscall; it never *writes a heap/stack page of the forked address space*, and every
  musl fork test runs on x86. The concrete next cut is therefore a hand-encoded aarch64
  milestone in which the fork child writes a COW page and the parent verifies isolation -
  that both pins COW-split and, if it faults, localises the std abort to COW with a
  traceable region instead of leaving it inferred.


  **`/proc` stays honestly refused on aarch64 - and it is gated, not one line from
  serving (corrected by an attempt).** An earlier note here said `vfs::procfs`
  compiled on aarch64 and the `mount -t proc` arm was a single `#[cfg]` from opening.
  Measured false: `procfs` is `#[cfg(target_arch = "x86_64")]` in `vfs/mod.rs`,
  because it reads `crate::net`'s socket table and `crate::drivers::pci` config - two
  x86_64-gated subsystems - so the module is absent on aarch64 entirely. Attempting to
  route `/proc/cpuinfo` through a new `hal::Cpu::cpuinfo` capability (to avoid the
  hardcoded `cpu family : x86_64` string, a wrong shape on ARM) hit a second wall:
  `hal` is `#[cfg(target_os = "none")]`, so `crate::hal` is absent in the host build
  where `procfs` *does* compile (host is x86_64-linux-gnu) - a procfs body cannot call
  the machine capability. The change was reverted (no shipped partial-`/proc`, no
  broken build). The real gap, now precisely named: **`/proc` on aarch64 is blocked
  behind the `net` stack and PCI/virtio enumeration that `procfs` depends on** - i.e.
  largely the same un-gating work as the `net` stage - not merely a `cpuinfo` fix. Once
  `procfs` is made to compile without net/pci (cfg-gating `/proc/net` and the PCI-backed
  nodes, the same shape as `drivers`'s prep commit), the `cpuinfo` needs a `MIDR_EL1`
  read reached *without* naming `arch` from `vfs` (a per-arch `cpuinfo` inside procfs
  itself, reading `MIDR_EL1` via `core::arch::asm` behind `target_arch`, or a capability
  that also exists on host). Named, not papered over. **Both halves landed later** -
  `/proc` was un-gated when the `net`/PCI stages gave the second machine a stack
  (§13.12), and `cpuinfo_text` is the per-arch rendering this paragraph suggested,
  reading `MIDR_EL1`/`ID_AA64*` behind `target_arch`. It had no pin until
  **`t-cpuinfo`** (2026-10-27): the first line, the x86 `model name` on one machine
  and `CPU implementer`/`CPU part`/`CPU architecture: 8`/`Features` on the other, and
  the *absent* field each machine must not carry. The aarch64 arm runs in the sweep,
  so the shape is checked by the machine rather than inferred from the source.




- **The `-kernel` bring-up was an instrument, and it is gone.** Stages 1b-1d
  entered at a fixed physical address with the MMU off, because that is the
  smallest thing that proves the toolchain, the linker script and the machine
  body agree. Stage 1e deleted it and boots from the ESP through AAVMF, which is
  the promise this section makes: a second machine with the *same* boot path,
  not a shortcut past it. Deleting it meant more than removing a QEMU flag -
  the image had to become relocatable (`relocation-model=pic`, `-pie`, a script
  with no base address), the stack had to move from the script's `.stack` to one
  the loader allocates, and `_start` had to stop being an assembly stub. What is
  left of the instrument is one entry in `AGENTS.md`'s trap table saying not to
  bring it back.
- **The exception level is measured rather than assumed, and it survived the
  move to the real boot path.** QEMU 11.1.1 with `-M virt` and a bare aarch64 ELF
  entered the `-kernel` image with `x0 = 0` and `sp = 0` (the banner printed
  `x0`, and QEMU's own `-d cpu` log agreed). The AAVMF path is a different
  machine in every respect - a UEFI application is entered with the firmware's
  calling convention, not a register of our choosing - and it lands at **EL1**
  as well, which is what the kernel's `CurrentEL` check now reports and what its
  EL1-only vectors and physical timer need. A machine that arrived at EL2 would
  stop with a line saying so rather than configure the wrong one.
- **The tree is read now - and it does not arrive in a register, nor under the
  name this section used to assume.** `x0` is the handoff record's register on
  both machines. Under `virt` + AAVMF the tree is published in the **UEFI
  configuration table**, which is firmware data that does not survive
  `exit_boot_services`, so the loader reads the header's `totalsize` there and
  copies that many bytes into frames of its own (stage 5a). Two corrections came
  out of doing it, and both were measured rather than reasoned. First, that
  table has **no entry for a tree at all** when QEMU builds ACPI tables, which
  `-M virt` does by default on aarch64: the firmware advertised `ACPI2`
  (`8868e871-e4f1-11d3-bc22-0080c73c8881`) among the nine tables the loader
  prints, and no device-tree GUID. The boot therefore selects
  `-machine virt,acpi=off`, which is the same machine with the other
  description of itself, and the tree is the one this port reads. A real aarch64
  server would hand over ACPI and no tree; reading *that* is a gap this port
  names rather than half-does, and `check_devices` failing by name is what makes
  it visible instead of silent. Second, the cost: `totalsize` is 1052672 bytes
  for a tree of 7160 bytes of structure and 569 of strings, and copying what the
  tree says it is costs one MiB of the machine - the difference between `192
  MiB` and `191 MiB` of usable RAM in the log above.
- **The GICv2 bring-up is three bits across two frames, and the wrong value is
  silent.** Measured on this machine, and recorded because the register values
  are load-bearing: the timer's line needs `GICD_CTLR` *and* `GICC_CTLR` bit 1
  set - group 1 is what an IRQ *is*, so a line left in group 0 with only bit 0
  written is pending forever and raises nothing - and `GICC_CTLR.AckCtl` (bit 2)
  as well. Without AckCtl the acknowledge reads **1022** instead of 30, which by
  the GICv2 specification is its answer when the pending interrupt is group 1
  and the acknowledging access is secure; the interrupt is therefore never
  retired, `nIRQ` stays asserted, and the vector is re-entered every few hundred
  nanoseconds. That is not "the clock is broken" from the outside - it is a
  clock that looks *very fast*, and it cost a 1022/30 reading to tell apart.
  Linux programs the same `0x7`. The check's failure branch now prints the five
  registers that separate the three causes.
- **The machine addresses are hardcoded, and now they are checked at every
  boot.** QEMU virt puts PL011 at `0x0900_0000`, the GIC at
  `0x0800_0000`/`0x0801_0000` and RAM at `0x4000_0000`, and those are what the
  kernel is compiled with: the tree does not yet *supply* them. What stage 5a
  made it do is ask whether they are still right - `check_devices` reads each
  node back and compares it with the *physical* bases the kernel keeps for that
  (`pl011::PHYS`, `gic::DIST_PHYS`/`gic::CPU_PHYS`, `gic::TIMER_INTID` - the
  addresses the kernel *reaches* those registers at are stage 5b's
  `pl011::BASE`/`gic::DIST`/`gic::CPU`, and they are deliberately different
  numbers now), and the boot stops with `M4_DEVICES_FAILED` on any
  disagreement. A machine that moves a device is now a boot failure rather than
  a driver faulting on its first register. What is still hardcoded is everything
  with no constant to compare against, and stage 5b took the direction that
  matters for discovery: the 32 virtio-MMIO windows, whose addresses are in the
  tree and nowhere else, were **counted and bounds-checked** and are now
  **read** - `check_virtio` walks them, translates each into the kernel half and
  reads the transport's own identity registers, so a window with nothing behind
  it is a named `M5_VIRTIO_FAILED` rather than a driver that times out later. Two
  things about that read are measurements rather than assumptions, and both are
  recorded because a driver inherits them: the transports answer **version 1**
  (the legacy register map) unless the machine is told
  `-global virtio-mmio.force-legacy=false`, and the tree cannot say which window
  has a device - all thirty-two nodes are published either way, and the attached
  device answers from the *last* window, not the first. Still missing is the
  routing of anyone's interrupt line: the PL011's own is SPI 1 (INTID 33) and
  nothing decodes it, because the console is a polled writer until stage 4.
  The window deliberately does not start at zero - a null pointer that silently
  works is the one thing x86_64's own `check_arch` asserts cannot happen.
- **The kernel's address space is its own now, and the window that was open
  across 1e is closed.** Two things went together and are worth keeping as one
  measurement: the loader builds its identity map and its direct map with a
  single call and a single set of flags, so on the loader's tables both come out
  `rwx` - a window x86_64 closes the moment its kernel installs its own tables
  and stage 1e could not. Stage 1f closes it: the plan gives the direct map
  `READ | WRITE`, the boot log prints `direct map ... rw and it stores`, and the
  check walks every one of the 63616 pages the plan names to say so. What the
  loader's tables are still needed for is the length of the jump: the kernel
  cannot build its own until the frame allocator is up, and the frame allocator
  needs the direct map.
- **The device window is in the kernel half now, and the low half is empty.**
  Stage 5b closed this one, and it was closed by changing where the registers are
  reached rather than by naming the collision better: the kernel mapped 64 MiB
  from `0x0800_0000` - PL011 and GIC - at the addresses the devices physically
  are, which is inside `TTBR0`, the half a process is going to own. It maps them
  at `0xffffffff40000000` instead, the same hole x86_64's `mm::MMIO_BASE` uses,
  and `check_paging` now asserts both halves of that: the alias translates to the
  right frame as Device, and neither the image's physical address nor the device
  window's is reachable any more. What is *not* closed is the mechanism: the
  window is a fixed 64 MiB range mapped whole at boot, because `mm::map_mmio`
  goes through `hal::Paging` and `hal` cannot compile for this machine until the
  last of its six traits exists (`Paging`'s user half and a `WallClock` are what
  6b leaves), so the dynamic allocator cannot exist.
  A device outside the tree's window is refused by `paging::device_virt` rather
  than mapped, which is the honest answer for a machine whose devices are a
  firmware-described range - and not the general one a PCIe aarch64 server needs.
- **Stage 1d's RAM window is gone; the map is the firmware's and the plan's.**
  The 1 GiB ceiling it mapped "because the code that would care had not arrived"
  is replaced twice over: stage 1e's loader walks the EFI memory map region by
  region, and stage 1f has the kernel rebuild the address space from that record
  by way of `mm::vm::kernel_mappings`. This boot reports `14 regions, 192 MiB
  the allocator may use` out of 248 MiB of RAM, `pmm: 5 zones, 49209 frames`.
  The floor the whole thing stands on is `mm::init`, which is machine-neutral
  and simply had no aarch64 caller until 1f gave it one.
- **`hal` compiles for aarch64, and the "two blockers" this note used to name were
  an understatement.** Until stage 6f the module was x86_64-only for a *structural*
  reason this ledger missed: `hal::Current` gated on `target_arch = "x86_64"` so the
  module could not even be named here, and four capability groups were x86-shaped
  *inside* the baseline traits. A caller survey found them (MSI, segment selectors,
  the lazy-FPU trap protocol, and the first-user-entry shape) and that every one of
  their callers lived in a module aarch64 does not compile yet. So rather than make a
  second machine fake answers (`AGENTS.md` §3 rule 4), stage 6f split them into three
  *optional* capability traits - `Msi`, `FpuTrap`, `Segments` - that only x86's
  `Current` implements, un-gated `Current` to `target_os = "none"`, and had
  `arch/aarch64` write the six baseline impls. The two things this note claimed were
  the *last* blockers were in fact the easy half: the running-process `Paging` methods
  (6c), the `WallClock` (6d), and `Cpu`/`Interrupts` primitives (6e) - each a real
  body read back against the hardware it names, not a stub. `crate::hal` now compiles
  for aarch64 under `cargo clippy --target aarch64-unknown-none`.
  What remains is *below* the gate, not blocking it: the trait `enter_user`/`resume_user`
  and `set_kernel_stack` share the `enter_frame` that `check_el0` now exercises, but
  the running-process caller that schedules tasks is stage 3; `to_user_regs`/`from_user_regs`
  write the aarch64 `user_pt_regs` order but their field-order pin against `ptrace` is
  stage 6's ABI; the copy-on-write fault resolver (the next bullet) needs a faulting
  task. The then-x86-only `Msi`/`FpuTrap`/`Segments` are *not implemented here on purpose* -
  a `virt`+GIC machine has no MSI controller, no lazy FPU and no segments, and the
  sched/runtime code that wants them un-gates at stage 3 with its own arch branch. (Of
  those, `Segments` was removed outright in §13.2 - `hal::Gate`'s frame ops read the
  selectors from the GDT themselves - so `Msi` and `FpuTrap` are what this sentence still
  describes.)
- **A copy-on-write page can be resolved, and nothing faults into the resolver
  yet.** Stage 6b landed the operation - `resolve_cow` copies the frame, rebinds
  the descriptor and answers `(old, new)` - and the check drives it in the order a
  `fork` would, but the other half is deliberately absent: a data abort from EL0
  on a read-only page has to be recognised as a copy-on-write fault, resolved and
  the faulting instruction retried, and today the EL0 vector treats anything that
  is not an `svc` as fatal. That belongs with the `Paging` methods that take a
  *running* process - `is_cow_page` is what such a handler asks first - which is
  the other remaining `hal` blocker, so no code was written for it here and a page
  in a user space cannot yet be shared by two processes that are both running.
- **No ACPI consumer exists to reuse.** `virt` + AAVMF passes a *device tree*,
  so device discovery is new DTB-parsing code in stage 5, not a port of an
  existing table walk.
- **`rand` has no aarch64 hardware source yet**, so `seed()` takes its
  deterministic arm there; `RNDR` (where present) and the boot-epoch mix land
  with stage 5's drivers.
- **A host-compiled module is excluded from the aarch64 build without being
  excluded from the host.** `sched`/`vfs`/`syscall`/`uaccess` are the host unit
  suite's, so their gate is `any(not(target_os = "none"), target_arch =
  "x86_64")` - "the host, or x86_64 bare metal" - not `target_arch =
  "x86_64"`, which would have silently deleted 434 host tests while compiling
  cleanly.

**The older prerequisite list, kept for the record** (all six still stand, and
the stage table above is where each lands): the `VBAR_EL1` exception vector
table, a GICv2/v3 driver, 4 KiB pages through `TTBR0_EL1`/`TTBR1_EL1`, PSCI for
reboot/shutdown, the generic timer, and a PL011 or SBSA UART.

### 9.2 RISC-V Support

- SBI interface for boot/runtime services
- PLIC interrupt controller
- Sv39/Sv48 page tables
- `mtime`/`mtimecmp` timer
- UART via SIFIVE_UART or NS16550

---

