# Web 管理分层：从建库到备份软件存储层

`vtladm serve` 的 Web/CLI 面向 **磁带库服务器**：由本机完成 **库与磁带元数据**、**内核 SCSI 带库语义**，再经可选 **iSCSI / FC** 把同一套 SCSI 设备呈现给 **备份软件（Initiator）** 作为 **存储层**。各层职责如下。

## 四层模型

```
┌─────────────────────────────────────────────────────────────┐
│ ④ 备份软件（NetBackup / Veeam / 国产备份等）                  │
│    发现 SCSI 带库 · 机械手换带 · 读写虚拟磁带                  │
└───────────────────────────────▲─────────────────────────────┘
                                │ SCSI（本地 / iSCSI / FC）
┌───────────────────────────────┴─────────────────────────────┐
│ ③ 传输链路（Web：传输向导 / iSCSI 页）                        │
│    local：本机 /dev/st*、/dev/sg*                              │
│    iscsi：LIO pscsi + library-export（vtladm-iscsi）           │
│    fc：部署意图 + 集成指引（系统级 FC target）                  │
└───────────────────────────────▲─────────────────────────────┘
                                │ 内核枚举
┌───────────────────────────────┴─────────────────────────────┐
│ ② 磁带与槽位（Web：磁带与货架 · 入槽 · 出库 · 对账）          │
│    .vtltape 镜像 · 槽位/驱动器/货架 · DB 元数据                │
└───────────────────────────────▲─────────────────────────────┘
                                │ 库几何
┌───────────────────────────────┴─────────────────────────────┐
│ ① 磁带库（Web：磁带库页）                                      │
│    library create · max_drives/slots · 对齐 vtl.ko 几何        │
└─────────────────────────────────────────────────────────────┘
```

| 层 | Web 入口 | 做什么 | 不做什么 |
|----|----------|--------|----------|
| **① 磁带库** | `/admin/library` | 建库、改驱动/槽位数、删库；自动 ioctl + **SCSI scan**（本机 `lsscsi`） | 不代替备份软件做换带；**不等同** iSCSI LUN 映射 |
| **② 磁带与存储内容** | `/admin/tapes`、`/admin/assign-slot`、`/admin/shelf*`、`/admin/changer` | 建磁带、入槽/出库、货架、DB↔内核 inventory 对账 | 不直接写 LIO/targetcli |
| **③ 传输链路** | `/admin/transport`、`/admin/iscsi`、`/browse/fabric` | 声明 `transport`；iSCSI **library-export/unexport**（记录入库、按库一键卸除）；FC 指引 | 不实现 FC/iSCSI 协议栈本身 |
| **④ 备份软件** | （系统外） | 连 IQN/本机 sg，当物理带库用 | — |

协议关系：**SCSI = 磁带/机械手设备模型**；**iSCSI / FC = 可选网络承载**，见 [TRANSPORT.md](TRANSPORT.md)。

## 推荐操作顺序（新库上线）

1. **内核**：`vtl-kernelctl start` 或 `insmod`，`lsscsi -g` 可见 VTL（见 [SCSI.md](SCSI.md)）。
2. **① 建库**：Web **磁带库** → 创建在线库（驱动数、槽位数与规划一致）。
3. **② 建带**：**磁带与货架** → 批量创建 `.vtltape`；需要时 **磁带入槽**。
4. **③ 暴露给备份机**：
   - 备份机与本机同台或 SAN 本地 SCSI：通常 **`transport=local`**，备份软件扫本机 `/dev/sg*`。
   - 备份机经以太网：**`/admin/iscsi`** → 扫描 sg → **library-export**（非 dry-run 须允许执行）；导出参数写入 DB，下次自动回填。
   - FC：按 [TRANSPORT.md](TRANSPORT.md) 在系统侧配 target，`vtl.conf` 中 `transport=fc` 仅作意图声明。
5. **④ 备份软件**：添加带库/存储单元，指向 IQN 或本机设备；换带、备份任务在备份侧完成。
6. **运维**：搬带后与 **inventory 对账**（`/admin/changer`）；改几何优先 ioctl，避免随意 `rmmod`（见 SCSI §1c）。

## 自动巡检（`vtladm patrol` / `vtl-patrol.timer`）

每小时或手工执行 **`vtladm patrol`**（Web：**传输向导** →「运行巡检」，或 `GET /api/patrol`）会检查：

| 类别 | 内容 |
|------|------|
| 内核 | `vtl.ko`、`/dev/vtl`、`lsscsi` VTL 行 |
| 数据库 | SQLite 可打开、在线库数量、`iscsi_library_exports` |
| 几何对齐 | DB `vtl_instances` 规格 vs sysfs |
| 传输 | `transport=local/iscsi/fc` 与 configfs / FC sysfs / targetcli |
| iSCSI 一致性 | DB 记录 IQN、pscsi 前缀、`/dev/sg` 是否在 LIO / `lsscsi` 中 |
| 机械手 | 内核真相；Web/CLI ioctl 入槽；sync-db / reconcile --pull |

环境变量：

- **`VTL_PATROL_STRICT=1`**：有 WARN 时退出码 1（便于监控）。
- **`VTL_PATROL_CLEAR_STALE_ISCSI_DB=1`**：若 DB 有导出记录但 LIO 无对应 IQN，**自动删除**该 DB 行（不删 LIO 对象）。

`vtl-kernelctl stop` 在 fuser 通过后会对 VTL 行执行 **scsi delete**，并提示先 **library-unexport** 若 LIO 仍引用 sg。

## 配置与持久化

| 数据 | 位置 |
|------|------|
| 库/磁带/槽位/驱动器 | SQLite（`db_path`） |
| 磁带镜像 | `<tape_dir>/<库名>/*.vtltape`（库名仅 `[A-Za-z0-9_-]+`；批量名为 `{库名}_tapeNN`） |
| 内核可见路径 | `<tape_dir>/{磁带名}.vtltape`（`robot sync` 前自动 symlink；亦可 `vtl-link-kernel-tapes`） |
| iSCSI 上次成功导出（IQN、sg、LUN 等） | SQLite `iscsi_library_exports` |
| 部署意图（transport、门户、IQN 模板） | `vtl.conf` |
| 实际 LIO 对象 | 内核 configfs + 可选 `/etc/target/saveconfig.json` |

## 相关文档

- [TRANSPORT.md](TRANSPORT.md) — SCSI / iSCSI / FC 职责
- [VTLADM-ISCSI.md](VTLADM-ISCSI.md) — library-export、DB 记录
- [SCSI.md](SCSI.md) — 内核 LUN、ioctl、重载风险
- [TAPE-LIBRARY.md](TAPE-LIBRARY.md) — 与物理带库能力对照
- [ARCHITECTURE-UI-DB-KERNEL.md](ARCHITECTURE-UI-DB-KERNEL.md) — UI/DB/内核与 mhVTL 对照  
- [ROBOT-SYNC.md](ROBOT-SYNC.md) — 机械手单一路径、对账
