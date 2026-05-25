# 内核 VTL 模块：SCSI 暴露与命令矩阵

本文描述 **`vtl.ko` 在 Linux SCSI 子系统中的呈现方式**（LUN 划分、设备类型、已实现与未实现的 CDB），便于与物理磁带库及备份软件的预期对齐。用户态 `vtladm` 的元数据与镜像管理仍见根目录 [README.md](../../README.md)。

## LUN 布局（重要）

每个 `vtl` SCSI host 上：

| LUN | 设备类型 (INQUIRY) | 典型用途 |
|-----|-------------------|----------|
| **0** | Medium changer (`0x08`) | `MOVE_MEDIUM`、`READ_ELEMENT_STATUS`、机械手语义 |
| **1 … N** | Sequential-access (`0x01`)，N = 该库的驱动器数 | 每 LUN 对应 **一台磁带机**（`READ`/`WRITE`/`REWIND`/`SPACE`/`LOG SENSE` 等） |

因此：**磁带 I/O 应针对 LUN ≥ 1 的设备节点**（如 `/dev/st*`、`/dev/sg*` 中 LUN 与 `lsscsi` 输出一致）；**LUN 0 仅作带库机械手**。若备份软件仍按「单 LUN 同时当带库与磁带」的旧习惯配置，需改为分离的 changer 与 drive 设备。

`scsi_scan_host` 使用 `shost->max_lun = num_drives + 1`（**上界独占**），有效 LUN 为 **`0 … num_drives`**（共 **`1 + num_drives`** 个：LUN 0 机械手 + LUN `1…num_drives` 各一台磁带机）。

## 与 vtladm 库模型对齐（一致做法）

内核 **`vtl.ko`** 在 **`modprobe`/`insmod` 时** 决定几何，**不会**读 SQLite。`vtladm` 的 **`inventory` / `load` / `unload`** 等维护 **数据库**；与 SCSI 对齐需 **`num_drives`/`num_slots`** 或 **`vtl_instances`**（见下），并理解 **槽位/驱动器数量** 与 **磁带是否在内核驱动器里** 是两层状态。

### 1. 单 SCSI host（默认）

未设置 **`vtl_instances`** 时，模块只注册 **一个** SCSI host，参数为 **`num_drives`**、**`num_slots`**。

| 含义 | vtladm / 数据库 | 内核模块参数 |
|------|-----------------|--------------|
| 磁带机台数 | 该库的 **`drives` 行数**（`create-library --drives N` / `library-config max_drives`） | **`num_drives=N`**（仅单 host 模式） |
| 数据槽位数（机械手 storage slot） | **`slots` 表中 `is_import_export=0` 的槽位数**（`--slots M`） | **`num_slots=M`**（仅单 host 模式） |

内核会把参数夹紧到编译**上限**（产品规格 / 方案 B）：**最多 8 个 SCSI host**（库）、每库 **`num_drives` ≤ 8**、**`num_slots` ≤ 256**。每库 **1 个 medium changer（LUN0）+ 若干磁带 LUN**。若 DB 超过该上限，**vtladm 会拒绝或夹紧**，与内核一致。

**示例**（库 `marstor` 为 2 台驱、32 个数据槽；与 `lsscsi` 里 2 个 `st` + 1 个 changer 一致）：

```bash
rmmod vtl 2>/dev/null
modprobe vtl num_drives=2 num_slots=32
```

### 1b. 多 SCSI host（多库 = 多套独立 changer + drives）

模块参数 **`vtl_instances`** 为逗号分隔的 **`驱动器数x数据槽位数`**，每一项对应 **一个** SCSI host（各自 LUN0 机械手 + LUN1…N 磁带机），例如两台几何不同的虚拟库：

```bash
rmmod vtl 2>/dev/null
modprobe vtl vtl_instances=2x32,1x10
```

**与 DB 的对应关系**：`vtladm` 按在线库（**不含**保留库 `__offline__`）的 **`vtl_libraries.id` 升序** 生成规格串；第 1 个库 → 第 1 个 SCSI host，以此类推。最多 **8** 个实例（**上限**，与内核 `VTL_MAX_SCSI_INSTANCES` 一致；库数可更少）；超出部分在生成规格时会被截断并记日志。

**建库 / 改几何 与 `lsscsi`、iSCSI 的时机（勿混淆）**：

