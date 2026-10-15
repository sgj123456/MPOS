/// of these values changes after negotiation, so they are handed over once
/// (`video/render.rs`, at the task's start) rather than asked for per request.
/// Before that publication this is the "no card" shape, which is what a machine
/// whose GPU never came up owes a caller: every 3D answer `0`, every 3D request
/// refused, rather than anything a caller could read as a capability.
use super::abi::{MAX_HANDLES, PAGES_PER_BUFFER};
use super::ioctl::libc_einval;
use super::props::handle_names_buffer;
use super::state::{Dumb, Resource, STATE};

use crate::mm::{PAGE_SIZE, free_frames, phys_to_virt};
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};
use critical_section::Mutex;

struct Published {
    /// The card, as `drm_virtgpu::Card` describes it.
    card: crate::gpu::virtgpu::Card,
    /// How many cap sets the *device* said it had.
    ///
    /// Kept apart from `capsets.len()`, which can be smaller: a set whose fetch
    /// failed is skipped, and the walk stays honest about the others. This is
    /// what `GET_CAPS`'s `ENOSYS` is gated on, because Linux's gate is the
    /// device's own count - so a card that offered two sets and produced one
    /// answers `EINVAL` for the other rather than `ENOSYS` for the family.
    num_capsets: u32,
    /// The sets that were fetched, in the order the device named them.
    capsets: Vec<crate::gpu::virtgpu::Capset>,
}

static PUBLISHED: Mutex<RefCell<Published>> = Mutex::new(RefCell::new(Published {
    card: crate::gpu::virtgpu::Card::NONE,
    num_capsets: 0,
    capsets: Vec::new(),
}));

/// Hand the DRM layer what the GPU driver negotiated.
///
/// Called once, by the task that owns the device. It is a plain setter rather
/// than something the ioctl path could also reach, because there is exactly one
/// moment at which the answer is knowable and exactly one task that knows it.
pub fn publish_card(
    card: crate::gpu::virtgpu::Card,
    num_capsets: u32,
    capsets: &[crate::gpu::virtgpu::Capset],
) {
    critical_section::with(|cs| {
        let mut published = PUBLISHED.borrow(cs).borrow_mut();
        published.card = card;
        published.num_capsets = num_capsets;
        published.capsets.clear();
        published.capsets.extend_from_slice(capsets);
    });
}

/// The card to answer a `DRM_VIRTGPU_*` request from.
pub(crate) fn card() -> crate::gpu::virtgpu::Card {
    critical_section::with(|cs| PUBLISHED.borrow(cs).borrow().card)
}

/// How many cap sets the device claimed - see [`Published::num_capsets`].
fn num_capsets() -> u32 {
    critical_section::with(|cs| PUBLISHED.borrow(cs).borrow().num_capsets)
}

/// The cap set `id` offers at `version`, or `None` if it offers neither.
///
/// Linux's search, down to the comparison: it accepts `id == cap_set_id` with
/// `max_version >= cap_set_ver`, so a caller may ask for any version the set is
/// good for rather than only the newest one it could have named. A set that is
/// not there, or a version past what it has, is `EINVAL` - which is what `None`
/// becomes at the one place that asks.
fn capset(id: u32, version: u32) -> Option<crate::gpu::virtgpu::Capset> {
    critical_section::with(|cs| {
        PUBLISHED
            .borrow(cs)
            .borrow()
            .capsets
            .iter()
            .find(|c| c.id == id && c.max_version >= version)
            .cloned()
    })
}

/// One open file description's 3D context.
///
/// Per *description* rather than per process, for the reason the master is
/// (`MASTER_OFD`): the identity that survives a seat manager handing a card over
/// `SCM_RIGHTS` is the description, and Linux's context hangs off
/// `drm_file->driver_priv`, which is the same thing.
pub(crate) struct Context {
    /// The file description that made it - `crate::vfs::File::ofd`.
    ofd: u64,
    /// The id the device knows the context by. This card chooses it, from 1: 0 is
    /// the protocol's "no context", which is what every 2D header carries.
    pub(crate) id: u32,
    /// Whether the *device* has been told to make it.
    ///
    /// Linux's `context_created`, and the whole reason `CONTEXT_INIT` can answer
    /// `EEXIST`: the first 3D command a file makes creates the context
    /// implicitly, so an explicit request arrives second. Only `ensure_context`
    /// sets it.
    created: bool,
    /// The next fence id to hand out for this context.
    ///
    /// Per context because a fence id names a command within *that* context's
    /// rings, which is why Linux allocates a `dma_fence_context` per file rather
    /// than numbering globally. It starts at 1 so 0 keeps meaning "no fence".
    next_fence: u64,
    /// The device resource ids this context has been told about, in the order
    /// they were attached.
    ///
    /// Kept so that a stream or a transfer names each of its resources to the
    /// device once rather than on every request: [`attach_resources`] sends only
    /// what is missing from here. Its lifetime is the context's, and a context's
    /// lifetime is its file description's - so a second file that names the same
    /// resource is told about it from scratch, which is the guarantee vrend's
    /// per-context resource table asks for.
    pub(crate) attached: Vec<u32>,
}

/// The contexts on the card, one per file description that needed one.
pub(crate) static CONTEXTS: Mutex<RefCell<Vec<Context>>> = Mutex::new(RefCell::new(Vec::new()));

/// The next context id. Never reused within a boot, so an id names one context
/// for the life of the machine.
static NEXT_CTX_ID: AtomicU32 = AtomicU32::new(1);

/// This file's context: its id, and whether the device has been told to make it.
///
/// Allocating an id is bookkeeping and sends nothing - the command that *uses* a
/// context is what has to be told, and it is `ensure_context` that does the
/// telling.
fn context_of(ofd: u64) -> (u32, bool) {
    critical_section::with(|cs| {
        let mut contexts = CONTEXTS.borrow(cs).borrow_mut();
        if let Some(existing) = contexts.iter().find(|c| c.ofd == ofd) {
            return (existing.id, existing.created);
        }
        let id = NEXT_CTX_ID.fetch_add(1, Ordering::Relaxed);
        contexts.push(Context {
            ofd,
            id,
            created: false,
            next_fence: 1,
            attached: Vec::new(),
        });
        (id, false)
    })
}

