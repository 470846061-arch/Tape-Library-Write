# 内核兼容说明（Linux 4.18 – 6.10）

VTL 在**运行内核**上编译 `vtl.ko`（`make -C kernel` 使用 `/lib/modules/$(uname -r)/build`）。**不支持**把为 6.6 编译的模块加载到 4.18 内核，反之亦然（`vermagic` 必须一致）。

## 支持范围

| 项目 | 说明 |
|------|------|
| **内核版本** | **4.18 ≤ 内核 ≤ 6.10**（含 Kylin 4.19、openEuler 24.03 / 6.6、RHEL 8/9、VMware 来宾等） |
| **架构** | **x86_64**、**aarch64**（均为 64 位 LP64；须在本机架构上编译 `vtl.ko` 与 `vtladm`） |
| **用户态** | 与内核版本无关；同一 `vtladm` 二进制可管理不同内核上的 `vtl.ko`（只要在该主机本地编译安装） |

## 代码级适配

| 内核 | 行为 |
|------|------|
| **&lt; 6.5** | SCSI 完成路径使用 `(cmd)->scsi_done(cmd)` |
| **≥ 6.5** | 使用 `scsi_done(cmd)`（6.5+ 移除 `scsi_cmnd.scsi_done` 成员） |
| **全版本** | `scsi_add_host` / `scsi_scan_host` 推迟到 `vtl_bringup` 工作队列；`serial_full_bringup` / `serial_scsi_scan` 默认开启 |
| **全版本** | SCSI CDB 操作码本地 `#ifndef` 兜底（避免头文件差异） |

## 推荐模块参数（生产默认见 `/etc/default/vtladm`）

下列参数在 **4.18–6.10** 上均适用；数值偏保守，优先避免 scan/rmmod 竞态：

```text
scan_delay_ms=500
post_add_scan_delay_ms=600
bringup_stagger_ms=400
scan_host_stagger_ms=3000
scan_async_quiesce_ms=5000
rmmod_quiesce_ms=12000
```

- **单库 1×10 / 2×10**：保持上述默认即可；务必安装 **`59-vtl-scsi.rules`**（跳过 udev `scsi_id`）。
- **多库 / Plan B（8×8×256）**：`vtl-kernelctl` 会自动加 **`noscan=1`**；用 `vtl-scsi-scan-all-hosts.sh` 分段 scan。详见 [SCSI.md](../../userspace/docs/SCSI.md) §1g。
- **禁止**在生产环境开启 `allow_hot_geom=1`（热改几何易与 LIO/备份 I/O 竞态）。

## 按内核代的运维注意

### 4.18 – 5.x（含 Kylin 4.19）

- `rmmod` 时若 `/dev/st*`、`/dev/sg*` 仍被占用，可能 **kdump**；必须用 `vtl-kernelctl stop` / `install.sh`（安全 reload）/ `uninstall.sh`（`vtl-kernel-safe.sh`：停 timer、LIO 预检、scsi delete、加长 post-rmmod 等待）。
- 仅更新 userspace：`install.sh --no-reload` 或 `VTL_SKIP_KERNEL_RELOAD=1`。
- 换 `vtl.ko`（默认）：`install.sh --enable` 会 **rmmod 后重启**，由 `vtl-kernel.service` 在开机时 insmod；避免在同一脚本进程里 rmmod 后再 fork。
- 不重启、当场 reload：`install.sh --no-reboot` 或 `VTL_NO_REBOOT=1`。
- 卸载：`uninstall.sh` 默认 rmmod 后同样重启；`--no-reboot` 保留长等待。
- `scsi_complete_async_scans()` **未对模块导出**；依赖 `scan_async_quiesce_ms` 睡眠近似等待异步 scan。
- insmod 后**不要**立刻狂刷 `lsscsi`；等 `vtl_post_insmod_settle` 或日志出现 `... finished`（ch 初始化）。

### 5.10 – 6.4

- 与 4.18 相同 SCSI 完成宏路径；默认 bringup 策略已覆盖多数发行版。

### 6.5 – 6.10（含 openEuler 6.6）

- 除上述外，曾见 **udev `scsi_id` 在 scan 后数十毫秒内读 sysfs 触发 GPF**；**必须**启用 `packaging/udev/59-vtl-scsi.rules`。
- 编译器与内核构建 GCC 小版本不一致时可能有 WARN，一般可加载；严重 vermagic/GCC 不匹配时 `insmod` 会失败。

## udev（4.18+）

`ENV{ID_SCSI}="skip"` 由 systemd-udev 的 `60-persistent-storage.rules` 识别（RHEL 7+ / 主流 openEuler/Kylin 均具备）。规则文件须命名为 **`59-vtl-scsi.rules`**，以便在 `60-*` 之前执行。

## 验证清单（换内核或换机器后）

```bash
uname -r
modinfo /opt/vtladm/ko/vtl.ko | grep vermagic
/opt/vtladm/sbin/vtl-kernelctl start
sleep 12
lsscsi -g | grep -i VTL
dmesg | tail -30 | grep -iE 'vtl|Oops|protection fault'
test -f /etc/udev/rules.d/59-vtl-scsi.rules && udevadm control --reload-rules
```

## aarch64（ARM64）

| 组件 | 状态 |
|------|------|
| **`vtl.ko`** | 无 x86 专用汇编；在 ARM 主机上用 `kernel-devel` + `make` 即可 |
| **`vtladm`** | `build.rs` 仅要求 Linux + 64 位指针；本机 `cargo build --release` 产出 `aarch64` ELF |
| **ioctl** | `repr(C)` 固定布局 + 标准 `_IOW` 编码，与 x86_64 64 位一致 |
| **运维脚本** | shell + udev 规则与架构无关 |

**注意**：不能把 x86_64 的 `vtl.ko` / `vtladm` 拷到 ARM 上；交叉编译用户态示例：

```bash
rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu --bin vtladm --bin vtladm-iscsi
```

生产环境建议在 ARM 上完整跑一遍 `install.sh --enable` 并核对 `uname -m`、`file /opt/vtladm/bin/vtladm`、`modinfo vtl.ko`。

## 不支持

- **32 位**（`i686`、`armv7l` 等）
- 内核 **&lt; 4.18**
- 与运行中内核 **vermagic 不一致** 的预编译 `vtl.ko`
- 在 **Windows / macOS** 上直接 `insmod` 或运行 `vtladm`（用户态需 Linux 64 位 ELF）
