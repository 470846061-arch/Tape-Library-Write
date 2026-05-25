# UI / DB / 内核 — mhVTL 对照与 vtladm 模型

## 三层对照

| 层 | mhVTL | vtladm |
|----|--------|--------|
| **UI** | `vtlcmd`、GUI → 守护进程 | `vtladm` CLI、Web → SQLite + `/dev/vtl` ioctl |
| **持久配置** | `/etc/mhvtl/*.conf`、`library_contents` | `vtl.conf`、SQLite（库几何、磁带目录、货架） |
| **运行时机械手** | **vtllibrary 进程内存** | **vtl.ko 内核**（与 `mtx`/备份软件一致） |
| **备份软件路径** | SCSI → mhvtl.ko → `/dev/mhvtl` → vtllibrary | SCSI → vtl.ko |

mhVTL 没有「SQLite 槽位表 vs 内存状态」的双重账本。vtladm **不再提供** `robot_authority=kernel|db` 切换。

## 单一模型（测试/生产默认）

| 组件 | 职责 |
|------|------|
| **vtl.ko** | 机械手现场唯一真相（MOVE、READ ELEMENT STATUS、`mtx`） |
| **UI / `vtladm inventory`** | **GET_INVENTORY** 显示槽/驱（与 mhVTL 看「现场」一致） |
| **SQLite** | **磁带目录**：名、条码、容量、货架；**`slots.tape_id` / `drives.tape_id` 不表示现场机械手位置**（展示用 `inventory` 读内核）；`tapes.slot` 仅作目录提示，由 `robot sync-db` / `reconcile --pull` 从内核镜像 |
| **`assign-slot` / load / unload** | 更新目录 + **ioctl** 改内核（等同 `vtlcmd`） |
| **`robot sync-db` / timer** | 把内核**数据槽号**镜像到 `tapes.slot`（目录提示），不推机械手 |
| **`robot reconcile --pull`** | 显式全量写回 `slots`/`drives` 表（运维用） |
| **`robot sync`（DB→内核全量）** | **已移除** |

自动闭环：**备份软件或 vtladm ioctl MOVE → 内核变 → UI 读内核**；可选 `sync-db` 更新目录提示。

## 数据流

```
备份软件 / vtladm ioctl ──MOVE/RES──► /dev/sg* ──► vtl.ko（现场状态）
                              ▲
                              │ GET_INVENTORY
vtladm / Web 库状态 ──────────┘

Web 建带 / 货架 ──► SQLite（目录）
```

## 常见误区

| 误区 | 实际 |
|------|------|
| Web 批量入槽只改 DB | 会 **ioctl** 写入内核；inventory 仍读内核 |
| `sync-db` 会把空槽填满 | 只镜像内核已有槽位到 `tapes.slot` |
| 需要 `robot_authority=db` 才能入槽 | 已删除；默认即内核真相 + ioctl 入槽 |

## 相关命令

```bash
vtladm -L marstor inventory          # source=kernel
vtladm -L marstor list-tapes
vtladm robot sync-db
vtladm -L marstor robot reconcile --pull
```

详见 [ROBOT-SYNC.md](ROBOT-SYNC.md)、[WEB-WORKFLOW.md](WEB-WORKFLOW.md).