/// Make sure the device has been told to create this file's context, and answer
/// its id.
///
/// The implicit creation Linux does from the first command that needs a context:
/// `virtio_gpu_create_context` runs from resource-create, the transfers and
/// execbuffer alike, and it is what makes an explicit `CONTEXT_INIT` afterwards
/// answer `EEXIST`. A context whose command the device refused leaves `created`
/// false, so the next request tries again rather than believing in a context that
/// is not there.
///
/// `context_init` is the wire word, which is the cap set id and nothing else -
/// Linux's `vfpriv->context_init` is only ever `|`-ed with a cap set id. The
/// implicit path passes zero, which is what a device without `CONTEXT_INIT` would
/// have got anyway.
fn ensure_context(ofd: u64, context_init: u32, name: &[u8]) -> Result<u32, isize> {
    let (id, created) = context_of(ofd);
    if created {
        return Ok(id);
    }
    let made =
        crate::drivers::video::render::job(crate::drivers::video::render::Job::ContextCreate {
            id,
            context_init,
            name: name.to_vec(),
        });
    if made != 0 {
        return Err(made);
    }
    critical_section::with(|cs| {
        if let Some(context) = CONTEXTS
            .borrow(cs)
            .borrow_mut()
            .iter_mut()
            .find(|c| c.ofd == ofd)
        {
            context.created = true;
        }
    });
    Ok(id)
}

/// The fence id for this file's next submitted command.
///
/// Zero when the file has no context, which cannot happen at a call site that has
/// just been through [`ensure_context`] - and zero is the protocol's "no fence",
/// so a mistake there is a missing fence rather than a wrong one.
fn take_fence(ofd: u64) -> u64 {
    critical_section::with(|cs| {
        let mut contexts = CONTEXTS.borrow(cs).borrow_mut();
        let Some(context) = contexts.iter_mut().find(|c| c.ofd == ofd) else {
            return 0;
        };
        let fence = context.next_fence;
        context.next_fence = context.next_fence.wrapping_add(1);
        fence
    })
}

/// Tell `ofd`'s context about the resources `ids`, so the commands that context
/// runs may name them - answering `0` or the errno the device gave.
///
/// # Why this exists at all
///
/// `vrend` resolves a resource **for the context that names it**
/// (`vrend_renderer_ctx_res_lookup`, whose context struct calls the table
/// "resource bounds to this context"), and the context of a command stream is the
/// file description that submitted it, because this card keeps one context per
/// description. So a resource the submitting file's context has not been told
/// about is one the device answers `Illegal resource <n>` for - and *drops the
/// whole batch* over, whatever else was in it.
///
/// Linux tells a file's context at the two moments a file can come to hold a
/// resource: when it creates one (`virtio_gpu_object_create`, which is
/// `virtio_gpu_cmd_resource_create_3d` whenever the card has 3D, whatever the
/// target) and when it *opens* a handle to one another file made
/// (`virtio_gpu_gem_object_open` - the only sender of `CTX_ATTACH_RESOURCE` in
/// the kernel's own `virtio-gpu` module - with `virtio_gpu_gem_object_close`
/// sending the mirror). QEMU forwards exactly that pair and nothing else: its
/// `virgl_renderer_ctx_attach_resource` has one call site, the
/// `CTX_ATTACH_RESOURCE` handler, and neither create handler attaches anything.
///
/// This card implemented one of those doors - the 3D create's own attach, for
/// `target == 4` only - and left every other way a resource reaches a client's
/// hands unwired. The measured consequence is niri's blank screen: Mesa's GBM
/// buffers arrive as `target == 2` creates, `target == 2` takes the 2D path and
/// the 2D path attaches nothing, so the renderer's *first* stream naming one of
/// its own buffers died with `context error reported 1 "mpos" Illegal resource 2`
/// and its 852011-byte batch was dropped. `t-virtgpu` pins both halves of the
/// statement - the file that made the resource and the file that imported it, a
/// `TRANSFER3D` in each stream whose landing is read back out of the resource's
/// own pages - and the pin was red on both before this function existed
/// (`docs/notes/` §7.4).
///
/// # Why here, and not at Linux's doors
///
/// Because the question a submission asks is "may **this** context name what I
/// pass", and the `bo_handles` a submit carries are exactly the resources its
/// commands spell - Mesa's winsys fills that list from the encoder that wrote the
/// stream. One door, one answer, and no path can arrive holding a resource the
/// door did not know about; the two-door shape is what this card got wrong, by
/// implementing one of them. The same statement is made for the 3D transfers,
/// whose request names one resource and no list at all.
///
/// The context is made before it can be told anything, which is why this can
/// create one: a file that never names a resource never gets here, and Linux's
/// own hook creates the file's context for the same reason.
fn attach_resources(ofd: u64, ids: &[u32]) -> isize {
    if !card().virgl || ids.is_empty() {
        return 0;
    }
    let ctx = match ensure_context(ofd, 0, b"mpos") {
        Ok(id) => id,
        Err(errno) => return errno,
    };
    // What this context has not been told about. Read, not remembered: the ids
    // are marked only once the device has taken them, so a command the device
    // refused is sent again by the next request rather than assumed done.
    let todo: Vec<u32> = critical_section::with(|cs| {
        let contexts = CONTEXTS.borrow(cs).borrow();
        let Some(context) = contexts.iter().find(|c| c.ofd == ofd) else {
            return Vec::new();
        };
        ids.iter()
            .copied()
            .filter(|id| !context.attached.contains(id))
            .collect()
    });
    if todo.is_empty() {
        return 0;
    }
    let made = crate::drivers::video::render::job(crate::drivers::video::render::Job::CtxAttach {
        ctx,
        ids: todo.clone(),
    });
    if made != 0 {
        return made;
    }
    critical_section::with(|cs| {
        if let Some(context) = CONTEXTS
            .borrow(cs)
            .borrow_mut()
            .iter_mut()
            .find(|c| c.ofd == ofd)
        {
            context.attached.extend_from_slice(&todo);
        }
    });
    0
}

/// `DRM_VIRTGPU_GETPARAM`: `struct drm_virtgpu_getparam { u64 param; u64 value; }`
/// where `value` is the *address* of the caller's answer rather than the answer.
///
/// Linux writes a four-byte `int` there, not the eight the field's width
/// suggests: the field is declared `__u64` and the copy is `sizeof(int)`, so a
/// caller that zeroed eight bytes finds the low half changed and the high half
/// as it left it. Writing eight would be the wrong shape in exactly the place
/// it is invisible, which is why the width is written down here.
///
/// The order is Linux's: an unknown parameter is `EINVAL` *before* the
/// destination is looked at, so a caller that asked for nothing hears what is
/// wrong with the question rather than with its buffer. Measured - `GETPARAM(0)`,
/// `(9)` and `(100)` all answer `EINVAL` on a real card.
///
/// The card the answers come from is the one the GPU driver published
/// ([`Published`]); before that publication it is `Card::NONE`, because the
/// answers are a statement about what this kernel can do and a machine whose GPU
/// has not come up can do none of it.
pub(crate) fn virtgpu_getparam(arg: usize) -> isize {
    let param = unsafe { (arg as *const u64).read_unaligned() };
    let Ok(value) = crate::gpu::virtgpu::param(param, &card()) else {
        return crate::err::EINVAL;
    };
    let out = unsafe { ((arg + 8) as *const u64).read_unaligned() } as *mut i32;
    if out.is_null() || !crate::uaccess::buffer_ok(out as usize, 4, true, true) {
        return crate::err::EFAULT;
    }
    unsafe { out.write_unaligned(value as i32) };
    0
}

