# 测试与引导工程重构：六项任务的具体改动方案

本文对应六项任务，逐项给出：**现状与证据 → 具体改动点（文件/模块）→ 与其他项的依赖 → 多架构一致性的验证方式**。

其中两项已经落地并通过门禁，其余四项给出可直接施工的分期方案。所有路径以工作区根 `/home/mp/mpos` 为基准。

**状态总览**

| # | 任务 | 状态 |
|---|---|---|
| 3 | 修复 `FAIL t-unix` | **已修复并验证**（`=== SUMMARY pass=155 fail=0`） |
| 4 | `main` 架构无关 | **切片 1 已落地**（hook 表实现合一，−263 行）；切片 2–4 已给出方案 |
| 1 | no_std 用户测试 → std + rustix | 已给出范围清单、测试集合与迁移机制 |
| 2 | xtask `run`/`test` 共用逻辑 | **已落地**（2026-10-08；形状比本节设想更彻底，见 §2.5 与 `NOTES.md` §15.7） |
| 5 | boot 由可执行文件改为 crate，内核成为 UEFI 存根 | 已给出改动点与依赖顺序 |
| 6 | boot 架构无关，架构码下沉 arch crate | 已给出改动点与依赖顺序 |

---

## 1. 用户态测试：no_std → std + rustix

### 1.1 现状（实测）

仓库里现在有**两套**用户态测试，且只有一套是跨架构的：

| 测试集合 | 位置 | 语言/运行时 | 构建目标 | 规模 | 谁在跑 |
|---|---|---|---|---|---|
| ABI 一致性套件 | `user/abitest/src/bin/{syscall,integration}/` | `#![no_std]`，直呼 `lib/lin.rs` 的裸 syscall 桩 | `x86_64-unknown-none` | **138 个程序** | 仅 x86_64 扫描 |
| std 套件 | `user/tests/src/bin/` | `std` + `rustix`（少量 `libc`） | `x86_64-unknown-linux-musl` **和** `aarch64-unknown-linux-musl` | **27 个程序** | 两台机器都跑 |

证据：
- `xtask/src/selftest.rs::build_user_tests()` 显式构建两个包，abitest 那一次用 `RUSTFLAGS=-C relocation-model=static` 覆盖根配置（因为它继承了内核的 linker script）。
- `xtask/src/aarch64.rs::build_user_tests_for_aarch64()` 只构建 `mpos-user-tests`（`--target aarch64-unknown-linux-musl`），**完全不含 abitest**。
- x86_64 扫描：165 个程序构建，`tests.toml` 排除 10 个（`external` 5 + `slow` 4 + `diagnostic` 1），实际跑 **155**。
- aarch64 扫描：27 个构建，`external` 排除 2 个（`t-stdresolve`、`t-udp`），实际跑 **25**。

**结论：跨架构一致性的缺口是"138 个程序只有一个架构能跑"，而不是"两个架构跑了不同的行为"。** aarch64 之所以能跑 std 套件，是因为它已经能执行 musl 静态程序（`aarch64-unknown-linux-musl` + `+crt-static` + `linker=rust-lld`，由 `xtask/src/selftest.rs::the_aarch64_user_programs_are_declared_static_and_linked_by_rust_lld` 钉住）。所以迁移的收益是**让 aarch64 也跑那 138 个 ABI 测试**，代价是重写它们。

### 1.2 改动点

**范围**：`user/abitest` 的 138 个程序全部迁到 `user/tests`，改用 `std` + `rustix`；`user/abitest` 整个包退役（连同 `lib/lin.rs`、`build.rs` 的裸链接参数、`x86_64-unknown-none` 构建路径）。

**逐文件的机械映射**（每个 `t-*.rs` 都适用）：

| 现在（no_std） | 迁后（std + rustix） |
|---|---|
| `#![no_std] #![no_main]` + `#[path="../../../lib/lin.rs"] mod lin;` | 删掉；改用 `use rustix::…` 与 `std::…` |
| `#[unsafe(no_mangle)] extern "C" fn mpos_user_main(…)` | `fn main()` |
| `#[panic_handler] fn panic` | 删掉（std 自带） |
| `lin::raw_sys(nr, a, b, c, …)` | 对应的 `rustix::` 命名 API；`rustix` 未绑定的调用用 `libc::syscall(libc::SYS_*, …)` |
| `lin::check_true/check_eq` | 共享断言助手（见下） |
| `lin::done(failed)` | `std::process::exit(if failed { 1 } else { 0 })` |
| `lin::nr::*`（与内核 `src/syscall/linux/nr.rs` 同源） | 不再需要——`rustix`/`libc` 自带**各架构**的编号表，这正是本任务"适配不同架构内核"的关键：aarch64 的 syscall 号与 x86_64 不同，而 `rustix` 按目标三元组选表，仓库不必再自己维护第二份 |

**必须新建的一小块**：138 个程序共用的判定协议（现在各自从 `lin.rs` 拿）。放 `user/tests/src/lib.rs`（该 crate 已是 `[lib]`+多 bin 形状）或新 leaf crate `crates/user-testkit`，提供 `check_true/check_eq`、`ok!/fail!` 打印与退出码。**这一步不能省**：否则 138 份判定逻辑各写一遍，正是本仓库反复拒绝的"同一决定写两处"。

**学习样本**：已经迁过的两个程序就是模板——`user/tests/src/bin/t-unix-rustix.rs`（对应 `abitest/src/bin/syscall/t-unix.rs`）和 `t-process-vm-libc.rs`（对应 `t-process-vm.rs`）。二者演示了"rustix 优先、rustix 无绑定时用 `libc::SYS_*`"的取舍，并且 `user/tests/Cargo.toml` 的头部注释已经把这条规则写成了仓规。

**内核侧几乎不用改**：两台机器的内核本来就按 Linux ABI 回答，且 aarch64 已经会跑 musl 程序。唯一要核对的是 `tests.toml` 的 `aarch64` 分组（现为空 `[]`）——迁完之后它就是"这台机器确实还做不到的 ABI"，必须逐条填**真实**的能力缺口，而不是把跑不通的测试塞进去凑绿（这是本仓库明令禁止的"静默跑少一点"）。

### 1.3 依赖

- 依赖 #2：两套扫描的构建/装载逻辑若不先合一，迁移期间要在**两条几乎相同的路径**上同时维护新旧两套程序。
- 被 #5/#6 依赖：见 §5，#5 之后用户程序仍在 initramfs/ESP 里，不受影响。

### 1.4 多架构一致性验证

