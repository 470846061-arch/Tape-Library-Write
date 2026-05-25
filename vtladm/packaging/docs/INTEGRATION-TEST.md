# openEuler VTL + iSCSI 集成测试清单

面向 **VTL 服务器（openEuler 6.6 + `/opt/vtladm`）** 与 **iSCSI initiator（如 Kylin/msa）** 的联调。按阶段执行；每阶段通过后再进入下一阶段。

---

## 0. 已知结论（Review 摘要）

| 项 | 结论 |
|----|------|
| **整机重启** | kdump 证实为 **内核 panic**；05:32 栈为 **`postgres` + `semget`**，**无 `vtl_` 调用链**（可能无关，或内存已损坏） |
| **iSCSI** | openEuler 上 **pscsi 导出可成功**；重启后 **LUN/pscsi 会丢**，须 **重新 `library-export`** |
| **遗留名 `default`** | **不再自动创建**；**不参与** `vtl_instances`（仅 `marstor` 等真实在线库进内核规格）。DB 里若仍有旧 `default` 行可保留，但不影响 SCSI 几何 |
| **方案 B（推荐）** | `vtl.conf`：`kernel_geometry_mode=fixed`；`vtl-kernelctl start` → sysfs **`8x256,…`×8** + **`noscan=1`**；改驱/槽 → **`vtladm kernel-align`**（resize ioctl）；扩大驱数后必要时 **`vtl-scsi-scan-all-hosts.sh`** |
| **DB vs 内核** | 推荐 **`robot_authority=kernel`**：备份软件 MOVE 后 **`vtladm robot sync-db`** 或 **`vtl-robot-sync.timer`**（5 分钟）；**勿** `robot sync` / `reconcile --apply`（DB→内核） |
| **tape_dir** | 内核 **`tape_dir`** 须与 `vtl.conf` 一致（`insmod` / sysfs）；镜像在 `<tape_dir>/<名>.vtltape` |
| **mtx** | **可能部分通过**（RES 头、MODE SENSE 0x1D/0x1E、IE 2000+）；仍以 **备份软件机械手** 为准 |
| **Web 库存/对账** | **`/admin/assign-slot`**（入槽）、**`/admin/shelf-place`**（出库）、**`/admin/changer`**（inventory 对账：仅 **`robot reconcile`** / **`auto-align`** pull 写 DB）；**load/unload/eject/robot sync（DB→内核）API 返回 403** |
| **巡检** | **`vtladm patrol`** + **`vtl-patrol.timer`**（1h）；与 robot-sync 分层见 `docs/PATROL.md` |
| **读标签失败** | 驱动器 **未装带** 时正常失败；先装带再读 |
| **错误 LUN3** | 曾有 initiator 访问 **不存在的 LUN3**；仅使用 **lun0/1/2** |

---

## 1. 环境准备（两台机）

### 1.1 VTL 服务器（openEuler）

```bash
# 同步代码后完整安装（勿长期 --no-build，否则缺 reset-web-auth 等）
cd /root/vtladm
sed -i 's/\r$//' install.sh uninstall.sh packaging/sbin/* packaging/scripts/*
sudo sh install.sh --enable

# 方案 B（推荐）：编辑 vtl.conf 启用 fixed 后维护窗口 reload
#   kernel_geometry_mode=fixed
#   kernel_geom_prefer_ioctl=true
#   kernel_reload_on_db_change=false
# sudo /opt/vtladm/sbin/vtl-kernelctl reload
# sudo sh /opt/vtladm/scripts/vtl-scsi-scan-all-hosts.sh 5
# /opt/vtladm/bin/vtladm kernel-align

chronyc makestep 2>/dev/null || true
mkdir -p /var/log/journal && systemctl restart systemd-journald

systemctl status vtl-kernel vtladm-web -l
systemctl status vtl-patrol.timer vtl-robot-sync.timer
/opt/vtladm/bin/vtladm patrol
# 备份软件搬带后（或等待 ≤5 分钟）：
/opt/vtladm/bin/vtladm robot sync-db
```

### 1.2 登录 Web

- URL：`http://<VTL-IP>:8765/login`
- 用户：`admin` / 初始密码见登录页；失败则：`/opt/vtladm/bin/vtladm reset-web-auth`

### 1.3 仅保留 marstor（推荐）

```bash
sqlite3 /opt/vtladm/var/vtl.db "
SELECT id,name,
  (SELECT COUNT(*) FROM drives WHERE library_id=l.id) drv,
  (SELECT COUNT(*) FROM slots WHERE library_id=l.id AND is_import_export=0) slots
FROM vtl_libraries l WHERE name != '__offline__' ORDER BY id;"

# default 不再参与 vtl_instances；仅保留 marstor 等自建在线库即可
```

---

## 2. 阶段 A — 本机 SCSI（无 iSCSI）