| 步骤 | 作用 | 是否必需才能在本机 `lsscsi -g` 看到设备 |
|------|------|----------------------------------------|
| `library create`（写入 DB 驱动器/槽位数） | 定义库几何 | 否（仅 SQLite） |
| `vtladm` 自动 **ioctl / reload**（`maybe_reload_kernel_vtl_after_db_change`） | 让 **vtl.ko** 的 SCSI host/LUN 数与 DB 一致 | **是**（几何真相在内核） |
| **SCSI `scan`**（改几何成功后自动 rescan；或 `vtl-scsi-scan-all-hosts.sh`） | 让内核**枚举** LUN，出现 `/dev/sg*`、`lsscsi` 行 | **是**（ioctl 不代替 scan） |
| **`vtladm-iscsi library-export`** | 把**已有**本机 `/dev/sg*` 挂到 LIO 给备份机 | **否**（仅远程 initiator；本机 `lsscsi` 不依赖 export） |

期望行数（每库一个 SCSI host）：**1 行 mediumx（机械手 LUN0）+ N 行 tape（N = 该库 `drives`）**。若建库后数量不对，先 `vtladm kernel-align`，再 `lsscsi -g | grep -i vtl`，**不要**先去做 LUN 映射。

**改库后对齐内核（默认：自动 ioctl + 可选脚本）**：**`kernel_geom_prefer_ioctl` 默认为 `true`** 时，创建/删除库或 **`library-config` 改几何后**，`vtladm` **会自动尝试** **`/dev/vtl` `VTL_IOCTL_SET_INSTANCES`**（见 **§1e**），**与 `kernel_reload_on_db_change` 是否为 `false` 无关**——在无整模块 **`rmmod`/`insmod`** 的前提下对齐 DB 生成的 **`vtl_instances`**。**`kernel_reload_on_db_change` 默认为 `false`**：仅表示 **ioctl 失败或不适用时**不自动执行 **`kernel_vtl_reload_script`**（整模块重载，类 mhVTL 规避重启风险）；ioctl 成功则**不会**跑脚本。若 ioctl 失败且需自动回退脚本，设 **`kernel_reload_on_db_change=true`**（或 **`VTL_KERNEL_RELOAD_ON_DB_CHANGE=1`**）并配置 **`kernel_vtl_reload_script`**。可选 **`vtl_ko=`** / **`VTL_KO`**、**`vtl_reload_scan_delay_ms`** / **`VTL_SCAN_DELAY_MS`**；脚本默认 **不再 `insmod -f`**；**`VTL_INSMOD_FORCE=1`**、**`VTL_RELOAD_SLEEP_SEC`** 见脚本注释。调用脚本走 **`/bin/sh /path/to/script <spec>`**。**`VTL_SKIP_KERNEL_RELOAD=1`** 跳过 ioctl 与脚本。参考 **`userspace/scripts/vtl-kernel-reload.sh`**。**ioctl 或 `rmmod`/`insmod` 前**尽量释放 **`/dev/st*`、`/dev/sg*`**。异常时查 **`dmesg`** 与 **`vtl.ko` vermagic**。

### 1c. 稳定性与内核崩溃风险（运维必读）

下列情况在真实环境中曾表现为 **内核 OOPS、整机重启或 SCSI 栈锁死**（具体取决于内核版本与并发负载），**不属于** `vtladm` 用户态可完全兜底的范围，但可通过流程规避：

| 风险 | 建议 |
|------|------|
| **`rmmod vtl` 时仍有 I/O**（备份任务、`dd`/`mt`、目标机对 pscsi 的访问） | 在维护窗口停止相关进程后再改库几何或执行重载脚本；必要时先从 LIO 侧 **`library-unexport`** 再卸载模块。 |
| **加载错误或与运行内核不匹配的 `vtl.ko`** | 始终针对 **`uname -r`** 编译安装；勿对生产机使用 **`insmod -f`**（脚本默认已关闭，见上）。 |
| **DB 几何与 `vtl_instances` 长期不一致** | 备份软件按「另一套」槽位/驱动器拓扑发机械手命令，易导致内核驱动路径上的异常组合；改库后应尽快重载并对齐 **`lsscsi`**。 |
| **`vtladm-iscsi library-export` 指向非本 VTL 的 `/dev/sg`** | LIO pscsi 与错误节点组合可能拖垮本机 I/O；**只使用 `lsscsi -g` 确认的 VTL changer/drive 节点**，并先 **`--dry-run`**。 |
| **默认行为（类 mhVTL + 自动 ioctl）** | **`kernel_reload_on_db_change` 默认为 `false`**：ioctl 失败时**不**自动跑整模块重载脚本。**`kernel_geom_prefer_ioctl` 默认为 `true`**：改库后**仍会自动尝试 ioctl**；成功则 DB 与内核对齐且**无** `rmmod`/`insmod`。若 DB 规格与 **`/var/lib/vtl/.last_vtl_instances_spec`** 一致，**只**做 **SCSI `scan` 刷新**（§1f），不重跑 ioctl。ioctl 失败且未开脚本回退时，须维护窗口手工重载或设 **`kernel_reload_on_db_change=true`** 并配置 **`kernel_vtl_reload_script`**。 |

