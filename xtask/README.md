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

**Rust 工具链 | Rust toolchain** — 六个 target | six targets:

| target | 用途 | used for |
| --- | --- | --- |
| `x86_64-unknown-none` | 内核（bare-metal） | the kernel |
| `x86_64-unknown-uefi` | EFI loader (`mpos-boot`) | the UEFI boot loader |
| `x86_64-unknown-linux-musl` | 静态用户程序 | the static user programs |
| `x86_64-unknown-linux-gnu` | 宿主单元测试 + xtask 自身 | host unit tests + xtask itself |
| `aarch64-unknown-none` | 第二台机器的内核（`boot-aarch64`） | the second machine's kernel |
| `aarch64-unknown-uefi` | 第二台机器的 loader | its loader |

```
rustup target add x86_64-unknown-none x86_64-unknown-uefi x86_64-unknown-linux-musl x86_64-unknown-linux-gnu aarch64-unknown-none aarch64-unknown-uefi
```

`rust-toolchain.toml` 已经列了这六个，所以第一次用到时 rustup 会自动装。
`rust-toolchain.toml` already names all six, so rustup installs them on first use.

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

**第二台机器 | the second machine**（只 `boot-aarch64` 用到 | only `boot-aarch64`）：

- `qemu-system-aarch64`
- AAVMF 固件 | AAVMF firmware（`/usr/share/AAVMF/AAVMF_CODE.fd` + `AAVMF_VARS.fd`，
  或 `/usr/share/edk2/aarch64/QEMU_EFI.fd` + `QEMU_VARS.fd`）