1. 构建：`cargo build -p mpos-user-tests --target x86_64-unknown-linux-musl` 与 `--target aarch64-unknown-linux-musl` 都必须成功，且**产物名集合相同**。
2. 计数：`cargo xtask test` 的 `(N built)` 与 `cargo xtask test --arch aarch64` 的 staged 数，差值必须**恰好等于** `tests.toml` 中 `aarch64` 分组的大小。
3. 结论：两边都必须出现 `=== SUMMARY pass=N fail=0`。
4. 逐程序：同一 `t-X` 在两台机器的串口日志里必须给出**同一串 `ok:` 行**（日志可直接 diff）。
5. 收尾：`xtask/src/selftest.rs` 去掉 abitest 那一支后，`LEAF_CRATES` 与 `every_leaf_crate_is_tested` 不受影响（abitest 不是 leaf crate）。

---

## 2. xtask：`run`/`test` 逻辑共用

### 2.1 现状：重复的是"选哪些、怎么报"，不重复的是"怎么启动"

| 环节 | x86_64（`selftest.rs`） | aarch64（`aarch64.rs`） | 是否可共用 |
|---|---|---|---|
| 读 `tests.toml`、校验 `--groups`、`unknown()`/`unmatched()` 报错 | `run_tests()` 66–95 行 | `run_sweep()` 680–691 行 | **可** |
| `selection()` + `runs()` 选出 staged/skipped，并按分组打印"被排除的是谁、为什么" | `run_tests()` 96–124 行 | `run_sweep()` 692–720 行 | **可**（只差载荷类型） |
| 构建用户程序 | `build_user_tests()`：两个包、两个 target | `build_user_tests_for_aarch64()`：一个包、一个 target，读回字节 | 半共用（见下） |
| 装载 | tar 打 initramfs + QEMU `fat:` 目录 | 直接写 ESP 目录 + ext4 磁盘镜像 | **不可**（机制不同） |
| 启动 | `qemu-system-x86_64` + OVMF + `isa-debug-exit` | `qemu-system-aarch64` + AAVMF + `=== END` 轮询 | **不可** |
| 取日志 | 读 `build/test-serial.log` 文件 | 内存里的 `String` | 可统一为 `String` |
| 判定解析（`=== RUN/PASS/FAIL/SUMMARY/END`）与报错文案 | `report()` | `report_sweep()` | **可** |
| 机器专属的"缺口分组"校验 | `config.unmatched()` | `gaps_without_programs()` | 同一条规则，两种写法 |

即：**约 90 行的配置/选择/打印逻辑是逐字重复的**（上面表里标"可"的三行），而启动机制必须保持分叉——`xtask/src/main.rs` 的 `Commands::Test` 分支注释已经把这个判断写清楚了。

### 2.2 改动点：抽出 `xtask/src/sweep.rs`，用"传输"参数化

```
xtask/src/sweep.rs              新文件
  struct Program { name: String, bytes: Vec<u8> }
  struct Plan { staged: Vec<Program>, skipped: Vec<Program>, config: TestConfig, machine: Machine }
  fn plan(machine, built, groups, all) -> Result<Plan>      // 现在两处重复的那 90 行
  fn report(machine, plan, log: &str) -> Result<()>         // 现在两处重复的判定解析
  trait Boot {
      fn stage(&self, build_dir, plan: &Plan) -> Result<Payload>;
      fn run(&self, build_dir, profile, payload) -> Result<String>;   // 返回串口日志
  }
  fn sweep(boot: impl Boot, build_dir, profile, groups, all) -> Result<()>   // 唯一的编排
```

- `X86InitramfsBoot`（搬 `selftest.rs` 的 `build_test_initramfs` + `boot_tests`）
- `Aarch64EspBoot`（搬 `aarch64.rs` 的 `build_esp` + `boot`）

`selftest::run_tests` 与 `aarch64::run_sweep` 缩成各自十几行的适配器：**只描述"这台机器怎么把程序送上机器、怎么把日志取回来"**，其余一律走 `sweep()`。

**共用的边界**（三条，必须写进 `sweep.rs` 的模块文档）：
1. `tests.toml` 的语义（分组、机器专属缺口、`--groups`/`--all-groups` 的拒绝规则）——与机器无关；
2. 判定协议（`=== RUN/PASS/FAIL/SUMMARY/END` 与"staged 必须全部 PASS"的断言）——与机器无关；
3. 报告的措辞与"报绿必须真的跑满"这条不变量——与机器无关。

**必须留在机器侧的**（不得上提）：
1. 构建哪个 target（`x86_64-unknown-none`+musl 两个包 vs `aarch64-unknown-linux-musl` 一个包）；
2. 载荷形态（tar+虚拟 FAT vs 目录 ESP+ext4）；
3. 固件与终止条件（OVMF/`isa-debug-exit`/退出码 33 vs AAVMF/`=== END`/`deadline`）。

`#1` 落地后，第 1 条会自然收敛为"一个包、两个 target"，这正是让 §2 的抽取变简单的前置。

### 2.3 依赖

- 先做 #1 再做 #2：届时"构建用户程序"这一支在两侧形状相同，只需参数化 target。
- 与 #4 无关；与 #5 相交于 ESP 布局（#5 之后 `mpos.elf` 不再单独存在，`Payload` 的构造要跟着改）。

### 2.4 验证

1. `cargo test -p mpos-xtask --target x86_64-unknown-linux-gnu`（现有 65 条 + 为 `plan()`/`report()` 补的纯函数测试；这两块本来就是纯逻辑，天然可宿主测试）。
2. `cargo xtask test` 与 `cargo xtask test --arch aarch64` 的**输出行**（`user tests: N staged, M left out`、分组清单、`FAIL …` 文案）必须与重构前逐字一致——这是"共用没有改变语义"的唯一硬证据。
3. `cargo xtask test --groups slow` 仍被 aarch64 分支按名拒绝；`--groups nosuch` 仍被两机拒绝。

### 2.5 实际落地（2026-10-08）：共用走到了比本节设想更彻底的一步

本节原本设想的是"抽出一个 `trait Boot`，两台机器各给一份载荷 + 启动实现"。落地时发现，
`run`/`test` 之外还有**第三**条重复：第二台机器为自己的三个入口（`boot-aarch64`、
`alpine-aarch64`、`run --arch aarch64`）各留了一套命令行走廊、终止词表、marker 词表、ESP
布局、铺盘逻辑与 staleness 规则——而其中真正的机器差异只有"包是哪一架构的""引导文件名是哪
个""overlay 是哪棵树"。所以最终做的是更大的删除，而不是参数化两种启动器：

