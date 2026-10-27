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
| `aarch64-unknown-none` | 第二台机器的内核（`--arch aarch64`） | the second machine's kernel |
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

**第二台机器 | the second machine**（`--arch aarch64` 用得到它 | needed by `--arch aarch64`）：

- `qemu-system-aarch64`（`run`/`test` 的 aarch64 臂，默认就在 sweep 里）
- AAVMF 固件 | AAVMF firmware（`/usr/share/AAVMF/AAVMF_CODE.fd` + `AAVMF_VARS.fd`，
  或 `/usr/share/edk2/aarch64/QEMU_EFI.fd` + `QEMU_VARS.fd`）

> 两台机器走**同一条**命令与同一条构建路径，`--arch` 只是选机器：`build`、`test`、
> `depgraph` 和 `initramfs --arch aarch64` 全程**不需要** root；`disk`/`rootfs`/`all`
> 需要（它们经块设备挂载镜像来装根文件系统），需要重建镜像的 `run` 也一样——两条机器
> 的规则现在完全相同。
> Both machines take the **same** commands and the same build path; `--arch` only picks
> the machine. `build`, `test`, `depgraph` and `initramfs --arch aarch64` never need
> root; `disk`/`rootfs`/`all` do (they mount the image through the block layer to
> install a root filesystem), and so does a `run` that has to rebuild — one rule for
> both machines.

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
| 运行模式 | `--mode <MODE>` | 仅 `run`：`terminal` / `gui` / `mirror`（默认取 `machine.toml` 的 `qemu.mode`，再默认 `mirror`）| `run` only: `terminal`/`gui`/`mirror` (defaults to `qemu.mode` in `machine.toml`, else `mirror`) |
| 机器 | `--arch <ARCH>` | 全局：`x86_64`（默认）或 `aarch64`。**每条命令都真的处理那台机器**：`build` 编它的内核与 loader，`disk`/`rootfs`/`all` 铺并安装**它自己的**磁盘镜像（`mpos-uefi-x86_64.img` / `aarch64-disk.img`，各带自己的 stamp），`run` 启动它（两台机器的 pid 1 都是盘上标准的 `/sbin/init`，busybox init 读 `/etc/inittab`——同一条链），`test` 走同一套盘上 sweep（选哪些程序的规则也相同，只有 `tests.toml` 的 `[aarch64]` 段是第二台自己的），`initramfs` 建那台机器的归档。两台机器之间只剩真事实的差别，都在 `Arch` 与各配置文件的 `[aarch64]` 段里 | global: `x86_64` (default) or `aarch64`. **Every command really handles that machine**: `build` compiles its kernel and loader, `disk`/`rootfs`/`all` lay and install **its own** disk image (`mpos-uefi-x86_64.img` / `aarch64-disk.img`, each with its own stamp), `run` boots it (both machines' pid 1 is the disk's own standard `/sbin/init`, busybox init reading `/etc/inittab` — one chain), `test` takes the same on-disk sweep (the selection rule is the same too — only `tests.toml`'s `[aarch64]` section is the second machine's own) and `initramfs` builds that machine's archive. What is left between the machines is only real differences, held in `Arch` and in each file's `[aarch64]` section |
| 全部机器 | `--all-targets` / `--no-all-targets` | 仅 `test`：**两台机器是默认**（`--all-targets` 只是把默认说出口）；每台机器跑**同一套** lanes、同一块实时面板、同一份报告，只有 payload 的载体不同（第一台的 boot 归档就是它的根，第二台铺自己的盘）。指了 `--arch` 后此旗标无事可做 | `test` only: **both machines are the default** (`--all-targets` just spells the default out); each machine runs the **same** lanes, the same live dashboard and the same report, only the payload's channel differs (the first machine's boot archive is its root; the second lays its own disk). With `--arch` named, this flag adds nothing |
| 测试分组 | `--groups <A,B>` | 仅 `test`：除分组外的测试之外再跑这些分组（取代 `tests.toml` 的 `default`）| `test` only: run these groups *in addition* to the ungrouped tests (replaces the file's `default`) |
| 全部分组 | `--all-groups` | 仅 `test`：连非默认分组一起跑 | `test` only: run every group, including the ones left out by default |

### 子命令 | Commands

| 命令 | command | 作用 | what it does | root? |
| --- | --- | --- | --- | --- |
| `build` | build | 编译内核与 EFI loader（产物在 `target/…`） | build the kernel and the EFI loader | 否 no |
| `initramfs` | initramfs | 构建 initramfs；`--arch` 选机器——两条路都真的建那台机器的归档，走同一条管线。第二台机器不用 root（它的 busybox 跨架构不能 exec，所以 farm 是手搓的符号链接、apk 走 `--usermode`），第一台机器要（它的 farm 是 `chroot … busybox --install -s`） | build the initramfs; `--arch` picks the machine — both really build that machine's archive, from one pipeline. The second machine needs no root (its cross-arch busybox cannot be execed, so the farm is linked by hand and apk runs with `--usermode`); the first does (its farm is `chroot … busybox --install -s`) | 见说明 see note |
| `disk` | disk | 创建**该机器**的空磁盘镜像（`build/mpos-uefi-x86_64.img` 或 `build/aarch64-disk.img`：GPT + 文件系统） | create that machine's empty disk image (a GPT: partitions + filesystems) | 是 yes |
| `rootfs` | rootfs | 把该机器要的 Alpine 根文件系统装进它的镜像（会顺带更新 initramfs）。按包安装：`--arch x86_64`/`aarch64` 各自解析自己的包，跨架构那台加 `--no-scripts` 并跳过需要跑镜像内程序的 chroot 半区 | install that machine's Alpine root into its image (refreshes the initramfs too). Per package: each machine resolves its own `--arch`, and the cross-arch one adds `--no-scripts` and skips the chroot half that runs the image's own programs | 是 yes |
| `run` | run | 在 QEMU 中启动该机器的镜像；若产物过期会先自动重建（只改了内核/loader/initramfs → 只刷新引导文件，不重装包） | boot that machine's image in QEMU, rebuilding first if stale (a kernel/loader/initramfs change refreshes the boot files alone — no package install) | 视情况 when needed |
| `all` | all | 一次做完：内核、initramfs、镜像、根文件系统 | build everything in one go | 是 yes |
| `test` | test | 自测：宿主单元测试一次 + 对**默认两台机器各跑同一套 sweep**（`--arch` 收窄到一台，`--no-all-targets` 是旧的单机默认）。每台机器同一条 lanes、同一块实时面板（`tui::Panel`）、同一份报告：payload 的载体是唯一差别——第一台的 boot 归档就是它的根（lane 的 QEMU 走完写 debug-exit 33），第二台各 lane 铺自己的盘、跑完打印 `=== END` 由 harness 停机。选哪些程序是同一条规则——一份 `tests.toml` 同时回答两台机器：不属于任何分组、或属于本次点名的分组的程序才跑，唯一不能按名点的是机器自己的 `gaps`；两台机器都不可能静默跑另一台 | self-test: host unit tests once + the **same sweep on both machines by default** (`--arch` narrows to one, `--no-all-targets` is the old single-machine default). Each machine runs the same lanes, the same live dashboard (`tui::Panel`) and the same report; the payload's channel is the only difference — the first machine's boot archive *is* its root (a lane ends on the debug exit 33), the second lays its own disk per lane and ends on `=== END`, stopped by the harness. The *selection* is one rule — one `tests.toml` answers both machines: a program runs when it is in no group or in a group the run asked for, and the only group neither may ask for is a machine's own `gaps`. Neither arm can silently boot the other machine | 否 no |
| `test-host` | test-host | 在**宿主机**（真实 Linux 内核）上跑全部 `t-*`：为 `x86_64-unknown-linux-gnu` 构建 `mpos-user-tests`，逐个执行，输出留在 `build/test-host/<name>.log`。**不是 mpos 门禁**——它量的是宿主内核；依赖 root、guest 设备或 mpos 专属 `/proc` 行的程序会失败，那正是它要显示的东西。拒绝以 root 运行（`t-reboot-libc` 会真的重启本机）；每个程序独立进程组与工作目录 | run every `t-*` on the **host** (the real Linux kernel): build `mpos-user-tests` for `x86_64-unknown-linux-gnu`, execute each program, keep its output under `build/test-host/<name>.log`. **Not the mpos gate** — it measures the host; programs that expect root, the guest's devices or mpos-only `/proc` rows fail, which is what it is there to show. Refuses to run as root (`t-reboot-libc` would really reboot the machine); each program gets its own process group and working directory | 否 no |
| `depgraph` | depgraph | 重新测量内核模块图并对照 Phase 11 的棘轮：任何新增的向上依赖或增长的计数都会让命令失败（它同时是一个宿主测试，`cargo xtask test` 会跑到） | re-measure the kernel's module graph against the Phase-11 ratchet; fails when an upward edge appears or a count grows (also a host test, so `test` runs it) | 否 no |
| `clean` | clean | 删除 `build/` 与 `target/` | remove the build and target directories | 视情况 when needed |

### 环境变量 | Environment variables

| 变量 | variable | 作用 | meaning |
| --- | --- | --- | --- |
| `MEM` | `MEM=2048` | 覆盖本次启动的内存(MB)，优先于 `machine.toml` 的 `qemu.memory` | one-off memory override, wins over `qemu.memory` in `machine.toml` |
| `ACCEL` | `ACCEL=kvm` / `tcg` | 强制 QEMU 加速器，否则自动探测 | force the accelerator, else autodetected |
| `MPOS_TEST_TIMEOUT` | 秒 seconds | `test` 里**整次扫描**的最长时间（默认 900）：各 lane 共用这一个预算，任一 lane 超时就整体失败 | the **whole sweep's** budget for `test` (default 900s): the lanes share it, and a lane that outlives it fails the run |
| `MPOS_TEST_LANES` | 台 machines | `test` 同时启动几台测试机（默认：每个宿主核心一台，最多 6 台；`1` 就是串行扫描）。每台是一整台机器（`machine.toml` 的 vCPU 与内存上限），程序按**上一次扫描测出的耗时**从长到短分配（表在 `build/test-costs-x86_64.tsv`；没有表时按数量均分） | how many test machines `test` boots at once (default: one per host core, at most 6; `1` is the serial sweep). Each lane is a whole machine (`machine.toml`'s vCPU and memory ceiling), and the programs are dealt longest-first from **the previous sweep's measured costs** (the table is `build/test-costs-x86_64.tsv`; with no table they are dealt evenly by count) |
| `MPOS_TEST_TUI` | `0` | 关掉 `test` 的实时面板（默认只在 stdout 是终端时画；重定向、管道、CI 自动是纯文本） | turn off `test`'s live panel (`0`); the panel is only drawn when stdout is a terminal, so a redirect, a pipe or CI is plain text anyway |
| `MPOS_HOST_TEST_FILTER` | 子串 substring | 仅 `test-host`：只跑名字含该子串的程序。本地调试用；命令本身默认跑**全部** `t-*` | `test-host` only: run the programs whose name contains this substring. Local debugging; the command runs **every** `t-*` by default |
| `MPOS_HOST_TEST_TIMEOUT_SECS` | 秒 seconds | 仅 `test-host`：覆盖 `tests.toml` 给每个程序的预算（临时逃生口；正常配置写在文件里）；超时按整个进程组终止 | `test-host` only: override every per-program budget from `tests.toml` (a temporary escape hatch; normal configuration lives in the file); a timeout kills the whole process group |
| `MPOS_BUILD_DIR` | path | 默认构建目录 | default build directory |
| `PROFILE` | name | 默认构建档 | default profile |

### `test` 的输出 | what `test` prints

扫描运行时若 stdout 是终端，会画一个实时面板：每条 lane 一行（进度条、`done/total`、
正在跑的程序、这台机器自己的耗时），上面一行是整次扫描的进度与时钟。面板结束后
（回到原来的屏幕）打印最终报告：

```
── self-test ────────────────────────────────────────────────
  stage 1  host unit tests   1068 tests, 0 failed        6.28 s
  stage 2  boot + sweep       192 programs over 6 lane(s) 5.99 s
  lane  0    1 program(s)    5.99 s  ok    heaviest t-udpblock 3.21 s
  ...
  verdict  pass=192 fail=0
  kunit    mpos-kunit 32 passed, 0 failed
  slowest  t-udpblock 3.21 s · t-waitid 1.21 s · ...
  total    6.43 s   (previous green run 6.93 s, -7.2%)
```

- 每条 lane 的 `heaviest` 来自同一张实测成本表，下一次分配也用它，所以两处的结论不会互相矛盾。
- `total` 与**上一次绿色运行**比较：每个机器一份 `build/test-gate-history-<machine>.txt`
  （每行一次毫秒数；红运行不记录，因为它的时长只是“早停”的时长）。
- 报告在失败时也打印（失败正是有人读它的那次）；面板不会写进任何日志：
  重定向、管道、CI，或 `MPOS_TEST_TUI=0`，输出的仍是原来那些逐行文本。
- 面板下 `ctrl-c` 仍然中断扫描（退出码 130）：raw 模式吃掉了终端的信号键，所以有一个
  线程专门读这个键并先恢复终端。

While the lanes run, a terminal on stdout gets a live panel — one line per lane with a bar,
`done/total`, the program it is inside and that machine's own time, under the sweep's
progress and clock — and the command ends with the report above (stage times, every lane,
the verdict, the ten slowest programs, and the delta against the last green run). Each
lane's `heaviest` comes from the same measured cost table the next deal uses, so the two
readings cannot disagree. `total` is compared with the **last green run** of that machine,
kept in `build/test-gate-history-<machine>.txt` (one millisecond count per line; a red run
is not recorded, since its duration is the duration of a run that stopped early). The report
also prints on a failure, and the panel never writes into a log: a redirect, a pipe, CI or
`MPOS_TEST_TUI=0` gets the plain lines this command always printed. `ctrl-c` still stops the
sweep under the panel (exit 130). The panel and this summary are the lane arm's: the aarch64
disk sweep is one serial machine and ends with its own report.

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
cargo xtask test-host   # 在宿主 Linux 内核上跑全部 t-*（对照，不是门禁）
MPOS_HOST_TEST_FILTER=t-mount cargo xtask test-host   # 只跑名字含 t-mount 的

# 看模块依赖有没有长回去 | check the module graph has not grown back
cargo xtask depgraph    # 打印模块边与互指模块对，越过棘轮就失败

# 第二台机器：同一条命令、同一条路径（固件 → ESP → loader → 高半区），只换 --arch
# the second machine: the same command and the same boot path, with another --arch
cargo xtask all --arch aarch64      # 建镜像 + 装它的根文件系统 (需要 sudo)
cargo xtask run --arch aarch64      # 交互式：登录提示在串口线上（root，无密码）
cargo xtask test --arch aarch64     # 它的用户测试扫描：=== SUMMARY pass=N fail=M


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

## 配置：`rootfs.toml` 与 `machine.toml`（在 `assets/configs/` | under `assets/configs/`）

配置分成两个文件，分界不是按大小而是按**代价**：`rootfs.toml` 决定装什么，但不再以
mtime 一刀切——stamp 记录两组内容哈希：**shape keys**（镜像站/分支/`repos`/文件系统/
磁盘几何）变了才全量重装；只改**包列表**则做差集增删（读 `lib/apk/db/installed`，只删
上次构建 own 过的包）。`.apk` 下载落在 `build/apk-cache/<arch>/`，索引命中即可离线重跑。
`--force-full` 强制回到全量路径。`machine.toml` 里改任何东西都不会动镜像。每份配置
都只有一个文件、同时描述**两台**机器，用三个段：`[default]` 是共享基线，`[x86_64]` /
`[aarch64]` 各自覆盖它——键名自带前缀（`qemu.memory`、`rootfs.mirror`、`system.hostname`）。
机器本身不是键：apk 的包架构就是构建时 `--arch` 的那个词（`xtask/src/arch.rs`）。

The configuration is two files, and the line between them is cost rather than
tidiness: `rootfs.toml` decides what is installed, but no longer by a single
mtime — the stamp records two content hashes. A change to the **shape keys**
(mirror, branch, `repos`, filesystem, disk geometry) still lays the whole image;
a change to the **package lists** alone installs and removes only the difference,
read against the tree's own `lib/apk/db/installed`, and removes only packages an
earlier build of this series owned. Downloaded `.apk` files land in
`build/apk-cache/<arch>/`, so a warm cache answers an offline host; `--force-full`
lays the image fresh whatever the hashes say. Nothing that is not part of the
image lives in `rootfs.toml`. Each file
describes **both** machines in three sections — `[default]` is the shared baseline
and `[x86_64]`/`[aarch64]` override it — with keys dotted by what they belong to.
The machine itself is not a key: the package architecture `apk` is asked for is
the `--arch` the build was given (`xtask/src/arch.rs`).

| 文件 | file | 内容 | contents |
| --- | --- | --- | --- |
| `rootfs.toml` | | **镜像里装什么**：`rootfs.*`（镜像站/分支/架构、`repos`、文件系统、磁盘几何 `image_size`/`esp_size`、包列表）、`system.*`（主机名、时区、语言、`dns`、`root_password`）、`user.*` | **what the image contains**: Alpine source, repositories, filesystem, disk geometry, package lists, hostname/timezone/locale/dns/root password, the extra account |
| `machine.toml` | | **拿什么启动它**：`qemu.*` 的 `memory`、`cpus`、`vsock`、`audio`、`display`、`extra_args`、`mode`。第二台机器不共享的值写在它自己的 `[aarch64]` 段里（`qemu.memory = "256"` 胜过基线），第一台机器看不到它们——一台机器一个值的，一处写一次。**设备的传输形态不在这里**：第二台机器全部走 virtio-MMIO，因为它靠自己的设备树找设备，那是机器的事实而不是设置（`xtask/src/qemu.rs::virt_machine_args`） | **what it is booted on**: memory, cpus, vsock, audio, window backend, extra QEMU args, the standing run mode. A value the second machine does not share is written in its own `[aarch64]` section (`qemu.memory = "256"` beats the baseline) and is invisible to the first — one value per machine, written once. **The device *transports* are not here**: the second machine drives every device over virtio-MMIO, because it finds them by walking its own device tree — that is a machine fact, not a setting (`xtask/src/qemu.rs::virt_machine_args`) |

两个键跨在线上，都是**读过去**而不是各写一份：`system.hostname` 也用作 QEMU DHCP
告诉客机的名字，`rootfs.serial_port` 同时是引导控制台与镜像里 getty 的名字——它们存
在镜像那侧（是写进镜像的），`machine.toml` 那侧读它。

Two keys cross the line and both are read *through* rather than duplicated:
`system.hostname` is also the name QEMU's DHCP server is told, and
`rootfs.serial_port` names both the boot console and the image's getty. They are
stored with the image, which is where they are written, and the machine half reads
them from there. `rootfs.image_size`/`esp_size` stay with the image because changing
either re-makes the partitions and their filesystems — a rebuild that file should
ask for.

> 需要写进根文件系统的具体文件（`/etc/inittab`、`/etc/network/interfaces`、
> `/etc/resolv.conf`、`/etc/doas.conf` 等）作为真实文件放在 `assets/rootfs/<machine>/` 目录树里；
> 它们会在 toml 写入的内容之后被叠加，**所以目录树里的文件优先**（唯一例外是
> `system.dns` 会在叠加之后重写 `resolv.conf`）。
> Concrete files go in the `assets/rootfs/<machine>/` tree and are overlaid *after* anything written
> from the toml, so the tree wins (the one exception is `system.dns`, which
> rewrites `resolv.conf` afterwards).

`qemu.display` 控制交互式 `run` 的窗口后端：缺省为 `sdl`；可填 `gtk` 等，或
`none` 只看串口日志（但控制台已迁到 framebuffer，`none` 下串口就没有画面了）。
`test` 永远以 headless 方式跑，忽略此项。
`qemu.display` picks the window backend for `run` (default `sdl`); use `none` to
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
| `gaps` | | 不是理由而是**排除**：写在哪台机器自己的段里，就是那台机器还不能服务的程序（今天只有 `t-arch-prctl`——`arch_prctl` 是 x86_64 自己的调用）。它按名点不动，`--groups gaps` 会被拒绝 | not a reason but an **exclusion**: written in a machine's own section, it lists what *that* machine cannot serve yet (today only `t-arch-prctl` — `arch_prctl` is x86_64's own call). It cannot be asked for by name: `--groups gaps` is refused |

文件用这里每个配置文件都有的三段写成：`[default]` 是两台机器都读的基线，`[x86_64]` /
`[aarch64]` 是各自的半边。基线里的分组两台都认（DHCP 服务器、宿主的解析器、一个公网地址
在两台机器外面都一样远）；写在机器自己段里的分组只属于它，而且是**加进**基线同名分组，
不是替换——`[aarch64] external = ["t-x"]` 说的是"那边多一个需要互联网的程序"，而不是
"`external` 在那边是另一个意思"。一个分组比一台机器活得久，所以段的边界才是机器属性的
位置，组名不是。
The file is written in the three sections every config file here uses:
`[default]` is the baseline both machines read, `[x86_64]`/`[aarch64]` are each
machine's own half. A group in the baseline binds both sweeps (slirp's DHCP
server, the host's resolver and a public address are equally outside either
machine); a group under a machine's own section is that machine's alone and is
*added* to the baseline's group of the same name rather than replacing it —
`[aarch64] external = ["t-x"]` says "one more program needs the internet
*there*", not "`external` means something else over there". A group outlives a
machine, so the section boundary is where machine-ness lives, not the group name.