### 1e. 几何对齐：推荐 `vtl-kernelctl reload`（默认禁用热 ioctl）

**推荐路径（生产默认）**：

1. 取消 LIO 导出 / 停止备份对 VTL 节点的占用  
2. **`/opt/vtladm/sbin/vtl-kernelctl reload`**（或 `stop` → 确认 `lsscsi` 无 VTL → `start`）  
3. `start` 会从 **`vtl.db`** 推导 **`vtl_instances=`** 并在 **`insmod`** 时传入（见 **`packaging/sbin/vtl-kernelctl`**）

**`/dev/vtl` `VTL_IOCTL_SET_INSTANCES`**（**`kernel/src/vtl_misc.c`**）在内核里拆除并重建 platform/SCSI host，**不** `rmmod` 模块本身。自当前树起：

| 机制 | 说明 |
|------|------|
| **`allow_hot_geom=N`（默认）** | ioctl 返回 **`EBUSY`**；`vtladm` 在配置了 **`kernel_vtl_reload_script`** 时自动改跑 **rmmod/insmod 脚本**，否则提示 **`vtl-kernelctl reload`** |
| **`allow_hot_geom=Y`** | 仅维护窗口；且默认 **`hotgeom_require_no_sdevs=Y`**：VTL host 上仍有 **`scsi_device`** 时拒绝 ioctl |
| **规格未变** | 跳过拆除（仅同步 sysfs） |
| **注册失败** | **回滚**到变更前几何并尽量恢复 SCSI host |
| **磁带对象** | 热重配 **不** 调用 **`vtl_tapes_release_all()`**；磁带镜像用 **`kref`** 管理，仅模块卸载时释放 |

**`vtladm`** 在 **`kernel_geom_prefer_ioctl=true`**（默认）时仍会 **尝试** ioctl；收到 **`EBUSY`** 后走 **`kernel_vtl_reload_script`**（若已配置）。**`insmod` 成功**后 **`/sys/module/vtl/parameters/vtl_instances`** 与 DB 一致（模块 init 会 **`vtl_publish_live_instances_spec`**）。需 **root**；**`ENOTTY`** 表示 **`vtl.ko` 过旧**。用户态仅 **64 位**（`userspace/build.rs`）。

### 1f. mhVTL 类比：仅刷新 SCSI 总线（`scan`）与 `vtladm` 缓存路径

**mhVTL 常见做法**是不 **`rmmod`** 模块，而对已有 SCSI host **写 `scan`** 让内核重新枚举 LUN（等价 **`echo "- - -" > /sys/class/scsi_host/hostN/scan`**）。

本仓库 **`vtl.ko`** 下：

- **在线库个数 / `vtl_instances` 段数或各段 `NxM` 变化**时，内核必须 **增删 SCSI host** 或重配几何，这**不能**仅靠对旧 host 的 **`scan`** 完成；须 **`VTL_IOCTL_SET_INSTANCES`**（或 **`rmmod`/`insmod` + 脚本**）。ioctl 路径在内核里 **不卸载模块**（见 `vtl_main.c` 日志 *without module reload*），但会 **拆除并重建 platform/SCSI host**，比单纯 **`scan`** 更重。
- 当 DB 生成的 **`vtl_instances` 规格串与上次成功应用（ioctl 或钩子脚本）后写入的 **`/var/lib/vtl/.last_vtl_instances_spec` 文件内容一致**时，`vtladm` 在改库钩子中**只**对 **`/sys/class/scsi_host/host*/proc_name == vtl`** 的节点写 **`"- - -\n"`** 到 **`scan`**（与 **`echo '- - -' > scan`**、**`vtl-scsi-rescan.sh`** 中 **`printf '%s\n' '- - -'`** 一致；实现见 **`userspace/src/scsi_rescan_vtl.rs`**），**跳过 ioctl 与整模块脚本**——接近「不重配 host、只刷总线」的 mhVTL 体验。若 **`scan` 失败**（权限、尚无 host 等），会回退到原有 ioctl/脚本逻辑。可用环境变量 **`VTL_SKIP_SPEC_CACHE=1`** 强制跳过该分支；**`VTL_NO_SCSI_RESCAN_ON_UNCHANGED_SPEC=1`** 关闭「规格未变时的 rescan」。手工可运行 **`userspace/scripts/vtl-scsi-rescan.sh`**（须 root）。各 host 之间的间隔见 **`VTL_SCSI_RESCAN_STAGGER_MS`**（默认 **50** ms，**仅** Rust 路径在两次写入之间睡眠；**不在**最后一个 host 之后再睡）。
- **缓存与运行内核可能漂移**：若管理员曾**不经 `vtladm`** 重载 **`vtl.ko`**、或 DB 与 **`/var/lib/vtl`** 状态被人为恢复，可能出现 **文件与 DB 字符串仍一致** 但 **内存中几何已不同** 的情况；此时仍可能只走 **`scan`** 而无法纠偏。不信任环境时请 **`VTL_SKIP_SPEC_CACHE=1`**、删除 **`.last_vtl_instances_spec`**，或维护窗口用 ioctl/脚本对齐后再让 `vtladm` 写回缓存。