| 原设想 | 实际做法 |
|---|---|
| 抽出 `sweep.rs` 的 `plan()`/`report()` 参数化 | 已按原样落地（`xtask/src/sweep.rs`，两台机器同一套选择与判定） |
| `X86InitramfsBoot` / `Aarch64EspBoot` 两个 `Boot` 实现 | 不需要：`cargo xtask run` 对两台机器就是**同一个函数**（`qemu::run_qemu`），`disk`/`rootfs`/`all` 同样按 `--arch` 走同一条安装路径 |
| 保留两个 aarch64 判定命令 | **删除**：`cargo xtask boot-aarch64`、`alpine-aarch64` 退役，第二台机器的门禁与第一台机器同形（`build`/`disk`/`rootfs`/`all`/`run`/`test` + `--arch`），marker 链不再由命令断言 |
| 第二台机器的安装继续走 staging 树 + `fakeroot` + `mkfs -d` | 改为与第一台机器同一路：`qemu-nbd` 挂载 + `apk add --arch aarch64 --no-scripts`（以 root 运行，所以属主天然正确，也无需 `fakeroot`） |

改动清单、逐条替换关系与实测结果见 `docs/NOTES.md` §15.7；本节只在方案层面记录"为什么不按
原计划做"。原计划里"#1 之后形状才收敛"的判断仍然成立——用户程序迁到 std 套件后才有了今天这条
单一路径。

---

## 3. 修复 `FAIL t-unix`（已完成）

### 3.1 根因

`bind` 在一个**普通文件**之下时，内核回答 `EACCES(-13)`，而 Linux 回答 `ENOTDIR(-20)`：

```
FAIL: a bind under a plain file is ENOTDIR, whatever occupies its name (got -13, want -20)
```

链路：
1. `src/net/unix.rs::bind_path`（1146 行）先调 `vfs::resolve_parent_at(path, cwd)` 拿父目录，再依次做 `lookup` → `may_create_in` → `create`。
2. `Vfs::resolve_parent_from`（`src/vfs/mount.rs`）**只解析父路径、不检查它是不是目录**，于是把 `/tmp/t-unix-notdir` 这个普通文件当成了父目录返回。
3. `parent.lookup(name)` 在普通文件上返回 `NotDir`，但 `bind_path` 用 `if let Ok(_)` 忽略了错误。
4. 随后 `may_create_in(&parent)` 做 `may_access(dir, W_OK|X_OK)`；该文件被 `chmod 0666`，**没有执行位** → `EACCES`。
5. `parent.create(...)`（本会返回 `NotDir`）永远没被走到。

对照 `Vfs::resolve_no_follow`（同文件 1515 行）——它有 `if parent.metadata().kind != NodeKind::Dir { return Err(VfsError::NotDir) }`。**同一份"父必须是目录"的规则，一处写了、三处没写。**

### 3.2 改动

**`src/vfs/mod.rs`**：新增一处判定，供所有"造名/删名"的入口共用：

```rust
pub(crate) fn require_directory(dir: &Arc<dyn Inode>) -> VfsResult<()> {
    if dir.metadata().kind != NodeKind::Dir { return Err(VfsError::NotDir); }
    Ok(())
}
```
并让 `resolve_parent_opt` 的 `None` 分支（原本是第三份"切路径 + 解析父"的复制）也走它。

**`src/vfs/mount.rs`**：`Vfs::resolve_parent` 与 `Vfs::resolve_parent_from` 在解析出父之后各加一行 `crate::vfs::require_directory(&parent)?;`。

这样 `unlink`/`rmdir`/`bind`/`mkdirat`/`symlink`/`link` 会**一起**在权限之前给出 `ENOTDIR`——与 Linux 的 `link_path_walk`/`walk_component`（`d_can_lookup`）顺序一致：先判"这里能不能放名字"，再判"你有没有权限"。

**`src/vfs/mount.rs` 测试模块**：新增 `a_parent_that_is_not_a_directory_is_not_dir`，把两个入口（绝对路径的 `resolve_parent`、带 start 的 `resolve_parent_from`）都钉住。它同时是这条修复的反事实说明：没有这行检查时，第二次断言会回到 `EACCES`。

### 3.3 验证（已跑）

| 检查 | 结果 |
|---|---|
| `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu vfs::` | 356 passed / 0 failed（原 355） |
| 全部 leaf crate 宿主单测 | 全绿（`mpos` 511） |
| `cargo xtask test`（x86_64，155 个用户程序） | `=== SUMMARY pass=155 fail=0`；`=== PASS t-unix`；exit 0 |
| `cargo xtask test --arch aarch64`（25 个） | `=== SUMMARY pass=25 fail=0` |
| `cargo xtask boot-aarch64` | 26 个 marker 全出（`M*_OK` + `INIT_EXITED`） |
| `cargo xtask depgraph` | ratchet 内，互指对 8/8 |

**一个操作教训**（不是代码问题，但会让门禁说谎）：本工作区的 `build/` 是**多会话共享**的，`build/test-serial.log` 与 `build/test-esp` 是固定名字。两个会话同时跑扫描时，QEMU 各自的写入偏移互相撕裂日志文件，实测出现"前 158 KB 全是 `\x00`、只剩尾部一段文本"的日志——判定解析器读不到任何 `=== PASS`，于是把 155 个测试**全部**报成失败。这不是内核问题（同一份日志里 `=== SUMMARY pass=155 fail=0` 完好）。路径名是 `xtask/README.md` 明确记载的稳定产物，**不应为并发而改名**；正确做法是跑门禁时确认没有第二个会话在跑，或按上面的证据（SUMMARY 行 + 逐条 `ok:`）判断真伪。

---

## 4. `main` 架构无关

### 4.1 现状：真正架构相关的只有三样，`main` 的分叉比看起来小

`src/main.rs` 现在是 34 行的 `#[path]` 派发器：

```rust
#[cfg(target_arch = "x86_64")]  #[path = "board/x86_64.rs"]  mod body;
#[cfg(target_arch = "aarch64")] #[path = "board/aarch64.rs"] mod body;
```

两个 body 各 1740 / 1231 行。逐一分类后，"架构相关"其实只有：