```sh
cargo xtask test                      # 默认集：分组外的全部测试
cargo xtask test                       # 两台机器,默认(= --all-targets)
cargo xtask test --arch aarch64        # 只跑第二台
cargo xtask test --no-all-targets      # 只跑第一台(旧的默认)
cargo xtask test --groups external     # 再多跑 external（命名分组会取代文件里的 default）
cargo xtask test --groups external,slow
cargo xtask test --all-groups         # 这台机器看得见的全部（gaps 除外，它谁都不能点）
```

`default` 键给出默认额外要跑的分组（出厂为空）。分组名随你加，一个测试也能出现在多个分组
里，那么在任一被点名的分组下都会跑。两种「静默少跑」都会被拒绝：命令行写了文件里没有的
分组名会报错并列出可选名字，文件里写了没有任何已构建程序对应的名字也会报错（分组比测试
活得久，或者拼错了一个名字——第一台机器的 sweep 拿着整份文件问这个问题，第二台拿着自己
的 `gaps` 问，因为它的载荷曾经和第一台不是同一批程序）。被跳过的测试按分组打印出来，判决
只对**装进镜像的那些**要求，所以「N built」和「M left out」两件事都说得清楚。

`default` names the groups the sweep adds to the ungrouped tests (empty as
shipped), group names are yours to add, and a test may appear in several. Two
kinds of silent narrowing are refused: a group name on the command line that the
file does not define, and a name in the file that no built program answers to —
the first machine's sweep asks that question of the whole file, the second asks
it of its own `gaps`, because its payload was not always the same set of
programs the first machine's was. What was skipped is printed group by group,
and the verdict check only requires a `=== PASS` from the programs that were
staged — so "N built" and "M left out" are both said out loud.