| # | 步骤 | 期望 |
|---|------|------|
| A1 | `cat /sys/module/vtl/parameters/vtl_instances` | **legacy**：`2x32`（仅 marstor）；**fixed**：`8x256,…`×8，DB 运行规格见 `vtladm kernel-spec` |
| A2 | `lsscsi -g \| grep -i VTL` | **legacy**：1 host + 2 tape；**fixed**：多 host，仅 marstor 对应 host 显示 DB 驱数 |
| A3 | `fuser -v /dev/st0 /dev/sch0 /dev/sg*` | 无占用 |
| A4 | `vtladm -L marstor config set max_drives=2 slots=32` | **ioctl_ok** / resize；fixed 下 sysfs 仍为满配段 |
| A4b | `vtladm kernel-align`（fixed） | resize 成功；必要时再跑 `vtl-scsi-scan-all-hosts.sh` |
| A5 | 重复 A1–A2 | DB 与 `lsscsi` 一致（fixed：按 host 核对，勿期望 sysfs=2x32） |
| A6 | `vtladm patrol` | exit 0（无 CRIT；默认允许 WARN 仍 exit 0，见 `docs/PATROL.md`） |
| A7 | `vtladm robot sync-db`（`robot_authority=kernel`） | 成功或「无在线库」；**inventory 不 reload ko** |

**若 ioctl 失败**：贴 `dmesg | tail -30`；勿 `rmmod`（有占用时禁止）。

---

## 3. 阶段 B — iSCSI 导出

| # | 步骤 | 期望 |
|---|------|------|
| B1 | `lsscsi -g \| grep -i VTL` | 记下 **changer**、**2×tape** 的 `/dev/sg*`（同一 host） |
| B2 | `vtladm-iscsi check` | OK |
| B3 | `vtladm-iscsi --dry-run library-export ...` | 脚本含 pscsi create + luns |
| B4 | 正式 `library-export`（Web 或 CLI，**id/iqn/sg 与本次一致**） | `ok: true`，stderr 仅 Note |
| B5 | `targetcli ls` | pscsi **3**，luns **lun0–2**，portal **3260** |
| B6 | `targetcli saveconfig` | 可选，便于重启后恢复 LIO |

**CLI 模板**（替换 sg）：

```bash
/opt/vtladm/bin/vtladm-iscsi library-export \
  --id mmarstor_YYYYMMDDhhmmss \
  --iqn iqn.2026-05.com.marstor:marstor-YYYYMMDDhhmmss \
  --changer-sg /dev/sgX \
  --drive-sg /dev/sgY \
  --drive-sg /dev/sgZ \
  --portal-ip 0.0.0.0 --portal-port 3260
```

**`library-unexport` 时 stderr 含 `No such path`**：对象已被重启清掉，**`ok: true` 可忽略**。

---

## 4. 阶段 C — Initiator（msa）

| # | 步骤 | 期望 |
|---|------|------|
| C1 | `iscsiadm` discovery + login **本次 IQN** | 成功 |
| C2 | `lsscsi \| grep -i VTL` | **1 host**：`VTL CHANGER` + **2× `VTL TAPE`** |
| C3 | **勿访问 LUN ≥3** | 无 target 报错 |
| C4 | 备份软件识别带库 | changer + 2 drives（非 mtx 必需） |
| C5 | **装带**（软件机械手或本机 sg） | 驱动器非空 |
| C6 | **读标签** | 装带后成功 |
| C7 | 备份/扫带（可选） | 无 target 机 panic |

**mtx**：可尝试 `mtx -f /dev/sgX status`；**部分环境可能通过**，**不作为唯一通过标准**（以备份软件机械手为准）。

---

## 5. 阶段 D — 稳定性（维护窗口）

| # | 步骤 | 注意 |
|---|------|------|
| D1 | 停 initiator 会话 | `iscsiadm logout` |
| D2 | `library-unexport` | 再测 export/unexport 循环 |
| D3 | `VTL_SKIP_RMMOD=1` 跑 `vtl-kernel-stability.sh` | 见 `userspace/scripts/` |
| D4 | 压测期间 **停 postgres**（若同机） | 排除 05:32 类无关 panic |
| D5 | 开启 kdump + 持久 journal | 再 panic 可分析 vmcore |

**禁止**：iSCSI 占用 sg 时 **`rmmod vtl`**。

---

## 6. 故障速查

| 现象 | 处理 |
|------|------|
| `vtl-kernel` / Web 起不来 `$'\r'` | `sed -i 's/\r$//' /etc/default/vtladm`；重装 scripts |
| 仅 1 驱 / `vtl_instances=(null)` | `-L marstor config set max_drives=2 slots=10` + `vtl-kernelctl reload`（`default` 不参与 spec） |
| targetcli **LUNs: 0** | 重新 **library-export** |
| Web iscsi `ok` 但 stderr `No such path` | 多为 **unexport** 清已不存在的对象，可忽略 |
| initiator 读标签失败 | 先 **装带**；确认 **LUN 0/1/2** |
| 服务器 reboot | 查 `/var/crash/*/vmcore-dmesg.txt`；**非 vtl 栈**则查 postgres/VM |

---

## 7. 通过标准（建议）

- [ ] A2：本机 **1 host，2 tape**
- [ ] B5：targetcli **3 LUN**
- [ ] C2：initiator **1 host，2 tape**
- [ ] C6：装带后 **读标签成功**
- [ ] 连续 **1h** 无 kdump（阶段 D，单 initiator、无并发 `rmmod`）

---

## 8. 相关文档

- [STACK.md](STACK.md) — 栈关系  
- [../../userspace/docs/SCSI.md](../../userspace/docs/SCSI.md) — ioctl / rmmod 风险  
- [../../userspace/docs/VTLADM-ISCSI.md](../../userspace/docs/VTLADM-ISCSI.md) — export/unexport  
- [../../INSTALL.md](../../INSTALL.md) — 安装与 Web 登录  