### 1g. 方案 B（半薄内核）：8 库 × 8 驱 × 256 槽

**产品上限**：最多 **8** 个在线磁带库（SCSI host）、每库最多 **8** 台磁带机、**256** 个数据槽。

**`vtl.conf` 启用**（`install.sh` 后编辑 `/opt/vtladm/var/vtl.conf`）：

```ini
kernel_geometry_mode=fixed
kernel_geom_prefer_ioctl=true
kernel_reload_on_db_change=false
```

**首次 / 维护窗口加载**（`vtl-kernelctl` 在 `fixed` 模式下自动使用满配 **`8x256` × 8** 并附加 **`noscan=1`**）：

```bash
sudo /opt/vtladm/sbin/vtl-kernelctl start
sudo sh /opt/vtladm/scripts/vtl-scsi-scan-all-hosts.sh 5
```

**日常改库**（例如 `marstor` 从 2×10 改为 2×32，或调整 `max_drives`）：`vtladm` 通过 **`VTL_IOCTL_RESIZE_GEOMETRY`** 调整各 host 的 `num_drives`/`num_slots`，**不**拆除 SCSI host（须 **host 段数与 insmod 时一致**）。未占用 host 段**保持 sysfs 当前几何**（不会缩成 `1x1`）。**新增第 9 个库**或 **增删 SCSI host 个数** 仍须维护窗口 **`vtl-kernelctl reload`**。**扩大驱动器数**后若备份/`lsscsi` 未见新磁带 LUN，请再跑 **`vtl-scsi-scan-all-hosts.sh`**。

| 操作 | 方案 B |
|------|--------|
| 改驱动器/槽位数（同库数） | `vtladm library create` / `library-config` → 自动 **resize ioctl** |
| 新增/删除在线库（改变 host 数） | **`vtl-kernelctl reload`** |
| 查看 DB 运行时规格 | `vtladm kernel-spec` |
| 查看 insmod 满配规格 | `vtladm kernel-spec --insmod-max` |

### 1d0. 勿在 insmod 时一次扫描 8×8×256

**`vtl_instances=8x256` × 8 段** = 8 个 SCSI host、每 host **9 个 LUN**（1 changer + 8 tape），合计 **72 个 LUN**。在 Kylin 4.19 上若 **insmod 时自动 `scsi_scan_host`**，仍可能触发大量 `st_probe`/sysfs 导致 **整机重启**。

**方案 B** 下 **`vtl-kernelctl start`** 默认 **`noscan=1`**，再用 **`vtl-scsi-scan-all-hosts.sh`** 逐个 host 扫描。新 `vtl.ko` 在 **≥4 host 或总 LUN>48** 时也会 **自动 `noscan=1`** 并打 `dmesg` 警告。

### 1d. `insmod` / `rmmod` 压测与 panic

**原因（旧 `vtl.ko`）**：多 host 时 `scsi_add_host` / `scsi_scan_host` 在 **`system_long_wq`** 上按 host 错峰执行；`rmmod` 若只拆掉 **host0** 的 platform 设备，**host1–5 的 scan 仍在跑**，与 `scsi_remove_host` / slab 释放竞态，可延迟 panic。

**当前 `vtl.ko` 行为**：

- 专用 **`vtl_bringup`** 工作队列，`rmmod` 时 **`flush` + 取消全部 delayed work**
- **`vtl_quiesce_all_hosts()`**：对所有 host **`scsi_remove_host`** 后再 `platform_device_unregister`
- 模块参数 **`rmmod_quiesce_ms`**（默认 **12000**）：remove 前额外等待 async scan 收尾
- **`/etc/udev/rules.d/59-vtl-scsi.rules`**：`ENV{ID_SCSI}="skip"`，避免 **`scsi_id`** 在 scan 期间读 sysfs（见 **§1d3**）
- 压测脚本示例：`packaging/scripts/vtl-ko-insmod-stress.sh`（`sleep` 用秒数，勿写 `120s`）

生产环境请用 **`vtl-kernelctl stop`**（含 holder/LIO 检查），不要裸 `rmmod` 循环。

### 1d2. `insmod vtl.ko` 后立刻整机重启（panic）