### 每个程序的超时 | per-program timeouts

`timeout` 是每个程序允许运行的秒数（出厂 120），`timeouts` 给个别程序自己的预算：

```toml
[default]
timeout = 120
timeouts = ["t-fill-ramfs=600"]
```

超时后 runner 杀掉该程序、打印 `=== FAIL <name> timeout=<n>s`，然后**继续跑同一条 lane
里的下一个**。没有它时，一个卡住的程序会耗掉 lane 的 900 秒预算，排在它后面的程序一个都不
会跑。这是诚实失败而不是跳过：该程序计入 `fail`，门禁仍然红，只是红得有名有姓且不掩盖其
余结果。两台机器按各自段里的值取（机器自己的 `timeout`/`timeouts` 覆盖基线），
`cargo xtask test-host` 读同一份值，所以宿主与 guest 的预算不会各写一套。超时是「一个程序
的预算」而不是「一次扫描的预算」——整轮的 900 秒仍是 `MPOS_TEST_TIMEOUT`。

`timeout` is the seconds one program may run (120 as shipped), and `timeouts`
gives individual programs their own budget. On expiry the runner kills the
program, prints `=== FAIL <name> timeout=<n>s`, and **carries on with the next
program in the same lane** — without it, one stuck program spends the lane's
900-second budget and every program behind it never runs. The failure is named
rather than hidden: it counts in `fail` and the gate is still red. Each machine
reads its own section's values, and `cargo xtask test-host` reads the same ones,
so the host and the guest cannot drift apart.

