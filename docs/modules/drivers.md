# drivers

**Home:** `src/drivers/` · **Owns:** device discovery, the virtio transports and the devices they carry, and the `/dev` node set userland is handed

## What it owns

This module is the seam between the kernel above it and the paravirtual machine below it. It owns
three things that are easy to conflate and are deliberately kept apart: *finding* a device (PCI
config space on x86_64, a memory-mapped `virtio,mmio` window on aarch64), *driving* the device
(the per-class driver, built over `virtio-drivers`), and *naming* the result as a node in `/dev`
that the VFS, the block layer and the network stack can then open by name. Everything above the
seam holds a device through a contract and never through a bus; everything below it is a machine
fact.

The module's own header (`src/drivers/mod.rs`) draws the boundary the rest of this page expands.
Only `ahci` and `pci` are x86_64's machine outright — they are port I/O, `uart_16550`, PCI
enumeration and AHCI, and they are `#[cfg(target_arch = "x86_64")]`-ed out (a no-op there, not a
stub). `rtc_dev` is gated on `target_os = "none"`, because what its node reads and writes is
`crate::hal::WallClock`, which both machines implement; the only x86-shaped thing about it was the
`target_arch` gate that used to hide it. `uinput` is gated on the `mpos_input_registry`
*capability*, not an architecture. `devices` and `virtio` compile on both machines, and the reason
is the point of the seam rather than an accident: the `Hal` impl the virtio crate builds on
(`virtio/hal.rs`) is machine-neutral — the MMIO-mapping call that used to be x86_64-only became
`crate::hal::map_mmio_window` — so the aarch64 MMIO transport builds on the same memory layer, and
`devices.rs`'s devfs is a generic node set with the still-x86-only nodes `#[cfg]`-ed out *inside*
it. What the two machines share is a contract; what differs is which bus answered.

The kernel does not reimplement VirtIO 1.0. The `virtio-drivers` crate owns the queues, the feature
negotiation and the configuration space; what is left to a driver here is *which device to open*
and *what the rest of the kernel calls the result*. Two pieces of the crate's work are this
module's: the `Hal` impl (`HalImpl`, in `virtio/hal.rs`) that hands the device memory it can reach
and turns a device-given address back into a pointer — short here because the kernel has a direct
map and no IOMMU — and the transport: a PCI transport over a BAR on x86_64, the MMIO transport on
aarch64. Both feed the *same* driver code, which is why the devices are one implementation per
class and not one per machine.

**Two traps are load-bearing here, and both are shapes in the code rather than rules to remember.**

The first is that a driver which holds its transport *inside* its device struct cannot be one
concrete `static`-held type shared by two machines. The crate's `Transport` is not object-safe —
`Transport::begin_init<F: Flags>` is a generic method and `VirtQueue::new` takes
`&mut T where T: Transport` — so `Box<dyn Transport>` does not exist. `AGENTS.md` §5 records that two
shapes work and that the compiler's error count does not tell them apart. This module uses both,
and which one a device gets is decided by whether something above it has to name the device type:

- *Open the device inside the driver module and hand the rest only a contract.* `virtio/mod.rs`
  declares `NetDevice`, `VsockDevice`, `P9Device`, `ConsolePort`, `EntropySource`, `SoundCard` and
  `InputDevice`; each concrete driver (`VirtioNet`, `VirtioVsock`, `Virtio9p`, `VirtioHvc`,
  `VirtioRng<T>`, `VirtioSnd<T>`, `VirtioInputDev<T>`) implements one of them over whatever
  transport it arrived on. The network stack holds `Arc<dyn NetDevice>` and never learns whether a
  BAR or a window carried the card; `net/vsock.rs`, `vfs/p9.rs` and `vfs/tty`'s console face are
  one copy each for the same reason. `VirtioRng<T>`, `VirtioHvc<T>`, `VirtioSnd<T>`,
  `Virtio9p<T>`, `VirtioVsock<T>` and `VirtioInputDev<T>` are generic over the crate's `Transport`
  precisely so a second bus gets its own `Transport` and nothing else.
