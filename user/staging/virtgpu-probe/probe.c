// virtgpu-probe - what a real Linux virtio-gpu card answers, request by request.
//
// Run on a Linux guest, once on a machine whose GPU is `virtio-gpu-pci` (no
// virgl) and once on `virtio-gpu-gl-pci`. Every DRM_VIRTGPU_* request is issued
// and its errno and out-values printed, so the kernel side of MPOS can be
// written from an observation rather than from the header.
//
// Not a test: no assertion, no exit code. It is an instrument.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <drm/drm.h>
#include <drm/virtgpu_drm.h>

static const char *node;

static void hdr(const char *what) { printf("\n--- %s ---\n", what); }

// One line per request: the name, the errno (0 = success), and a note.
static void rep(const char *name, int r) {
    int e = r == 0 ? 0 : errno;
    printf("%-28s rc=%2d errno=%-3d %s\n", name, r, e,
           e ? strerror(e) : "OK");
}

static int probe_getparam(int fd, uint64_t param) {
    uint64_t value = 0xdeadbeef;
    struct drm_virtgpu_getparam a = {.param = param, .value = (uint64_t)&value};
    int r = ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &a);
    char nm[64];
    snprintf(nm, sizeof nm, "GETPARAM(%llu)", (unsigned long long)param);
    rep(nm, r);
    if (r == 0) printf("    value=%llu (0x%llx)\n",
                       (unsigned long long)value, (unsigned long long)value);
    return r;
}

static int probe_caps(int fd, uint32_t set, uint32_t ver) {
    uint8_t buf[4096];
    memset(buf, 0, sizeof buf);
    struct drm_virtgpu_get_caps a = {.cap_set_id = set,
                                     .cap_set_ver = ver,
                                     .addr = (uint64_t)buf,
                                     .size = sizeof buf};
    int r = ioctl(fd, DRM_IOCTL_VIRTGPU_GET_CAPS, &a);
    char nm[64];
    snprintf(nm, sizeof nm, "GET_CAPS(set=%u)", set);
    rep(nm, r);
    if (r == 0)
        printf("    ver=%u size=%u first16=%02x%02x%02x%02x %02x%02x%02x%02x "
               "%02x%02x%02x%02x %02x%02x%02x%02x\n",
               a.cap_set_ver, a.size, buf[0], buf[1], buf[2], buf[3], buf[4],
               buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
               buf[12], buf[13], buf[14], buf[15]);
    return r;
}

