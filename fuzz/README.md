# `cargo fuzz` — 内核解析器的模糊测试 | Fuzzing the kernel's parsers

`fuzz/` 是 mpos 的模糊测试套件，独立于内核 workspace（`Cargo.toml` 里有一个空的
`[workspace]`）。它只测 `src/lib.rs` 在宿主架构下仍会编译的那些模块 —— 也就是
所有「把不可信字节变成内核随后要相信的东西」的入口：ELF 装载、ustar initramfs、
磁盘上的 ext2/ext4 结构、分区表、路径、PCI 尺寸计算、控制台的转义序列解析器、以及
把用户态请求变成设备自述的 `uinput`/`evdev` 口径。机器相关的代码（`arch`、
`drivers`、`console`、`net`）不在这里，它们的测试是 `user/abitest` 里跑在真机上的
那些一致性程序。

`fuzz/` is the fuzzing suite for mpos, and its own cargo workspace (the empty
`[workspace]` in `Cargo.toml`). It covers exactly the modules `src/lib.rs` still
compiles for a host build - which is the set of places where untrusted bytes
become something the kernel then trusts: the ELF loader, the ustar initramfs,
ext2/ext4's on-disk structures, the partition table, path spelling, PCI sizing,
and the console's escape parser. Machine-dependent code (`arch`, `drivers`,
`console`, `net`) is not here; its tests are the conformance programs in
`user/abitest` that run on the machine.

## 怎么跑 | How to run

需要 nightly 工具链（`rust-toolchain.toml` 已经固定）和 `cargo-fuzz`
(`cargo install cargo-fuzz`)。

```sh
cargo install cargo-fuzz          # 一次性 | once
cd fuzz
cargo fuzz build                  # 编译全部目标 | build every target
cargo fuzz run elf_parse          # 一直跑到崩溃 | run until it crashes
cargo fuzz run ext4_image -- -max_total_time=300
cargo fuzz tmin ext4_image artifacts/ext4_image/crash-<hash>
```

`cargo fuzz` 用 ASan，并打开 debug assertion 与溢出检查——比内核自身的 release
配置更严格。这正是重点：`date`、`term` 那类算术在内核里是**静默回绕**，在这里会
直接 panic。Fuzz builds run with ASan, debug assertions and overflow checks -
stricter than the kernel's own release profile, which is the point: the
arithmetic in `date` and `term` **wraps silently** in the kernel and panics here.

种子语料在 `seeds/<target>/`，是真实制品（`mke2fs` 生成的镜像、`target/` 里的静态
musl ELF、构建出的 initramfs 的一部分）。首次运行前拷进语料目录：
Seed corpora live in `seeds/<target>/` and are real artefacts - `mke2fs` images,
a static musl ELF out of `target/`, the head of a built initramfs. Copy them in
before the first run:

```sh
for d in seeds/*/; do
    t=$(basename "$d")
    mkdir -p "corpus/$t"
    cp -n "$d"* "corpus/$t/"
done
mkdir -p corpus/ext4_write && cp -n seeds/ext4_image/* corpus/ext4_write/
```

`ext4_write` 用 `ext4_image` 的镜像：两个目标是同一份磁盘，只是一个读、一个改。
`corpus/`, `artifacts/` and `target/` are ignored; `seeds/` and `fuzz_targets/`
are committed. `ext4_write` reuses `ext4_image`'s images - the same disk, read
by one and written by the other.

`virtgpu_capset` 的种子是**例外**：它不是抓下来的制品，而是按实测形状拼出来的
——响应码是 `RESP_OK_CAPSET`，载荷长度是 `user/staging/virtgpu-probe/` 测到的两个
capset 大小（VIRGL 308、VIRGL2 1408）。真实的是码与长度这两个事实，载荷字节本身
是零。种子在这里的作用只是把 fuzzer 领进载荷分支：`resp_capset_payload` 要求前
四个字节正好是那个响应码，纯字节 fuzzer 几乎撞不到。`virtgpu_capset`'s seeds are
the exception: not a captured artefact but the measured *shape* - the response
code, and the two cap set sizes `user/staging/virtgpu-probe/` recorded (VIRGL
308, VIRGL2 1408). The code and the lengths are the real facts; the payload
bytes are zeroes. All the seeds do is lead the fuzzer into the payload branch,
which needs those first four bytes to be exactly that code.

## 目标 | Targets

每个目标除了「不 panic」之外还断言一个该模块自己承诺过的性质 —— 崩溃只是最不值得
知道的那种结果。Every target asserts a property the module already promised,
not merely that it does not panic - a crash is the least informative outcome.