1. **入口 ABI**：`_start` 的记录在 `RDI`（x86_64）或 `x0`（aarch64）。aarch64 **已经**把这层包成了 HAL 形状的 `mpos_arch::arch::boot(handoff, milestone, init, devices)`——`_start` 只负责把寄存器里的指针交给它。x86_64 还没有对应物。
2. **机器专属的里程碑检查**：x86_64 有 `check_mm/check_arch/check_sched/check_vfs/check_ipc`；aarch64 有 `check_virtio/rng/blk/fs/net/input/dev/disk_root/serial_tx/console_rx`。这些**内容**不同，但形状相同（都是 `fn(&BootInfo)`，失败即 `exit`）。
3. **panic 打印路径**：x86_64 还要 `stack_scan()` 扫启动栈，aarch64 走 `MachineWriter`。

而 `impl hal::kernel::Kernel for KernelServices` —— 乍看最像机器相关的一块 —— **实际上不是**。实测两台机器的 25 个方法里 24 个逐字相同，唯一差异 `device_not_available` 由 `mpos_fpu_trap` 这个**能力**（不是架构）门控，trait 本身已经 `#[cfg]` 掉了它。

### 4.2 改动点

**切片 1（已落地）**：抽出 `src/kernel_services.rs`（内核 lib，`#[cfg(target_os = "none")]`），给出**唯一**一份 `impl Kernel for Services` + `Services` 静态值 + `pub fn register()`。两个 body 各自删掉约 136 行的 struct+impl，改为在入口处调一行 `mpos::kernel_services::register();`。删除随之空掉的 `use`（x86_64 的 `mpos::console`/`mpos::net`，aarch64 的 `boot`/`console`/`mm`/`net`）。

实测：`src/board/x86_64.rs` 1876 → 1740 行，`src/board/aarch64.rs` 1358 → 1231 行，**合计少 263 行**，两台机器编译无新告警。

**切片 2：把 `_start` 也交给 arch crate。** 给 x86_64 补一个与 aarch64 对称的 `mpos_arch::arch::x86_64::boot(handoff, callbacks) -> !`（建页表、装 IDT、再由它调用内核侧的 `kernel_main`）。`src/main.rs` 从"派发器"变成**唯一的、架构无关的 `kernel_main`**，`src/main/{x86_64,aarch64}.rs` 消失，`#[path]` 派发与 `#[cfg(target_arch)]` 一起消失；`src/main.rs` 里的 `compile_error!` 兜底也就不需要了（arch crate 缺哪个 target 的 `boot` 就是编译错误，更早也更准）。

**切片 3：里程碑检查按形状收拢。** 保留每台机器的**内容**，但把"运行一串 `fn(&BootInfo)`、任一失败即 `hal::exit_qemu(TEST_EXIT_PANIC)`"变成一处；检查列表按机器来自 `mpos-arch` 的一个 `const CHECKS: &[(&str, fn(&BootInfo))]`。这一步与 §3 已经做的"设备探针搬回 main 并改走 `hal::mmio_window`"是同一方向的延续。

**切片 4：panic 合一。** 一个 `#[panic_handler]`，走 `hal::kernel::force_serial` + `print_line` + `hal::exit_qemu`；x86_64 的栈扫描做成 `hal` 的一个可选能力（无此能力即跳过），而不是 `#[cfg]` 分支。

### 4.3 依赖

- 依赖已完成的 S4（`arch` 已是独立 crate `mpos-arch`，内核通过 `pub use mpos_arch::{arch, hal}` 保留名字）。
- 与 #6 强耦合：#6 把引导期页表/串口的架构码也沉进 `mpos-arch`；两者合起来才是"机器层只有 `mpos-arch` 一个家"。
- 与 #5 有顺序耦合：见 §5.4。

### 4.4 验证

- 切片 1：两台机器 `cargo build (-p mpos / --target aarch64-unknown-none --release -p mpos)`；`cargo xtask boot-aarch64`（这条同时证明 hook 表在启动早期已注册——aarch64 的 `_start` 在装向量表之前调 `register()`）。
- 切片 2–4：两台机器都必须**能启动**（`boot-aarch64` + `cargo xtask test` 的 UEFI 启动），且两个扫描的 `=== SUMMARY` 都不变；`cargo xtask depgraph` 必须无新增边（`main` 消失后 `ARCH_EXEMPT` 可再收一格）。
- 一致性的专用探针：`cargo xtask test`（x86_64）与 `cargo xtask test --arch aarch64` 的**用户程序结果**必须一致（迁移前是 155 vs 25，迁移后是同一列表减去 `tests.toml` 的 `aarch64` 缺口）。

### 4.5 实际落地（2026-10-06）

切片 2 与切片 4 已落地；切片 3 **没有按原方案做**，原因见下。

| 文件 | 改动 |
|---|---|
| `src/main.rs` | 从 34 行的 `#[path]` 派发器变成**唯一的、架构无关的入口与 panic**：`_start` 在平台的首参寄存器里拿到记录（`RDI`/`x0` 是编译器的事，不是这个文件的事），注册内核服务后交给 `board::start`；panic 用 `mpos_arch::arch::print_args` 打一行、调用本机器的上下文转储、结束运行。全文**没有 `#[cfg]`，也没有 `#[path]`** |
| `src/board/mod.rs` | **新增**：一处选出本机器的 body（与 `arch/src/arch/mod.rs` 同一手法），导出 `start`/`panic_context` |
| `src/board/x86_64.rs` | 原 `src/main/x86_64.rs`：保留验收检查、总线枚举（`devices`）、控制台与 banner（`milestone`）、内核自己的启动（`init`）、panic 上下文转储；交出入口、`init_arch`、`#[panic_handler]` |
| `src/board/aarch64.rs` | 原 `src/main/aarch64.rs`：保留设备探针、EL0 交接与各检查；交出入口与 `#[panic_handler]` |
| `arch/src/arch/x86_64/mod.rs` | 新增 `boot(handoff, milestone, devices, init)` 与 `print_args` —— x86_64 半边接缝，与 aarch64 对称，走 §14.2 已有的 `hal::kernel` 表 |
| `arch/src/arch/mod.rs` | 两个名字各一处导出，`mpos_arch::arch::{boot, print_args}` 在两台机器上都成立 |
| `arch/src/arch/aarch64/mod.rs` | `milestone` 改为接收记录（命令行里的 `console=` 决定内核日志去哪），这是第二台机器唯一需要的形状改动 |

**为什么切片 3 没有按原方案做。** §4.2 设想「检查列表来自 `mpos-arch` 的一个
`const CHECKS: &[(&str, fn(&BootInfo))]`」。落地时这个设想不成立：检查函数体调
`vfs`/`blk`/`net`/`drivers`/`sched`，而这些名字在 `mpos-arch` 里**不存在**（那正是 §14 的
要点）。所以检查留在内核侧，成为 `src/board/<arch>.rs` 里 `init`/`devices` 回调的内容——
一个 body，不是第二份 `main`。

