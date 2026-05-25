# 虚拟磁带库 (VTL)

一个基于 Linux 内核的虚拟磁带库实现，支持内核 **4.18–6.10**（Kylin / openEuler / RHEL 等；每机按 `uname -r` 编译模块）。兼容说明见 **[packaging/docs/KERNEL-COMPAT.md](packaging/docs/KERNEL-COMPAT.md)**。

## 架构

```
┌─────────────────┐      ┌──────────────────┐      ┌───────────────────┐
│  用户态 vtladm  │      │ 内核 vtl.ko       │      │   磁带镜像        │
│ CLI + Web       │◄────►│ SCSI: ch + st    │◄────►│ *.vtltape         │
│ SQLite 目录     │ ioctl│ 机械手现场状态    │ 块I/O │ tape_dir/         │
│ assign/load     │      │ INQUIRY/MOVE/…   │      │                   │
└────────┬────────┘      └────────▲─────────┘      └───────────────────┘
         │                          │
         │   备份软件 ──iSCSI/FC──►│  /dev/sg* /dev/st*（LIO pscsi 导出）
         └──────────────────────────┘
```

运行时机械手真相在 **vtl.ko**；SQLite 为磁带目录（`slots.tape_id` 仅作镜像/对账）。详见 [userspace/docs/ARCHITECTURE-UI-DB-KERNEL.md](userspace/docs/ARCHITECTURE-UI-DB-KERNEL.md)。

## 物理磁带库背景（延伸阅读）

实体磁带库在大容量备份、长期归档、合规与气隙等方面的常见能力与典型用法，已整理为独立说明，便于对照本仓库的**虚拟库 / 机械手 / 槽位与驱动器**等概念：

**[userspace/docs/TAPE-LIBRARY.md](userspace/docs/TAPE-LIBRARY.md)**

其中也标明了**软件 VTL 与物理库、备份软件栈**之间的能力边界，避免将营销语境下的磁带库特性默认等同于当前模拟实现。

## 项目结构

```
vtladm/
├── kernel/           # Linux 内核模块 (C)
│   ├── include/      # 内核头文件
│   ├── src/          # 内核源代码
│   │   ├── vtl_main.c    # 模块初始化
│   │   ├── vtl_scsi.c    # SCSI 命令处理
│   │   ├── vtl_tape.c    # 磁带设备操作
│   │   ├── vtl_misc.c    # 杂项操作
│   │   └── vtl_sysfs.c   # sysfs 接口
│   ├── Kbuild        # 内核构建文件
│   └── Makefile      # 构建脚本
├── userspace/        # 用户态工具 (Rust)
│   ├── src/
│   │   ├── main.rs
│   │   ├── fab_transport.rs  # 传输模式与检查
│   │   ├── web.rs            # Web：登录后页面与 API；验证码 / iSCSI 向导
│   │   ├── web_auth.rs       # Web 登录：验证码、会话、bcrypt 凭据文件
│   │   └── bin/
│   │       └── vtladm_iscsi.rs  # vtladm-iscsi 二进制
│   ├── docs/
│   │   ├── TAPE-LIBRARY.md   # 物理磁带库能力与典型场景（理解 VTL 语义背景）
│   │   ├── SCSI.md           # 内核 vtl：LUN 划分与 SCSI 命令矩阵
│   │   ├── TRANSPORT.md      # SCSI / FC / iSCSI 部署说明
│   │   └── VTLADM-ISCSI.md   # vtladm-iscsi（targetcli 封装）
│   ├── Cargo.toml
│   └── TESTS.md
├── install.sh        # 编译并安装到 /opt/vtladm + systemd
├── packaging/        # systemd 单元、vtl-kernelctl、巡检脚本
├── README.md         # 本文档
├── INSTALL.md        # 安装与使用示例
├── TEST.md           # 测试说明（完整）
└── docs/             # 其他文档
```

## 功能特性

### 磁带管理

- 创建/删除虚拟磁带，支持多种大小单位 (B/K/M/G/T)
- **`init-tape`**：将磁带恢复为空白（`used_bytes` 置 0，镜像截断为标称容量）；磁带须**在货架上**（未在机械手槽内）、**不得在驱动中**。与入槽/回架一致：在驱动内报错为 **`TapeInDrive`**，未在货架（含仅在槽内）为 **`TapeNotOnShelf`**；成功路径先截断并尽量 `sync` 再写库，若写库失败会尽力恢复镜像长度
- 自动生成唯一条形码；快照与回滚
- **命名虚拟库**：同一数据库中可有多套库，槽位/驱动器/磁带元数据按 `library_id` 隔离；**`library delete`** 可删除整库（不可删 `__offline__`，且须至少保留一个在线库）。若磁盘上个别镜像或目录删除失败，命令仍可能成功结束并打印 **Warning**；Web API `POST /api/manage/library/delete` 成功时**始终**包含 `file_warnings` 数组（无警告时为空 `[]`），请按提示手工清理并查看日志
- **磁带架（shelf）**：每库自带默认「未使用」架 `unused`；可自建磁带架；**`shelf delete`** 可删除非默认架（架上须无磁带）