int main(int argc, char **argv) {
    int fd;
    node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
    printf("=== virtgpu-probe on %s ===\n", node);

    fd = open(node, O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        printf("open(%s) failed: %s\n", node, strerror(errno));
        return 1;
    }

    hdr("identity");
    struct drm_version v;
    char name[64] = {0}, date[64] = {0}, desc[128] = {0};
    memset(&v, 0, sizeof v);
    v.name = name;
    v.name_len = sizeof name;
    v.date = date;
    v.date_len = sizeof date;
    v.desc = desc;
    v.desc_len = sizeof desc;
    if (ioctl(fd, DRM_IOCTL_VERSION, &v) == 0)
        printf("version %d.%d.%d name=%s date=%s desc=%s\n", v.version_major,
               v.version_minor, v.version_patchlevel, name, date, desc);
    else
        printf("VERSION failed: %s\n", strerror(errno));

    // The capabilities Mesa's virgl winsys branches on, plus one number that is
    // deliberately outside the table: `drm.h` has no virtual-GPU capability, and
    // a caller asking for nothing must hear `EINVAL` rather than a zero that
    // looks like a capability whose value happens to be zero.
    uint64_t caps[] = {DRM_CAP_DUMB_BUFFER, DRM_CAP_PRIME, DRM_CAP_SYNCOBJ,
                       DRM_CAP_SYNCOBJ_TIMELINE, 0x1e /* past the table */};
    for (unsigned i = 0; i < sizeof caps / sizeof caps[0]; i++) {
        uint64_t val = 0;
        struct drm_get_cap a = {.capability = caps[i], .value = 0};
        int r = ioctl(fd, DRM_IOCTL_GET_CAP, &a);
        val = a.value;
        char nm[64];
        snprintf(nm, sizeof nm, "GET_CAP(%llu)", (unsigned long long)caps[i]);
        rep(nm, r);
        if (r == 0) printf("    value=%llu\n", (unsigned long long)val);
    }

    hdr("GETPARAM - every parameter the UAPI names");
    for (uint64_t p = 1; p <= 9; p++) probe_getparam(fd, p);
    probe_getparam(fd, 0);
    probe_getparam(fd, 100);

    hdr("GET_CAPS - the capsets");
    for (uint32_t s = 1; s <= 6; s++) probe_caps(fd, s, 0);
    probe_caps(fd, 0, 0);
    probe_caps(fd, 100, 0);

    hdr("RESOURCE_CREATE - a 2D resource first, so the rest have a handle");
    struct drm_virtgpu_resource_create rc;
    memset(&rc, 0, sizeof rc);
    rc.target = 2; // VIRTGPU_PIPE
    rc.format = 1; // VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM
    rc.bind = 2;   // VIRTIO_GPU_BIND_SCANOUT
    rc.width = 64;
    rc.height = 64;
    rc.depth = 1;
    rc.array_size = 1;
    int has_2d = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, &rc) == 0;
    rep("RESOURCE_CREATE(2D 64x64)", has_2d ? 0 : -1);
    if (has_2d)
        printf("    bo_handle=%u res_handle=%u size=%u stride=%u\n",
               rc.bo_handle, rc.res_handle, rc.size, rc.stride);
    else
        printf("    %s\n", strerror(errno));

    uint32_t bo = has_2d ? rc.bo_handle : 0;

    hdr("RESOURCE_CREATE - a 3D resource");
    struct drm_virtgpu_resource_create rc3;
    memset(&rc3, 0, sizeof rc3);
    rc3.target = 4; // VIRTGPU_VIRGL
    rc3.format = 1;
    rc3.bind = 2;
    rc3.width = 64;
    rc3.height = 64;
    rc3.depth = 1;
    rc3.array_size = 1;
    int has_3d = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, &rc3) == 0;
    rep("RESOURCE_CREATE(3D virgl)", has_3d ? 0 : -1);
    if (has_3d)
        printf("    bo_handle=%u res_handle=%u size=%u stride=%u\n",
               rc3.bo_handle, rc3.res_handle, rc3.size, rc3.stride);
    else
        printf("    %s\n", strerror(errno));

    hdr("RESOURCE_INFO");
    struct drm_virtgpu_resource_info ri = {.bo_handle = bo};
    int r = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, &ri);
    rep("RESOURCE_INFO(2D handle)", r);
    if (r == 0)
        printf("    res_handle=%u size=%u blob_mem=%u\n", ri.res_handle,
               ri.size, ri.blob_mem);
    if (has_3d) {
        struct drm_virtgpu_resource_info ri3 = {.bo_handle = rc3.bo_handle};
        r = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, &ri3);
        rep("RESOURCE_INFO(3D handle)", r);
        if (r == 0)
            printf("    res_handle=%u size=%u blob_mem=%u\n", ri3.res_handle,
                   ri3.size, ri3.blob_mem);
    }

    hdr("TRANSFER_{TO,FROM}_HOST");
    struct drm_virtgpu_3d_transfer_to_host t2h;
    memset(&t2h, 0, sizeof t2h);
    t2h.bo_handle = bo;
    t2h.box.w = 64;
    t2h.box.h = 64;
    t2h.box.d = 1;
    rep("TRANSFER_TO_HOST(2D handle)",
        ioctl(fd, DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST, &t2h));
    struct drm_virtgpu_3d_transfer_from_host h2t;
    memset(&h2t, 0, sizeof h2t);
    h2t.bo_handle = bo;
    h2t.box.w = 64;
    h2t.box.h = 64;
    h2t.box.d = 1;
    rep("TRANSFER_FROM_HOST(2D handle)",
        ioctl(fd, DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST, &h2t));

    hdr("WAIT");
    struct drm_virtgpu_3d_wait w = {.handle = bo, .flags = 0};
    rep("WAIT(2D handle)", ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &w));
    struct drm_virtgpu_3d_wait wn = {.handle = bo, .flags = VIRTGPU_WAIT_NOWAIT};
    rep("WAIT(NOWAIT flag)", ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &wn));

    hdr("MAP");
    struct drm_virtgpu_map m = {.handle = bo};
    r = ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &m);
    rep("MAP(2D handle)", r);
    if (r == 0) printf("    offset=0x%llx\n", (unsigned long long)m.offset);

    hdr("EXECBUFFER");
    // A four-byte command stream is enough to see whether the request is
    // reachable at all; the GL machine will reject the *content*, not the call.
    uint32_t cmdbuf[1] = {0x00000000};
    uint32_t handles[1] = {bo};
    struct drm_virtgpu_execbuffer eb;
    memset(&eb, 0, sizeof eb);
    eb.flags = 0;
    eb.size = sizeof cmdbuf;
    eb.command = (uint64_t)cmdbuf;
    eb.bo_handles = (uint64_t)handles;
    eb.num_bo_handles = 1;
    eb.fence_fd = -1;
    rep("EXECBUFFER(size=4)", ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &eb));

    hdr("CONTEXT_INIT");
    struct drm_virtgpu_context_set_param csp[2];
    memset(csp, 0, sizeof csp);
    csp[0].param = VIRTGPU_CONTEXT_PARAM_CAPSET_ID;
    csp[0].value = VIRTGPU_DRM_CAPSET_VIRGL2;
    csp[1].param = VIRTGPU_CONTEXT_PARAM_NUM_RINGS;
    csp[1].value = 1;
    struct drm_virtgpu_context_init ci = {.num_params = 2,
                                          .ctx_set_params = (uint64_t)csp};
    rep("CONTEXT_INIT(virgl2, 1 ring)",
        ioctl(fd, DRM_IOCTL_VIRTGPU_CONTEXT_INIT, &ci));

    hdr("RESOURCE_CREATE_BLOB");
    struct drm_virtgpu_resource_create_blob rb;
    memset(&rb, 0, sizeof rb);
    rb.blob_mem = VIRTGPU_BLOB_MEM_GUEST;
    rb.blob_flags = VIRTGPU_BLOB_FLAG_USE_MAPPABLE;
    rb.size = 65536;
    rb.blob_id = 0;
    int hasblob = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB, &rb) == 0;
    rep("RESOURCE_CREATE_BLOB(64K guest)", hasblob ? 0 : -1);
    if (hasblob)
        printf("    bo_handle=%u res_handle=%u blob_id=%llu\n", rb.bo_handle,
               rb.res_handle, (unsigned long long)rb.blob_id);
    else
        printf("    %s\n", strerror(errno));

    hdr("an unregistered / beyond-the-family request");
    // DRM_COMMAND_BASE + 0x30: no virtgpu request is numbered here.
    rep("VERSION-sized unknown command",
        ioctl(fd, _IOWR('d', 0x40 + 0x30, struct drm_version), &v));
    // A request number the family does not name, with the header's own size.
    rep("DRM_IOCTL_VIRTGPU_MAP on a bad handle",
        ({
            struct drm_virtgpu_map bad = {.handle = 0xffffffff};
            ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &bad);
        }));

    close(fd);
    printf("\n=== done ===\n");
    return 0;
}