说明：**参数非法**时模块通常在 `module_init` 里 **`return -EINVAL`**，一般不会直接重启；**一加载就重启** 多见于 **与运行内核 ABI 不匹配的 `.ko`**、**发行版内核 + 未针对该配置测试的驱动路径**，或 **SCSI 注册/扫描与当前内核的交互 bug**。请按下述顺序缩小范围：

1. **核对 vermagic（必做）**  
   `modinfo /path/to/vtl.ko | grep vermagic` 与 **`uname -r`** 及 **`/proc/version`** 是否一致。不一致时 **不要用 `insmod -f`**；在 **当前内核头文件** 下重新 `make clean && make` 再安装。  
2. **最小参数试加载**（冷启动、无备份占用后，在控制台执行）  
   先不挂 **`vtl_instances`**：`modprobe vtl num_drives=1 num_slots=4`（或 `insmod vtl.ko num_drives=1 num_slots=4`）。若仍重启，问题多半在 **驱动与内核组合**，与 `vtladm` 生成的规格串无关。  
3. **再试多实例**  
   仅在 (2) 稳定后：`rmmod vtl` → `insmod vtl.ko vtl_instances=1x4`（或 `2x8`），逐步加大；若某档开始重启，把 **`dmesg` / `journalctl -k -b -1`** 最后几十行与参数一并保留便于排错。  
4. **保留现场**  
   配置 **串口控制台**、**`netconsole`**、**`pstore`** 或 **kdump**，否则重启后看不到 panic 栈。  
5. **临时规避**  
   在查明前可 **关闭** `vtl.conf` 里的 **`kernel_vtl_reload_script`**，改用手动 **`modprobe`/`insmod`** 并在维护窗口操作。  
6. **`vtl_instances` 含义（易混）**  
   **`2x8`** 表示 **一个** SCSI host：**2 台磁带机 + 8 个数据槽**（不是「两个库各 8 槽」）。两个库应为 **`1x4,1x4`** 这类逗号两段。  
7. **Kylin 4.19 等：`vtl_instances=2x8` 加载即重启**  
   **`vtl.ko`** 将 **`scsi_add_host`** 与 **`scsi_scan_host`** 均推迟到 **`system_long_wq`**：**`scan_delay_ms`**（默认 **500**）+ **`bringup_stagger_ms`×实例序号** 后执行 **`scsi_add_host`**；**再**经 **`post_add_scan_delay_ms` + `scan_host_stagger_ms`×实例序号**（默认 **600 + 3000×id**）后才 **`scsi_scan_host`**，以减轻多 host 并行 **`do_scan_async`/`st_probe`**（麒麟 4.19 上曾见 **`st`/`kstrdup`** 类故障；规格上最多 **8 库×多 LUN**，默认 stagger 偏保守）。重载脚本可通过 **`VTL_POST_ADD_SCAN_DELAY_MS`**、**`VTL_SCAN_HOST_STAGGER_MS`** 调整。**`noscan=1`** 时仍推迟 **`scsi_add_host`**，但**不**排队 **`scsi_scan_host`**，由你手动 **`echo "- - -" > /sys/class/scsi_host/hostN/scan`**。  
   - **`insmod` 已返回、约数百毫秒后才重启**：对照 **`dmesg`** 中 **`deferred scsi_add_host`** 与 **`deferred scsi_scan_host`** 的先后；若总在 scan 一行之后，优先加大 **`post_add_scan_delay_ms`** 或 **`noscan=1`** 再手动 scan。  
   - **`insmod` 尚未返回就重启**：多半在 **`vtl_sysfs_init` / `vtl_misc_init`** 或 **`platform_device_add`** 之前路径；请 **重新编译** 当前树中的 `vtl.ko`，并抓 **kdump / 串口 panic 栈**。

### 1d3. `install.sh --enable` / reload 后 kdump：`scsi_id` + GPF

**典型栈**（openEuler 6.6 / VMware，`vmcore-dmesg.txt`）：

- `general protection fault`，`Comm: scsi_id`
- `__kmem_cache_alloc_node` ← `proc_sys_call_handler` ← `ksys_read`
- 时间点在 **`st0`/`sg*` attach 后约数十毫秒**（`scsi_scan_host` 尚未被 `scan_async_quiesce_ms` 完全“盖住”）

**原因**：udev **`scsi_id`** 在 **`scsi_scan_host` / `st_probe`** 仍为内核热点时读 sysfs，与 slab/扫描路径竞态；损坏的 `kmem_cache` 指针常呈路径碎片（如 `../vtl` 类字节）。

**修复（安装包）**：