### 机械手控制

- 槽位到驱动器的磁带加载、驱动器到槽位的卸载
- 邮箱槽（导入/导出）
- **`assign-slot`** / **批量入槽**：将已在架上的磁带移入指定槽位（须不在驱动器内）；批量为**逐条**提交，中途失败时前面已成功的不会自动回滚，请核对后再重试剩余项
- **`robot_sync`**（默认开启）：`load` / `unload` / `assign-slot` 后通过 **`/dev/vtl` ioctl** 同步内核机械手，使 `/dev/st*` 对备份软件 Ready（类 mhVTL）；详见 **[userspace/docs/ROBOT-SYNC.md](userspace/docs/ROBOT-SYNC.md)**。手工对齐：**`vtladm -L <lib> robot sync`**

### 库配置

- 可配置驱动器和槽位数量（按库）
- 全局配置与 **按库** 配置/配额（SQLite：`config` 与 `library_config`）
- 运行时通过 CLI 更新

### 改库与内核模块（与 mhVTL 一致 + 自动 ioctl）

- **产品上限**：**8** 库 × **8** 驱 × **256** 槽（SCSI host 段数上限 **8**）。
- **方案 B（推荐）**：`kernel_geometry_mode=fixed` 时 **`vtl-kernelctl start`** 以 **`8x256` × 8** + **`noscan=1`** 加载；日常改驱/槽用 **`VTL_IOCTL_RESIZE_GEOMETRY`**（**`vtladm kernel-align`**），空闲 host **保持满配几何**（不缩成 `1x1`）。见 **[userspace/docs/SCSI.md](userspace/docs/SCSI.md)** §1g。
- **`kernel_reload_on_db_change` 默认为 `false`**：改库后**不执行**整模块重载脚本（**`rmmod`/`insmod`**），避免你遇到的整机重启风险。
- **`kernel_geom_prefer_ioctl` 默认为 `true`**：改库后 **`vtladm` 仍会自动尝试** **`/dev/vtl` ioctl** 对齐 **`vtl_instances`**（无整模块卸载）；**无需**把 `kernel_reload_on_db_change` 设为 `true` 即可获得「自动对齐」。ioctl 失败或内核模块过旧时，再设 **`kernel_reload_on_db_change=true`** 并配置 **`kernel_vtl_reload_script`** 作为回退，或维护窗口手工重载。
- **规格未变时的 mhVTL 类比**：若 DB 生成的 **`vtl_instances` 串**与 **`/var/lib/vtl/.last_vtl_instances_spec`** 一致，**只**对 **`vtl`** SCSI host 写 **`scan`** 刷新总线（不重跑 ioctl / 整模块脚本）；**新增库段 / 改 `NxM`** 仍须 ioctl 或重载。详见 **[userspace/docs/SCSI.md](userspace/docs/SCSI.md)** §1f。
- 若需**改库即自动跑脚本**（ioctl 失败时），在 **`vtl.conf`** 设 **`kernel_reload_on_db_change=true`**（或 **`VTL_KERNEL_RELOAD_ON_DB_CHANGE=1`**），并保留 **`kernel_geom_prefer_ioctl=true`** + 带 ioctl 的 **`vtl.ko`** 以减少整模块重载。详见 **[userspace/docs/SCSI.md](userspace/docs/SCSI.md)** §1c、§1e。

### SCSI 命令支持（内核 `vtl.ko`）