- *Make the device generic and let its consumer hold a `Box<dyn YourDevice>`.* The GPU cannot be
  opened inside a module that hands back a contract, because the render task is its sole owner and
  the only caller of `flush`; so `virtio/gpu_ctrl.rs` defines `Gpu<T: Transport>` and the
  `GpuDevice` trait (the command surface the task actually sends), and `video/render.rs` holds the
  device as a `static DEVICE: Mutex<RefCell<Option<Box<dyn GpuDevice>>>>`. The type is erased at
  the consumer, not at the constructor.

The second trap is that a capability flag makes a port cheap to complete **only if everything
behind it is gated on the capability and not on the architecture**. `mpos_input_registry` is the
worked example, and this module is where the gating is visible: the same flag guards the
`virtio/input` module (`virtio/mod.rs`), the `uinput` module (`drivers/mod.rs`), the `/dev/input`
and `/dev/uinput` block in `devices.rs`, the input arm of `virtio::spawn_service`, and the
`uinput::close_at_exit` exit hook (`board/steps.rs`'s `register_exit_hooks`). A machine that flips
the flag gets the whole stack, because the modules, the node set and the exit hook all ask the
flag. `.cargo/config.toml` declares `mpos_input_registry` for *both* machines; the code, not this
page, is the record of what the flag gates.

What the module deliberately does **not** own: the pure ABI facts and vocabularies of the devices
it drives. The DRM ioctl vocabulary, the EDID and `IN_FORMATS` blobs, the cursor blending, the
evdev record window, the uinput request vocabulary, the AHCI task-file decisions and the GPT/MBR
partition walk live in host-testable leaf crates (`crates/gpu`, `crates/evdev`, `crates/uinput`,
`crates/ahci`, `crates/blk`), where a host build tests them; the driver modules are the kernel-only
half that dispatches over live device state and reaches registers a host does not have. The module
also does not decide *who may call it* — those edges are `docs/ARCHITECTURE.md` §2/§3.

## Key files

The module is one directory per concern: `src/drivers/` for the bus, the node set and the
machine-only drivers, `src/drivers/virtio/` for the transport seam and the device classes, and
`src/drivers/video/` for the render task.

### Bus and transport

| File | What lives there |
|---|---|
| `src/drivers/pci.rs` | PCI config space over mechanism 1 (`read8`/`read16`/`read32` and their writers, `config_read`/`config_write`), the `Func` address type (`bdf`, `io_bar0`, `command_on`), the `COMMAND_*` bits, `find`/`find_any`, and the one-shot bus snapshot `enumerate()` (`scan_bus`, `secondary_bus`, `read_function`, `read_bars`) that `init()` publishes. Only the *port I/O* is here; every decision about what was read is in `crate::pci`. `discover_ecam`/`read_table` also read the firmware's ACPI tables - through the root pointer the loader records - to find the memory-mapped configuration window MCFG names and report it, which is the measurement the switch off the port pair is built on (`docs/NOTES.md` §7.6) |
| `src/drivers/virtio/hal.rs` | `HalImpl` — the crate's `Hal` over the kernel's frame allocator and direct map (`dma_alloc`/`dma_dealloc`, `mmio_phys_to_virt` through `crate::hal::map_mmio_window`, `share`/`unshare`). Below it, the x86_64-only `pci_transport` module: `IoCam` (`ConfigurationAccess` over `pci.rs`), `pci_root()`, `transport()`/`transport_all()` (probe + command-on + `PciTransport::new` + `pci::claim`), `device_config()` and `claim_drm_card()` |
| `src/drivers/virtio/mmio.rs` | The aarch64 transport. Turns a `virtio,mmio` window into an `MmioTransport` and opens one device per class: `blockdev`, `init_net_global`/`net_device`, `gpu_device`, `sound_device`, `console_device`, `vsock_device`, `p9_device`, `input_device`, `rng_device`, plus the boot-probe helpers `read_blocks`/`capacity`/`write_readback`. `SECTOR`, `DEVICE_CONFIG_OFFSET` and the `NET_*` constants live here |
| `src/drivers/virtio/msix.rs` | MSI-X for a virtio PCI function, by hand: `enable` (walk the capability list, map the table, arm entry 0 at an IDT vector) and `set_queue_vectors` (bind every queue to that vector through the common-configuration window), plus `device_config_of` and the capability-walk helpers `find_cap`/`find_next`/`bar_address`/`map_structure`/`vendor_cap_window`. x86_64-only |

There is no `virtio/pci.rs`: the PCI transport is `hal.rs`'s `pci_transport` module (x86_64-only),
re-exported as `hal::{pci_root, transport, transport_all, claim_drm_card, device_config}`. The
module header that used to name a `virtio::pci` describes this arrangement.

### Block

| File | What lives there |
|---|---|
| `src/drivers/virtio/blk.rs` | `VirtioBlk`, the PCI virtio-blk as the block layer's `BlockDev` (`capacity_sectors`, `read_at`, `write_at`, `flush`). Holds `BOUNCE_SECTORS` and the `dma_safe` check that routes a caller's non-direct-map buffer through a bounce copy; `VirtioBlk::init` is the PCI probe |
| `src/drivers/ahci.rs` | `AhciDisk` and the whole SATA controller: `init` (scan by class code, `bring_up_controller`), the register file (`GHC`, the `PX_*` offsets, `rd`/`wr`, `wait_until`), `bring_up`/`stop`/`release` for the command list, FIS area, command table and `BOUNCE_PAGES` bounce, `issue`/`read`/`write`/`flush`/`identify`, and the `check_span` sector contract. x86_64-only |

`AhciDisk` and `VirtioBlk` both implement `crate::blk::BlockDev`; the MMIO counterpart is
`MmioBlk` in `virtio/mmio.rs`. How a disk reaches a `BlockDev` implementation and how its name and
tree position are decided are `virtio::register_block_disk`'s (see Contracts).

### Network

| File | What lives there |
|---|---|
| `src/drivers/virtio/net.rs` | `VirtioNet`, the PCI virtio-net as a `NetDevice`. `QUEUE_SIZE`/`BUFFER_BYTES`, the inherent `mac`/`rx_frames`/`poll`/`transmit`/`receive`/`ack_interrupt`, and `init` — which arms MSI-X through `msix::enable` + `msix::set_queue_vectors` after `crate::hal::init_message_signalled()` and falls back to pure polling if either step fails |

### Display

| File | What lives there |
|---|---|
| `src/drivers/virtio/gpu.rs` | `VirtioGpuDev` (the buffer view `/dev/fb0` and the page pins share — `Scanout`, `dims`/`advertised_dims`/`fb_phys`/`fb_len`/`resized`, `install`), the `CONFIG` address for the device's event word and `take_display_event`, `global()`, and the `FbDev` inode (its `mmap_frame`/`frame_backed`). `init_global` is the x86_64 probe |
| `src/drivers/virtio/gpu_ctrl.rs` | `Gpu<T: Transport>` — the control queue the kernel drives itself (`new`, `features`, `num_capsets`, `change_resolution`, `flush`, the 3D commands `create_3d`/`ctx_create`/`ctx_attach_resource`/`transfer_host_3d`/`submit_3d`, the cap-set walk) — the `GpuDevice` trait that erases the transport for the render task, and the `Frames` DMA block |
| `src/drivers/drm.rs` | The DRM/KMS UAPI: `DrmCard` (the two nodes), `DrmState`, `drm_ioctl`/`drm_request` and `arg_shape`, the KMS object model (one CRTC/encoder/connector/`PLANE_ID`/`CURSOR_PLANE_ID`, fixed ids and properties), dumb buffers, `ADDFB`/`ADDFB2`, page-flip and vblank events, atomic commits, blob and property stores, `PRIME`, the lease family, the `SYNCOBJ_*` family and the `DRM_VIRTGPU_*` family. Also the session side the render task and VCMUX talk to: `master_request`/`master_ok`/`master_held`/`is_master`, `publish_card`, `frame_landed`, `display_changed`, `grant_and_resent`/`re_present_now`, `master_closed`/`revoke_now` |
| `src/drivers/video/mod.rs` | The display module. The pure pixel math is `crate::term::blit` (host-testable); this file is the `#[cfg(target_os = "none")]` gate on `render` |
| `src/drivers/video/render.rs` | The render task: the single owner of the `Gpu` and the only caller of `flush`. `spawn`, the wake/condition flags (`notify`, `request_display_probe`, `request_re_present`), the two request/park handshakes (`job`/`Job`, `resize_blocking`), the `/dev/fb0` write-through (`request_flush_blocking`), `landings`, `running`, `render_pid` and the frame loop |

### Input

| File | What lives there |
|---|---|
| `src/drivers/virtio/input.rs` | The evdev registry: `EvdevCaps` and `read_caps`, the `EvdevNode` inode (its `'E'` ioctl surface, the record window), `INPUT_MAJOR`/`input_minor`, `register`/`init`/`nodes`, and `service` — the task half that harvests the rings and routes keyboard events to the active virtual console when a text console is live |
| `src/drivers/uinput.rs` | `Uinput`, the `/dev/uinput` inode: the per-open-file-description `Client`/`State`, the `'U'` request dispatch (`set_bit`/`setup`/`create`/`destroy`/`forget`/`ioctl`/`inject`), and `close_at_exit` for the exit path. The request vocabulary lives in `crates/uinput` |

### Other devices

| File | What lives there |
|---|---|
| `src/drivers/virtio/console.rs` | `VirtioHvc<T: Transport>` (the `ConsolePort`), `install`/`init_global`/`global`, and `HvcDev` — the blocking `CharDevice` face `/dev/hvc0` opens |
| `src/drivers/virtio/rng.rs` | `VirtioRng<T: Transport>` (the `EntropySource`), `install`/`install_probed`/`init_global`/`global`, and `entropy()` — the bytes the kernel seed folds in |
| `src/drivers/virtio/sound.rs` | `VirtioSnd<T: Transport>` (the `SoundCard`: `format`/`configure`/`play`, the OSS parameter ioctls, the period-sized staging buffer), the `Probe` outcome, `install`/`install_probed`/`init_global`/`global`, and `DspDev` — the `/dev/dsp` `CharDevice` |
| `src/drivers/virtio/fs9p.rs` | `Virtio9p<T: Transport>` (the `P9Device`: `mount_tag`/`request`), the `DEVICES` registry and `install`/`init_global`/`devices`/`by_tag` |
| `src/drivers/virtio/vsock.rs` | `VirtioVsock<T: Transport>` (the `VsockDevice`: the ten methods over the crate's connection manager), `VSOCK_PER_CONNECTION_BUFFER`, and `init_global` |
| `src/drivers/rtc_dev.rs` | `rtc_dev`'s `RtcDev`, the `/dev/rtc0`/`/dev/rtc` character device, and its `RTC_*` ioctl handling over `crate::hal::WallClock` |
| `src/drivers/virtio/mod.rs` | The seam itself: every contract trait, `VirtioInputDev<T>`, `entropy()`, `register_block_disk`, `PCI_DRIVER`, and `spawn_service` — the one permanent task that pumps the input registry, the vsock family and the GPU's display event |

### The node set

| File | What lives there |
|---|---|
| `src/drivers/devices.rs` | `standard_devfs()` — the whole `/dev` node set, and `publish`/`Publisher`, the registry a driver fills so the set need not name one. The **core set** is built there: the tty and console names, the virtual consoles, `pts`/`shm`, the `null`/`zero`/`full`/`kmsg` sinks, the entropy pair, the `/proc` symlinks, `ptmx`. The nodes that *are* a probe's result — `hwrng`, `hvc0`/`hvc`, `dsp`, `fb0`, `/dev/input/eventN` + `/dev/uinput` — are published by their drivers at the install door each one already had (`rng::install`, `console::install`, `sound::install`, `VirtioGpuDev::install`, `input::register`), and `standard_devfs` drains that registry without naming a driver. `rtc0`/`rtc`, the `/dev/dri` pair and the disk projection stay here: those subsystems exist whether or not a device answered, so their nodes are part of the interface rather than a probe. The `CharDevice` adapters that remain are `TtyDev`, `ConsoleDev`, `NullDev`, `KmsgDev`, `FullDev`, `ZeroDev`, `RandomDev`; `HwrngDev` lives in `virtio/rng.rs` with its node |

## Contracts

The contracts this module publishes fall into four groups: the virtio device contracts, the
transport seam, the block/inode contracts it implements, and the display handshake between the
DRM layer and the render task. The *edges* — who is allowed to name this module — are
`docs/ARCHITECTURE.md` §2/§3; this section is only what a caller may rely on.

**The virtio device contracts** (`src/drivers/virtio/mod.rs`). Each erases the transport and is
implemented once per crate driver, so a machine's bus stops at the impl:

| Contract | What a caller may rely on |
|---|---|
| `NetDevice` | `mac`, `rx_frames`, `poll`, `transmit`, `ack_interrupt`. `rx_frames` is what the stack advertises as its TCP window, because it answers "how much can arrive before something is dropped"; `poll` never claims a frame that is not there, and `transmit`'s `false` is a full or absent queue (the frame is gone, as on the wire) |
| `VsockDevice` | the ten methods the `AF_VSOCK` family holds — `guest_cid`, `listen`/`unlisten`, `connect`, `send`, `recv`/`recv_available`, `poll_event`, `shutdown`, `force_close`. `poll_event` hands over only the family's half of the news, because the device's connection manager has already done the rest |
| `P9Device` | `mount_tag` and one `request`/`resp` round trip. The fid table, the 9P2000 encoding and the `Inode` mapping are `src/vfs/p9.rs`'s one copy |
| `ConsolePort` | `size`, `drain`, `send`. A port whose far end has no reader is still a port: `send` succeeds into the host's socket regardless |
| `EntropySource` | `fill`, answering how many bytes the host generated — the whole buffer for a healthy device, fewer for a quiet one. The seed mixer and `/dev/hwrng` decide what a short answer means |
| `SoundCard` | `format`, `configure`, `play`. `configure` answers what the device settled on (the OSS contract reports rather than promises); `play`'s short answer is a full ring, not a failure |
| `InputDevice` | `name`, `ev_bits`, `abs_info`, `prop_bits`, `ids`, `pop_pending_event`. A question the device cannot answer answers `None` or empty rather than inventing one — the conservative picture the evdev ABI is written against |

`VirtioInputDev<T>` is the one implementation of `InputDevice` over the crate's input driver; the
`Mutex<RefCell<_>>` is what turns the crate's `&mut` methods into the contract's `&self` ones.

**The transport seam** (`virtio/hal.rs`, `virtio/mmio.rs`, `virtio/msix.rs`). The PCI helpers —
`hal::transport`/`transport_all`, `hal::device_config`, `hal::claim_drm_card`, `hal::pci_root` —
probe a BAR and hand back a `PciTransport`; the MMIO helpers open a window and hand back a device
already wrapped in its contract. Each of these answers `None` honestly rather than fabricating a
device: a window that does not open as a transport, or a transport that is not the class asked
for, is a machine with no such device, which the layers above turn into `ENODEV`/`ENETDOWN`/
`EAFNOSUPPORT` rather than an open that goes nowhere. `msix::enable`/`set_queue_vectors` answer
`false` on any failure and the caller keeps polling, which is what the kernel did before MSI-X
existed.

**The node and block contracts.** `devices.rs`'s adapters implement `crate::vfs::devfs::CharDevice`
(reads/writes and `write_error`); its `FbDev` is a full `Inode` because its face is bigger than a
stream (`mmap` of the pixels, `FBIOGET_*`); `AhciDisk`, `VirtioBlk` and `mmio::MmioBlk` implement
`crate::blk::BlockDev`, whose sector contract both drivers assert identically and whose `flush`
`false` means "this device has no such concept" — a device that *was* asked and refused panics
instead, which is the block layer's own rule. `virtio::register_block_disk` is the one place a
virtio-blk disk's name, major number and tree position are decided, so the two machines file the
same disk at the same path (`uevent::VIRTIO_BLK_DEVPATH`).

**The display handshake** (`drivers/drm.rs` ↔ `drivers/video/render.rs`). The render task owns the
`Gpu` and is the only caller of `flush`; the DRM layer therefore cannot touch the device. Two
request/park pairs carry work across: `render::job(Job)` for resource commands, and
`render::resize_blocking(width, height)` for a committed mode. Both register a waiter before the
wake and post the answer before the `done` flag. `drm::frame_landed()` is called by the task after
a flush — the one moment in the kernel that honestly means "the pixels are where they were asked
to be" — and is what the DRM layer timestamps flip events with and what `FBIO_WAITFORVSYNC` waits
for. `drm::publish_card` is called once by the task at bring-up to hand the DRM layer what the
device negotiated.

**The capability gate.** A block guarded by `#[cfg(mpos_input_registry)]` must have *everything*
behind it gated the same way. The gate names the capability and never the architecture, and it
covers the `virtio::input` and `uinput` modules, the `/dev/input`/`/dev/uinput` block, the service
task's input arm and the exit hook — so a port that declares the capability gets the whole stack
and one that does not gets none of it, which is what makes the flag a switch a machine flips
rather than an edit it makes.

## What userspace sees

This module owns the `/dev` node set and the ioctl families behind the nodes it publishes. The
families themselves — their numbers, their verdicts and their remaining gaps — are
`docs/UAPI.md` §2's rows; this section names the doors, not their contract:

| Family | Device(s) | Owning code |
|---|---|---|
| `'d'` (DRM/KMS + `DRM_VIRTGPU_*`) | `/dev/dri/card0`, `/dev/dri/renderD128` | `drivers/drm.rs`, over `crates/gpu` |
| `0x4600`+ (fbdev) | `/dev/fb0` | `drivers/video/`, over `drivers/virtio/gpu.rs` |
| `'P'` (OSS) | `/dev/dsp` | `drivers/virtio/sound.rs` |
| `'E'` (evdev) | `/dev/input/eventN` | `crates/evdev`, over `drivers/virtio/input.rs` |
| `'U'` (uinput) | `/dev/uinput` | `drivers/uinput.rs`, over `crates/uinput` |
| `'p'` (`RTC_*`) | `/dev/rtc0`, `/dev/rtc` | `drivers/rtc_dev.rs` |
| `0x12` (block) | `/dev/vda` and its partitions | `src/blk/`, over `crates/blk`'s `BlockDev` |

The node set `standard_devfs()` builds is the same one `mount -t devfs /dev` gives a real init. It
is deliberately larger than the virtio devices: the tty and console names (the console tty under
the machine's own serial name, the `console` sink, `tty0` and one node per virtual console),
`pts`/`shm` mountpoints, `kmsg`, `null`/`zero`/`full`, the entropy pair, `ptmx`, the `/proc` links
(`fd`, `stdin`, `stdout`, `stderr`), the disk names and their partitions, and the virtio nodes,
which a driver publishes when its probe succeeds (`devices::publish` — see the Key files row). A
driver that never came up is simply not in the registry, so a machine without a device has no node
rather than one whose open goes nowhere. `docs/UAPI.md` §4/§6 hold the layouts and the pins; the
node *names* are Linux's, and a name a machine does not have is absent rather than a node whose
open goes nowhere.

## Pins

The kernel-only halves of these drivers — port I/O, virtio rings, real registers — cannot run on
the host, so their decisions were split into host-testable leaf crates and their effects are
pinned by guest programs. Both halves are listed here; a claim with neither is not a pin.

### Host tests

The driver modules themselves carry no `#[cfg(test)] mod tests` — they touch hardware. Their pure
halves live in the leaf crates and are run by `cargo test -p mpos --lib --target
x86_64-unknown-linux-gnu` and the crate-native suites:

| Crate | Test module | What it pins |
|---|---|---|
| `crates/pci` | `src/lib.rs` tests | BAR sizing and flags (`a_memory_bar_decodes_its_address_and_size`, `an_io_bar_sizes_within_the_port_space`, `a_bar_nothing_answers_for_decodes_nothing`, `the_rom_bar_keeps_its_enable_and_never_sizes_the_reserved_bits`), the Linux spellings `pci.rs`'s snapshot feeds (`the_attributes_carry_the_prefix_pciutils_needs`, `the_devices_line_is_the_seventeen_numbers_pciutils_insists_on`, `the_modalias_matches_linuxs_spelling`, `the_uevent_attribute_carries_the_five_keys_udev_reads`) and the claim/publish model (`publishing_replaces_the_list_and_a_claim_lands_on_its_device`) |
| `crates/ahci` | `src/lib.rs` tests | The decisions `ahci.rs` reads the registers into: the task-file error ending the wait (`a_command_that_never_retires_but_reported_an_error_stops_the_wait`, `the_task_file_error_bits_alone_end_the_wait`, `a_busy_port_is_waited_on_and_a_retired_one_is_not`), the slot rule (`only_the_slot_this_driver_uses_answers_for_the_command`), the ATAPI exclusion, the PRDT encoding (`a_longer_table_says_done_only_when_the_last_run_is_moved`, `a_run_the_count_field_cannot_say_is_refused_rather_than_wrapped`, `one_command_page_holds_two_hundred_forty_eight_descriptors`) and the request planning (`the_two_measured_long_requests_are_planned_whole`) |
| `crates/gpu` | `src/abi.rs`, `src/virtgpu.rs`, `src/blob.rs`, `src/cursor.rs`, `src/sync.rs` tests | The `virtio-gpu` wire ABI (`the_command_numbers_are_the_headers`, `the_feature_bits_are_the_headers`, `every_struct_is_the_size_the_device_reads`, `an_error_response_is_not_a_success`), the DRM answer table (`a_virgl_card_answers_the_measured_row`, `an_unknown_parameter_is_einval`, `the_capset_mask_is_the_table`, `the_refusals_are_not_one_gate`), the EDID and `IN_FORMATS` blobs (the 15 blob tests) and the cursor blend |
| `crates/evdev` | `src/lib.rs` tests | The record window `EvdevNode` is built on: multiple readers, `EVIOCREVOKE`, grab/ungrab, backlog and wake rules |
| `crates/uinput` | `src/lib.rs` tests | The request vocabulary `Uinput::ioctl` dispatches over: `the_family_magic_is_what_makes_a_word_a_uinput_request`, `a_setup_is_read_out_of_its_own_bytes`, `a_write_that_is_not_whole_records_is_refused`, `the_bit_requests_name_the_type_they_set` |
| `crates/blk` | `src/part.rs` tests | The GPT/MBR partition walk `devices.rs` names the disks from, including the `Window` a partition node opens |

### Guest programs

Run by `cargo xtask test`; the integration suite lives in
`user/abitest/src/bin/integration/`, the `std`/`rustix` suite in `user/tests/src/bin/`.

| Area | Programs |
|---|---|
| Bus / DRM | `t-drm`, `t-drm-args`, `t-drm-props`, `t-drm-lease`, `t-drm-sync`, `t-virtgpu`, `t-prime`, `t-seat`, `t-fbdev`, `t-ioctl` |
| Block | `t-ahci`; `t-statfs` and `t-9p` read through the mounted disk and the host share |
| Display | the same DRM/fbdev programs above; the render task is exercised through `/dev/fb0` writes and `FBIO_WAITFORVSYNC` in `t-ioctl`/`t-fbdev` |
| Input | `t-evdev` (the capability ioctls and the record window), `t-vt` (the console the registry feeds) |
| Other devices | `t-dsp` (the OSS parameter family on `/dev/dsp`), `t-rtc` (`RTC_RD_TIME`/`RTC_SET_TIME` and the `ENOTTY` half), `t-9p` (the mount and the request wire), `t-uevent` (the device uevents `devices.rs` publishes) |
| Network seam | `t-dhcp`, `t-udp`, `t-tcp`, `t-tcp-listen` drive the `NetDevice` contract from above; there is no guest program for `VsockDevice` today |

## Evidence and open work

The reasoning, the measurements and the lessons behind every shape above are in `docs/NOTES.md`;
this page states the shape and points there rather than restating a number.

- **The device classes and their measurements.** `NOTES.md` §7.1 (the display: virtio-gpu,
  framebuffer and DRM/KMS), §7.1b/§7.1c (what the console answers and which consoles the kernel
  has), §7.2 (virtio-input/evdev), §7.2b (`/dev/dsp` and the OSS parameters), §7.3 (AHCI — the
  stretch item, its measurements and what measuring it corrected), §7.4 (the next compositor and
  the `DRM_VIRTGPU_*` answer table). The status board is `docs/ROADMAP.md`.
- **The seam and the aarch64 classes.** `NOTES.md` §9.1 (`map_mmio_window` taking the last
  x86_64-only call out of `HalImpl`; the virtio-rng over MMIO as the drivers stage's first
  device), §13.4 (the network stack holds the adapter as a contract), §13.24 (the input registry),
  §13.25 (the console port and `/dev/hvc0`), §13.26 (the card and the render task), §13.27 (the
  sound card), §13.28 (the vsock device), §13.29 (the 9p share and the probe with no caller). The
  one-copy claim per class is §15.2.
- **The two traps.** `AGENTS.md` §5 carries them; `NOTES.md` §13.26 is the measurement that the
  GPU class cost a type-erasure refactor (`Gpu<T>` + `Box<dyn GpuDevice>`), and §13.23/§13.24 the
  re-measurement of the capability flip once the module gates moved behind it.
- **The edges.** `docs/ARCHITECTURE.md` §2 (the module graph — `drivers` calls `mm`, `pci`,
  `sched`; `net` calls `drivers`, `sched`, `uevent`), §3 (what each layer owns, including the
  "must not parse a device's ioctl ABI" row that the driver-side ioctl dispatch is measured
  against), §8 (target gating and host testability) and §9 (the debt audit).
- **The userspace surface.** `docs/UAPI.md` §2 (the ioctl families by name), §4 (the struct
  layouts, including the DRM and EDID blobs) and §6 (where the pins live).

Open work, named rather than stubbed in code:

- **3D / virgl.** The `DRM_VIRTGPU_*` family answers per card shape; the renderer capabilities are
  gated on what the transport actually agreed to (`Gpu::features`), and `RESOURCE_CREATE_BLOB`
  stays refused on both card shapes. The plan and the deliberate scope are `docs/ROADMAP.md`
  (3D/virgl/venus) with the reasoning in `NOTES.md` §7.4.
- **DRM gaps this card declines.** `gamma_refused` and `sequence_refused` answer for the
  `GAMMA`/`CRTC_GET_SEQUENCE`/`CRTC_QUEUE_SEQUENCE` requests as this card's answer rather than the
  catch-all's; the `DRM_CAP_ASYNC_PAGE_FLIP` refusal is named as a capability this hardware does
  not have.
- **uinput.** `UI_ABS_SETUP`, force feedback and a runtime `/sys/class/input` entry are recorded as
  absent in `drivers/uinput.rs`'s header and in `docs/UAPI.md` §2's `'U'` row.
- **AHCI scope.** One controller, one port, `IDENTIFY`/`READ DMA EXT`/`WRITE DMA EXT`/
  `FLUSH CACHE EXT`; NCQ, ATAPI, port multipliers, hotplug and 28-bit LBA are absent by the
  scope `drivers/ahci.rs` states.
- **The sysfs side of a card.** The DRM card's sysfs nodes hang off a PCI function, which a machine
  with no PCI bus has none of; `virtio/gpu.rs`'s `init_global` records that as a named gap rather
  than a claim the machine lacks a display.
- **vsock.** QEMU does not emulate vsock in userspace, so the wiring attaches the device only where
  the host can back it (a `/dev/vhost-vsock`), and the kernel prints the absence rather than dying
  of it (`AGENTS.md` §5); no guest program pins the `VsockDevice` contract today.
