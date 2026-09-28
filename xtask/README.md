# `cargo xtask` — mpos 构建 / 运行工具 | Build & run front-end

`xtask` 是 mpos 内核的统一构建入口。它把过去散落在 `scripts/mkdisk.sh`、
`scripts/run.sh` 里的步骤收进一条 `cargo xtask <命令>`，并且和脚本一样处理权限：
用你的普通用户身份跑 `cargo` 编译，只在真正需要 root 的那一步（写块设备、挂载
镜像、`chroot`）临时提权，事后再把产物目录归还给你。

`xtask` is the single build front-end for the mpos kernel. It folds the steps that
used to live in `scripts/mkdisk.sh` and `scripts/run.sh` into one
`cargo xtask <command>`, and keeps their privilege model: `cargo` compiles as your
own user, root is taken only for the steps that genuinely need it (touching a block
device, mounting the image, `chroot`), and everything root wrote is handed back
afterwards.

命令通过 `Cargo.toml` 里的 alias 注册：
The command is wired up through the `Cargo.toml` alias:

```toml
[alias]
xtask = "run --quiet --package mpos-xtask --target x86_64-unknown-linux-gnu --"
```

所以 `cargo xtask all` 会先把 xtask 自身（为宿主架构 `x86_64-unknown-linux-gnu`
编译）构建出来，再执行它。
So `cargo xtask all` first builds the xtask binary itself (for the host architecture)
and then runs it.

---

## 前置要求 | Prerequisites

**Rust 工具链 | Rust toolchain** — 四个 target 都要装上 | all four targets installed:

| target | 用途 | used for |
| --- | --- | --- |
| `x86_64-unknown-none` | 内核（bare-metal） | the kernel |
| `x86_64-unknown-uefi` | EFI loader (`mpos-boot`) | the UEFI boot loader |
| `x86_64-unknown-linux-musl` | 静态用户程序 | the static user programs |
| `x86_64-unknown-linux-gnu` | 宿主单元测试 + xtask 自身 | host unit tests + xtask itself |

```
rustup target add x86_64-unknown-none x86_64-unknown-uefi x86_64-unknown-linux-musl x86_64-unknown-linux-gnu
```

**系统工具 | System tools**（`run`/`test`/`rootfs` 用到 | needed by run/test/rootfs）：

- `qemu-system-x86_64`（带 SDL/GTK 显示后端，供 `run` 开窗）
  with an SDL/GTK display backend, for the `run` window
- UEFI 固件 **OVMF**（`/usr/share/OVMF/x64`、`/usr/share/edk2/x64` 等目录下的
  `OVMF_CODE.4m.fd` / `OVMF_VARS.4m.fd`）
  OVMF firmware (`OVMF_CODE.4m.fd` / `OVMF_VARS.4m.fd`)
- 镜像制作相关 | image-building tools: `qemu-img`、`qemu-nbd`、`sfdisk`、`partx`、
  `blkid`（util-linux）、`mkfs.ext4`（e2fsprogs）、`mkfs.fat`（dosfstools）、`tar`
- **root / sudo** — `disk`、`rootfs`、`initramfs`、`all`，以及需要重建镜像时的
  `run` 都会自动 `sudo`
  these commands elevate to `sudo` automatically
- **网络 | network** — `rootfs`/`initramfs` 会从 Alpine 镜像站拉包（本机没有 `apk`
  时会自动下载 `apk.static`）
  the Alpine rootfs is fetched over the network (a host `apk` is used if present,
  otherwise `apk.static` is downloaded)

> `build` 和 `test` 全程**不需要** root。
> `build` and `test` never need root.

---

## 用法 | Usage

```
cargo xtask [全局参数] <子命令>
cargo xtask [global flags] <command>
```

### 全局参数 | Global flags

| 参数 | flag | 说明 | meaning |
| --- | --- | --- | --- |
| 构建目录 | `--build-dir <PATH>` | 默认 `$MPOS_BUILD_DIR`，再默认 `build/` | default `$MPOS_BUILD_DIR`, else `build/` |
| 构建档 | `--profile <NAME>` | 默认 `$PROFILE`，再默认 `debug`；发布用 `--profile release` | default `$PROFILE`, else `debug`; use `--profile release` |
| 运行模式 | `--mode <MODE>` | 仅 `run`：`terminal` / `gui` / `mirror` / `weston`（默认取 `[qemu] mode`，再默认 `mirror`）| `run` only: `terminal`/`gui`/`mirror`/`weston` (defaults to `[qemu] mode`, else `mirror`) |

