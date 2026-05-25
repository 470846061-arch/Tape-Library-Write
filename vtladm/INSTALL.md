# 安装指南

## 系统要求

- **Linux 内核 4.18 – 6.10**（含 Kylin 4.19、openEuler 6.x、RHEL 8/9 等；**必须在目标机上**用对应 `kernel-devel` / `linux-headers` 编译 `vtl.ko`）
- 详细兼容矩阵与按内核代的运维注意：**[packaging/docs/KERNEL-COMPAT.md](packaging/docs/KERNEL-COMPAT.md)**（安装后位于 `/opt/vtladm/docs/KERNEL-COMPAT.md`）
- GCC、GNU Make
- Rust 工具链与 Cargo（用于用户态 `vtladm`）。**rustc ≥ 1.66**（`axum` 0.7 要求）；发行版 **Cargo 1.75–1.82** 通常可用。
- 若仓库含 **`Cargo.lock` version 4**（由 **Cargo ≥ 1.83** 生成），旧版 Cargo 会报错；`install.sh` 会自动删除该 lock 后按 **`Cargo.toml` 钉版本** 解析，或你可手动：`rm -f userspace/Cargo.lock && cargo build --release`。

## 构建步骤

### 1. 准备构建环境

```bash
# Debian/Ubuntu：内核头文件与基础编译工具
sudo apt-get install linux-headers-$(uname -r) build-essential

# RHEL/CentOS 系列
sudo yum install kernel-devel-$(uname -r) kernel-headers-$(uname -r)

# Rust（若尚未安装）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### 2. 构建内核模块

```bash
cd kernel
make
```

### 3. 构建用户态工具

```bash
cd ../userspace
cargo build --release
```

产物：`userspace/target/release/vtladm`

## 安装到系统

### 一键安装到 `/opt/vtladm`（推荐）

在仓库根目录（需 root、已安装内核头文件与 Rust）：

```bash
cd /path/to/vtladm
sudo sed -i 's/\r$//' install.sh
sudo sh install.sh --enable
```

将编译并安装到 **`/opt/vtladm`**（目录不存在会自动创建）：

| 路径 | 内容 |
|------|------|
| `/opt/vtladm/ko/vtl.ko` | 针对当前 `uname -r` 编译的模块 |
| `/opt/vtladm/bin/vtladm` | 管理 CLI / Web |
| `/opt/vtladm/bin/vtladm-iscsi` | iSCSI 辅助（可选） |
| `/opt/vtladm/scripts/` | 巡检、rescan、诊断等 |
| `/opt/vtladm/sbin/vtl-kernelctl` | `start\|stop\|status` 加载模块 |
| `/etc/default/vtladm` | 模块参数、Web 绑定地址 |
| `/opt/vtladm/var/` | `vtl.conf`、`vtl.db`、`tapes/`、`log/`（运行时数据） |
| `/opt/vtladm/sbin/vtl-uninstall` | 卸载脚本（`uninstall.sh` 安装副本） |

`--enable` 会启用并启动：`vtl-kernel.service`、`vtladm-web.service`、`vtl-patrol.timer`（始终）。**`vtl-robot-sync.timer`** 在 `robot_sync=true` 且 `auto_sync_db_from_kernel!=false` 时启用。`init-config` 默认：运行时机械手在 **vtl.ko**，`auto_sync_db_from_kernel=true` 仅同步目录提示。

### 内核模块：默认卸载后重启（推荐）

涉及 **`vtl.ko`** 的安装/升级/卸载，脚本**默认接受整机重启**（Kylin / openEuler 上最稳妥，避免 `rmmod` 后延迟 GPF）：

| 操作 | 默认行为 |
|------|----------|
| `sudo sh install.sh --enable` | 若本次编译了 kernel 或 `vtl.ko` 有变化且模块已加载 → **安全 `rmmod` → 约 10s 后 `reboot`** → 开机由 `vtl-kernel.service` 自动 `insmod` |
| `sudo sh uninstall.sh` | 安全 `rmmod` → 删除文件 → **重启** |
| 仅更新用户态 | `sudo sh install.sh --no-reload`（不碰 `vtl.ko`） |
| 维护窗口不重启 | `sudo sh install.sh --enable --no-reboot` 或 `VTL_NO_REBOOT=1`（进程内 `vtl_safe_reload`，需停备份/LIO） |

```bash
# 典型升级（接受重启）
cd /root/vtladm
sudo sed -i 's/\r$//' install.sh uninstall.sh packaging/scripts/*.sh
sudo sh install.sh --enable
# 主机会自动 reboot；登录后检查：
systemctl status vtl-kernel.service
lsmod | grep vtl
/opt/vtladm/bin/vtladm -L marstor inventory
```

卸载（保留数据）：

```bash
sudo /opt/vtladm/sbin/vtl-uninstall
# 或: sudo sh uninstall.sh
```

彻底删除数据：`sudo sh uninstall.sh --purge`。

**产品上限**：最多 **8** 个在线库（SCSI host）、每库 **8** 台驱动器、**256** 个数据槽。推荐 **方案 B（半薄内核）**：`insmod` 一次满配拓扑，日常改驱/槽用 **resize ioctl**，无需 `rmmod`/`insmod`。在 `/opt/vtladm/var/vtl.conf` 取消注释并启用：

```ini
kernel_geometry_mode=fixed
kernel_geom_prefer_ioctl=true
kernel_reload_on_db_change=false
```

然后 `sudo /opt/vtladm/sbin/vtl-kernelctl reload`（维护窗口），或 `install.sh --enable` 在已启用 fixed 时会自动跑 **`vtl-scsi-scan-all-hosts.sh`** 与 **`vtladm kernel-align`**。详见 **`/opt/vtladm/docs/SCSI.md`** §1g。

**ko / lsscsi / UI 关系**见 **`/opt/vtladm/docs/STACK.md`**。巡检：

```bash
/opt/vtladm/scripts/vtl-patrol.sh
vtladm patrol
```

### `vtl-kernel.service` 启动失败

在 openEuler / RHEL 等环境上，安装日志里若出现 **Clock skew** 或 **`Job for vtl-kernel.service failed`**，按顺序排查：

```bash
# 1) 同步时钟后重装模块（避免 make 跳过编译）
chronyc makestep 2>/dev/null || ntpdate -u pool.ntp.org 2>/dev/null || true
cd /root/vtladm   # 你的源码目录
sudo sed -i 's/\r$//' install.sh packaging/sbin/* packaging/scripts/*.sh /etc/default/vtladm
sudo sh install.sh --enable

# 2) 若仍失败，看具体原因（新版 vtl-kernelctl 会打印 dmesg）
systemctl status vtl-kernel.service -l
journalctl -xeu vtl-kernel.service --no-pager
/opt/vtladm/sbin/vtl-kernelctl start
modinfo /opt/vtladm/ko/vtl.ko | grep vermagic
uname -r
```

常见原因：

| 现象 | 处理 |
|------|------|
| `vermagic` 与 `uname -r` 不一致 | 在本机重新 `install.sh`（勿拷贝别的主机 `vtl.ko`） |
| `Invalid module format` / `disagrees about version` | 同上；检查是否装了对应 `kernel-devel` |
| `Unknown parameter` 且参数名带奇怪后缀 | `/etc/default/vtladm` 含 CRLF：`sed -i 's/\r$//' /etc/default/vtladm` |
| make 报 **modification time in the future** | 先 `chronyc makestep`，再 `make -C kernel clean && make` |
| `insmod: ERROR: could not insert module` + dmesg 初始化失败 | `dmesg \| tail -30` 看 `VTL:` 行；把日志附到 issue |

`install.sh` 会把 `vtl.ko` 安装到 `/lib/modules/$(uname -r)/extra/vtl.ko` 并 `depmod`，`vtl-kernelctl` 在 `insmod` 失败时会尝试 `modprobe vtl`。

### Web 登录「用户名或密码错误」

默认用户 **`admin`**，初始密码 **`4rfVBNji9`**（登录页有说明）。须正确填写**算术验证码**（错误时提示「验证码错误或已过期」）。

若仍无法登录，多为 **`web_admin.json` 已存在且密码已改过**，或 `log_dir` 指向旧目录：

```bash
grep '^log_dir=' /opt/vtladm/var/vtl.conf
ls -la "$(grep '^log_dir=' /opt/vtladm/var/vtl.conf | cut -d= -f2)/web_admin.json"
/opt/vtladm/bin/vtladm reset-web-auth
systemctl restart vtladm-web.service
```

再用 **admin** / **4rfVBNji9** 登录并尽快改密。

### 手工安装（旧步骤）

### 1. 加载内核模块

```bash
cd kernel
sudo insmod vtl.ko
lsmod | grep vtl
dmesg | tail -30
```

**勿**对运行中的内核使用 **`insmod -f`** 强塞与 **`uname -r` 不匹配** 的 `vtl.ko`（极易 **panic / 重启**）。若使用 **`vtl.conf`** 的 **`kernel_vtl_reload_script`**，见 **`userspace/scripts/vtl-kernel-reload.sh`**（默认普通 `insmod`；可选 **`VTL_INSMOD_FORCE=1`**）及 **`userspace/docs/SCSI.md`** §1c。

### 2. 安装 `vtladm` 与 `vtladm-iscsi`

```bash
cd ../userspace
sudo cp target/release/vtladm /usr/local/bin/
sudo chmod 755 /usr/local/bin/vtladm
sudo cp target/release/vtladm-iscsi /usr/local/bin/
sudo chmod 755 /usr/local/bin/vtladm-iscsi
```

`vtladm-iscsi` 仅在 **Linux** 上与 `targetcli` 配合使用，用于简化 iSCSI FILEIO 导出，见 `userspace/docs/VTLADM-ISCSI.md`。

### 3. 数据目录

```bash
sudo mkdir -p /opt/vtladm/var/tapes /opt/vtladm/var/log/vtl
sudo chmod 755 /opt/vtladm/var
```

首次使用前生成配置：

```bash
sudo vtladm init-config
```

编辑 **`/opt/vtladm/var/vtl.conf`**（默认主配置；可用环境变量 **`VTL_CONF_PATH`** 覆盖）可调整 `db_path`、`tape_dir`、`log_dir`。也可用 `VTL_DB_PATH`、`VTL_TAPE_DIR`、`VTL_LOG_DIR`（测试或非默认路径）。

卸载（保留数据）：

```bash
sudo sh uninstall.sh
# 连库/磁带/日志一起删：
sudo sh uninstall.sh --purge
```

**重要**：在 **LIO/iSCSI 仍导出** 或 **备份仍占用** `/dev/sg*`/`st*` 时执行卸载并 **`rmmod vtl`**，在麒麟/openEuler 上可能 **内核 panic → kdump 自动重启**（SSH 会显示 `Software caused connection abort`）。卸载脚本会：

1. `systemctl stop` 相关服务  
2. **`targetcli clearconfig`**（失败则**中止**，除非 `VTL_FORCE_RMMOD=1`）  
3. **`fuser`** / LIO 检查 → sysfs 删除 VTL 设备 → 等待多 host 收尾 → **`vtl-kernelctl stop`**

维护窗口正确顺序：

```bash
# 先停备份与 iSCSI
targetcli clearconfig confirm=true
lsscsi -g | grep -i VTL    # 应无输出或已无占用
fuser -v /dev/sg* /dev/st* 2>/dev/null | grep -i vtl || true

sudo sh uninstall.sh --purge
```

若仍 panic，用 kdump 分析：`grep -E 'Oops|panic|vtl|Call Trace' /var/crash/*/vmcore-dmesg.txt | tail -80`

## 磁带文件布局

配置项 `tape_dir` 为根目录时，镜像路径为：

`<tape_dir>/<库目录名>/<磁带名>.vtltape`

其中「库目录名」由库名做安全化（字母、数字、`-`、`_` 保留，其余替换为 `_`）。例如库 `marstor` 对应子目录 `marstor`。

## 使用示例

### 虚拟库与磁带架

所有需区分库的操作可加 **`-L` / `--library <name>`**；省略 `-L` 时使用第一个在线库（不含 `__offline__` / 测试名 `default`）。须先 **`vtladm library create NAME --drives N --slots M`**。

```bash
# 列出已有库
vtladm library list