---

## `run` 的三种运行模式 | The three `run` modes

`cargo xtask run` 用 `--mode` 一次决定整机怎么启动（缺省取 `machine.toml` 里的
`qemu.mode`，再缺省为 `mirror`）。窗口和串口怎么接线是启动方的事；客机只被告知
一件事——它有哪些控制台——而这件事用的是 Linux 自己的参数 `console=`：

| 模式 | mode | 画面 | QEMU 接线 | 客机引导参数 |
| --- | --- | --- | --- | --- |
| 终端 | `terminal` | 只有串口终端，无窗口 | `-display none` + 串口走 stdio | `console=ttyS0` |
| 图形 | `gui` | 整机进窗口：文本 tty（启动日志、虚拟终端、登录、shell）全显示在窗口里，宿主机终端静默 | `-display <后端>` + 串口接到 `build/serial.log` | `console=tty0` |
| 镜像 | `mirror` | 窗口为主，串口尽力跟一份 | `-display <后端>` + 串口走 stdio | `console=ttyS0 console=tty0` |

表中 `console=` 的串口名是**那台机器**的（`rootfs.serial_port`：x86_64 是 `ttyS0`，
第二台是 `ttyAMA0`），模式本身只决定窗口、framebuffer 与串口的组合。第二台机器的
`qemu.display` 是 `none`（没有可看的窗口后端），所以它的每条模式都把串口留在终端上——
窗口不存在时把串口丢进日志文件，就等于把整台机器藏起来。
The serial name in that table is **the machine's** (`rootfs.serial_port`: `ttyS0` on
x86_64, `ttyAMA0` on the second machine); the mode itself only picks the combination of
window, framebuffer and wire. The second machine's `qemu.display` is `none` (it has no
window backend to look at), so every mode leaves its wire on the terminal — moving it
into a log file while no window exists would hide the whole machine.

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
`machine.toml`'s `qemu.mode`, else `mirror`). How the window and the serial line
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