把两套检查合并成一套「能力驱动」的检查是另一件事，值得做但必须单独立项：它会改变
aarch64 **接受什么**（今天它不跑 `check_vfs`/`check_ipc`/`check_sched`），而 aarch64 的
里程碑顺序是按教训排出来的——`check_dev` 必须在 `vfs::tty::init` 之后、`check_console_rx`
必须在接收中断武装之前、`check_serial_tx` 在之后——合并不能顺手做。

**验证（全部实跑）**：

| 门 | 结果 |
|---|---|
| `cargo xtask test` | `=== SUMMARY pass=156 fail=0 (156 built)`，exit 33 |
| `cargo xtask boot-aarch64` | 22 个里程碑全绿 + `INIT_EXITED` |
| `cargo xtask depgraph` | 在棘轮内，互指对 8/8，无新增边 |
| 宿主单测 | `mpos` 511 通过 |
| 两机构建 | `x86_64-unknown-none`、`aarch64-unknown-none --release` 均通过（只剩既有漂移告警） |
| 陈旧路径扫描 | `AGENTS.md docs/ src/ arch/` 下不再有 `main/x86_64.rs`、`main/aarch64.rs`、`src/main/` |

### 4.6 `init` 的共享半边（2026-10-06）

切片 2/4 之后，`main` 是一个文件；但两台机器的 `init` 仍然各写一遍同样的五步。本节把**共享的
五步**抽成一个模块，`init` 本身仍是每台机器一个——两个结论都有硬约束支撑，见下。

| 文件 | 改动 |
|---|---|
| `src/board/steps.rs` | **新增**：五个带顺序契约的步骤——`start_scheduler`、`mount_ramfs_root`、`unpack`、`arm_console`、`hand_to_init`；模块文档同时说明「为什么两个 `init` 不是同一个函数」 |
| `src/board/x86_64.rs` | `init`/`handoff_to_init` 改调这些步骤；一行的 `init_sched` 包装删除。`handoff_to_init` 只留自己的部分：最深处的 boot stack 检查、`discard_exit_code`、等 shell 子进程 |
| `src/board/aarch64.rs` | `init` 按本机器的顺序调用这些步骤；原本内联的 read/`argv`/spawn 三句合成一次调用。顺带删掉因之失效的 `alloc::string::String`、`alloc::vec` 导入（两条新告警，已清） |
| `src/board/mod.rs` | 声明 `mod steps;`；文档写明一个 body 里哪一半不是本机器的 |

**为什么放在接线层（bin）而不是库里。** `arm_console` 有一半是机器接线——武装接收中断是
`crate::arch::enable_console_interrupts`——而 `xtask/src/depgraph.rs` 的 `ARCH_EXEMPT` 只允许
`main`/`lib` 触到机器层，并且它在注释里点名的正是这个调用（"arms its console interrupts"）。
库里的 `boot` 模块是先试过的位置，depgraph 以**第二条独立理由**拒绝：这些步骤要用
`crate::sched` 与 `crate::vfs`，而 `vfs` 已经有一次 `boot` 引用（`/proc/cmdline` 的命令行），
`boot` 反向引用会闭合第 9 对互指。接线层在两者之上、有权点名机器层，且不产生棘轮看得见的边。

**为什么仍然不是一个函数。** 共享的只是这五步，**位置**不是：x86_64 在 spawn 之后才武装控制台
（shell 起来前没人读这跟线），aarch64 在 `check_serial_tx` **之前**武装——那个检查正是「对已武装
路径的检查」；x86_64 在自己的文件系统启动里 unpack，aarch64 在设备之后 unpack、再切根。合成一个
`run()` 就得给它一串 hook 去重排顺序，那就是「两个序列戴一顶帽子」（§14.8 的结论不变）。

**验证（全部实跑）**：

| 门 | 结果 |
|---|---|
| `cargo xtask test` | `=== SUMMARY pass=156 fail=0 (156 built)`，exit 33 |
| `cargo xtask boot-aarch64` | 22 个里程碑全绿 + `INIT_EXITED` |
| `cargo xtask depgraph` | 在棘轮内，互指对 8/8，无新增边 |
| 宿主单测 | `mpos` 511、`mpos-xtask` 65 |
| 两机构建 | 均通过，告警条数与改动前一致（4 / 57） |

depgraph 在本节不是事后校验而是设计约束：`boot::init` 这个初版位置就是它否掉的，两条理由都成立。

---

## 5. `boot` 改为 crate，内核成为 UEFI 存根

### 5.1 现状

- `boot/` 是独立包 `mpos-boot`，`[[bin]]`，目标 `x86_64-unknown-uefi`（由 `boot/.cargo/config.toml` 钉住；根配置钉的是内核的 `x86_64-unknown-none`）。
- 它读 ESP 上的 `\mpos.elf`、`\initramfs.tar`、`\mpos.cmdline`（`boot/src/main.rs` 的 `KERNEL_PATH`/`INITRAMFS_PATH`/`CMDLINE_PATH`），走 ELF 加载 + 重定位（`boot/src/elf.rs`，含 `R_X86_64_RELATIVE`/`R_AARCH64_RELATIVE`）、建页表（`boot/src/vm*`）、`ExitBootServices`、填 `Handoff`（`boot/src/handoff.rs`），最后 `enter()` 跳 `KERNEL_VIRT_BASE + e_entry`，记录在 `RDI`/`x0`。
- xtask 把 `mpos.elf` 与 `BOOTX64.EFI`/`BOOTAA64.EFI` 一起写进 ESP（`xtask/src/rootfs.rs:1282`、`xtask/src/aarch64.rs:1247`）。

**已先行落地的一刀（2026-10-06）**：`boot/src/handoff.rs` 已删除，loader 改为依赖 `mpos-bootinfo`
leaf，与内核共读**同一份** `Handoff` 定义（`boot/Cargo.toml` 加 `mpos-bootinfo` 依赖；`boot/src/main.rs`
与 `boot/src/vm.rs` 的 `use` 改为从该 leaf 取，`AbiRegion as Region`/`HANDOFF_MAGIC as MAGIC` 两个别名
让调用点一字未改）。这消灭了"两处同步"的隐患，但**没有**消灭记录本身——它仍是跨编译单元的线上格式。
#5 要做的是让记录变成**同一进程内的 `&BootInfo`**，那一步才让 `Handoff`（连同 `mpos.elf`）一起消失。
验证：`cargo build -p mpos-boot --target {x86_64,aarch64}-unknown-uefi` 两个目标零告警。