# 新建库（每库驱动器 ≤8，数据槽 ≤256，在线库总数 ≤8）
vtladm library create backup_site --drives 4 --slots 32

# 等价快捷方式（默认槽位/驱动器见 help）
vtladm create-library --name backup_site --drives 4 --slots 32

# 在指定库中建带（默认在 unused 架）
vtladm -L backup_site create-tape weekly-01 --size 500G

# 自建磁带架并将磁带创建到该架
vtladm -L backup_site shelf create vault
vtladm -L backup_site create-tape weekly-02 --size 500G --shelf vault

# 将架上的磁带放入机械手槽位后再加载
vtladm -L backup_site assign-slot weekly-01 slot0
vtladm -L backup_site load slot0 drive0
vtladm -L backup_site unload drive0

# 删除磁带 / 初始化磁带（空白带；须先在货架上且未在槽位内、不得在驱动中）
vtladm -L backup_site delete-tape weekly-02
vtladm -L backup_site shelf place weekly-01
vtladm -L backup_site init-tape weekly-01

# 删除自建货架（架上须无磁带；不可删默认 unused 架）
vtladm -L backup_site shelf delete vault

# 删除命名在线库（须至少保留一个在线库；不可删 __offline__）。若出现磁盘清理警告，请按提示检查 tape_dir
vtladm library delete backup_site
```

### 查看状态

```bash
vtladm status
vtladm inventory
```

### 创建与导入磁带

```bash
# 默认库、默认大小单位等与 CLI 帮助一致
vtladm create-tape mytape0 --size 100M
vtladm create-tape mytape1 --size 1G