/// `DRM_VIRTGPU_GET_CAPS`: `struct drm_virtgpu_get_caps { u32 cap_set_id;
/// u32 cap_set_ver; u64 addr; u32 size; }`, where `addr` is the caller's buffer
/// and `size` how much of it may be written.
///
/// Linux's order, which the measurement backs at every step: a card with no cap
/// sets at all answers `ENOSYS` for the whole request; a caller that asked for
/// nothing is `EINVAL`; a set that is not offered, or not at the version asked
/// for, is `EINVAL`; and only then is the buffer looked at. A copy *smaller* than
/// the set is a **success** - `min(caller, device)`, which is the half of this
/// rule a guess gets wrong, and `drm_virtgpu::caps_may_copy` is where it lives.
///
/// The bytes come out of the cache the render task built at bring-up, so this
/// never touches the device. `GET_CAPS` is a request a compositor makes on every
/// startup, and a control-queue round trip per set would be one per startup.
pub(crate) fn virtgpu_get_caps(arg: usize) -> isize {
    // struct drm_virtgpu_get_caps: cap_set_id@0, cap_set_ver@4, addr@8 (u64),
    // size@16.
    let id = unsafe { (arg as *const u32).read_unaligned() };
    let version = unsafe { ((arg + 4) as *const u32).read_unaligned() };
    let addr = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    let size = unsafe { ((arg + 16) as *const u32).read_unaligned() };

    if num_capsets() == 0 {
        return crate::gpu::virtgpu::CAPS;
    }
    let Some(set) = capset(id, version) else {
        // Not offered, or a version past what the set has. Linux answers the
        // same `EINVAL` for both, so there is nothing to tell apart here.
        return crate::err::EINVAL;
    };
    let Ok(copy) = crate::gpu::virtgpu::caps_may_copy(size, set.max_size) else {
        return crate::err::EINVAL;
    };
    if !crate::uaccess::buffer_ok(addr, copy, true, true) {
        return crate::err::EFAULT;
    }
    // SAFETY: the destination is the caller's, and `buffer_ok` has just checked
    // that all `copy` bytes of it are mapped and writable. The source is the
    // cache's own `Vec`, which is exactly `max_size` bytes: a fetch that came
    // back short is not cached at all (`gpu_ctrl::get_capset` refuses it), so
    // `copy <= max_size` is within it.
    unsafe {
        core::ptr::copy_nonoverlapping(set.bytes.as_ptr(), addr as *mut u8, copy);
    }
    0
}

/// `DRM_VIRTGPU_CONTEXT_INIT`: `struct drm_virtgpu_context_init { u32 num_params;
/// u32 pad; u64 ctx_set_params; }`, where that pointer is `num_params` of
/// `struct drm_virtgpu_context_set_param { u64 param; u64 value; }`.
///
/// Linux's order, and two of its steps are worth following exactly:
///
/// 1. `EINVAL` unless the device has **both** `CONTEXT_INIT` and `VIRGL`, which is
///    why this is one of the refusals a card without 3D makes with `EINVAL` rather
///    than `ENOSYS`.
/// 2. `EINVAL` for more than four parameters, before the array is read at all.
/// 3. **`EEXIST` if this file already has a context** - and that comes *before*
///    the parameters are validated. The first 3D command made one implicitly, so a
///    second explicit request is not a malformed request, it is a late one, and
///    the measured card is where this shows: `CONTEXT_INIT` answers `EEXIST` on a
///    virgl machine.
/// 4. The parameters, each at most once. `CAPSET_ID` must be one the device
///    offers; `DEBUG_NAME` is the caller's string; `NUM_RINGS` and
///    `POLL_RINGS_MASK` are accepted and not acted on, because the only thing
///    either gates is the multi-ring fence protocol, and this kernel refuses that
///    protocol's flag in `EXECBUFFER` rather than half-driving it.
pub(crate) fn virtgpu_context_init(arg: usize, ofd: u64) -> isize {
    let num_params = unsafe { (arg as *const u32).read_unaligned() };
    let params = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;

    let card = card();
    if !card.context_init || !card.virgl {
        return crate::gpu::virtgpu::CONTEXT_INIT;
    }
    if num_params > 4 {
        return libc_einval();
    }
    if context_of(ofd).1 {
        return -17; // EEXIST
    }
    if num_params > 0 && !crate::uaccess::buffer_ok(params, num_params as usize * 16, false, true) {
        return -14; // EFAULT
    }

    // The wire word is the cap set id and nothing else: Linux only ever `|`-es
    // that into it, and `VIRTIO_GPU_CONTEXT_INIT_CAPSET_ID_MASK` is the low byte
    // for that reason.
    let mut context_init = 0u32;
    let mut name: Option<Vec<u8>> = None;
    for index in 0..num_params as usize {
        let param = unsafe { ((params + index * 16) as *const u64).read_unaligned() };
        let value = unsafe { ((params + index * 16 + 8) as *const u64).read_unaligned() };
        match param {
            // VIRTGPU_CONTEXT_PARAM_CAPSET_ID
            1 => {
                if context_init & 0xff != 0 || value > 31 || card.capset_ids & (1u32 << value) == 0
                {
                    return libc_einval();
                }
                context_init |= value as u32;
            }
            // VIRTGPU_CONTEXT_PARAM_NUM_RINGS / _POLL_RINGS_MASK: accepted and
            // not acted on. Only the ring-count fence protocol reads either, and
            // `EXECBUFFER` refuses `VIRTGPU_EXECBUF_RING_IDX` outright, so a
            // caller that asked for rings cannot reach the path that would use
            // them. Refusing here instead would turn away a caller Linux serves.
            2 | 3 => {}
            // VIRTGPU_CONTEXT_PARAM_DEBUG_NAME
            4 => {
                if name.is_some() || value == 0 {
                    return libc_einval();
                }
                if !crate::uaccess::buffer_ok(value as usize, 64, false, true) {
                    return -14; // EFAULT
                }
                let mut bytes = [0u8; 64];
                // SAFETY: `buffer_ok` has just checked all 64 bytes are mapped
                // readable in the caller, and `bytes` is 64 bytes of local.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        value as *const u8,
                        bytes.as_mut_ptr(),
                        bytes.len(),
                    );
                }
                // A name, not a string: the field is 64 bytes and the caller's is
                // a C string, so it ends at the first NUL. `strnlen`'s job.
                let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                name = Some(bytes[..end].to_vec());
            }
            _ => return libc_einval(),
        }
    }
    let name = name.unwrap_or_else(|| b"mpos".to_vec());
    match ensure_context(ofd, context_init, &name) {
        Ok(_) => 0,
        Err(errno) => errno,
    }
}