1. **`59-vtl-scsi.rules`**（`install.sh` 会装到 `/etc/udev/rules.d/`）：对 **`ATTRS{vendor}=="VTL*"`** 设 **`ENV{ID_SCSI}="skip"`**。
2. **`vtl-kernelctl start` / reload** 在 insmod 后调用 **`vtl_post_insmod_settle`**（按 **`scan_async_quiesce_ms`** 等待）。
3. **`vtl-kernelctl stop`** / **`uninstall.sh`**：仍须 **删 SCSI 节点 + 长等待** 再 **`rmmod`**（见 **§1d**）。

**验证**（reload 后不应再出现 GPF）：

```bash
udevadm control --reload-rules
/opt/vtladm/sbin/vtl-kernelctl reload
dmesg | tail -40 | grep -iE 'vtl|protection fault|BUG|Oops'
lsscsi -g | grep -i VTL
```

若规则未生效，临时：`systemctl stop systemd-udevd` → reload → `systemctl start systemd-udevd`（仅维护窗口）。

核对：

```bash
lsscsi -g
```

单 host **且未**设置 **`vtl_instances`** 时，还可对照 **`/sys/module/vtl/parameters/num_drives`**、**`num_slots`**。若加载时使用了 **`vtl_instances`**，各 host 的几何以该参数为准；**sysfs 里的 `num_drives`/`num_slots` 仍为模块形参表项，不一定反映每个实例的 N×M**。

单 host 模式下期望：**1 个 medium changer** + **`num_drives` 条 tape**。多 host 时 **`lsscsi`** 会出现 **多组** host 号，每组 1 个 changer + 若干 `st*`。

### 2. 多库与内核的两种用法

- **单 host**：只用 **`num_drives` / `num_slots`** 时，多库在 DB 里可并存，但 SCSI 上只有 **一套** 机械手；通常只让 **一个** 库的几何与模块参数一致。  
- **多 host**：使用 **`vtl_instances`** 时，每个在线库（按 `id` 序，不含 `__offline__`）可对应 **独立** SCSI host（不同 **host** 号），便于备份软件把库当作多台独立带库。

### 3. 导入/导出槽（mailslots）

内核侧 **固定 4 个** I/E 槽（`VTL_MAX_MAILSLOTS`）。`vtladm create-library` 会固定插入 **4** 条 `is_import_export=1` 的槽位（展示为 **`mail0`…`mail3`**）；若手工改库导致 mailslot 行数 ≠ 4，元素状态/换带脚本可能与备份软件预期有偏差。

### 4. 介质位置（槽位 vs 驱动器）

当 **`robot_sync=true`**（`vtl.conf` 默认）且 **`vtl.ko`** 含 robot ioctl 时：**`vtladm load` / `unload` / `assign-slot`** 会在更新 DB 后通过 **`/dev/vtl`** 同步内核机械手（见 **`userspace/docs/ROBOT-SYNC.md`**）。改库几何成功后会自动 **`robot sync`** 各库槽位。

若关闭 `robot_sync` 或 ioctl 失败，仍可能出现 **DB 有带、内核驱空**：备份软件 / initiator 需自行对 LUN0 发 **`MOVE_MEDIUM`**，或执行 **`vtladm -L <lib> robot sync`** / **`load`** 恢复。

---

## 机械手与元素地址

与用户态 CLI / `vtladm` DB 一致，元素地址约定为：

- **数据槽位**：`0 … num_slots-1`
- **驱动器**：`1000 + drive_index`（`drive_index` 从 0 开始）
- **进出口（mailslot / I/E）**：`2000 + mail_index`（`mail_index` 0…3；DB 中 `mailK` 对应 `slot_id = 100+K`）

`MOVE_MEDIUM`、`READ_ELEMENT_STATUS` 与 **`vtladm eject`**（`robot_sync`）使用上述编号。

**MODE SENSE 页 0x1D（Element Address Assignment）** 须按 SMC-3 字段顺序：字节 **6–7** = 首数据槽地址（VTL 为 **0**）、**8–9** = 槽位数；**14–15** = 首驱动器地址（**1000**）、**16–17** = 驱动器数；**10–11/12–13** = I/E @2000。旧版误把槽位数写在 **4–5**、把 1000 写在 **6–7**，备份软件会只认 **2 个 storage @1000–1001**（界面仅 2 槽），且 **清单** 失败。

**MODE SENSE(10) 头长度**：字节 **0–1** 须为「从字节 2 起到页末」的字节数（`off - 2`）。若误为 `off - 6`，initiator/`sg_modes` 会少读 **4** 字节，**0x1D 页末尾驱动器 @1000 / 台数被截断**，清单/搜索磁带可失败（`mtx` 仍正常）。