> `build`、`test`、`boot-aarch64` 和 `initramfs --arch aarch64` 全程**不需要** root。
> `build`, `test`, `boot-aarch64` and `initramfs --arch aarch64` never need root.

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
| 运行模式 | `--mode <MODE>` | 仅 `run`：`terminal` / `gui` / `mirror`（默认取 `machine.toml` 的 `[qemu] mode`，再默认 `mirror`）| `run` only: `terminal`/`gui`/`mirror` (defaults to `[qemu] mode` in `machine.toml`, else `mirror`) |
| 机器 | `--arch <ARCH>` | 全局：`x86_64`（默认）或 `aarch64`。三条路都真的处理那台机器：`run` 让第二台机器把 pid 1 交给它自己的 shell（`/init` 建好 `/tmp` 后 `execve("/bin/sh")`，`/bin` 是仓库内建的多调用二进制，见下），`test` 走它自己的 sweep（见下），`initramfs` 建那台机器的归档（见下）。`disk`/`rootfs`/`all` 组装的是 x86_64 的磁盘镜像，`--arch aarch64` 按名拒绝 | global: `x86_64` (default) or `aarch64`. Three commands really handle that machine: `run` hands the second one's pid 1 to its own shell (`/init` makes `/tmp` and `execve`s `/bin/sh`; `/bin` is the repository's multi-call binary — see below), `test` takes its own sweep (below), and `initramfs` builds that machine's archive (below). `disk`/`rootfs`/`all` assemble the x86_64 disk image, so `--arch aarch64` is refused by name |
| 测试分组 | `--groups <A,B>` | 仅 `test`：除分组外的测试之外再跑这些分组（取代 `tests.toml` 的 `default`）| `test` only: run these groups *in addition* to the ungrouped tests (replaces the file's `default`) |
| 全部分组 | `--all-groups` | 仅 `test`：连非默认分组一起跑 | `test` only: run every group, including the ones left out by default |

### 子命令 | Commands

| 命令 | command | 作用 | what it does | root? |
| --- | --- | --- | --- | --- |
| `build` | build | 编译内核与 EFI loader（产物在 `target/…`） | build the kernel and the EFI loader | 否 no |
| `initramfs` | initramfs | 构建 initramfs；`--arch` 选机器——两条路都真的建那台机器的归档，走同一条管线。第二台机器不用 root（它的 busybox 跨架构不能 exec，所以 farm 是手搓的符号链接、apk 走 `--usermode`），第一台机器要（它的 farm 是 `chroot … busybox --install -s`） | build the initramfs; `--arch` picks the machine — both really build that machine's archive, from one pipeline. The second machine needs no root (its cross-arch busybox cannot be execed, so the farm is linked by hand and apk runs with `--usermode`); the first does (its farm is `chroot … busybox --install -s`) | 见说明 see note |
| `disk` | disk | 创建空的磁盘镜像 `build/mpos-uefi-x86_64.img`（分区 + 文件系统） | create the empty disk image (partitions + filesystems) | 是 yes |
| `rootfs` | rootfs | 把 Alpine 根文件系统装进镜像（会顺带更新 initramfs） | install the root filesystem into the image (refreshes the initramfs too) | 是 yes |
| `run` | run | 在 QEMU 中启动镜像；若产物过期会先自动重建 | boot the image in QEMU, rebuilding first if stale | 视情况 when needed |
| `all` | all | 一次做完：内核、initramfs、镜像、根文件系统 | build everything in one go | 是 yes |
| `test` | test | 自测：宿主单元测试 + 在 QEMU 里跑 `tests.toml` 选中的用户测试程序并解析结果。`--arch aarch64` 走第二台机器自己的路：把 musl 测试程序、它们要启动的 `/bin` 工具与一个 runner（它同时做机器准备，例如建 `/tmp`）装上它的 ESP/磁盘、在 AAVMF 下启动、读同一套 `=== RUN/PASS/FAIL/SUMMARY`/`END` 协议。哪些程序算数由 `tests.toml` 定：`aarch64` 分组是第二台机器还服务不了的测试（现在是空的——socket 家族在栈落地后移出，spawn 家族在盘根有了 `/bin` 后移出，那个 `/bin` 是仓库内建的多调用二进制 `xtask/src/aarch64_bin.rs`），按分组逐个打印名字（原因写在该文件里），既不静默跳过也不记成失败；两台机器都不可能静默跑另一台 | self-test sweep: host unit tests + the user test programs `tests.toml` selects, booted in QEMU. `--arch aarch64` takes the second machine's own path: the musl test programs, the `/bin` tools they start and a runner (which also does the machine setup, e.g. making `/tmp`) are packed onto its ESP/disk, booted under AAVMF, and read with the same `=== RUN/PASS/FAIL/SUMMARY`/`END` protocol. Which programs count is `tests.toml`'s answer: its `aarch64` group is what the second machine cannot serve yet (empty now — the socket family left it when the stack landed and the spawn family when the disk root got a `/bin`, built in-tree as a multi-call binary in `xtask/src/aarch64_bin.rs`), printed by group and by name (the reason is in that file) rather than silently skipped or counted as a failure. Neither arm can silently boot the other machine | 否 no |
| `depgraph` | depgraph | 重新测量内核模块图并对照 Phase 11 的棘轮：任何新增的向上依赖或增长的计数都会让命令失败（它同时是一个宿主测试，`cargo xtask test` 会跑到） | re-measure the kernel's module graph against the Phase-11 ratchet; fails when an upward edge appears or a count grows (also a host test, so `test` runs it) | 否 no |
| `boot-aarch64` | boot-aarch64 | **第二台机器**：编译 aarch64 内核与它的 UEFI loader，铺一个 ESP，用 AAVMF 启动，再从 PL011 日志读结论（从 `M4_CONSOLE_OK` 到 `INIT_EXITED` 的整条 marker 链，含磁盘根、各 MMIO virtio 设备与它自己的 net 栈；`PLAN.md` §9.1） | **the second machine**: build the aarch64 kernel and its UEFI loader, lay out an ESP, boot it under AAVMF, and read the verdict off the PL011 log (the whole marker chain, `M4_CONSOLE_OK` through `INIT_EXITED`, including the disk root, the MMIO virtio devices and its own network stack; `PLAN.md` §9.1) | 否 no |
| `clean` | clean | 删除 `build/` 与 `target/` | remove the build and target directories | 视情况 when needed |

### 环境变量 | Environment variables

| 变量 | variable | 作用 | meaning |
| --- | --- | --- | --- |
| `MEM` | `MEM=2048` | 覆盖本次启动的内存(MB)，优先于 `machine.toml` 的 `[qemu] memory` | one-off memory override, wins over `[qemu] memory` in `machine.toml` |
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
cargo xtask run --mode terminal   # 只看串口终端 | serial terminal only
cargo xtask run --mode gui        # 整机进窗口：文本 tty 都在窗口里，终端静默 | the whole machine in the window
cargo xtask run --mode mirror     # 窗口和终端同时看到控制台 | both views at once

# 跑完整自测 | full self-test
cargo test  -p mpos --lib --target x86_64-unknown-linux-gnu   # 只跑宿主单测
cargo xtask test        # 宿主单测 + 虚拟机内的用户测试程序

# 看模块依赖有没有长回去 | check the module graph has not grown back
cargo xtask depgraph    # 打印模块边与互指模块对，越过棘轮就失败

# 第二台机器：走的是和 x86_64 一样的启动路径（固件 → ESP → loader → 高半区）
# the second machine, along the same boot path x86_64 takes
cargo xtask boot-aarch64            # 日志在 build/aarch64-serial.log


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

## 配置：`rootfs.toml` 与 `machine.toml`（仓库根目录 | repo root）

配置分成两个文件，分界不是按大小而是按**代价**：`rootfs.toml` 的 mtime 决定要不要重装
Alpine 全树（三十多个包，走网络），`machine.toml` 里改任何东西都不会动镜像。

The configuration is two files, and the line between them is cost rather than
tidiness: `rootfs.toml`'s mtime is what decides whether the Alpine tree is
reinstalled, so nothing that is not part of the image lives there.

| 文件 | file | 内容 | contents |
| --- | --- | --- | --- |
| `rootfs.toml` | | **镜像里装什么**：`[rootfs]`（镜像站/分支/架构、`repos`、文件系统、磁盘几何 `image_size`/`esp_size`、包列表）、`[system]`（主机名、时区、语言、`dns`、`root_password`）、`[user]` | **what the image contains**: Alpine source, repositories, filesystem, disk geometry, package lists, hostname/timezone/locale/dns/root password, the extra account |
| `machine.toml` | | **拿什么启动它**：`[qemu]` 的 `memory`、`cpus`、`vsock`、`audio`、`display`、`extra_args`、`mode`。第二台机器不共享的值以 `aarch64_` 前缀写在同一个文件里（`aarch64_memory = "256"` 胜过 `memory`），第一台机器看不到它们——一台机器一个值的，一处写一次。**设备的传输形态不在这里**：第二台机器全部走 virtio-MMIO，因为它靠自己的设备树找设备，那是机器的事实而不是设置（`xtask/src/qemu.rs::aarch64_machine_args`） | **what it is booted on**: memory, cpus, vsock, audio, window backend, extra QEMU args, the standing run mode. A value the second machine does not share is written in the same file with its own name in front (`aarch64_memory = "256"` beats `memory`) and is invisible to the first — one value per machine, written once. **The device *transports* are not here**: the second machine drives every device over virtio-MMIO, because it finds them by walking its own device tree — that is a machine fact, not a setting (`xtask/src/qemu.rs::aarch64_machine_args`) |

两个键跨在线上，都是**读过去**而不是各写一份：`[system] hostname` 也用作 QEMU DHCP
告诉客机的名字，`[rootfs] serial_port` 同时是引导控制台与镜像里 getty 的名字——它们存
在镜像那侧（是写进镜像的），`machine.toml` 那侧读它。

Two keys cross the line and both are read *through* rather than duplicated:
`[system] hostname` is also the name QEMU's DHCP server is told, and
`[rootfs] serial_port` names both the boot console and the image's getty. They are
stored with the image, which is where they are written, and the machine half reads
them from there. `image_size`/`esp_size` stay with the image because changing
either re-makes the partitions and their filesystems — a rebuild that file should
ask for.

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

## 配置：`tests.toml` —— 跑哪些用户测试 | which user tests run

`cargo xtask test` 默认只跑**不属于任何分组**的测试程序。一个测试在 `tests.toml` 里被列进
某个分组，就不再默认运行——因为它需要机器之外的东西（宿主的网络、宿主上的监听进程），
或者慢、或者只是测量，或者根本不是断言（`t-sh` 是诊断工具）。这么做的收益是门禁的意义
本身：默认集变红，说的就是内核，而不是网络、宿主的解析器，或恰好很忙的宿主。

`cargo xtask test` runs the tests that are in **no group**. A test listed in a
group is left out of the default sweep, because it needs something outside the
machine, or takes seconds, or measures rather than asserts:

| 分组 | group | 为什么默认不跑 | why it is not in the default sweep |
| --- | --- | --- | --- |
| `external` | | 需要机器之外的东西：宿主的网络（经 QEMU 用户态栈）或宿主上监听的服务 | needs something outside the machine: the host's network, or a listener on the host |
| `slow` | | 慢，或只是测量：`t-fill-ramfs` 最多写 1024 MiB，其余是计时仪器 | slow, or measurement rather than assertion |
| `diagnostic` | | 不是检查：`t-sh` 驱动一个交互 shell 并打印观察 | not a check: it drives an interactive shell and prints what it sees |

```sh
cargo xtask test                      # 默认集：分组外的全部测试
cargo xtask test --groups external    # 再多跑 external（命名分组会取代文件里的 default）
cargo xtask test --groups external,slow
cargo xtask test --all-groups         # 全部，含 slow / external / diagnostic
```

`default` 键给出默认额外要跑的分组（出厂为空）。分组名随你加，一个测试也能出现在多个分组
里，那么在任一被点名的分组下都会跑。两种「静默少跑」都会被拒绝：命令行写了文件里没有的
分组名会报错并列出可选名字，文件里写了没有任何已构建程序对应的名字也会报错（分组比测试
活得久，或者拼错了一个名字）。被跳过的测试按分组打印出来，判决只对**装进镜像的那些**要求，
所以「N built」和「M left out」两件事都说得清楚。

`default` names the groups the sweep adds to the ungrouped tests (empty as
shipped), group names are yours to add, and a test may appear in several. Two
kinds of silent narrowing are refused: a group name on the command line that the
file does not define, and a name in the file that no built program answers to.
What was skipped is printed group by group, and the verdict check only requires a
`=== PASS` from the programs that were staged — so "N built" and "M left out"
are both said out loud.

---

## `run` 的三种运行模式 | The three `run` modes

`cargo xtask run` 用 `--mode` 一次决定整机怎么启动（缺省取 `machine.toml` 里的
`[qemu] mode`，再缺省为 `mirror`）。窗口和串口怎么接线是启动方的事；客机只被告知
一件事——它有哪些控制台——而这件事用的是 Linux 自己的参数 `console=`：

| 模式 | mode | 画面 | QEMU 接线 | 客机引导参数 |
| --- | --- | --- | --- | --- |
| 终端 | `terminal` | 只有串口终端，无窗口 | `-display none` + 串口走 stdio | `console=ttyS0` |
| 图形 | `gui` | 整机进窗口：文本 tty（启动日志、虚拟终端、登录、shell）全显示在窗口里，宿主机终端静默 | `-display <后端>` + 串口接到 `build/serial.log` | `console=tty0` |
| 镜像 | `mirror` | 窗口为主，串口尽力跟一份 | `-display <后端>` + 串口走 stdio | `console=ttyS0 console=tty0` |

只有两个问题要回答——有没有窗口、framebuffer 算不算客机的控制台——所以模式只有三个，
且三个各有一条自己的 `console=` 列表。把窗口让给合成器是运行时的事实（weston 会
`SET_MASTER` 拿走光栅、用 `KDSETMODE(KD_GRAPHICS)` 声明屏幕归它画），命令行说的只是
"客机有哪些控制台"：`gui` 的答案是 framebuffer 独占，于是文本 tty 就显示在窗口里。

三个模式都保留串口这根线，但只有 `terminal` 和 `mirror` 把它接到宿主机终端；`gui`
把它接到 `build/serial.log`。以前有一个模式把串口丢进 `/dev/null`，它同时也把内核日志
丢了——客机虽然不再往串口写字节，但 UEFI 引导器无条件打 COM1、内核命令行解析之前的
banner 也在写、panic 的 `force_serial` 报告只会走这条线，所以宿主机终端可以静默，
不能失聪。

客机那一列只有 `console=` 一个词，规则也是 Linux 的：可以重复，列上的每一个都收日志，
**最后一个**是首选控制台（写不下去就等、不丢字节的那一个）。于是「镜像」不再是一个
新概念，它就是"两个都列上、framebuffer 在后"。命令行的形状因此不再随"这个镜像打算怎么
给人看"而变。

`--mode terminal` 也仍然会探测显示设备、`/dev/fb0` 仍然可用：控制台不在屏幕上，
不等于设备不在机器上。

一个这个词组里没有的名字（包括已经删除的 `weston` 与 `compositor`）在 `run` 处是
错误，不是默认值：它们烘焙的 `console=` 列表不同，静默回退等于启动一台没人要求的机器。
`test` 不开窗口，所以永远不读这个键，也不会被它的拼写错误绊住。

切换很快：`--mode` 指定的模式会随特权子步骤刷新**只改引导文件**（`rootfs`
本来就跳过重装 Alpine），重写镜像里的 `mpos.cmdline`；`build/console.mode`
记录档让 `run` 在镜像已是该模式时**跳过提权**（该文件名沿用旧称：它只是缓存，
改名会让每个既有镜像看起来都需要重新烘焙一次）。早于运行模式的镜像、以及上一次以
`weston` 烘焙的镜像都没有这个记录档可读，因此各花一次刷新——`weston` 当年烘焙的
是 `console=ttyS0`，刷新会真的把命令行改写成今日 `gui` 的 `console=tty0`。

`cargo xtask run` picks the whole machine with `--mode` (default from
`machine.toml`'s `[qemu] mode`, else `mirror`). How the window and the serial line
are wired is the launcher's business; the guest is told exactly one thing - which
consoles it has - and that is said in Linux's own word, `console=`:

- **terminal** — serial only, no window (`console=ttyS0`). The wire is the
  *lossless* console: it is the only one, so it is also the preferred one. The
  display device is still probed and `/dev/fb0` still answers; nothing is drawn
  on it.
- **gui** — the whole machine in the window: a window with every device behind
  it and `console=tty0`, so the text console — boot log, virtual consoles, the
  login prompt, the shells — shows **in the window**, and the launcher's terminal
  shows nothing. A compositor is still a runtime guest, not a word: log in on the
  window's tty1 and run root's `./start-weston.sh`; weston opens the card and takes
  `DRM_IOCTL_SET_MASTER`, which binds the master to its console, and `KDSETMODE`'s
  `KD_GRAPHICS` is its own word that the screen is its to draw — from then on the
  kernel text lands in the grid's memory but stops being painted over it. `^A
  1`..`^A 6` — the kernel's own mux, read from the window's keyboard — switches
  back to the console and away again (and a session in `VT_PROCESS` is *asked*,
  which is why `^A` twice is the escape hatch).
- **mirror** — both consoles, framebuffer preferred (`console=ttyS0
  console=tty0`): the window takes the console over once the framebuffer is up,
  and the wire keeps a **best-effort** copy of the same text on the host terminal.

Every mode keeps the serial line tapped, but only `terminal` and `mirror` read it
off stdio; `gui` reads it into `build/serial.log`. One mode used to send it to
`/dev/null`, and it threw the kernel log away with it — the guest may stop writing
to the port, but the UEFI loader prints to COM1 unconditionally, the kernel's banner
precedes the command-line parse, and a panic's `force_serial` report reaches no
screen. The terminal may go quiet; it must not go deaf.

A word neither source recognises - including the retired `weston` and `compositor` -
is an error at `run`, not a default: the modes bake different `console=` lists, so
silently booting `mirror` would be a machine nobody asked for. `test` never attaches
a window, so it never reads the key and cannot be tripped by its spelling.

Switching is fast: the requested mode rides into the privileged step, which
refreshes **only the boot files** (`rootfs` already skips the Alpine reinstall) and
rewrites `mpos.cmdline`; a `build/console.mode` marker lets `run` skip the root
step when the image already carries that mode. (That marker keeps its old
`console.mode` filename on purpose: it is a cache, and a new name would read as
"nothing baked" and make every existing image look like it needed a rebuild. An
image with no marker at all was built before the run modes existed, and one baked
as `weston` has a marker nothing reads any more; each costs the one refresh, and
`weston`'s rewrites the line for real — it baked `console=ttyS0`, which today's
`gui` replaces with `console=tty0`.)

> **镜像不再拖累图形 | The mirror no longer throttles the window.** 镜像模式下
> 串口是尽力而为的：洪水般的输出（`yes`、`bat`）可能让串口那份记录丢字节，
> 但窗口永远按帧率刷新、mux 不会被 16550 线速卡住。想要一条不丢的完整串口记录，
> 用 `terminal` 模式。
> In mirror mode the serial copy is best-effort: a flood may drop bytes from the
> terminal transcript, but the window always repaints at frame rate and the mux
> never stalls behind the wire. For a lossless full serial log, use `terminal`.

---

## 在 `run` 的虚拟机里操作 | Interacting with the running guest

- **画面 | Picture**：`gui` 把文本 tty（内核日志、登录、shell）整个渲染进 QEMU 的图形
  窗口（framebuffer 是客机唯一的控制台），宿主机终端静默、串口副本在 `build/serial.log`；
  `./start-weston.sh` 之后窗口归 weston，`^A 1..6` 切回文本控制台；`mirror` 同样把
  shell 与日志画进窗口（framebuffer 是首选控制台），但串口在终端上尽力跟一份；
  `terminal` 则只有串口。
  In `gui` the shells, the login prompt and the kernel log all render into the QEMU
  window (the framebuffer is the guest's only console) and the host terminal stays
  quiet, with the wire copied to `build/serial.log`; weston owns the window once
  `./start-weston.sh` runs it, and `^A 1..6` switches back to the text console.
  `mirror` draws the same console into the window too (the framebuffer is
  preferred) but keeps a best-effort copy on the terminal; `terminal` shows only
  the serial line.
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
| aarch64 内核 | aarch64 kernel | `target/aarch64-unknown-none/<profile>/mpos`（`boot-aarch64`） | |
| aarch64 loader | aarch64 loader | `target/aarch64-unknown-uefi/<profile>/mpos-boot.efi` | |
| aarch64 ESP | aarch64 ESP | `build/aarch64-esp/`（`EFI/BOOT/BOOTAA64.EFI` + `mpos.elf` + `mpos.cmdline`） | |
| aarch64 串口日志 | aarch64 serial log | `build/aarch64-serial.log`（`boot-aarch64` 时生成，结论就从这里读） | the log `boot-aarch64` reads its verdict from |