/// `DRM_VIRTGPU_EXECBUFFER`: `struct drm_virtgpu_execbuffer { u32 flags;
/// u32 size; u64 command; u64 bo_handles; u32 num_bo_handles; s32 fence_fd;
/// u32 ring_idx; u32 syncobj_stride; u32 num_in_syncobjs; u32 num_out_syncobjs;
/// u64 in_syncobjs; u64 out_syncobjs; }` - 64 bytes.
///
/// Four things are refused rather than ignored, and each because ignoring it would
/// be a promise:
///
/// - **`FENCE_FD_IN`/`FENCE_FD_OUT` and the syncobj fields.** They are explicit
///   synchronisation, which needs `sync_file` and `drm_syncobj` - both of which
///   this kernel refuses (`SYNCOBJ_*`), so a descriptor or a handle here names a
///   synchronisation primitive that does not exist. `EINVAL`, not a silent
///   accepted-and-dropped fence, which is how a caller's buffer would be read with
///   no ordering at all while it believed otherwise.
/// - **`VIRTGPU_EXECBUF_RING_IDX`.** One ring is driven; a submit on another would
///   be a command nothing ever completes.
/// - **A `bo_handle` this card never made.** Linux answers `ENOENT` there, and the
///   handles are checked even though they do not reach the wire (see below),
///   because that errno is what a caller branches on.
///
/// What is *not* checked is `size`: Linux does not check it either - it hands
/// `size` bytes to `vmemdup_user` and forwards whatever comes back - so a
/// zero-length stream is submitted as a zero-length stream rather than invented
/// into an error. `buffer_ok` is what bounds it, and it bounds it by the caller's
/// own mapping.
///
/// The stream is forwarded untouched. Linux's `virtio_gpu_init_submit` is
/// `vmemdup_user(exbuf->command, exbuf->size)` and nothing more, and the
/// `bo_handles` become a guest-side reservation list there and never a wire
/// element - `virtgpu_abi::CmdSubmit` has the long version.
pub(crate) fn virtgpu_execbuffer(arg: usize, ofd: u64) -> isize {
    let field = |offset: usize| unsafe { ((arg + offset) as *const u32).read_unaligned() };
    let (flags, size) = (field(0), field(4));
    let command = unsafe { ((arg + 8) as *const u64).read_unaligned() } as usize;
    let handles = unsafe { ((arg + 16) as *const u64).read_unaligned() } as usize;
    let num_handles = field(24);
    let ring_idx = field(32);
    let (syncobj_stride, num_in_syncobjs, num_out_syncobjs) = (field(36), field(40), field(44));

    // Every flag this kernel knows, refused by name rather than as a group: an
    // unknown flag is a caller that expects something else to happen, and it gets
    // `EINVAL` rather than a submit that quietly did less.
    //
    // The capability gate comes first, because it is the whole family's answer on
    // a card with no 3D (measured: `ENOSYS`) and a caller that hears `EINVAL`
    // instead would go looking for the malformed field it does not have.
    if !card().virgl {
        return crate::gpu::virtgpu::EXECBUFFER;
    }
    const FENCE_FD_IN: u32 = 0x01;
    const FENCE_FD_OUT: u32 = 0x02;
    const RING_IDX: u32 = 0x04;
    if flags & (FENCE_FD_IN | FENCE_FD_OUT | RING_IDX) != 0 || flags & !0x07 != 0 {
        return libc_einval();
    }
    if ring_idx != 0 {
        return libc_einval();
    }
    if syncobj_stride != 0 || num_in_syncobjs != 0 || num_out_syncobjs != 0 {
        return libc_einval();
    }
    if size > 0 && !crate::uaccess::buffer_ok(command, size as usize, false, true) {
        return -14; // EFAULT
    }
    // The handles are resolved here for two reasons now: Linux's answer for a
    // handle this file does not hold is `ENOENT` and a caller acts on it before
    // it ever looks at the fence, and the *resources* those handles name are what
    // this context has to be told about before the stream that spells them is
    // submitted - a resource id a context has not been told about is one the
    // device answers `Illegal resource <n>` for and drops the whole batch over
    // (`attach_resources` carries the measurement). The handles themselves still
    // do not go on the wire; their resource ids do, as attach commands.
    let mut named = Vec::new();
    if num_handles > 0 {
        if !crate::uaccess::buffer_ok(handles, num_handles as usize * 4, false, true) {
            return -14; // EFAULT
        }
        // Why the loop gave up, when it did: gathered inside the section and
        // printed outside it, and it is the whole of what the caller cannot see -
        // `ENOENT` is one number for three different states, and only one of them
        // is the caller's fault. Mesa turns this answer into
        // `got error from kernel - expect bad rendering 2` and marks its winsys
        // failed, so the difference between "you named a name you released" and
        // "this buffer is not a device resource" decides what the fix is.
        let mut refused: Option<(u32, &'static str)> = None;
        let known = critical_section::with(|cs| {
            let st = STATE.borrow(cs).borrow();
            for index in 0..num_handles as usize {
                let handle =
                    unsafe { ((handles + index * 4) as *const u32).read_unaligned() } as usize;
                let slot = st
                    .dumb
                    .get(handle.wrapping_sub(1))
                    .and_then(|slot| slot.as_ref());
                // A released name is not a buffer this submit may name, even
                // when a framebuffer still holds the buffer itself - Linux's
                // lookup is against the *file's* handle table.
                let Some(resource) = slot
                    .filter(|d| !d.holders.is_empty())
                    .and_then(|d| d.resource.as_ref())
                else {
                    refused = Some((
                        handle as u32,
                        match slot {
                            None => "no buffer is recorded at that slot",
                            Some(d) if d.holders.is_empty() => {
                                "the name was released and a framebuffer is holding the buffer"
                            }
                            Some(_) => "the buffer is a plain dumb buffer with no device resource",
                        },
                    ));
                    return false;
                };
                named.push(resource.id);
            }
            true
        });
        if let Some((handle, why)) = refused {
            crate::println!(
                "mpos-dbg: EXECBUFFER refused: handle {handle} - {why} \
                 (num_handles={num_handles}, bytes={size}, ofd={ofd})"
            );
        }
        if !known {
            return -2; // ENOENT
        }
    }

    let mut stream = Vec::new();
    stream.resize(size as usize, 0);
    if size > 0 {
        // SAFETY: `buffer_ok` has just checked `size` bytes at `command` are
        // mapped readable in the caller, and `stream` is `size` bytes of local.
        unsafe {
            core::ptr::copy_nonoverlapping(
                command as *const u8,
                stream.as_mut_ptr(),
                size as usize,
            );
        }
    }

    let ctx = match ensure_context(ofd, 0, b"mpos") {
        Ok(id) => id,
        Err(errno) => return errno,
    };
    // The stream's resources, named to this context *before* the submit: a
    // resource this context has not been told about is one the device answers
    // `Illegal resource <n>` for, and it drops the whole batch over that one
    // unknown number - every command in it, half a megabyte of them if that is
    // what the caller sent.
    let attached = attach_resources(ofd, &named);
    if attached != 0 {
        return attached;
    }
    let fence = take_fence(ofd);
    crate::drivers::video::render::job(crate::drivers::video::render::Job::Submit3d {
        ctx,
        ring_idx: 0,
        fence,
        stream,
    })
}

/// `DRM_VIRTGPU_TRANSFER_FROM_HOST`: the same struct as
/// `DRM_VIRTGPU_TRANSFER_TO_HOST`, and the other direction.
///
/// The asymmetry is Linux's and was the least guessable thing in the measurement:
/// a card without 3D transfers *to* the host through the 2D command and answers
/// this one `ENOSYS`, because the only command that reads a resource back is the
/// 3D one. So this is `ENOSYS` without 3D and a real transfer with it, which is
/// the opposite of a symmetric pair - and the reason both are written out.
pub(crate) fn virtgpu_transfer_from_host(arg: usize, ofd: u64) -> isize {
    let field = |offset: usize| unsafe { ((arg + offset) as *const u32).read_unaligned() };
    let handle = field(0) as usize;
    let box3d = crate::gpu::abi::Box3d {
        x: field(4),
        y: field(8),
        z: field(12),
        w: field(16),
        h: field(20),
        d: field(24),
    };
    let (level, offset, stride, layer_stride) = (field(28), field(32), field(36), field(40));

    if !card().virgl {
        return crate::gpu::virtgpu::TRANSFER_FROM_HOST;
    }
    let found = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        st.dumb
            .get(handle.wrapping_sub(1))
            .and_then(|slot| slot.as_ref())
            .filter(|d| !d.holders.is_empty())
            .and_then(|d| d.resource.as_ref())
            .map(|r| r.id)
    });
    let Some(id) = found else {
        return -2; // ENOENT
    };
    let ctx = match ensure_context(ofd, 0, b"mpos") {
        Ok(id) => id,
        Err(errno) => return errno,
    };
    // The transfer names one resource and no handle list at all, so it says which
    // resource it needs told to this context itself. A transfer whose resource the
    // context does not know is one the device answers `Illegal resource <n>` for
    // and then quietly does nothing about - QEMU discards what virglrenderer made
    // of it - so the pixels simply never move.
    let attached = attach_resources(ofd, &[id]);
    if attached != 0 {
        return attached;
    }
    let request = crate::gpu::abi::TransferHost3d {
        header: crate::gpu::abi::CtrlHeader::new(crate::gpu::abi::CMD_TRANSFER_FROM_HOST_3D),
        box3d,
        offset: offset as u64,
        resource_id: id,
        level,
        stride,
        layer_stride,
    };
    crate::drivers::video::render::job(crate::drivers::video::render::Job::TransferHost3d {
        to_host: false,
        ctx,
        request,
    })
}