| 目标 Target | 入口 Entry point | 断言的性质 Property asserted |
| --- | --- | --- |
| `elf_parse` | `mm::elf::parse` | 映像留在低半区；段的内存不小于其中的文件字节；每个重定位落段内 |
| `tar_unpack` | `vfs::tar::unpack_into` | 解出的树读回来就是归档里的字节；成员名不会爬出根目录 |
| `ext4_image` | `vfs::ext4::Ext4Fs::mount` + 读 | 挂载之后可以按任意 inode 号与任意名字读，边界算术不出错 |
| `ext4_write` | 同一挂载 + 创建/写/删除 | 分配器、目录索引分裂与 `statfs` 计数在改动之后仍然自洽 |
| `partition_table` | `blk::part::{partitions, find_*}` | 任何分区窗口都落在设备之内，长度不会回绕 |
| `binfmt_shebang` | `binfmt::shebang` | 交给 `exec` 的解释器路径非空且不含 NUL |
| `vfs_path` | `vfs::path::{normalize, components}` | 规范化是绝对的、幂等的；分量重新拼回同一路径 |
| `pci_bars` | `pci::{bar_range, rom_bar}` + 属性格式 | 尺寸是 2 的幂或为 0；`/sys`、`/proc` 的行保持 pciutils 的形状 |
| `uevent_encode` | `uevent::Event::encode_seq` | 设备名不能改变一个 uevent 持有几条记录；`SEQNUM` 单调递增 |
| `term_grid` | `vfs::term::Grid::feed` | 光标不出网格（末列的 wrap-pending 是 `col == cols`，blit 会钳）；逐字节喂入与整块喂入同屏 |
| `keymap_events` | `vfs::term::keymap::Keymap::event` | 长度不超过缓冲；组合键永不产出字节 |
| `signal_sets` | `sched::signal` | 表外信号被丢弃；pending 集按序取空 |
| `sched_policy` | `sched::policy::Scheduler` | 每次转移恰好移动它声称的那数量；时钟不回退 |
| `date_civil` | `date::{days_from_civil, civil_from_epoch}` | 两个方向互为逆运算，包括闰年规则 |
| `logging_cmdline` | `logging::level_from_cmdline` | 任何命令行都落在可比较的等级区间内 |
| `inotify_record` | `vfs::inotify::{pad_name, record_len, event_matches}` | 记录声明的长度与它的字节一致，且对齐 |
| `uinput_setup` | `uinput::{parse_setup, record_count, bit_request, Device::set_bit}` | 短于 `struct uinput_setup` 的字节被拒；名字不越字段、不含 NUL；id 就是给定字节；声明的位落进 `EVIOCGBIT` 读回的那张位图 |
| `evdev_window` | `evdev::Records` | 不满一条记录的读是 `None`（`EINVAL`）；`readable` 与紧随其后的 read 一致；抓取期间非持有者读不到；等待者只在记录属于它时才 due |
| `virtgpu_capset` | `virtgpu_abi::resp_capset_payload` | 载荷永远是入参的子切片且不长于它；只有回复码真是 `RESP_OK_CAPSET` 时才返回；`None` 只有两种成因（短于头部，或码不对） |

`support.rs` 里是共用的 `MemDev`（一个包着 `Vec<u8>` 的 `BlockDev`，越界读返回零）
和几个钳位：设备最大 1 MiB，屏幕最大 `MAX_ROWS` 行。一个被构造出来的**长度**会让
笔记本 OOM，而那不是内核的问题，值得区分开。`support.rs` holds the shared
[`BlockDev`](../crates/blk/src/lib.rs) over a `Vec<u8>` - reads past the end answer
zeros, which is what a short read on a real disk does - and the clamps that keep
a crafted *length* (which is the fuzzing host running out of RAM, not a kernel
bug) apart from a crafted *value*.

## 泄漏检查 | Leak checking

`ext4_write`（有时还有 `ext4_image`）会被 LeakSanitizer 报出来：一次挂载的
`Core` 和它的每个 inode 互相引用（`Core::inodes` 存 `Arc<dyn Inode>`，而 inode
持 `Arc<Core>`），所以整个文件系统的元数据永远不会被释放。对一个从不卸载的内核
这是设计如此，跑之前关掉泄漏检查即可：`ext4_write`, and sometimes `ext4_image`,
come back from LeakSanitizer: a mounted `Core` and each of its inodes reference
one another (`Core::inodes` holds `Arc<dyn Inode>`, an inode holds `Arc<Core>`),
so no mount's metadata is ever freed. For a kernel that never unmounts that is
by design - turn the check off:

```sh
ASAN_OPTIONS=detect_leaks=0 cargo fuzz run ext4_write
```