- **LUN 0**：机械手（`MOVE_MEDIUM`、`READ_ELEMENT_STATUS` 等）；**LUN 1…N**：各磁带驱动器（`READ`/`WRITE` 6/10/12、`REWIND`、`SPACE`、`LOG SENSE` 页 `0x00`/`0x11`、`READ POSITION`、`PREVENT ALLOW MEDIUM REMOVAL` 等）。完整矩阵与编号约定见 **[userspace/docs/SCSI.md](userspace/docs/SCSI.md)**。
- **LUN 越界**：`CHECK CONDITION`，`ILLEGAL REQUEST` / ASC **`0x25`** / ASCQ **`0x00`**（*logical unit not supported*）。`MODE SENSE(6|10)`、`LOG SENSE(6|10)` 的分配长度按 CDB 长度解析；单次 READ/WRITE 传输字节上限见 `SCSI.md`。
- 磁带元数据内维护 **读写字节计数与装入次数**（供 `LOG SENSE` 展示；当前不单独持久化到镜像头）。
- 模块参数 **`move_delay_ms`**：每次成功的 `MOVE MEDIUM` 后可选睡眠若干毫秒（培训 / 压测节奏，默认 0）。

### 网络传输（目标端：SCSI / iSCSI / FC）

- **产品目标**：VTL 作为 **目标端** 收数据，在行为上 **模仿物理磁带库**；协议上需要 **SCSI（磁带/机械手语义）**，并可选通过 **iSCSI** 或 **FC** 把 SCSI 承载到网络（三者关系与职责划分见 [userspace/docs/TRANSPORT.md](userspace/docs/TRANSPORT.md)）。
- **本仓库当前不包含** 内置的 iSCSI Target、FC Target，也不保证「把本机 `/dev/st*` 一键变成网络上 SCSI tape」；**磁带类 SCSI 语义**主要由 **内核 `vtl.ko` + 本机 SCSI 栈** 提供。
- 本机路径：加载内核 `vtl.ko` 后，由 **本地 SCSI 子系统** 枚举磁带/机械手设备。
- `vtl.conf` 中的 **`transport`**（`local` / `iscsi` / `fc`）用于声明部署意图；`vtladm transport show|check|guide` 给出说明与本机环境线索；**网络上的 FC/iSCSI 暴露**需在系统上另行部署 Linux-IO、TGT、FC target 等；Linux 上可选用同目录 **`vtladm-iscsi`** 辅助 `targetcli`（见上文 Web 节与 [VTLADM-ISCSI.md](userspace/docs/VTLADM-ISCSI.md)）。

### Web 管理（须登录）

- **分层职责**（详见 [userspace/docs/WEB-WORKFLOW.md](userspace/docs/WEB-WORKFLOW.md)）：**① 磁带库**（建库/几何）→ **② 磁带与槽位**（建带、入槽、对账）→ **③ 传输**（本机 SCSI / iSCSI / FC 指引与 `library-export`）→ **④ 备份软件** 将 VTL 作为存储层。Web 入口 **`/admin/transport`** 汇总传输向导。
- `vtladm serve --host 127.0.0.1 --port 8765` 启动内置 HTTP：**除** `/login`、`GET /api/captcha`、`POST /api/login` 与 `POST /api/logout` 外，**所有页面与数据 API 均须有效会话**；未登录访问页面会重定向到 `/login`，未登录调用受保护 API 返回 **401**。登录后可通过首页侧栏进入磁带列表、库状态、传输配置，以及 **`/admin/overview`** 起的后台分页（账户、**`/admin/tapes`** 磁带与货架（批量建带、迁移、**初始化/删除磁带**）、**`/admin/library`**（建库与**删除在线库**）、**`/admin/shelf`**（离线新建货架与**删除在线/离线货架**）、磁带入槽/回架、iSCSI 等），并可在允许时调用同目录 `vtladm-iscsi`（镜像路径须在配置的 `tape_dir` 下，真正执行需凭据 JSON 中 `allow_iscsi_exec: true`）。**`/admin/iscsi`** 提供 Target 向导：`GET /api/manage/iscsi/config` 加载推荐值、`POST /api/manage/iscsi/check` 自检 `targetcli`（均须登录；**不**需要 `allow_iscsi_exec`），再调用既有 `quick-export` / `quick-unexport` API；自动化用例见根目录 [TEST.md](TEST.md) §3.2。
- 首次启动在 **`{log_dir}/web_admin.json`** 写入默认用户 **`admin`** 与 bcrypt 哈希（初始口令见 `web_auth.rs` 中 `DEFAULT_WEB_PASSWORD`，**务必尽快在管理页修改**）。算术验证码通过 `GET /api/captcha` + `POST /api/login`（`HttpOnly` cookie）完成。
- **默认仅本机监听**，勿在未加固网络下对 `0.0.0.0` 暴露。
- 构建：若使用较旧的发行版 **Cargo（如 1.82）**，请保留 **`userspace/Cargo.toml`** 中对 `uuid`、`time` 等钉版本；建议在 `userspace` 目录提交 **`Cargo.lock`** 并使用 **`cargo build --release --locked`**。若 **`cargo`** 报 **`rsproxy.cn`** / **`Could not resolve host`** 等索引错误，多为本机 **`~/.cargo/config.toml`** 镜像源不可达，见根目录 [TEST.md](TEST.md) **§9.1**。若 **`rustup-init`** 无法访问 **`static.rust-lang.org`**，见 **§9.2**（镜像 / 离线 / 发行版包 / 仅部署二进制）。