/// `DRM_VIRTGPU_RESOURCE_CREATE`: allocate a buffer and tell the device to make
/// a resource of it.
///
/// Two answers, and the order they are given in is the point. The five field
/// checks a card without 3D makes come first (`src/drm_virtgpu.rs`), so a caller
/// whose *arguments* are wrong hears what is wrong with them; a caller whose
/// arguments are fine gets a resource, which on Linux it would have got all
/// along. Before this the good case was `ENOSYS`, and the reason it could not be
/// served was that telling the device is a control-queue command - so the work
/// goes to the render task as a job and this parks.
///
/// The frames are made *before* the device is told about them, and that order is
/// forced: the attach command names their addresses, so a page allocated after
/// it would be a page the device was never told to read.
///
/// The returned `bo_handle` is this card's GEM handle - the same space
/// `CREATE_DUMB` allocates in, so `MAP`, `ADDFB` and `GEM_CLOSE` take it - and
/// `res_handle` is the id the device knows the resource by.
pub(crate) fn virtgpu_resource_create(arg: usize, ofd: u64) -> isize {
    let field = |offset: usize| unsafe { ((arg + offset) as *const u32).read_unaligned() };
    let (target, width, height) = (field(0), field(12), field(16));
    let virgl = card().virgl;
    // The validation is the *non*-virgl branch of Linux's driver, which is why a
    // card with 3D skips it: the virgl branch forwards `target`, `depth`,
    // `array_size`, `last_level`, `nr_samples` and `flags` rather than checking
    // them, and the measurement is where that shows - `target 4` is `EINVAL`
    // without 3D and a resource with it.
    if !virgl
        && let Err(errno) = crate::gpu::virtgpu::resource_create_args_ok(
            target,
            field(20),
            field(32),
            field(28),
            field(24),
        )
    {
        return errno;
    }
    if width == 0 || height == 0 {
        return libc_einval();
    }
    // Which command makes this resource is **the card's answer, not the target's**,
    // and that is Linux's rule: `virtio_gpu_object_create` picks
    // `virtio_gpu_cmd_resource_create_3d` whenever `has_virgl_3d`, whatever the
    // target - and it is the only command that *can* carry the caller's fields.
    // `virtio_gpu_resource_create_2d` is `resource_id`, `format`, `width`, `height`
    // and nothing else, so a guest whose driver is the 3D family (Mesa's virgl
    // winsys asks for every one of its resources through this ioctl) would have its
    // `bind`, `array_size`, `last_level`, `nr_samples` and `flags` dropped on the
    // floor - and a texture the client then uses as a render target, a depth
    // attachment or a vertex buffer is not the resource the device was told about.
    // The host's GL is what says so, and kmscube is where it was read:
    //
    //     vrend_check_no_error: context error reported 1 "mpos" Unknown 1282
    //     context 1 failed to dispatch DRAW_VBO: 22
    //
    // (`1282` is `GL_INVALID_OPERATION`, and vrend reports it against the *next*
    // batch it decodes, which is why the line names a different command than the
    // one that caused it - 786440 bytes of draws dropped, and a black screen.) The
    // target still decides what the *guest* asked for and is forwarded as it always
    // was; what it no longer decides is which command carries it.
    let three_d = card().virgl;
    // The object's size is the caller's, or a page when it asked for zero -
    // Linux's rule, and the reason the instrument measured 4096 for a 64x64
    // resource. See `Resource::size`.
    let asked = field(48) as u64;
    let size = if asked == 0 { PAGE_SIZE as u64 } else { asked };
    let pages = (size as usize).div_ceil(PAGE_SIZE);
    if pages == 0 || pages > PAGES_PER_BUFFER {
        return -28; // ENOSPC
    }

    let mut frames = Vec::with_capacity(pages);
    let mut entries = Vec::with_capacity(pages);
    for _ in 0..pages {
        let Some(p) = crate::mm::alloc_frames(0) else {
            for p in frames.drain(..) {
                free_frames(p, 0);
            }
            return -12; // ENOMEM
        };
        unsafe {
            core::ptr::write_bytes(phys_to_virt(p) as *mut u8, 0, PAGE_SIZE);
        }
        frames.push(p);
        entries.push((p as u64, PAGE_SIZE as u32));
    }

    // The handle and the wire resource id are one number, and the handle is
    // claimed *before* the device is told: a resource id is a field the guest's
    // command stream spells, and it spells it with the handle this ioctl
    // answers, so a card that kept two spaces - an id counter beside the handle
    // table - hands the stream ids the device never heard of. `vrend` answers
    // that stream with `Illegal resource <n>` and the guest's renderer stops,
    // which is the measured shape (`docs/notes/07-device-drivers.md`, the
    // virtio-gpu 3D row): the two counters drifted apart the first time a freed
    // slot was reused, because the id counter only went up while handles went
    // back into the table.
    //
    // The claim is one critical section rather than "pick a slot, leave the
    // lock, fill it later": the device is talked to outside every lock (the
    // render task has to run), so anything not settled inside the section is
    // open to a second caller picking the same slot - two handles, one id, the
    // same split wearing a different hat. A failure takes the slot back out,
    // below.
    //
    // A freed slot is reused before the table grows, the lowest-first rule the
    // blob table follows: without it the table fills with holes and every later
    // create answers `ENOSPC` for a card holding a handful of live resources.
    let handle = critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        let slot = match st.dumb.iter().position(|d| d.is_none()) {
            Some(slot) => slot,
            None if st.dumb.len() < MAX_HANDLES => {
                st.dumb.push(None);
                st.dumb.len() - 1
            }
            None => return 0,
        };
        let id = (slot + 1) as u32;
        st.dumb[slot] = Some(
            Dumb::resource(
                width,
                height,
                core::mem::take(&mut frames),
                Resource {
                    id,
                    size,
                    virgl: three_d,
                    // A resource this request made, on this card: the guest's 3D-ioctl
                    // family is how Mesa's virgl driver reaches the device, and it
                    // draws into the device's own copy rather than into these pages.
                    host_pixels: virgl,
                },
            )
            .named_by(ofd),
        );
        slot + 1
    });
    if handle == 0 {
        for p in frames.drain(..) {
            free_frames(p, 0);
        }
        return -28; // ENOSPC
    }
    let id = handle as u32;
    // Give the claimed slot back with every page behind it. Nothing owes the
    // device an unref here: every path below that reaches this either failed
    // before a command went out or had its one create command refused, so the
    // device holds nothing to let go of.
    let give_slot_back = |handle: usize| {
        critical_section::with(|cs| {
            let mut st = STATE.borrow(cs).borrow_mut();
            if let Some(mut d) = st.dumb[handle - 1].take() {
                d.release();
            }
        });
    };

    // The device is told, and this parks until it has answered - with no lock
    // held and no frames borrowed, because the task that answers is the render
    // task and a caller parked while holding either would be a task waiting for
    // something it is itself preventing.
    // The context is made *before* the target is consulted, because that is
    // Linux's order: `virtio_gpu_resource_create_ioctl` calls
    // `virtio_gpu_create_context` as soon as `has_virgl_3d` is set, and only then
    // looks at `target`. It is why a `CONTEXT_INIT` after a purely **2D** create
    // answers `EEXIST` on a virgl card, and matching it is cheaper than
    // explaining a divergence a caller can see for itself.
    //
    // Made, and nothing more: this request does *not* tell the context about the
    // resource. That is `attach_resources`'s statement, made on the request that
    // names the resource - the one every way of coming by a resource goes through -
    // and a second sender here is the shape of the bug this card had (see its own
    // comment).
    if virgl && let Err(errno) = ensure_context(ofd, 0, b"mpos") {
        give_slot_back(handle);
        return errno;
    }
    let made = if three_d {
        crate::drivers::video::render::job(crate::drivers::video::render::Job::Create3d {
            id,
            request: crate::gpu::abi::ResourceCreate3d {
                header: crate::gpu::abi::CtrlHeader::new(crate::gpu::abi::CMD_RESOURCE_CREATE_3D),
                resource_id: id,
                target,
                format: field(4),
                bind: field(8),
                width,
                height,
                depth: field(20),
                array_size: field(24),
                last_level: field(28),
                nr_samples: field(32),
                flags: field(36),
                padding: 0,
            },
            entries,
        })
    } else {
        crate::drivers::video::render::job(crate::drivers::video::render::Job::Create2d {
            id,
            width,
            height,
            entries,
        })
    };
    if made != 0 {
        // The device refused it: the slot goes back to the pool that found it,
        // with its pages, and no further command went out.
        give_slot_back(handle);
        return made;
    }

    unsafe {
        ((arg + 40) as *mut u32).write_unaligned(handle as u32); // bo_handle
        ((arg + 44) as *mut u32).write_unaligned(id); // res_handle
    }
    crate::println!(
        "mpos-dbg: RESOURCE_CREATE -> handle {handle} (target {target}, {width}x{height}) ofd={ofd}"
    );
    0
}