**SMC-3 `READ ELEMENT STATUS`**：8 字节 Element Status Data 头：**byte 0–1** 首个元素地址、**byte 2–3** 本响应中的描述符个数（**mtx** 按此循环）、**byte 4** 保留、**byte 5–7** 后续各 Element Status Page 总字节数；再按类型分页（**ST=2** 槽位、**DT=4** 驱动器 @1000+、**IE=3** 进出口 @2000+）。页头 **byte 0** 为类型码 **2/3/4**（与 **mtx** `Element2StatusPage` 一致，非仅 `(type<<5)`）；**voltag** 时页头 **byte 1** 置 **0x80**（`E2_PVOLTAG`），CDB byte1 **bit4（0x10）** 请求 voltag。每页 8 字节页头 + 12/32 字节描述符。**12 字节 CDB** 分配长度在 **byte 7–9**（24 位，与 **mtx** 一致）；误读 **byte 9–11** 会把 `…00 00 ff` 当成约 16 MiB。旧版若未填 byte 2–3、页头 `(type<<5)` 或 12 字节 alloc 偏移错误，会导致 **`mtx status` → no Data Transfer Element reported** 或 **sg_raw** 异常长度。

**对账 ioctl**：`VTL_IOCTL_GET_INVENTORY`（`8`）返回已占用元素与磁带名；`truncated=1` 表示超过 128 项被截断。`robot reconcile --pull` 将内核 inventory 写回 DB；**DB→内核全量 sync 已移除**（现场由备份软件 MOVE 或 `assign-slot`/`load` ioctl）。

**INQUIRY / personality**：模块参数 **`personality=`**（`vtl.conf` 中 `personality=ibm` 等，由 **`vtl-kernelctl start`** 传给 `insmod`）：

| 值 | 机械手 (LUN0) | 磁带机 (LUN≥1) | 典型用途 |
|----|---------------|----------------|----------|
| **`vtl`**（默认） | `VTL` / `VTL CHANGER` | `VTL` / `VTL TAPE DRV` | 开发、本机 `grep VTL` |
| **`ibm`** | `IBM` / `03584L32 A00` | `IBM` / `ULT3580-TD8` | TSM / 常见 IBM 带库清单 |
| **`stk`** | `STK` / `L700` | `STK` / `T10000B` | Spectra / STK 兼容 |
| **`hp`** | `HP` / `MSL6480` | `HP` / `Ultrium 5-SCSI` | MSL 系列 |

改 personality 后须 **`vtl-kernelctl reload`**（或维护窗口 `rmmod`/`insmod`）。本机巡检默认仍可用 `lsscsi -g`；备份侧按 vendor/product 识别，不必只 `grep VTL`。

### 备份软件「清单 / Inventory」失败而 `mtx` 正常

| 现象 | 常见原因 |
|------|----------|
| Mars/TSM 树上有 2 驱 + 10 槽但全 `(Empty)`，操作失败 | initiator 走了 **iSCSI**，与本机 `mtx -f /dev/sg5` 不是同一条路径；或 LUN 映射错（机械手须在 **LUN0**） |
| 本机 `mtx` 有 10 槽 Full，备份侧空 | **`MODE SENSE` page `0x3F`** 曾只返回空头（备份用 0x3F，mtx 用 0x1D）；或 **INQUIRY VPD 0x80** 缺失；或 **READ ELEMENT STATUS** 分配长度过小且未带 voltag |
| 设备名已是 `IBM 03584L32` 仍失败 | 几何/元素状态已识别，失败在 **voltag/条码** 或 **磁带 LUN** 探测；查备份日志与 `userspace/scripts/vtl-changer-inventory-probe.sh` |
| `sg_inq`/`mtx` 正常，`sg_turs` 报 *bad pass-through setup* 且 `-vvv` 有 *No SCSI command (cdb) given \[v3\]* | 多为 **sg3_utils 未挂上 CDB**（ioctl 未发出），与 INQUIRY 路径不同；用 **`sg_raw -r 0 /dev/sgN 00 00 00 00 00 00`** 或 **`sg_turs -l`** 验证；若 `sg_raw` 成功而裸 `sg_turs` 失败，Mars 仍可能正常 |

**在 5.64 上自检**（root，将 `/dev/sg5` 换成你的机械手节点）：

```bash
sh /opt/vtladm/scripts/vtl-changer-inventory-probe.sh /dev/sg5
grep '^personality=' /opt/vtladm/var/vtl.conf
cat /sys/module/vtl/parameters/personality
lsmod | grep '^vtl '
systemctl is-active vtl-kernel.service
sg_raw -r 0 /dev/sg6 00 00 00 00 00 00 && echo "sg6 TUR OK"
sg_turs -l /dev/sg6 && echo "sg6 sg_turs -l OK"
```