### 5.2 目标

`boot` 从"可执行文件"变成"**内核依赖的 crate**"：内核二进制本身按 `*-unknown-uefi` 构建，它的固件入口就是 loader 的 `main`；loader 逻辑在**同一进程**里跑完，然后就地进入内核主体（不再有"加载别人的 ELF 并跳转"这一步）。

### 5.3 改动点（按依赖顺序）

1. **`boot/Cargo.toml`**：去掉 `[[bin]]`，改 `[lib] name = "mpos_boot"`；`uefi` 依赖不变。`boot/.cargo/config.toml` 的 `[build] target` 删除（该目标现在由**内核**决定）。
2. **`boot/src/main.rs` → `boot/src/lib.rs`**：`fn main() -> Status` 变成 `pub fn run(image: Image) -> !` 之类的库入口；`panic()` 与 `log()` 保留（它们仍需要 `uefi` 的 `boot::` 服务）。
3. **`boot/src/elf.rs` 大幅缩小**：`PT_LOAD`/段映射/重定位整块消失（内核由固件加载，不需要被谁重定位）；`PAGE_SIZE`/`Range` 移到 `mpos-vm`（`crates/vm`，§S2 已建）。
4. **`boot/src/handoff.rs` 整个消失**：记录不再是"跨编译单元的线上格式"，而是进程内 `&mut mpos_bootinfo::BootInfo`。这顺手消灭了"S2 只解决了一半"的那件事——loader 与内核各写一份 `Handoff` 的同步问题**从此不存在**。
5. **内核侧**：`Cargo.toml` 增加 `mpos-boot = { path = "boot" }`；`.cargo/config.toml` 的 `[target.x86_64-unknown-none]`/`[target.aarch64-unknown-none]` 两个块改成 `*-unknown-uefi`（linker script、`mpos_*` 能力标志跟着走）；`src/main.rs` 的 `extern "C" fn _start` 换成 `uefi` crate 的 `#[entry]`，它先调 `mpos_boot::run(...)` 完成固件阶段，再调 `kernel_main(&BootInfo)`。
6. **xtask**：`utils::{kernel_path, loader_path}` 合并（内核就是那个 EFI 应用）；`rootfs.rs` 的 ESP 安装改为把内核写成 `EFI/BOOT/BOOTX64.EFI`、不再写 `boot/mpos.elf`；`selftest.rs::boot_tests` 与 `aarch64.rs::build_esp`/`boot` 的 ESP 布局同步（`mpos.elf` 一项消失）。
7. **文档**：`AGENTS.md` 仓库地图、`docs/ARCHITECTURE.md` 的启动链、`xtask/README.md` 的产物表。

**代价要如实记下**：`-kernel` 式的裸机 bring-up（QEMU 直接把 `mpos.elf` 当内核加载）会失去——那正是 `cargo xtask run` 现在的"快速试内核"路径之一。aarch64 侧没有这个损失（它一直走 UEFI/AAVMF）。

### 5.4 依赖与顺序

- **必须先做 #6**（或与它同一轮）：`boot` 一旦成为内核的依赖，它里面的 `#[cfg(target_arch)]` 就直接进了内核的 crate 图，"架构无关"不再只是口味问题。
- **依赖 S2 的 leaf crate**：`mpos-bootinfo`、`mpos-vm` 已经在工作区（`crates/`），新形状正好用它们。
- **与 #4 的顺序**：#4 切片 2 要把 `_start` 移进 `mpos-arch`，#5 要把入口换成 `uefi` 的 `#[entry]`——**先做 #4 切片 2 会白做一遍**。建议 #6 → #5 → #4 切片 2 的顺序，最后 `main` 自然只剩 `kernel_main`。
- 反向约束：`mpos-boot` 不得依赖 `mpos`（否则成环）。它只依赖 `mpos-arch` 与 `crates/{bootinfo,vm}`。

### 5.5 验证

- `cargo xtask build` 与 `cargo xtask build --arch aarch64`：两台机器都应产出**一个** EFI 应用（且 `ls build/…` 不再有独立的 `mpos.elf`）。
- `cargo xtask test`：x86_64 的 UEFI 启动 + `=== SUMMARY pass=155 fail=0`（这是"内核作为存根真的被固件拉起来了"的端到端证据）。
- `cargo xtask boot-aarch64`：26 个 marker 全出。
- `cargo xtask depgraph`：新增 `mpos-boot` 节点后边集仍须在 ratchet 内，且 `mpos-boot → mpos` 必须是 **0**（成环即失败）。
- `cargo xtask clean && cargo xtask all`：确认没有任何一步还假设 `mpos.elf` 存在于 ESP。

---

### 5.6 实测：这一步**不能**分期（2026-10-06）

试着用"过渡谓词"分期落地——把 `target_os = "none"` 一次性加宽成
`any(target_os = "none", target_os = "uefi")`，让两台构建并存再去搬——**实测走不通，已回退**：

- 加宽后 `arch/src/lib.rs` 的 `pub mod arch` / `pub mod hal` 也跟着在 **uefi** 下编译，而 `mpos-arch`
  的 `critical-section` 只声明在 `target_os = "none"` 下 → loader 构建立刻 47 个错（aarch64 15 个）。
- 更根本的原因就在 `arch/src/lib.rs:58` 那段注释里：`early` 是 uefi-only、`arch`/`hal` 是 none-only，
  正是因为**loader 是 uefi 而内核不是**。一旦内核也是 uefi，`target_os` 就再也分不开"loader 构建"和
  "内核构建"。所以 #5 是**原子**的：内核改 target、`boot` 变 lib、删掉 loader 那个 bin、`early` 与
  `arch`/`hal` 合并——必须一起发生，没有中间态。

分期试过、回退干净后树仍全绿（x86_64 `pass=155 fail=0`；aarch64 全 marker + `INIT_EXITED`；宿主 `mpos`
511、xtask 65；`depgraph` 8/8；fmt 干净）。两个脚本留在 `.workbuddy/{sweep,unwiden}_machine_cfg.py`，
真要原子翻转时那一刀仍是机械的（402 处）。

**动手前必须先定两件事**（不是改代码顺手能解决的口味问题）：

1. **两个 `#[global_allocator]`**：内核 `src/mm/mod.rs` 有一个，`uefi` 的 `alloc`+`global_allocator`
   feature 再给一个；同一个二进制里两个 = 编译错误。要么 loader 阶段别用全局分配器（把
   `FileSystem::read` 改成读进内核给的固定缓冲），要么不用 uefi 那个 feature 而自管。