# 导入已有 .vtltape 到槽位（路径为实际文件）
vtladm import /path/to/existing.vtltape slot0
```

### SCSI 与磁带机操作

加载 `vtl.ko` 后，**同一 SCSI host 上**会出现 **LUN 0（机械手）** 与 **LUN 1…N（各磁带驱动器）**；`lsscsi -g` 中应能看到多个 LUN。磁带顺序读写请使用 **LUN ≥ 1** 对应的 `/dev/st*` 或 `/dev/sg*`；机械手操作用 **LUN 0**。详见 `userspace/docs/SCSI.md`。

可选模块参数（根因培训或压测节奏）：

```bash
sudo modprobe vtl move_delay_ms=50   # 或 insmod 后: echo 50 | sudo tee /sys/module/vtl/parameters/move_delay_ms
```

```bash
lsscsi -g
cat /proc/scsi/scsi

sudo mt -f /dev/st0 status
sudo tar cvf /dev/st0 /path/to/files
sudo tar xvf /dev/st0
```

**说明**：具体 `/dev/st*` 编号取决于扫描顺序；多驱库请按 LUN 选择设备，勿假定总是 `st0` 对应第一台驱动器。

### 通过 sysfs 创建磁带（内核侧）

```bash
echo "mytape2 536870912" | sudo tee /sys/kernel/vtl/create_tape
```

（与 `vtladm` 数据库元数据是两条路径，生产环境建议以 `vtladm` 为准并保持一致。）

## Web 界面（须登录）

```bash
vtladm serve --host 127.0.0.1 --port 8765
```

浏览器打开提示的 URL：先访问 **`/login`** 完成验证码登录；之后可打开 **`/`** 首页（侧栏含磁带列表、库状态、传输、后台概览等）及 **`/admin/overview`** 起的管理分页；**`/admin/tapes`** 可对磁带**初始化/删除**，**`/admin/library`** 可**删除在线库**（须确认库名），**`/admin/shelf`** 可删除在线库或离线区的**自建货架**。旧路径 `/admin/libraries` 会重定向到 **`/admin/library`**，`/admin/slots` 重定向到 **`/admin/assign-slot`**。`/admin` 会重定向到概览。详见根目录 [README.md](README.md)「Web 管理」。

首次运行会在 **`VTL_LOG_DIR`（或配置 `log_dir`）下生成 `web_admin.json`**（默认用户 `admin`）。**请尽快修改默认密码**。默认仅监听回环，勿在未加固网络下对 `0.0.0.0` 监听。

## 传输模式（SCSI / FC / iSCSI）

```bash
vtladm transport show
vtladm transport check
vtladm transport guide
```

`vtladm` **不内置** iSCSI/FC Target；`transport` 配置与上述命令用于文档化与运维检查。详见 `userspace/docs/TRANSPORT.md`。

## 测试

在源码树中：

```bash
cd userspace
cargo test
cargo test --bin vtladm
cargo test --bin vtladm-iscsi
```

完整用例表与 CI 建议见仓库根目录 [TEST.md](TEST.md)。

## 卸载

### 卸载内核模块

```bash
sudo rmmod vtl
```

### 清理构建产物

```bash
cd kernel && make clean
cd ../userspace && cargo clean
```

## 故障排除

### 模块加载失败

```bash
uname -r
dmesg | tail -50
```

确认内核版本在支持范围内，且已安装对应 `kernel-devel` / headers。

### 设备未出现

将 `hostX` 换成本机 SCSI host 编号：

```bash
echo "- - -" | sudo tee /sys/class/scsi_host/hostX/scan
```

### `vtladm` 报库/架错误

- **`Library not found`**：先用 `vtladm library list` 确认库名，或 `library create` 创建。  
- **`Shelf not found`**：用 `vtladm shelf list` 查看；自定义架需 `shelf create`。  
- **`assign-slot` 失败**：磁带须已在架上且不在驱动器/槽位中；若在驱动器内请先 `unload`。
