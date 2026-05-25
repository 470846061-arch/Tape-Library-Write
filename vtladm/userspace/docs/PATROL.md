# vtl-patrol / `vtladm patrol`

`vtl-patrol.timer` 定期执行 `vtladm patrol`（或 `vtl-patrol.sh`）。

## 机械手 / inventory

在 `robot_sync=true`、`/dev/vtl` 可用且 `auto_sync_db_from_kernel=true` 时，patrol 会执行 **`robot sync-db`**（内核数据槽 → `tapes.slot` 目录提示；**不**改机械手现场）。

UI / `inventory` 仍读内核 GET_INVENTORY。

## 检查项（摘要）

- vtl.ko 是否加载、`/dev/vtl` 是否存在
- DB 完整性、库几何与内核是否一致
- Web 服务、日志轮转
- 可选：iSCSI / SCSI 扫描提示

配置见 `userspace/docs/ROBOT-SYNC.md`（`robot_sync`、`auto_sync_db_from_kernel`）。