2. **PE 链接器要提供内核的段符号**：`arch/src/arch/{x86_64,aarch64}/mod.rs` 用
   `extern "C" { static __text_start/__text_end/__rodata_start/__rodata_end/__data_start/kernel_end: u8; }`
   切 W^X 映射，aarch64 还 ASSERT 四个 `mpos_*` asm 符号落在 `.text` 内。现在由 ELF 的 `linker.ld` 给；
   改 PE 后要换成 PE 链接脚本（或 `--defsym`），并保证那四个函数仍在 text 段。

**一个已确认的好消息**：内核不依赖自己被链在高半——`src/**` 里 `KERNEL_VIRT_BASE` 出现 **0** 次，
内核只从记录里读 `kernel_virt_base`。所以"就地运行"或"自重定位到高半"都可行，地址模型不是障碍。

## 6. `boot` 架构无关（已完成）

### 6.1 原状：`boot` 里有三处架构分叉（文件头自己列了）

`boot/src/main.rs` 的文档注释写明了三样"属于机器"的东西：

| 现在的位置 | 内容 | 应该去哪 |
|---|---|---|
| `boot/src/vm/x86_64.rs`、`boot/src/vm/aarch64.rs` | 页表描述符格式（4 级/4 KiB vs MAIR/TCR/4 级） | `mpos-arch` |
| `boot/src/main.rs` 里的 `log()` 及其 PL011 分支 | 进度行的串口（COM1 端口 I/O vs PL011 MMIO） | `mpos-arch`（`arch/src/arch/aarch64/pl011.rs`、`arch/src/arch/x86_64/serial.rs` 已有对应件） |
| `boot/src/main.rs` 的两份 `enter()`（422/376 行） | 最后一条指令：`mov cr3` + `jmp` vs `msr ttbr0_el1` + `br`，栈 16 字节对齐 | `mpos-arch` |

`boot/src/vm.rs` 的**计划**部分（三段范围：identity / direct / higher-half alias，`Pool`、`map_physical_memory`、`map_kernel_image`、`is_ram`、`merge`、`pool_bytes_for`）本来就是架构无关的，注释也这么写——它留在 `boot`。

### 6.2 实际改动（已落地）

三处分叉全部下沉为 `arch/src/early/`，工作区根下面的新模块树：

| 文件 | 内容 |
|---|---|
| `arch/src/early/mod.rs` | `Frames` trait、`PageTables`/`serial_*`/`map_extra`/`extra_mapped_bytes`/`enter`/`FIRMWARE_PUBLISHES_DEVICE_TREE` 的重导出、`set_log`/`log` 进度行 hook |
| `arch/src/early/x86_64.rs` | 4 级 4 KiB 描述符、COM1、`mov cr3` + `jmp`；`map_extra` 是空操作、`extra_mapped_bytes()` 返回 0、`FIRMWARE_PUBLISHES_DEVICE_TREE = false` |
| `arch/src/early/aarch64.rs` | MAIR/TCR/4 级描述符、设备窗口、PL011、`msr ttbr0_el1` + `br`；`map_extra` 映射设备窗口、`extra_mapped_bytes()` 返回 `2 × DEVICE_WINDOW`、`FIRMWARE_PUBLISHES_DEVICE_TREE = true` |

**与本节原来计划的一处偏离**：计划写的是 `arch/src/arch/<arch>/early.rs`，实际落在 `arch/src/early/`（上一层，与 `arch/`、`hal/` 并列的独立模块树）。原因：`arch/src/arch/**` 是**内核**的机器层、门控 `#[cfg(target_os = "none")]`，而 loader 是 `{x86_64,aarch64}-unknown-uefi`；把 `early` 放进那棵树就得给 `arch/mod.rs` 的几乎所有重导出加 target-os 门控，比这一刀本身更大也更脆。于是 `arch/src/lib.rs` 多了全 crate 唯一一句 `#[cfg(target_os = "uefi")]`。#5 落地（内核自己成为 UEFI 应用）时它放宽成 `any(…)`，其余不动。

**两个新抽象，都是"谁决定什么"的问题**：

- **`Frames`**：loader 决定帧从哪来（一段连续的 `LOADER_DATA`、按 RAM 算大小、永不归还），`early` 决定往帧里写什么。`PageTables<F: Frames>`，`Pool` 在 boot 侧 `impl Frames`。单态化、无 `dyn`；而且建造器**不能**分配 loader 没交给它的任何东西——旧写法只能在注释里声明这一点。
- **`set_log`**：loader 拥有文本格式（`mpos-boot: ` 前缀 + 换行），`early` 拥有线。注册式，与 `hal::kernel` 同形。`early` 自己那两行（设备窗口、找不到树时的 GUID 列表）因此走同一套输出。

**两样刻意没下沉**，界线就是这一步的意义：

- `copy_device_tree` 留在 `boot`：读固件的 configuration table、按树头取长度、把字节拷进帧，是 UEFI 的活而不是机器的活，且两台机器上是同一段代码。机器只回答一个布尔 `FIRMWARE_PUBLISHES_DEVICE_TREE`（x86_64 为 false，它的固件用 ACPI 描述自己），loader 按名字问它，而不是把分支藏在 `#[cfg]` 里。`DTB_GUID`/`FDT_MAGIC`/`FDT_MAX_BYTES` 由此变成无条件常量——它们从来不是架构事实，只是碰巧只在一台机器上编译。
- `Pool`、三段范围、`merge`、`pool_bytes_for` 留在 `boot/src/vm.rs`，该文件现在**一个 `cfg` 都没有**：计划是架构无关的，只有编码是机器的。

**跟着代码走的一个依赖**：`mpos-arch` 的 `critical-section` 变成 `[target.'cfg(target_os = "none")'.dependencies]`。`hal::tx_ring` 是它唯一的使用者而 `hal` 不为 loader 编译，不门控就是两个 UEFI target 上各一条 `unused dependency`——cargo 自己报出来的。

### 6.3 依赖

- 依赖 S4（`mpos-arch` 存在）与 #5.4 的顺序（先 #6 再 #5）。
- 依赖 S3 已完成的"直映射是 HAL 事实"（`hal::phys_to_virt`/`direct_map_offset`）：引导期页表构建器需要它，而它现在归 `mpos-arch` 所有。

### 6.4 验证（全部实跑）

