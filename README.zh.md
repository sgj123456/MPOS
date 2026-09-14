# MPOS

<div align="center">

![Rust 2024](https://img.shields.io/badge/Rust-2024-E8453C?style=for-the-badge&logo=rust&logoColor=white)
![Target x86_64](https://img.shields.io/badge/Target-x86__64-000000?style=for-the-badge&logo=amd&logoColor=white)
![Limine v9.6.7](https://img.shields.io/badge/Limine-v9.6.7-4A90D9?style=for-the-badge&logo=gnu&logoColor=white)
![License MIT](https://img.shields.io/badge/License-MIT-blue?style=for-the-badge)

**里程碑验证操作系统内核**

一个用 Rust 编写的最小化、经过里程碑式验证的操作系统内核，通过 Limine 协议引导。每个子系统在构建下一个之前都会被证明是正确的——从帧分配器到完整的交互式 Shell。

[快速开始](#-快速开始) · [架构](#-架构) · [里程碑](#-里程碑) · [构建](#-构建) · [测试](#-测试) · [English](README.md)

</div>

---

## 🧩 概述

**MPOS** 是一个以**里程碑验证**为核心方法论构建的教学和研究型操作系统内核。与其渐进式地构建内核，MPOS 在构建每一层之前都会通过断言和主机端单元测试证明其正确性。

内核以 **Rust 库**（`src/lib.rs`）的形式组织，可在主机上进行单元测试（`cargo test --lib --target x86_64-unknown-linux-gnu`），而薄二进制入口（`src/main.rs`）则在 QEMU 上通过 M0 至 M14 的里程碑验证流程。

### 核心特性

| 特性 | 说明 |
|------|------|
| 🧪 **里程碑验证** | 每个子系统（分配器、调度器、VFS、Shell）在构建下一个之前都会被证明正确。串口日志携带标记（`M0_OK` 到 `M13_OK`），由测试框架断言验证。 |
| 🔬 **主机可测试拆分** | 所有不涉及机器状态的代码都在库 crate 中，通过 `cargo test` 在主机上运行。机器相关代码位于 `#[cfg(target_os = "none")]` 之后。 |
| 📐 **直写式 ext2** | 完整的可读写 ext2 实现，采用直写语义——无日志、无脏页追踪、崩溃即可见。 |
| 🏗️ **优先级调度器** | 四个优先级带（LOW、NORMAL、HIGH、CRITICAL）之间严格优先级，带内轮转调度，基于截止时间的休眠队列。 |
| 🐚 **交互式 Shell** | 用户空间 Shell，支持管道、输出重定向、环境变量、后台作业，以及可脚本化的 `init`。 |

---

## 🏛️ 架构

```
mpos/
├── src/
│   ├── main.rs              # 二进制入口，里程碑验证 M0–M14
│   ├── lib.rs               # 库根（主机可测试）
│   ├── arch/                # 架构相关代码
│   │   └── x86_64/          # GDT、IDT、分页、串口、定时器、系统调用门
│   ├── mm/                  # 内存管理
│   │   ├── pframe.rs        # 伙伴帧分配器
│   │   ├── heap.rs          # Slab 内核堆
│   │   ├── elf.rs           # ELF64 加载器
│   │   └── vm.rs            # 映射计划、KernelSections
│   ├── sched/               # 调度器
│   │   ├── policy.rs        # 优先级带、轮转调度、休眠队列
│   │   ├── runtime.rs       # 任务表、上下文切换、信号
│   │   └── signal.rs        # 信号编号（SIGINT、SIGPIPE、SIGTERM、SIGKILL）
│   ├── vfs/                 # 虚拟文件系统
│   │   ├── ramfs.rs         # 内存文件系统
│   │   ├── devfs.rs         # 设备文件系统（/dev/tty、/dev/null 等）
│   │   ├── ext2.rs          # 可读写 ext2 实现
│   │   ├── pipe.rs          # 管道实现
│   │   ├── tty.rs           # 行规程：规范模式、原始模式、方向键等转义键
│   │   └── termios.rs       # struct termios 与控制台默认值
│   ├── syscall/             # 系统调用门（17 个调用）
│   ├── blk/                 # 块设备层（virtio-blk）+ 分区表扫描
│   ├── boot/                # 引导协议转换（Limine）
│   └── （没有 Shell 模块：/bin/sh 是 Alpine 的 ash）
├── user/
│   ├── lib/sys.rs           # 精简 libc（系统调用封装）
│   └── bin/                 # 用户程序
│       ├── init.rs          # PID 1：重启 Shell、回收孤儿进程
│       ├── hello.rs         # Ring 3 概念验证（M12）
│       └── t_*.rs           # ABI 一致性测试，每个文件一类系统调用
├── scripts/
│   ├── mkdisk.sh            # 构建内核、用户程序与可引导磁盘镜像
│   ├── mkfat.py             # 手工写出 EFI 系统分区的 FAT32
│   ├── run.sh               # 在 QEMU 中启动
│   └── test.sh              # 完整 CI 流程
├── arch/x86_64/linker.ld    # 位置无关链接脚本
└── limine.conf              # 引导加载器配置
```

### 设计原则

- **库/二进制拆分**：`src/lib.rs` 编译目标为 `x86_64-unknown-linux-gnu`（主机测试）；机器代码通过 `target_os = "none"` 门控。
- **共享系统调用表**：用户程序通过 `#[path]` 包含 `src/syscall/nr.rs`——内核和用户空间不会在编号上产生分歧。
- **零分配 Shell 解析器**：Shell 语法解析器不使用堆分配，在主机上运行，并经过完整的单元测试。
- **伙伴分配器与侵入式空闲链表**：元数据从被管理内存中划出；通过空闲块中的双向链表实现 O(1) 合并。

---

## 🎯 里程碑

每个里程碑在构建下一个子系统之前证明当前子系统正确。

| 里程碑 | 子系统 | 验证内容 |
|:------:|--------|---------|
| **M0** | 分配器 | 全局堆分配器端到端工作（Vec、String） |
| **M1** | 中断 | 时钟以正确频率滴答；断点异常可捕获 |
| **M2** | 帧与堆 | 伙伴分配器分出不重叠的块；Slab 堆满足所有布局；两者对内存使用量一致 |
| **M3** | 分页 | 内核运行在自己的页表上；整个镜像 W^X 验证；进程获得独立地址空间并共享内核半区 |
| **M4** | 调度器 | 轮转调度、抢占、休眠、空闲、基于优先级的核窃取全部工作 |
| **M6** | 系统调用 | `syscall` 门正确分发；fd 检查工作；通过 dispatch 的 exit 结束任务 |
| **M7** | VFS | ramfs 根、挂载子文件系统、通过 fd 表的多任务文件 I/O |
| **M8** | Initramfs 与 devfs | tar 解包、`/dev/tty`、`/dev/console`、`/dev/null`、`/dev/zero` |
| **M9** | 进程 | ELF 加载到新地址空间；进入 Ring 3；退出码往返验证 |
| **M10** | 块设备与 ext2 | virtio-blk 检测；ext2 挂载、读取主机种子文件、通过 /mnt 写入、冷挂载验证 |
| **M11** | 管道、TTY、信号 | 管道字节传输和 EOF；TTY 行编辑、回显、Ctrl-D；SIGTERM、SIGKILL、kill(0) 探测 |
| **M12** | Shell 与核心工具 | Shell、echo、cat、ls、true、false、sleep——全部运行在 Ring 3 |
| **M13** | Init 移交 | `/sbin/init` 成为 PID 1；Shell 通过 UART 中断变为交互式 |
| **M14** | 退出状态 | Shell `$?` 携带上一条命令的退出码 |

---

## 🚀 快速开始

### 前置条件

- **Rust nightly**（包含 `rust-src`、`rustfmt`、`clippy`）
- **QEMU**（x86_64 或 aarch64）
- **OVMF** —— QEMU 用它替代 SeaBIOS 的 UEFI 固件
  （`/usr/share/OVMF/x64/OVMF_CODE.4m.fd`）
- **python3** —— `scripts/mkfat.py` 无需 loop 设备即可写出 FAT32 卷
- **parted** —— 划分镜像的 GPT 分区表
- **mke2fs** / **e2fsprogs**（用于创建 ext2 磁盘镜像）

```bash
# 安装 Rust nightly 及所需组件
rustup toolchain install nightly
rustup component add rust-src rustfmt clippy --toolchain nightly

# 添加裸机目标
rustup target add x86_64-unknown-none aarch64-unknown-none --toolchain nightly
```

### 快速开始

```bash
# 1. 获取 Limine 引导加载器（仅需一次）
./scripts/fetch-limine.sh

# 2. 构建内核、用户程序和可引导 ISO
./scripts/mkdisk.sh

# 3. 在 QEMU 中启动
./scripts/run.sh
```

内核将启动，运行 M0–M13 里程碑验证，然后进入交互式 Shell。

---

## 🔨 构建

### 仅构建内核

```bash
cargo build --target x86_64-unknown-none
```

### 完整镜像（单张磁盘）

```bash
./scripts/mkdisk.sh
```

此脚本编译内核、用 `rustc` 编译每个用户程序（独立、静态、`ET_EXEC`）、把 Alpine minirootfs 打成 initramfs tar，最终组装成一张带分区表的磁盘镜像 `build/linux-x86_64.img`，整个系统都在里面：

| 分区 | 类型 | 内容 |
| --- | --- | --- |
| 1，`ESP` | FAT32，128 MiB | `EFI/BOOT/BOOTX64.EFI`（Limine）、`limine.conf`、`boot/mpos.elf`、`boot/initramfs.tar` |
| 2，`root` | ext2，8 MiB | 内核挂载为 `/mnt` 的种子文件 |

QEMU 直接在 OVMF 下启动它：不需要 ISO、不需要光驱、宿主上也不需要 loop 设备 —— 这正是 FAT32 卷由 `mkfat.py` 逐字节写出、而不是 `mkfs.vfat` 加挂载的原因。

> **注意：** `third_party/limine/` 存放 Limine 引导加载器二进制文件，已被 gitignore。通过 `scripts/fetch-limine.sh` 自动填充（首次构建前运行一次）。若目录不存在，脚本会自动创建。

### 交叉架构

```bash
ARCH=aarch64 ./scripts/run.sh
```

aarch64 支持正在开发中——目前内核 ELF 可以构建，但启动流程尚未完成。

---

## 🧪 测试

### 主机端单元测试

库 crate 编译目标为主机，支持快速迭代：

```bash
cargo test --lib --target x86_64-unknown-linux-gnu
```

这会测试调度器策略、Shell 解析器、VFS 路径解析等架构无关代码。

### 完整 CI 流程

```bash
./scripts/test.sh
```

运行完整流程：

1. `cargo fmt --check`
2. `cargo clippy`（内核和主机目标）
3. `cargo test --lib`（主机）
4. 通过 `mkdisk.sh` 构建
5. 在 QEMU 中启动并捕获串口日志
6. 断言里程碑标记（`M0_OK` 到 `M13_OK`、`APK_OK`）
7. 检查 `KERNEL PANIC` 或 `INIT_EXITED`

---

## 📡 系统调用

mpos 通过唯一的 `syscall` 门暴露 17 个系统调用。mpos 号在 `rax` 高 32 位带有 `mpos` tag，避免与 musl 二进制直接裸发的 Linux x86_64 号冲突：

| # | 调用 | 描述 |
|:-:|------|------|
| 0 | `EXIT` | 终止进程 |
| 1 | `READ` | 从文件描述符读取 |
| 2 | `WRITE` | 写入文件描述符 |
| 3 | `YIELD` | 让出 CPU 给调度器 |
| 4 | `GETPID` | 获取当前进程 ID |
| 5 | `SLEEP` | 休眠 N 毫秒 |
| 6 | `PIPE` | 创建管道 |
| 7 | `KILL` | 向进程发送信号 |
| 8 | `OPEN` | 打开文件 |
| 9 | `CLOSE` | 关闭文件描述符 |
| 10 | `SPAWN` | 加载并执行程序 |
| 11 | `WAIT` | 等待子进程 |
| 12 | `LISTDIR` | 列出目录条目 |
| 13 | `DUP2` | 复制文件描述符 |
| 14 | `CREATE` | 创建文件或目录 |
| 15 | `UNLINK` | 删除文件系统条目 |
| 16 | `CHDIR` | 切换工作目录 |

---

## 🧰 用户程序

| 程序 | 描述 |
|------|------|
| `init` | PID 1 — 指数退避重启 `/bin/sh`，回收孤儿进程 |
| `sh` | 支持管道、重定向、变量、后台作业、脚本的 Shell |
| `hello` | Ring 3 概念验证（打印、让出、休眠、以退出码 42 结束） |
| `ls` | 目录列表 |
| `cat` | 文件连接 |
| `echo` | 参数回显 |
| `sleep` | 休眠 N 秒 |
| `true` / `false` | 退出 0 / 退出 1 |

所有用户程序均为 `#![no_std]`，使用 `rustc` 独立编译，共享 `user/lib/sys.rs` 中的精简 libc。

---

## 📜 许可证

本项目基于 [MIT 许可证](LICENSE) 授权。

---

<div align="center">
<sub>用 🦀 Rust 构建——逐个里程碑地证明正确。</sub>
</div>