/// Back a plain dumb buffer with the `DRM_VIRTGPU_*` device resource Linux's
/// `CREATE_DUMB` would have made for it, the first time a caller addresses the
/// handle as one.
///
/// On Linux there is no such thing as "a dumb buffer that is not a resource":
/// `virtio_gpu_mode_dumb_create` goes through `virtio_gpu_object_create`, the
/// same object creation `RESOURCE_CREATE` uses, so `RESOURCE_INFO` names it and
/// `TRANSFER_TO_HOST` completes. Here a dumb buffer has been the card's own
/// pages and nothing else - the scanout copy reads them directly and the device
/// was never told - and the two answers that produced are named in
/// `crates/gpu/src/virtgpu.rs`'s header.
///
/// **The backing is made here rather than in `CREATE_DUMB`, and that is a
/// difference in this implementation and not in the ABI.** Every dumb buffer
/// would otherwise be eagerly backed and cost a device round trip at creation -
/// and the console, weston and the fbdev node all allocate through exactly that
/// path while none of them ever asks this question. A caller that asks gets
/// Linux's answer; a caller that does not pays nothing. What a client can
/// observe is the same either way: the id is opaque, and no request reports
/// whether a resource exists without asking about it.
///
/// The pages are materialised before the device is told, because the command
/// names their addresses as the backing and cannot be handed a page that does
/// not exist yet - the same reason `Dumb::resource` takes its frames already
/// made. The list is grown through `ensure_page`, so the frames stay the ones
/// the scanout and the fbdev node have been reading: the device's copy is a
/// *second* view of the same pixels, and a transfer is what moves them.
///
/// Answers `(the resource's id, its size)`, or `ENOENT` for a handle this card
/// never made. Idempotent: a buffer that already has one answers with it, which
/// is also what a second caller finds if two race.
pub(crate) fn back_dumb_with_a_resource(handle: usize) -> Result<(u32, u64), isize> {
    let existing = critical_section::with(|cs| -> Result<Option<(u32, u64)>, isize> {
        let st = STATE.borrow(cs).borrow();
        let Some(d) = st
            .dumb
            .get(handle.wrapping_sub(1))
            .and_then(|slot| slot.as_ref())
            .filter(|d| !d.holders.is_empty())
        else {
            return Err(-2); // ENOENT
        };
        Ok(d.resource.as_ref().map(|r| (r.id, r.size)))
    })?;
    if let Some(found) = existing {
        return Ok(found);
    }

    // Everything under the lock is arithmetic and `Vec` growth; the device round
    // trip below happens with no lock held, because the task that answers it is
    // the render task and a caller parked holding this lock would be waiting for
    // something it is itself preventing (`RESOURCE_CREATE` says the same).
    let prepared = critical_section::with(|cs| -> Result<Option<_>, isize> {
        let mut st = STATE.borrow(cs).borrow_mut();
        let Some(d) = st
            .dumb
            .get_mut(handle.wrapping_sub(1))
            .and_then(|s| s.as_mut())
            .filter(|d| !d.holders.is_empty())
        else {
            return Err(-2); // ENOENT
        };
        if d.resource.is_some() {
            return Ok(None); // another caller got there first
        }
        let pages = (d.size() as usize).div_ceil(PAGE_SIZE);
        for page in 0..pages {
            if d.ensure_page(page) == 0 {
                // The pages made so far stay: they are this buffer's own, and
                // `DESTROY_DUMB` releases them whether a resource was made or
                // not.
                return Err(-12); // ENOMEM
            }
        }
        let entries: Vec<(u64, u32)> = d
            .frames
            .iter()
            .take(pages)
            .map(|&p| (p as u64, PAGE_SIZE as u32))
            .collect();
        Ok(Some((d.width, d.height, entries, d.size())))
    });
    let Some((width, height, entries, size)) = (match prepared {
        Ok(found) => found,
        Err(errno) => return Err(errno),
    }) else {
        // The race above: whoever won has recorded the resource, so its id is
        // there to answer with.
        return back_dumb_with_a_resource(handle);
    };

    // The resource id is the handle, exactly as `RESOURCE_CREATE` answers it:
    // one number, because the command stream spells a resource with the handle
    // and the device was told that same number. The id is taken from the buffer
    // rather than from a counter of its own, so a dumb buffer this card made and
    // a resource a client made can never be two names for one object.
    let id = handle as u32;

    let made = crate::drivers::video::render::job(crate::drivers::video::render::Job::Create2d {
        id,
        width,
        height,
        entries,
    });
    if made != 0 {
        return Err(made);
    }

    critical_section::with(|cs| {
        let mut st = STATE.borrow(cs).borrow_mut();
        if let Some(d) = st
            .dumb
            .get_mut(handle.wrapping_sub(1))
            .and_then(|s| s.as_mut())
            && d.resource.is_none()
        {
            d.resource = Some(Resource {
                id,
                size,
                virgl: false,
                // A dumb buffer's pixels are the guest's: whoever writes it maps
                // it and stores into those pages, which is why the scanout copy
                // carries this kind of frame correctly.
                host_pixels: false,
            });
        }
    });
    Ok((id, size))
}

