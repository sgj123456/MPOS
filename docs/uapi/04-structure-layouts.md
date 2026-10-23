# UAPI §4

Part of the split `docs/uapi/` inventory — index and reading rules in [index.md](index.md);
section numbers are unchanged from the original `UAPI.md`.

## 4. Structure layouts

The layouts the kernel writes into a caller's memory are pinned by host tests in
`src/syscall/linux/abi.rs` and `crates/gpu/src/blob.rs`, and by the guest programs that read them
back. This is the list, not the byte-by-byte record (that lives in the tests).

| Layout | Consumer | Pin |
|---|---|---|
| `struct stat` / `statx` | every libc | `t-statx`, `t-fstat`, `t-stat-buffer` (the length, **both** machines), `t-statx-agree` (the two stat families agreeing, **both** machines), `t-rmtree` (the tree walk that felt it), host `abi` tests |
| `rt_sigframe` + `sigcontext` | `rt_sigreturn` | `t-signal-delivery`, `t-sa-restart` |
| `siginfo_t` | signal handlers | `t-signal-delivery` |
| `struct sched_attr` | `sched_setattr`/`getattr` | `t-priority` |
| `struct rlimit` | `prlimit64` | `t-prlimit64` |
| `struct robust_list_head` | `set/get_robust_list` | `t-set-robust-list` (24 bytes) |
| EDID base block + `IN_FORMATS` blob | a DRM compositor | `crates/gpu/src/blob.rs` host tests, verified against `edid-decode`; `t-drm-props` |
| `struct sockaddr_ll` | `AF_PACKET` | `crates/link` host tests + `fuzz/link_wire`; `t-dhcp` on the machine |
| `struct nlmsghdr`/`rtattr`/`ifa*msg` | netlink route/uevent | `t-netlink`, `t-uevent` |
| `struct ifreq` | `SIOC*` | `t-netlink` |
| `struct rand_pool_info` | `RNDADDENTROPY` | `t-ioctl` |
| `struct fb_*` / `fb_var_screeninfo` | `/dev/fb0` | `t-fbdev` |
| `drm_mode_*` | `/dev/dri/card0` | `t-drm`, `t-drm-props`, `t-drm-lease`, `t-drm-sync` |
| `struct epoll_event` | `epoll_ctl`/`epoll_wait` | `t-epoll`, and it is the record's **machine's** own: twelve bytes with `data` at 4 on x86_64, sixteen with `data` at 8 on aarch64 (`EPOLL_PACKED` in the uapi is `#if defined(__x86_64__)`, and musl's `sys/epoll.h` copies that guard). A kernel that writes one machine's offsets fails the layout claim and the data-word claims with it — which is how the aarch64 half was found |