> **每台机都有一个控制通道 | Every booted machine carries a control socket.** 启动
> 时 QEMU 会在 `build/<arch>-9p-share/qmp.sock` 上绑一个 **QMP** 通道
> （`server=on,wait=off`，没人连它就静静放着，和旁边的 `console.sock` 一个道理）。
> 会话因此可以 `screendump` 截屏、`input-send-event` 注入指针与按键——这两条**只有
> QMP 有**，HMP 里没有。这是「把合成器的指针从窗口读回来」唯一可能的入口：光标队列
> 画进的是设备合成的那张扫描输出，不在 guest 内存里，guest 自己看不见它。
> `-monitor none` 仍然保留——那是另一个 chardev。
> The launcher binds a **QMP** channel at `build/<arch>-9p-share/qmp.sock`
> (`server=on,wait=off`, the console socket's shape: it waits for a holder that may
> never come). A session can therefore `screendump` the machine and
> `input-send-event` a pointer or a key - neither of which exists in HMP - and that
> is the only way a compositor's pointer can be read back: the cursor queue draws
> into the scanout the *device* composites, not into guest memory, so nothing
> inside the guest can see it. `-monitor none` stays; that is the other chardev.

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
| 磁盘镜像 | disk image | `build/mpos-uefi-x86_64.img`（`--arch aarch64` 是 `build/aarch64-disk.img`：GPT，p1 = ESP，p2 = ext4 根，同一形状） | `--arch aarch64` gets `build/aarch64-disk.img`, the same GPT shape (p1 = ESP, p2 = ext4 root) |
| 根文件系统 stamp | rootfs stamp | `build/.rootfs.stamp` / `build/.rootfs-aarch64.stamp`——每个镜像一份，决定要不要重装包还是只刷新引导文件 | one per image; decides package install against boot-file refresh |
| 烘焙的运行模式 | baked run mode | `build/console.mode` / `build/aarch64-console.mode`——缓存镜像里 `mpos.cmdline` 说的是哪个模式，好让 `run` 不必提权去读 | a cache of the mode `mpos.cmdline` already carries, so `run` need not take root to look |
| initramfs | initramfs | `build/initramfs.tar`（`--arch aarch64` 是 `build/aarch64-initramfs.tar`） | `--arch aarch64` gets `build/aarch64-initramfs.tar` |
| 固件变量副本 | firmware vars | `build/OVMF_VARS.4m.fd` / `build/AAVMF_VARS.fd` | per machine |
| 测试串口日志 | test serial log | 扫描的合并日志：`build/test-serial.log`（x86_64）/ `build/aarch64-test-serial.log`（aarch64），都是**各 lane 按序拼接**的整条线（跑完才写入，因此不能边跑边 tail）；每台的原始日志在 `build/test-lane-<arch>-<n>/serial.log`；任何 headless `run` 是 `build/aarch64-serial.log` | each sweep's combined wire is `build/test-serial.log` (x86_64) / `build/aarch64-test-serial.log` (aarch64), **lane by lane** (written once the run ends, so it cannot be tailed live); each lane's own log is `build/test-lane-<arch>-<n>/serial.log`; a headless `run` keeps `build/aarch64-serial.log` |
| 扫描启动盘 | sweep medium | `build/aarch64-sweep.img` + `build/aarch64-sweep.stamp`——`test --arch aarch64` 自己的盘（载荷铺在根分区上），与 `run` 装好的镜像分开，互相不覆盖；所有 lane **共用这一份**，各自以 `snapshot=on` 启动，写入落在该 lane 的私有 overlay 里 | `test --arch aarch64`'s own disk (its payload staged on the root partition), separate from the image `run` installs; **one file shared by every lane**, each booting it through `snapshot=on` so a lane's writes land in that lane's private overlay |
| aarch64 内核与 loader | aarch64 kernel + loader | `target/aarch64-unknown-none/<profile>/mpos`、`target/aarch64-unknown-uefi/<profile>/mpos-boot.efi`（`build`/`all`/`run`/`test --arch aarch64`） | built by `build`/`all`/`run`/`test --arch aarch64` |