### 子命令 | Commands

| 命令 | command | 作用 | what it does | root? |
| --- | --- | --- | --- | --- |
| `build` | build | 编译内核与 EFI loader（产物在 `target/…`） | build the kernel and the EFI loader | 否 no |
| `initramfs` | initramfs | 构建 initramfs | build the initramfs | 是 yes |
| `disk` | disk | 创建空的磁盘镜像 `build/mpos-uefi-x86_64.img`（分区 + 文件系统） | create the empty disk image (partitions + filesystems) | 是 yes |
| `rootfs` | rootfs | 把 Alpine 根文件系统装进镜像（会顺带更新 initramfs） | install the root filesystem into the image (refreshes the initramfs too) | 是 yes |
| `run` | run | 在 QEMU 中启动镜像；若产物过期会先自动重建 | boot the image in QEMU, rebuilding first if stale | 视情况 when needed |
| `all` | all | 一次做完：内核、initramfs、镜像、根文件系统 | build everything in one go | 是 yes |
| `test` | test | 自测：宿主单元测试 + 在 QEMU 里跑全部用户测试程序并解析结果 | self-test sweep: host unit tests + every user test program booted in QEMU | 否 no |
| `clean` | clean | 删除 `build/` 与 `target/` | remove the build and target directories | 视情况 when needed |

### 环境变量 | Environment variables

| 变量 | variable | 作用 | meaning |
| --- | --- | --- | --- |
| `MEM` | `MEM=2048` | 覆盖本次启动的内存(MB)，优先于 `[qemu] memory` | one-off memory override, wins over `[qemu] memory` |
| `ACCEL` | `ACCEL=kvm` / `tcg` | 强制 QEMU 加速器，否则自动探测 | force the accelerator, else autodetected |
| `MPOS_TEST_TIMEOUT` | 秒 seconds | `test` 里虚拟机最长运行时间（默认 900） | per-VM timeout for `test` (default 900s) |
| `MPOS_BUILD_DIR` | path | 默认构建目录 | default build directory |
| `PROFILE` | name | 默认构建档 | default profile |

---

## 典型流程 | Typical workflows

```sh
# 首次构建并启动 | first build and boot
cargo xtask all         # 建镜像 + 装根文件系统 (需要 sudo)
cargo xtask run         # 开窗启动

# 改了内核代码后反复验证 | iterate after editing the kernel
cargo xtask run         # 自动判断是否要重建，只重编必要部分

# 选运行模式启动 | boot in a chosen mode
cargo xtask run --mode gui        # 只看窗口 | window only
cargo xtask run --mode terminal   # 只看串口终端 | serial terminal only
cargo xtask run --mode weston     # 窗口留给合成器 | window for the compositor

# 跑完整自测 | full self-test
cargo test  -p mpos --lib --target x86_64-unknown-linux-gnu   # 只跑宿主单测
cargo xtask test        # 宿主单测 + 虚拟机内的用户测试程序

# 发布版 | release build
cargo xtask --profile release all
cargo xtask --profile release run

# 清理 | clean
cargo xtask clean
```

`run` 会在内核、loader、initramfs 或根文件系统有任何改动后重建磁盘镜像再启动——
也就是说 `run` 启动的一定是当前代码，而不是旧镜像。
`run` rebuilds the disk image whenever the kernel, loader, initramfs or root
filesystem has changed since it was last assembled, so it always boots current
code rather than a stale image.

---

## 配置：`rootfs.toml`（仓库根目录 | repo root）

镜像里装什么、开机用什么，都由根目录的 `rootfs.toml` 决定，被 `disk`/`rootfs`/
`initramfs`/`run` 读取。主要分节 | the image's contents and boot behaviour live
here, read by `disk`/`rootfs`/`initramfs`/`run`:

| 分节 | section | 内容 | contents |
| --- | --- | --- | --- |
| `[rootfs]` | | Alpine 镜像站 / 分支 / 架构、`repos`、文件系统类型、磁盘大小 `image_size`/`esp_size`、`base_packages` 与 `extra_packages` | Alpine mirror/branch/arch, filesystem, image sizes, package lists |
| `[system]` | | 主机名、时区、语言、`dns`（覆盖 `/etc/resolv.conf`）、`root_password` | hostname, timezone, locale, DNS, root password |
| `[user]` | | 除 root 外额外创建的账户（`name` 为空则跳过） | the non-root account (empty `name` skips it) |
| `[qemu]` | | `memory`、`cpus`、`vsock`、`audio`、`display`、`console`、`extra_args` | memory, cpus, vsock, audio, display backend, console mode, extra QEMU args |

> 需要写进根文件系统的具体文件（`/etc/inittab`、`/etc/network/interfaces`、
> `/etc/resolv.conf`、`/etc/doas.conf` 等）作为真实文件放在 `rootfs/` 目录树里；
> 它们会在 toml 写入的内容之后被叠加，**所以目录树里的文件优先**（唯一例外是
> `[system] dns` 会在叠加之后重写 `resolv.conf`）。
> Concrete files go in the `rootfs/` tree and are overlaid *after* anything written
> from the toml, so the tree wins (the one exception is `[system] dns`, which
> rewrites `resolv.conf` afterwards).

`[qemu] display` 控制交互式 `run` 的窗口后端：缺省为 `sdl`；可填 `gtk` 等，或
`none` 只看串口日志（但控制台已迁到 framebuffer，`none` 下串口就没有画面了）。
`test` 永远以 headless 方式跑，忽略此项。
`[qemu] display` picks the window backend for `run` (default `sdl`); use `none` to
watch only the serial log — though the console now renders to the framebuffer, so
`none` shows no picture. `test` always boots headless and ignores it.

---

## `run` 的四种运行模式 | The four `run` modes

`cargo xtask run` 用 `--mode` 一次决定整机怎么启动（缺省取 `rootfs.toml` 里的
`[qemu] mode`，再缺省为 `mirror`）。这个名字说的是整机模式而不是控制台选项：窗口、
串口以及客机自己的控制台策略必须一起定下来，所以是一个词。四种模式让 QEMU 的接线
（`-display` / `-serial`）和客机的引导参数（`mpos.*`）保持一致：

| 模式 | mode | 画面 | QEMU 接线 | 客机引导参数 |
| --- | --- | --- | --- | --- |
| 终端 | `terminal` | 只有串口终端，无窗口，GPU 不参与 | `-display none` + 串口走 stdio | `mpos.headless` |
| 图形 | `gui` | 只有 framebuffer 窗口，切换后串口静默 | `-display <后端>` + 串口丢弃 | `mpos.nomirror` |
| 镜像 | `mirror` | 窗口为主，串口尽力跟一份 | `-display <后端>` + 串口走 stdio | `mpos.mirror` |
| 合成器 | `weston` | 窗口归 weston（其 VT 在屏期间，由 root 的 `start-weston.sh` 启动）；`^A 1..6` 切走后窗口显示该控制台，串口始终有日志 | `-display <后端>` + 串口走 stdio | `mpos.compositor` |

切换很快：`--mode` 指定的模式会随特权子步骤刷新**只改引导文件**（`rootfs`
本来就跳过重装 Alpine），重写镜像里的 `mpos.cmdline`；`build/console.mode`
记录档让 `run` 在镜像已是该模式时**跳过提权**（该文件名沿用旧称：它只是缓存，
改名会让每个既有镜像看起来都需要重新烘焙一次）。

`weston` 一词是给人看的模式名，客机读到的引导字仍是 `mpos.compositor`——那是
`src/main.rs` 里的内核 ABI，不随 CLI 改名。