/// `DRM_VIRTGPU_RESOURCE_INFO`: what a handle names.
///
/// The handle has to be one this card made - anything else is `ENOENT`, which is
/// Linux's answer from `drm_gem_object_lookup` - and the size is the resource's
/// own when the handle is one.
///
/// A plain dumb buffer gets its resource made here, on this request, because
/// that is what Linux's `CREATE_DUMB` would have done at creation: see
/// [`back_dumb_with_a_resource`] for why this card makes it on the request that
/// asks rather than on the one that allocates, and for why the answer a caller
/// reads is Linux's either way.
pub(crate) fn virtgpu_resource_info(arg: usize) -> isize {
    let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
    let (id, size) = match back_dumb_with_a_resource(handle) {
        Ok(found) => found,
        Err(errno) => return errno,
    };
    unsafe {
        ((arg + 4) as *mut u32).write_unaligned(id);
        ((arg + 8) as *mut u32).write_unaligned(size as u32);
        ((arg + 12) as *mut u32).write_unaligned(0); // blob_mem: no blobs here
    }
    0
}

/// `DRM_VIRTGPU_MAP`: the mmap offset for a handle's pages.
///
/// The same offset `MAP_DUMB` computes, over the same table, because a resource
/// and a dumb buffer are the same kind of object here - both are the card's
/// pages. What differs is the answer to a handle this card never made, or to one
/// whose name the caller has released: Linux's `drm_gem_dumb_map_offset` looks
/// the handle up in the file's table and answers `ENOENT`, and so does this
/// rather than handing back an offset for pages the caller no longer has a name
/// for. The buffer itself may well still be there - a framebuffer that was made
/// from it holds it - which is exactly why the name is the question here and not
/// the buffer.
pub(crate) fn virtgpu_map(arg: usize) -> isize {
    // struct drm_virtgpu_map: offset@0 (u64), handle@8, pad@12.
    let handle = unsafe { ((arg + 8) as *const u32).read_unaligned() } as usize;
    let known =
        critical_section::with(|cs| handle_names_buffer(&STATE.borrow(cs).borrow(), handle));
    if !known {
        return -2; // ENOENT
    }
    let offset = (handle as u64) * (PAGES_PER_BUFFER as u64) * (PAGE_SIZE as u64);
    unsafe { (arg as *mut u64).write_unaligned(offset) };
    0
}