**在备份机（5.83）上**：确认 iSCSI 已 login，`lsscsi` 可见 `IBM`/`03584` changer + `ULT3580` 磁带机；再对 **initiator 侧的 changer sg** 跑同等探测（若装了 `sg3_utils`/`mtx`）。

## 已实现的主要 SCSI 命令

### LUN 0（Changer）

- `INQUIRY`、`TEST UNIT READY`、`REQUEST SENSE`
- `MODE SENSE` / `MODE SENSE(10)`、`MODE SELECT` / `MODE SELECT(10)`（changer 支持页 **0x00 / 0x1D / 0x1E**；tape LUN 含块描述符）
  - **分配长度**：`MODE SENSE(6)` 使用 **byte 4**；`MODE SENSE(10)` 使用 **bytes 7–8**（此前误用 10 字节偏移解析 6 字节 CDB 的问题已修正）。
- `INITIALIZE ELEMENT STATUS`（返回 GOOD，无操作）
- `MOVE_MEDIUM`（含槽 ↔ 驱 ↔ **I/E 2000+**）
- `READ_ELEMENT_STATUS`（SMC 风格头 + 描述符，支持 **voltag**）

### LUN 1…N（Tape）

- `INQUIRY`、`TEST UNIT READY`（**空驱也返回 GOOD**，便于 Mars/TSM 清单；`cmd->result` 按 `DID_OK` + `status<<1` 编码，否则 `sg_turs` 报 *bad pass-through setup*）、`REQUEST SENSE`
- 无带时 `READ`/`LOAD` 仍 `NOT READY` / `0x3a`
- `READ BLOCK LIMITS`（6 字节返回体为**简化**实现，**非**完整 SSC 块限制编码；若个别工具解析异常，以备份软件与 `mt` 实测为准）
- `READ(6)` / `READ(10)` / `READ(12)`、`WRITE(6)` / `WRITE(10)` / `WRITE(12)`（含固定块长度变体）
  - 单次传输 **`blocks × block_length`** 上限为 **64 MiB**，块长须在驱动允许的最小/最大块范围内；否则返回 **`ILLEGAL REQUEST` / ASC `0x24`**。
- `REWIND`、`SPACE`、`WRITE FILEMARKS`
- `LOAD` / `UNLOAD`（`LOAD_UNLOAD`）
- `LOG SENSE`：**页 `0x00`**（支持的页列表）、**页 `0x11`**（从磁带元数据汇总的读写字节计数；无带时页 `0x11` 返回 `NOT READY`）
  - **CDB 长度**：支持 **6 字节**（分配长度在 **byte 4**）与 **10 字节**（分配长度在 **bytes 7–8**），与 SPC 常见布局一致。
  - **页 `0x11` 二进制布局**为 VTL **自定义摘要**，**不等同**于某一 LTO/厂商 SSC 官方 LOG 页字段；对接备份软件时**勿假设**与物理驱位逐字节兼容，仅作计数参考。
- `READ POSITION`（长格式，服务动作 `0x00` / `0x01`，20 字节有效载荷）
- `PREVENT ALLOW MEDIUM REMOVAL`（空操作，返回 `GOOD`）

### LUN 非法时的 Sense

- 对本 SCSI host 上 **超出 `max_lun` 的 LUN**，返回 **`ILLEGAL REQUEST` / ASC `0x25` / ASCQ `0x00`**（*logical unit not supported*），便于 initiator 与物理库行为对齐。

## 介质健康与计数（内核侧）

`struct vtl_tape_metadata` 中维护（当前**未写入** `.vtltape` 镜像头，仅在内存中随会话累积）：

- `log_bytes_read` / `log_bytes_written`：由成功 `READ`/`WRITE` 路径累加，供 `LOG SENSE` 页 `0x11` 展示。
- `mount_count`：磁带装入驱动器时递增。

## 可选：机械手延迟（培训 / 压测）

模块参数：

```text
move_delay_ms=N   # 每次 MOVE MEDIUM 成功后额外睡眠 N 毫秒（0 关闭，上限 60000）
```

用于模拟机械臂耗时或串行化操作节奏，**不参与**持久化元数据。

## 未实现 / 与真机差异（节选）

- 多命令的 **ERASE**、**FORMAT**、完整 **LOG SENSE** 全页集、**WORM** 语义、驱动器清洗带等。（**`REPORT LUNS`** 已实现，用于枚举 LUN `0…num_drives`。）
- **FC/iSCSI Target** 不在本模块内（见 [TRANSPORT.md](TRANSPORT.md)）。
- **LTFS** 为另一软件栈，与本顺序 SCSI 磁带模型不同。

## 相关文档

- [TRANSPORT.md](TRANSPORT.md) — 传输与目标端职责  
- [TAPE-LIBRARY.md](TAPE-LIBRARY.md) — 物理磁带库背景  
