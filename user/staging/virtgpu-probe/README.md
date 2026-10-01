# virtgpu-probe — what a real Linux virtio-gpu card answers

The instrument `PLAN.md` §7.4's `DRM_VIRTGPU_*` work is designed from, and the
numbers it produced. It measures a **real Linux kernel**, not this one: the
guest is Alpine's netboot `virt` kernel in a throwaway initramfs, booted twice
on the same host QEMU — once on a card with no 3D behind it and once on the GL
card — with `probe.c` issuing every request the family names.

`AGENTS.md` §4.1 says behaviour is observed on a real Linux before a design is
argued from it, because a man page (or a source file) does not say which half of
a rule a caller actually branches on. This directory is that observation.

It is **not a test** and is wired into no build: it wants the network, a Linux
guest and `sudo` for one repack, and nothing in `cargo xtask` should acquire
those dependencies. Run it by hand:

```
./run.sh            # both machines
./run.sh nogl       # virtio-gpu-pci, no 3D
./run.sh gl         # virtio-gpu-gl-pci + -display egl-headless,gl=on
```

The guest is Alpine's `virt` kernel and **not** `lts` for one measured reason:
the netboot `initramfs-virt` already carries `virtio-gpu.ko`, `drm.ko`,
`drm_kms_helper.ko`, `drm_shmem_helper.ko` and a `modules.dep` whose closure
covers them, so the 218 MB `modloop-lts` is not needed at all. The whole guest
is a static `probe` plus a twenty-line `/init`.

## Measured: Linux 6.12.110-0-virt, QEMU 11.1.1, x86_64

### The card, as the driver described itself

| | `virtio-gpu-pci` | `virtio-gpu-gl-pci` |
|---|---|---|
| features | `-virgl +edid -resource_blob -host_visible -context_init` | `+virgl +edid -resource_blob -host_visible +context_init` |
| scanouts | 1 | 1 |
| cap sets | **0** | 2: id 1 (VIRGL) max-version 1 max-size 308; id 2 (VIRGL2) max-version 2 max-size 1408 |

### The family, request by request

| Request | no 3D | virgl |
|---|---|---|
| `GETPARAM(1)` 3D_FEATURES | `0` | `1` |
| `GETPARAM(2)` CAPSET_QUERY_FIX | `1` | `1` |
| `GETPARAM(3)` RESOURCE_BLOB | `0` | `0` |
| `GETPARAM(4)` HOST_VISIBLE | `0` | `0` |
| `GETPARAM(5)` CROSS_DEVICE | `0` | `0` |
| `GETPARAM(6)` CONTEXT_INIT | `0` | `1` |
| `GETPARAM(7)` SUPPORTED_CAPSET_IDs | `0` | `6` (capsets 1 and 2) |
| `GETPARAM(8)` EXPLICIT_DEBUG_NAME | `0` | `1` |
| `GETPARAM(0)`, `(9)`, `(100)` | `EINVAL` | `EINVAL` |
| `GET_CAPS(1..6)` | **`ENOSYS`** | `1`,`2` → `0`; `3..6` → `EINVAL` |
| `RESOURCE_CREATE` target **2** (2D) | `0` | `0` |
| `RESOURCE_CREATE` target **4** (3D) | **`EINVAL`** | `0` |
| `RESOURCE_INFO` | `0` (size 4096) | `0` (size 4096) |
| `TRANSFER_TO_HOST` | `0` | `0` |
| `TRANSFER_FROM_HOST` | **`ENOSYS`** | `0` |
| `WAIT`, with and without `NOWAIT` | `0` | `0` |
| `MAP` | `0` | `0` |
| `EXECBUFFER` | **`ENOSYS`** | `0` |
| `CONTEXT_INIT` | **`EINVAL`** | **`EEXIST`** |
| `RESOURCE_CREATE_BLOB` | `EINVAL` | `EINVAL` |
| an unknown command number (`0x40+0x30`) | `EINVAL` | `EINVAL` |
| `MAP` on handle `0xffffffff` | `ENOENT` | `ENOENT` |

## The four things worth naming

1. **The family is reachable on a card with no 3D at all.** `virtio_gpu_ioctls[]`
   is registered unconditionally, so nothing is `ENOTTY` and nothing is
   "unimplemented": each request is *answered*, and the answers differ per
   request. A card that returned one errno for the whole family would be
   unrecoverable for a caller that reads them one at a time.

2. **Three requests are refused when there is no 3D, and they are not the ones
   a guess would pick.** `TRANSFER_FROM_HOST`, `EXECBUFFER` and `GET_CAPS` are
   `ENOSYS`; `TRANSFER_TO_HOST` is **not** (it falls back to the 2D transfer),
   `MAP` and `WAIT` are not, and `CONTEXT_INIT` is `EINVAL` rather than
   `ENOSYS`. So the refusals are per-request facts, not a gate on the family.

3. **`CONTEXT_INIT` answers `EEXIST` on a virgl card** — because the driver
   creates a context *implicitly* on first use (any `RESOURCE_CREATE`,
   `EXECBUFFER` or blob before it), so an explicit `CONTEXT_INIT` arrives
   second. A kernel that serves the explicit request "successfully" first is
   wrong in a way a caller sees only later.

4. **`RESOURCE_CREATE`'s `target` is validated, and 2 is the only value without
   3D.** With `-virgl` the driver rejects `target != 2`, `depth > 1`,
   `nr_samples > 1`, `last_level > 1` and `array_size > 1` with `EINVAL`; with
   `+virgl` it forwards them. `GET_CAPS` is `ENOSYS` only because the *device*
   reported zero capsets — the request itself is fine.

Also measured, and not about the family: `GET_CAP` on this card answers
`DUMB_BUFFER=1`, `PRIME=3`, `SYNCOBJ=1`, `SYNCOBJ_TIMELINE=1`, and a number
outside the table `EINVAL`; and `RESOURCE_INFO` reports `size = 4096` for a
64×64 B8G8R8A8 resource created with `size = 0` — the driver's own floor is one
page, not `width * height * 4`.

## What this instrument does not do

It does not capture **Mesa's** own sequence. That needs a guest with Mesa's
`virtio_gpu_dri.so` and a tracer (`eglinfo`/`kmscube` under `strace`), which is
a full Alpine userland rather than the twenty-line one above. Until it is done,
what Mesa sends in which order is known from its own source, not from an
observation here — and the difference matters, because `AGENTS.md` §4.1 exists
precisely because sources mislead about that.

`RNDRESEEDCRNG`-class care applies: the numbers in the tables above are the ones
this instrument printed, on this host, with this QEMU and this kernel. A newer
kernel has more `VIRTGPU_PARAM_*` values than 8 (`BLOB_ALIGNMENT` is 9 and this
kernel answers it `EINVAL`), and a newer QEMU may report `resource_blob` or
`host_visible`, which is why the feature line is recorded beside the answers.