/// `DRM_VIRTGPU_WAIT`: wait for a resource's work to be done.
///
/// Always already done, and that is a fact rather than a shortcut: a fence here
/// means a device command still in flight, and every command this card sends is
/// waited for before the ioctl that sent it returns - so by the time a caller
/// can ask, nothing is pending. `VIRTGPU_WAIT_NOWAIT` therefore answers `0` too,
/// where a card with work in flight would answer `EBUSY`; `drm_sync.rs` records
/// the same absence for the sync-object family, and the two are the same fact
/// about this machine seen through two doors.
///
/// An unknown handle is still `ENOENT`: a caller waiting on something that does
/// not exist has a bug, and "done" would hide it. So is a released one, for the
/// same reason the lookup is against the name.
pub(crate) fn virtgpu_wait(arg: usize) -> isize {
    // struct drm_virtgpu_3d_wait: handle@0, flags@4.
    let handle = unsafe { (arg as *const u32).read_unaligned() } as usize;
    let known =
        critical_section::with(|cs| handle_names_buffer(&STATE.borrow(cs).borrow(), handle));
    if known { 0 } else { -2 } // ENOENT
}

/// `DRM_VIRTGPU_TRANSFER_TO_HOST`: copy a resource's pixels into the device.
///
/// The one direction a card without 3D has - the reverse is `ENOSYS` at the
/// dispatch arm, as measured. The box the caller sends is read for its width and
/// height, and the whole rectangle is transferred from offset zero, because that
/// is what this card's resources are: one buffer, no layers and no mip levels,
/// so a caller asking for anything else is asking about a shape the resource
/// does not have and is refused `EINVAL`.
///
/// A plain dumb buffer is a device resource on Linux, and it is one here now:
/// the first transfer against such a handle makes the resource and copies into
/// it, which is the answer Linux gives. Before that this was `EINVAL` - "this
/// command does not apply to that handle" - which was honest about this card and
/// was not Linux's; [`back_dumb_with_a_resource`] records what changed and why
/// the backing is made on the request that wants it rather than at
/// `CREATE_DUMB`.
///
/// **Two routes, chosen by the resource rather than by this request**, because
/// one struct carries both and cannot say which it means. A 3D resource goes
/// through `TRANSFER_TO_HOST_3D` with the caller's box, level, offset and
/// strides intact; a 2D one takes the path described above, where those are
/// restricted to the single-level, single-layer shape a 2D resource has. Linux
/// splits on the same fact - `virtio_gpu_transfer_to_host_ioctl` reads the
/// object's own kind - and the split is why the restrictions above are not a
/// property of the request.
pub(crate) fn virtgpu_transfer_to_host(arg: usize, ofd: u64) -> isize {
    // struct drm_virtgpu_3d_transfer_to_host: bo_handle@0, then the box
    // (x@4, y@8, z@12, w@16, h@20, d@24), level@28, offset@32, stride@36,
    // layer_stride@40.
    let field = |offset: usize| unsafe { ((arg + offset) as *const u32).read_unaligned() };
    let handle = field(0) as usize;
    let (width, height, depth, level, offset, stride, layer_stride) = (
        field(16),
        field(20),
        field(24),
        field(28),
        field(32),
        field(36),
        field(40),
    );
    let found = critical_section::with(|cs| {
        let st = STATE.borrow(cs).borrow();
        let d = st
            .dumb
            .get(handle.wrapping_sub(1))
            .and_then(|slot| slot.as_ref())
            // A released name is not a buffer this transfer may carry, even when a
            // framebuffer still holds the buffer.
            .filter(|d| !d.holders.is_empty())?;
        Some(d.resource.as_ref().map(|r| (r.id, r.size, r.virgl)))
    });
    let Some(res) = found else {
        return -2; // ENOENT
    };
    // A plain dumb buffer is a device resource on Linux, so the first transfer
    // against one has it made here and then takes the 2D path below: the
    // resource's pages are the ones the scanout has been reading, so the copy
    // this request sends is of the pixels the caller drew.
    let (id, size, virgl) = match res {
        Some(resource) => resource,
        None => match back_dumb_with_a_resource(handle) {
            Ok((id, size)) => (id, size, false),
            Err(errno) => return errno,
        },
    };

    if virgl {
        // A 3D resource goes through the 3D transfer, and Linux routes it the
        // same way: from the resource's own *kind* rather than from the request,
        // because the request is one struct for both directions and cannot say.
        //
        // The box, the level, the offset and the two strides are all the
        // caller's here. The checks below are restrictions of the **2D** command
        // - it has one level, no pitch of its own and no layer - and applying
        // them to this one would refuse a box a virgl caller legitimately sends.
        // The over-read guard is skipped for the same reason: it exists to catch
        // a caller that allocated a single page and then pushed a whole screen
        // through the 2D command, and a 3D resource's backing is sized by its own
        // create call rather than by this one.
        let ctx = match ensure_context(ofd, 0, b"mpos") {
            Ok(id) => id,
            Err(errno) => return errno,
        };
        // The same statement the transfers other direction make, and for the same
        // reason: this request names one resource to a context, and a context that
        // has not been told about it gets the transfer dropped rather than
        // refused.
        let attached = attach_resources(ofd, &[id]);
        if attached != 0 {
            return attached;
        }
        let request = crate::gpu::abi::TransferHost3d {
            header: crate::gpu::abi::CtrlHeader::new(crate::gpu::abi::CMD_TRANSFER_TO_HOST_3D),
            box3d: crate::gpu::abi::Box3d {
                x: field(4),
                y: field(8),
                z: field(12),
                w: width,
                h: height,
                d: depth,
            },
            offset: offset as u64,
            resource_id: id,
            level,
            stride,
            layer_stride,
        };
        return crate::drivers::video::render::job(
            crate::drivers::video::render::Job::TransferHost3d {
                to_host: true,
                ctx,
                request,
            },
        );
    }

    if width == 0
        || height == 0
        || depth > 1
        || level != 0
        || offset != 0
        || stride != 0
        || layer_stride != 0
    {
        return libc_einval();
    }
    // The transfer has to fit the backing the device was given. It reads
    // `width * height * 4` bytes from the frames attached at create, and a
    // resource whose caller asked for a smaller object - Linux substitutes a page
    // for a zero size, and Mesa passes zero - would have it read past the end of
    // them and into whatever the allocator put next. Linux makes no such check
    // and would over-read; this refuses the request instead. It is a divergence
    // that can only turn a broken caller's garbage into an error, which is the
    // direction `NOTES.md` §4's worst-case rule asks for.
    if width as u64 * height as u64 * 4 > size {
        return libc_einval();
    }
    crate::drivers::video::render::job(crate::drivers::video::render::Job::TransferToHost {
        id,
        width,
        height,
    })
}
