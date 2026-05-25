# 机械手同步（mhVTL 单一路径）

## 模型

- **运行时机械手**：永远只在 **vtl.ko**（与 `mtx`、备份软件 MOVE 一致）
- **SQLite**：磁带**目录**（名、条码、货架、`tapes.slot` 作提示）
- **已移除**：`robot_authority=kernel|db`、`robot sync`（DB→内核全量）、`reconcile --apply`

## vtl.conf（`init-config` 默认）

```ini
robot_sync=true
auto_reconcile_pull=true
auto_sync_db_from_kernel=true
```

| 键 | 默认 | 含义 |
|----|------|------|
| `robot_sync` | `true` | 启用 `/dev/vtl` ioctl（assign-slot、load、unload、eject） |
| `auto_reconcile_pull` | `true` | auto-align / 漂移时允许 kernel→DB pull |
| `auto_sync_db_from_kernel` | `true` | 启用 `vtl-robot-sync.timer` 与 patrol 的 `sync-db` |

旧键 **`robot_authority`**、**`auto_reconcile_apply`** 在解析时忽略；`vtladm config set` 会报错。

环境变量：`VTL_ROBOT_SYNC=0` 关闭 ioctl；`VTL_AUTO_RECONCILE_PULL` / `VTL_AUTO_SYNC_DB_FROM_KERNEL` 可覆盖 conf。

## 命令

| 命令 | 作用 |
|------|------|
| `vtladm -L <lib> inventory` | 读内核 GET_INVENTORY（`source=kernel`） |
| `vtladm assign-slot` / Web 批量入槽 | 更新目录 + **ioctl** 入槽 |
| `vtladm robot sync-db` | 内核数据槽 → `tapes.slot`（目录提示） |
| `vtladm -L <lib> robot reconcile --pull` | 全量 kernel→DB（`slots`/`drives` 表） |
| `vtladm -L <lib> robot auto-align` | 离架撤出 + 安全时 pull |
| ~~`vtladm robot sync`~~ | **已移除**（原 DB→内核全量） |

## 数据流

```
备份软件 MOVE / vtladm ioctl ──► vtl.ko
                                    ▲ GET_INVENTORY
Web / CLI 库状态 ───────────────────┘
SQLite 目录 ◄── sync-db / reconcile --pull（可选镜像）
```

## systemd

- **`vtl-robot-sync.timer`**：`robot_sync=true` 且 `auto_sync_db_from_kernel!=false` 时每 5 分钟 `robot sync-db`
- **`vtl-patrol.timer`**：同上条件时 patrol 内也跑 `sync-db`

详见 [ARCHITECTURE-UI-DB-KERNEL.md](ARCHITECTURE-UI-DB-KERNEL.md)。