1. **结构判据**：`grep -r target_arch boot/src/` = **0**（改前 20 处）；`cargo xtask depgraph` 中 `boot → mpos`（内核）= 0，ratchet 内、互指对 8/8、无新增边。
2. **行为判据**：`cargo xtask boot-aarch64` 的 marker 链**逐字不变**，含 `INIT_EXITED`；loader 自己的 11 行 `mpos-boot:` 输出逐字不变，包括 `device window 0x8000000..0xc000000 mapped at its own address and at 0xffffffff40000000, Device and not executable`——**这一行现在由 `mpos-arch` 产生**，是 `set_log` 那条 hook 走通的直接证据。
3. `cargo xtask test` = `=== SUMMARY pass=155 fail=0`；`cargo xtask test --arch aarch64` = `=== SUMMARY pass=25 fail=0`；宿主单测 `mpos` 511；xtask 单测 65。
4. `cargo build -p mpos-boot --target {x86_64,aarch64}-unknown-uefi` 与 `cargo build -p mpos-arch --target {x86_64,aarch64}-unknown-uefi` 各自能单独构建，四个目标零告警（`mpos-boot` 只剩 `write_handoff` 的 9 参数告警，HEAD 上就有）。

---

## 7. 跨项依赖与统一验证矩阵

```
        ┌─────────────────────────────────────────────┐
        │ S0–S4（已完成）: --arch 驱动构建、词表 leaf   │
        │ crate、hal::kernel hook 表、mpos-arch crate   │
        └───────────────┬─────────────────────────────┘
                        │
   #3 修复 t-unix ──────┤（独立，已完成）
                        │
   #4.main 合一 ────────┤ 切片 1 已完成
                        │
   #1 测试迁 std+rustix ┤
                        ▼
   #2 xtask 共用 ───────┘（#1 之后形状才收敛）
                        │
   #6 boot 架构无关 ────┤（已完成）
                        ▼
   #5 内核 = UEFI 存根 ─┤
                        ▼
   #4 切片 2–4 收尾
```

**每个切片都必须通过的同一组门禁**（顺序即成本从低到高）：

| 门禁 | 命令 | 期望 |
|---|---|---|
| 结构 | `cargo xtask depgraph` | ratchet 内；互指对 8/8；无新增边 |
| 宿主单测 | `cargo test -p mpos --lib --target x86_64-unknown-linux-gnu -p <LEAF_CRATES> …` | 511 + leaf 全绿 |
| xtask 单测 | `cargo test -p mpos-xtask --target x86_64-unknown-linux-gnu` | 65 条全绿（含 `every_leaf_crate_is_tested`） |
| x86_64 构建 | `cargo xtask build` | 产物为 x86_64 目标 |
| aarch64 构建 | `cargo xtask build --arch aarch64` | 产物为 aarch64 目标（kernel + loader） |
| aarch64 启动 | `cargo xtask boot-aarch64` | 26 个 marker，含 `INIT_EXITED` |
| x86_64 自检 | `cargo xtask test` | `=== SUMMARY pass=155 fail=0` |
| aarch64 自检 | `cargo xtask test --arch aarch64` | `=== SUMMARY pass=25 fail=0`（#1 落地后应等于 x86_64 减去 `tests.toml` 的 `aarch64` 缺口） |
| 格式 | `python3 .workbuddy/fmt-touched.py && cargo fmt -p mpos -p mpos-arch -p mpos-xtask -- --check` | 无差异（**不要**用 `cargo fmt -p mpos`：本工作区并发，会格式化别人的文件） |

**"多架构一致"在这份方案里的确切含义**（避免把三件事混为一谈）：
1. **同一份源码**：`src/**`、`mpos-arch/src/**` 里没有 `target_arch` 条件（`src/hal`+`src/arch` 今天已经是 0；#4/#6 把它扩到 `main` 与 `boot`）。
2. **同一份测试**：用户程序一套，两台机器都跑（#1）；扫描逻辑一套（#2）。
3. **同一套判据**：两台机器由**同一组命令**判绿，差异只允许出现在 `tests.toml` 里**具名**的能力缺口上，且缺口必须是"这台机器确实做不到"，不能是"跑不通就先排除"。

---

## 8. 本轮实际落地的改动（可复核清单）

| 文件 | 改动 |
|---|---|
| `src/vfs/mod.rs` | 新增 `require_directory`；`resolve_parent_opt` 的 `None` 分支改走它 |
| `src/vfs/mount.rs` | `resolve_parent`/`resolve_parent_from` 各加目录检查；新增回归测试 `a_parent_that_is_not_a_directory_is_not_dir` |
| `src/kernel_services.rs` | **新文件**：唯一的 `impl hal::kernel::Kernel` + `register()` |
| `src/lib.rs` | 声明 `#[cfg(target_os = "none")] pub mod kernel_services;` |
| `src/board/x86_64.rs` | 删 struct+impl（−136 行）与两个已空 import；入口改调 `mpos::kernel_services::register()` |
| `src/board/aarch64.rs` | 同上（−127 行）与四个已空 import |

第二轮（第 6 项落地，`boot` 架构无关）：

| 文件 | 改动 |
|---|---|
| `arch/src/early/mod.rs` | **新文件**：`Frames` trait、`set_log`/`log` 进度行 hook、各机件的重导出 |
| `arch/src/early/x86_64.rs` | **新文件**：4 级 4 KiB 描述符 + `PageTables<F>`（原 `boot/src/vm/x86_64.rs`）、COM1、`enter()`（原 `boot/src/main.rs`） |
| `arch/src/early/aarch64.rs` | **新文件**：MAIR/TCR/描述符 + `PageTables<F>`（原 `boot/src/vm/aarch64.rs`）、设备窗口 + `map_extra`、PL011、`enter()`（原 `boot/src/main.rs`） |
| `arch/src/lib.rs` | 加 `#[cfg(target_os = "uefi")] pub mod early;`；`arch`/`hal` 补上 `#[cfg(target_os = "none")]` |
| `arch/Cargo.toml` | `critical-section` 从无条件依赖改为 `[target.'cfg(target_os = "none")'.dependencies]`（cargo 的 `unused dependency` 报出来的） |
| `boot/Cargo.toml` | 加 `mpos-arch = { path = "../arch" }` |
| `boot/src/vm.rs` | 删 `vm/` 子模块与设备窗口常量/`extra_mapped_bytes`；`Pool` 加 `impl Frames`；两个 map 函数泛型化 |
| `boot/src/main.rs` | 删 `mod serial`、两份 `enter()`、`copy_device_tree` 与 DTB 常量的 `#[cfg]`；进度行改走 `early::serial_write`；入口加 `early::serial_init()` + `early::set_log(log)`；设备树与设备窗口改按名字问 `early` |