### 日志轮转

- `log_max_bytes`（或环境变量 `VTL_LOG_MAX_BYTES`，最小 4096）控制单文件大小；超过后将当前文件轮转为 `vtladm.log.1` … `.5` 后重建主日志（`vtladm_errors.log` 同理）。

### 后端存储

- 稀疏文件；磁带镜像路径：`<tape_dir>/<库目录名>/<磁带名>.vtltape`（库目录名由库名安全化得到）

## 全局 CLI 选项

| 选项 | 说明 |
|------|------|
| `-L`, `--library <name>` | 当前命令作用的在线库名；省略时使用第一个在线库（不含 `__offline__` / 遗留名 `default`） |

## 快速开始

### 构建

**说明**：`vtladm` / `vtladm-iscsi` 的 **Rust 编译与运行以 Linux 为目标**（生产、CI、内网构建机）；在 Windows 上可只改代码与文档，**不必**在本机跑通 `cargo`（若本机无工具链或网络策略不同，属正常）。

**工具链**：当前依赖（如 **`axum` 0.7.5**）声明的 **最低 rustc 为 1.66**；实际建议 **rustc / Cargo ≥ 1.82**（与常见发行版、旧索引钉版本等说明一致）。**Cargo 1.51（约 rustc 1.51）过旧，无法解析或编译本仓库**，请在 Linux 构建环境升级编译器后再执行 `cargo build`。

```bash
# 用户态工具（生成 vtladm 与 vtladm-iscsi）
cd userspace
cargo build --release
# 若已提交 Cargo.lock（推荐 CI / 固定工具链）：
# cargo build --release --locked
# vtladm-iscsi：Linux 上配合 targetcli 一键导出 FILEIO LUN，见 userspace/docs/VTLADM-ISCSI.md

# 内核模块
cd ../kernel
make
```

### 安装

详见 [INSTALL.md](INSTALL.md)。摘要：

```bash
sudo insmod kernel/vtl.ko
sudo cp userspace/target/release/vtladm /usr/local/bin/
sudo mkdir -p /opt/vtladm/var/tapes /opt/vtladm/var/log/vtl
```

### 基本使用

```bash
vtladm init-config

# 创建在线库（须显式命名；不再自动创建 default）
vtladm library create marstor --drives 2 --slots 10

# 在指定库下建带（默认放在 unused 架）
vtladm -L proj_a create-tape backup-01 --size 100G
vtladm -L proj_a shelf create archive
vtladm -L proj_a create-tape backup-02 --size 2.5T --shelf archive

vtladm -L proj_a list-tapes
vtladm -L proj_a assign-slot backup-01 slot0
vtladm -L proj_a load slot0 drive0
vtladm -L proj_a unload drive0

vtladm -L proj_a snapshot backup-01 snap-001
vtladm -L proj_a export slot0 /backup/tape.vtltape
# 可选：生成与 GNU sha256sum 兼容的旁路校验文件
vtladm -L proj_a export slot0 /backup/tape.vtltape --checksum

# 初始化磁带（清空逻辑已用空间，镜像截断为标称容量；磁带不得在驱动中）
vtladm -L proj_a init-tape backup-01

# 删除磁带
vtladm -L proj_a delete-tape backup-02

# 删除自建货架（架上须无磁带；不可删默认 unused 架）
vtladm -L proj_a shelf delete archive

# 删除整个在线库（须至少保留另一个在线库；不可删 __offline__）
vtladm library create old_site --drives 1 --slots 4
vtladm library delete old_site
```

导入外部镜像时，路径可为任意可读 `.vtltape` 文件；导入进槽位后磁带会离开磁带架记录。

## 命令参考（摘要）