`cargo xtask run` picks the whole machine with `--mode` (default from
`rootfs.toml`'s `[qemu] mode`, else `mirror`). The word is about the machine, not
about a console setting: the window, the serial line and the guest's own console
policy have to be decided together, so each mode keeps the QEMU wiring and the
guest boot flag in step:

- **terminal** — serial only, no window, GPU never engaged (`mpos.headless`).
  The wire is the *lossless* console.
- **gui** — the framebuffer window only; the serial line carries nothing after the
  console moves over (`mpos.nomirror`).
- **mirror** — the window plus a **best-effort** copy on the serial line, so the
  same console shows in the window and the terminal at once (`mpos.mirror`).
- **weston** — the window belongs to the compositor for as long as its own
  virtual console is the one on the screen: the kernel console never moves onto
  the framebuffer, so a typed line cannot paint over weston, while the boot log
  still rides the serial line. Nothing launches the compositor; root's
  `./start-weston.sh` does, once the login prompt appears. `^A 1`..`^A 6`
  switches away anyway — the session is asked to release the display (and a
  session in `VT_PROCESS` is *asked*, which is why `^A` twice is the escape
  hatch), its page flips start answering `EACCES`, and the window shows the
  console you moved to. The boot word it bakes stays `mpos.compositor`, which is
  the kernel's own ABI, read by name in `src/main.rs`.

Switching is fast: the requested mode rides into the privileged step, which
refreshes **only the boot files** (`rootfs` already skips the Alpine reinstall) and
rewrites `mpos.cmdline`; a `build/console.mode` marker lets `run` skip the root
step when the image already carries that mode. (That marker keeps its old
`console.mode` filename on purpose: it is a cache, and a new name would read as
"nothing baked" and make every existing image look like it needed a rebuild.)

> **镜像不再拖累图形 | The mirror no longer throttles the window.** 镜像模式下
> 串口是尽力而为的：洪水般的输出（`yes`、`bat`）可能让串口那份记录丢字节，
> 但窗口永远按帧率刷新、mux 不会被 16550 线速卡住。想要一条不丢的完整串口记录，
> 用 `terminal` 模式。
> In mirror mode the serial copy is best-effort: a flood may drop bytes from the
> terminal transcript, but the window always repaints at frame rate and the mux
> never stalls behind the wire. For a lossless full serial log, use `terminal`.

---

## 在 `run` 的虚拟机里操作 | Interacting with the running guest

- **画面 | Picture**：`gui` / `mirror` 模式下 shell 与内核日志渲染到 QEMU 的图形
  窗口（framebuffer 控制台）；`mirror` 还会在串口尽力跟一份，`terminal` 则只有串口。
  In `gui`/`mirror` the shells and kernel log render into the QEMU window
  (the framebuffer console); `mirror` also copies to the serial line best-effort,
  and `terminal` shows only the serial line.
- **键盘 | Keyboard**：直接在图形窗口里输入（virtio-input）。
  Type directly into the window (virtio-input).
- **虚拟终端切换 | Virtual consoles**：`Ctrl-A` 然后数字 `1`–`6` 在六个 `/dev/ttyN`
  之间切换，各自保留自己的屏幕内容。
  `Ctrl-A` then a digit `1`–`6` switches among the six virtual consoles; each keeps
  its own screen.
- **Ctrl-C 归客机 | Ctrl-C goes to the guest**：串口以 `signal=off` 挂载，`Ctrl-C`
  被当作普通字符交给客机的行规程，而不是杀掉模拟器。
  The serial line is mounted with `signal=off`, so `Ctrl-C` reaches the guest's line
  discipline instead of killing the emulator.
- **退出 | Exit**：在客机里 `poweroff`，或直接关闭 QEMU 窗口。
  `poweroff` inside the guest, or just close the QEMU window.

---

## 产物位置 | Where things land

| 产物 | artefact | 位置 | path |
| --- | --- | --- | --- |
| 内核 | kernel | `target/x86_64-unknown-none/<profile>/mpos` | |
| EFI loader | loader | `target/x86_64-unknown-uefi/<profile>/mpos-boot.efi` | |
| 磁盘镜像 | disk image | `build/mpos-uefi-x86_64.img` | |
| initramfs | initramfs | `build/initramfs.tar` | |
| OVMF 变量副本 | OVMF vars | `build/OVMF_VARS.4m.fd` | |
| 测试串口日志 | test serial log | `build/test-serial.log`（`test` 时生成） | written by `test` |
