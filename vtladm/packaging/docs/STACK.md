# VTL 栈：vtl.ko、lsscsi 与 Web UI 的关系

## 三层分工

```
┌─────────────────────────────────────────────────────────────────┐
│  Web UI / vtladm CLI（用户态）                                     │
│  SQLite 库/磁带元数据、ioctl 对齐几何、日志                        │
│  路径：/opt/vtladm/var/vtl.conf、vtl.db、tapes/                     │
└───────────────────────────┬─────────────────────────────────────┘
                            │ /dev/vtl  ioctl（SET_INSTANCES / RESIZE_GEOMETRY）
                            │ 方案 B：insmod 满配 + resize；legacy：SET_INSTANCES
                            ▼
┌─────────────────────────────────────────────────────────────────┐
│  vtl.ko（内核）                                                    │
│  注册 SCSI host：每库 1×changer(LUN0) + N×tape(LUN1…)              │
│  sysfs：/sys/module/vtl/parameters/vtl_instances（ioctl 成功后同步）│
└───────────────────────────┬─────────────────────────────────────┘
                            │ scsi_add_host / scan
                            ▼
┌─────────────────────────────────────────────────────────────────┐
│  Linux SCSI 总线（备份软件 / lsscsi 所见）                          │
│  /dev/sch* 机械手  /dev/st* 磁带  /dev/sg* 通用 SCSI               │
│  巡检：lsscsi -g | grep VTL                                       │
└─────────────────────────────────────────────────────────────────┘
```

| 层 | 看什么 | 谁维护 |
|----|--------|--------|
| **DB / UI** | 库名、驱数、槽位、磁带文件 | `vtladm`、Web |
| **内核** | 几个 SCSI host、每 host 几台驱 | `vtl.ko` 参数或 ioctl（**fixed**：8 host 满配 + resize） |
| **总线** | `lsscsi` 行、`/dev/sg*` | 内核扫描完成后自动出现 |

**权威核对**：改库或 ioctl 之后以 **`lsscsi -g`** 为准；**`/sys/module/vtl/parameters/vtl_instances`** 在较新 `vtl.ko` 上与 ioctl 结果一致；**`.last_vtl_instances_spec`** 是 vtladm 上次成功应用的缓存。

## /opt/vtladm 安装布局

```
/opt/vtladm/
  bin/vtladm
  bin/vtladm-iscsi
  ko/vtl.ko              # 安装时针对 uname -r 编译
  sbin/vtl-kernelctl     # start|stop|status|reload
  sbin/vtladm-web-serve  # systemd 用
  scripts/               # 稳定性、巡检、rescan、诊断…
  docs/STACK.md
/etc/default/vtladm      # 模块参数、Web 绑定地址
/opt/vtladm/var/         # 运行时数据（vtl.conf、db、tapes、log；首次 init-config 或 Web 向导）
```

## systemd 单元

| 单元 | 作用 |
|------|------|
| `vtl-kernel.service` | `insmod` / `rmmod`（停前检查 VTL sg 占用） |
| `vtladm-web.service` | `vtladm serve`（默认 `0.0.0.0:8765`） |
| `vtl-patrol.timer` | 每小时 `vtl-patrol.sh` 巡检 |
| `vtl-robot-sync.timer` | 每 5 分钟 `robot sync-db`（仅当 `robot_authority=kernel` 且 `auto_sync_db_from_kernel!=false`；`install.sh --enable` 按配置启用） |

## 巡检（patrol 模块）

实现：`userspace/src/patrol.rs`（`vtladm patrol`）。`vtl-patrol.sh` 仅转发到该命令。

```bash
vtladm patrol
# 或
/opt/vtladm/scripts/vtl-patrol.sh
```

退出码：`0` 正常（默认含 WARN 仍为 0），`1` 警告（仅 `VTL_PATROL_STRICT=1`），`2` 严重。详见 `userspace/docs/PATROL.md`。

巡检内嵌 **自动 inventory 同步**（与 `vtl-robot-sync.timer` 相同逻辑，周期不同）。

## iSCSI（可选，与上表独立）

`vtladm-iscsi library-export` 通过 **LIO pscsi** 再导出 `/dev/sg*`；许多环境（如部分麒麟）**无法**对 VTL 模拟 sg 做 pscsi。本机使用只需 **vtl.ko + lsscsi + vtladm**，不必经过 iSCSI。

详见 `userspace/docs/SCSI.md`（§1g 方案 B）、`userspace/docs/VTLADM-ISCSI.md`。

**openEuler + iSCSI 联调**：按阶段清单见 [INTEGRATION-TEST.md](INTEGRATION-TEST.md)。

**机械手 DB↔内核联动**：见 [ROBOT-SYNC.md](../../userspace/docs/ROBOT-SYNC.md)（安装后 `/opt/vtladm/docs/ROBOT-SYNC.md`）。