| 命令 | 描述 |
|------|------|
| `vtladm create-tape <name> [--size] [--shelf] [--tags...]` | 创建磁带（默认架 `unused`） |
| `vtladm delete-tape <name>` | 删除磁带（`-L` 指定库） |
| `vtladm init-tape <name>` | 初始化磁带：清空已用字节并截断镜像为标称容量（磁带不得在驱动中） |
| `vtladm list-tapes` | 列出当前库的磁带 |
| `vtladm load <slot> <drive>` | 加载 |
| `vtladm unload <drive>` | 卸载 |
| `vtladm eject <slot>` | 弹入邮箱槽 |
| `vtladm assign-slot <tape> <slot>` | 从磁带架放入槽位 |
| `vtladm inventory` / `status` | 库存 / 状态 |
| `vtladm snapshot <tape> <name>` | 快照 |
| `vtladm import <path> <slot>` | 导入 |
| `vtladm export <slot> <path> [--checksum]` | 导出槽内镜像；**`--checksum`** 时额外写入 `<path>.sha256`（64 位十六进制摘要 + 文件名，便于 `sha256sum -c`） |
| `vtladm create-library <name> [--drives] [--slots]` | 创建命名在线库 |
| `vtladm library list` / `library create` / **`library delete <name>`** | 列出 / 创建 / **删除**在线库（不可删最后一个在线库，不可删 `__offline__`） |
| `vtladm shelf list` / `create` / `tapes` / `place` / **`delete <name>`** | 磁带架管理（**`delete`**：仅非默认架且架上无磁带） |
| `vtladm config show` / `config set` | 全局配置 |
| `vtladm quota show|set|check` | 配额（当前库） |
| `vtladm tag ...` | 标签子命令 |
| `vtladm search ...` | 条件搜索（当前库） |
| `vtladm init-config` | 初始化配置文件 |
| `vtladm transport show|check|guide` | 传输模式与 FC/iSCSI 集成说明 |
| `vtladm serve` | 启动 Web 界面（`--host` / `--port`）：须登录访问页面与 API；含 iSCSI 向导 |
| `vtladm-iscsi`（独立二进制） | `quick-export` / `quick-unexport` / **`library-export` / `library-unexport`**（pscsi 多 LUN 带库）、`batch` 等，封装 `targetcli`（仅 Linux） |

完整子命令参数以 `vtladm --help` 与各子命令为准。

## 配置文件

默认：`/opt/vtladm/var/vtl.conf`（可用 **`VTL_CONF_PATH`** 覆盖；不设 `VTL_USE_ENV_ONLY=1` 时不会查找 `/etc` 或相对路径下的同名文件。）

```ini
db_path=/opt/vtladm/var/vtl.db
tape_dir=/opt/vtladm/var/tapes
log_dir=/opt/vtladm/var/log/vtl
# log_max_bytes=10485760
# transport=local
```

可通过环境变量 `VTL_DB_PATH`、`VTL_TAPE_DIR`、`VTL_LOG_DIR`、`VTL_LOG_MAX_BYTES`、`VTL_TRANSPORT`、`VTL_ISCSI_IQN`、`VTL_ISCSI_PORTALS`、`VTL_FC_WWPN` 覆盖（便于测试与自动化）。

## 日志

目录：`/opt/vtladm/var/log/vtl/`（或由 `log_dir` / `VTL_LOG_DIR` 指定）

- `vtladm.log` — 操作日志  
- `vtladm_errors.log` — 错误日志  

## 数据库（SQLite 概要）

除全局 `config`、`tags`、`tape_tags` 等外，核心多库相关表包括：

- `vtl_libraries` — 虚拟库
- `shelves` — 每库下的磁带架（含 `is_default_unused` 标记默认 `unused` 架）
- `library_config` — 每库键值配置（含配额等）
- `tapes` — 含 `library_id`、`shelf_id` 等
- `slots` / `drives` — 含 `library_id`，按库隔离

旧库无 `library_id` 列时会自动迁移到 v2 架构。

## 测试

```bash
cd userspace
cargo test                       # 含 vtladm-iscsi 内置单元测试
cargo test --bin vtladm          # 仅主程序
cargo test --bin vtladm-iscsi    # 仅 vtladm-iscsi（IQN/FILEIO 校验等，不调用 targetcli）
```

完整用例清单、编号表、环境变量与 CI 建议见 **[TEST.md](TEST.md)**；`userspace/TESTS.md` 为简短入口。

## 内核模块

提供 SCSI 磁带/机械手模拟、sysfs 等。加载与排查见 [INSTALL.md](INSTALL.md)。

## 故障排除

### 设备未出现

```bash
echo "- - -" | sudo tee /sys/class/scsi_host/hostX/scan
```

### 查看 SCSI 设备

```bash
lsscsi -g
cat /proc/scsi/scsi
```

### 使用磁带设备

```bash
mt -f /dev/st0 status
tar cvf /dev/st0 /path/to/files
tar xvf /dev/st0
```

## 许可证

GPL-2.0
