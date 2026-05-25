# vtladm-iscsi — targetcli 封装

独立二进制 **`vtladm-iscsi`**（与 `vtladm` 同仓构建）用于在 **Linux-IO（LIO）** 上通过 **`targetcli`** 完成常见 iSCSI target 操作，把多步交互合并为少量子命令。

## 依赖

- 已挂载 configfs，且加载 LIO 相关模块（发行版常见包：`targetcli`、`python3-rtslib-fb` 等）
- 通常需要 **root** 或具备写 `/sys/kernel/config/target` 的权限；可用 **`--sudo`** 调用 `sudo targetcli`

## 构建与安装

```bash
cd userspace
cargo build --release
sudo cp target/release/vtladm-iscsi /usr/local/bin/
```

## 子命令一览

| 子命令 | 说明 |
|--------|------|
| `check` | 检查 `/sys/kernel/config/target` 与 PATH 中的 `targetcli` |
| `ls` | 等价 `cd /` + `ls`（树状） |
| `save` | `saveconfig` 持久化 |
| `batch <文件>` | 每行一条 targetcli 命令，`#` 与空行忽略；文件末尾若无 `exit` 会自动补上 |
| `shell` | 直接启动 targetcli（继承终端） |
| `quick-export` | 一条链路：FILEIO → iSCSI → TPG1（demo 属性）→ LUN0 → portal |
| `quick-unexport` | 按相反顺序删除 LUN0、IQN、FILEIO（与默认 `quick-export` 对称） |
| `library-export` | **pscsi 多 LUN**：LUN0=机械手 `/dev/sg*`，LUN1…=各磁带机 `/dev/sg*`（须与 `lsscsi -g` 一致） |
| `library-unexport` | 删除 `library-export` 创建的各 `lun*`、IQN、各 pscsi 后端；**`--drives`**（顺序 lun0…）或 **`--lun-map`**（与导出时相同列表）二选一 |

全局选项：

- **`--dry-run`**：只打印将送入 targetcli 的脚本，不执行  
- **`--sudo`**：执行 `sudo targetcli …`

## 快速导出示例

```bash
sudo vtladm-iscsi --sudo quick-export \
  --file /var/lib/vtl/tapes/default/my.vtltape \
  --iqn iqn.2026-05.org.example:vtl1 \
  --fileio-name vtl_fileio0 \
  --portal-ip 0.0.0.0 \
  --portal-port 3260
```

先 **`--dry-run`** 检查生成的脚本是否符合预期。

## 带库 SCSI 多 LUN（`library-export` / `library-unexport`）

在 **已加载 `vtl.ko`** 且本机已出现 **Medium changer + 磁带机** 的 `/dev/sg*` 时，可将这些 **SCSI generic 节点** 以 **LIO pscsi** 方式挂到同一 iSCSI Target 的 **多个 LUN** 上，使 **备份服务器（iSCSI initiator）** 侧按带库模型枚举设备，并发送 **换带 / 加载 / 卸载** 等 SCSI 命令（由内核 `vtl` 执行）。

**步骤概要**：

1. 在 VTL 主机上：`lsscsi -g` 或等价工具，确认 **机械手** 与各 **磁带机** 对应的 **`/dev/sgN`**（须为 **字符设备**）。  
2. **`vtladm-iscsi library-export`**：`--changer-sg` 一个，`--drive-sg` 按 **LUN1、LUN2…** 顺序每个驱动器各写一次。  
3. 在备份机上：iSCSI discovery/login 后 **rescan**，再在带库软件里选 **changer + drives**。

示例（路径请按本机实际替换；先 `--dry-run`）：

```bash
sudo vtladm-iscsi --sudo library-export \
  --id vtlpt1 \
  --iqn iqn.2026-05.org.example:vtllib \
  --changer-sg /dev/sg3 \
  --drive-sg /dev/sg4 \
  --drive-sg /dev/sg5 \
  --portal-ip 0.0.0.0 \
  --portal-port 3260
```

### `--lun-map`（仅与默认相同的连续编号）

批处理生成的 targetcli 脚本在 **`luns/`** 下按创建顺序自动得到 **lun0、lun1…**，因此 **`library-export` 的 `--lun-map` 仅允许 `0,1,2,…`（长度 = `1 + --drive-sg` 个数）**，用于显式写出与默认一致的映射；**不支持**跳号（如 `3,4,5`）。若历史环境曾用非连续 LUN，请用手工 **`batch`/`shell`** 或自行编写 targetcli 步骤。

**卸载**：若导出时未使用 `--lun-map`，仍用 **`--drives N`**。若导出时传了 `--lun-map 0,1,…`，卸载可传 **同一列表**，或仍用 **`--drives`**（与顺序 lun0… 等价）。**`library-unexport --lun-map`** 仍接受任意互不重复的 LUN 列表，以便清理旧配置或非本工具生成的 layout。

卸载（`--drives` = 导出时 `--drive-sg` 的个数，**不含**机械手）：

```bash
sudo vtladm-iscsi --sudo library-unexport \
  --id vtlpt1 \
  --iqn iqn.2026-05.org.example:vtllib \
  --drives 2
```

**限制与说明**：

- **是否允许** 对给定 `/dev/sg` 做 pscsi 透传，取决于 **内核与 LIO**；若 `targetcli` 报错，需对照发行版文档或改用手工 `batch`。  
- 批处理脚本在 **`cd /iscsi`**、**`create <IQN>`** 之后用**相对路径** **`cd <IQN>`**（IQN 可含 **`:`**）、默认再 **`cd tpg1`**，然后 **`set attribute …`**、**`luns/ create`**、**`portals/ create`**（避免旧实现里 **`cd /iscsi/<IQN>/tpg1`** 一类绝对路径在部分 **Datera targetcli 2.1.x** 下解析失败而出现 **`No such path`**、**`Unknown configuration group: attribute`**）。  
- **`merged` / `VTL_ISCSI_SHELL_PATH=merged`**：仅当在 **`targetcli` 里 `cd /iscsi` → `cd <IQN>` 后 `ls` 无 `tpg1` 子项** 的旧 shell 使用（不执行 **`cd tpg1`**）。若 **`ls` 能看到 `tpg1`**（含 Datera 2.1.54 常见情况），请**不要**设 `merged`，保持默认 **`tpg1`**。  
- 若 stderr 出现 **`Cannot configure … already in use`**，说明 **`/dev/sg` 已被 LIO pscsi 占用**：请先 **`library-unexport`**（或 `targetcli` 手工删除对应 backstore/LUN），再重新导出。  
- 导出期间 **勿**让其他进程长期占用同一 `sg` 节点。  
- TPG 仍可能为 **demo 属性**（与 `quick-export` 类似）；生产请收紧 ACL/CHAP。  
- 若 initiator 仍无法识别为磁带库，需在 **客户端内核与备份软件** 侧排查枚举与驱动绑定。

### 内核与主机稳定性

`library-export` / **`targetcli` pscsi** 与 **`vtl.ko` 的 `rmmod`/`insmod`**（见 **`kernel_vtl_reload_script`**）均在 **内核态路径**上操作。错误 **`/dev/sg`**、与运行内核 **不匹配的模块**、或在 **仍有 SCSI/I/O** 时卸载 **`vtl`**，可能导致 **本机内核 panic、OOPS 或重启**（与发行版内核、LIO 版本及负载相关）。**务必**在维护窗口操作；对 `library-export` / 内核重载先使用 **`--dry-run`** / 停备份后再执行；几何与 **`lsscsi -g`** 对齐说明见 [SCSI.md](SCSI.md) **§1c**。

## 安全说明

`quick-export` 使用的 TPG 属性包含 **`authentication=0`**（关闭 CHAP）、**`demo_mode_write_protect=0`** 与 **`generate_node_acls=1`**，便于实验环境快速连通；**生产环境**请改为显式 ACL、认证与只读策略，并自行编写 `batch` 或手工 `targetcli`。

## 与 `vtladm` 的关系

- `vtladm` 管理 **SQLite 元数据** 与 **`.vtltape` 文件路径**；不启动 iSCSI 服务。  
- `vtladm-iscsi` 仅面向 **本机 LIO**，把镜像文件挂成 **FILEIO LUN**；与备份软件是否识别为「磁带设备」无关（一般为块/文件 LUN）。**`library-export`** 则尝试通过 **pscsi** 将本机 **`vtl` 的 `/dev/sg*`** 以 **多 LUN 带库** 形式暴露给 initiator（见上节）。  
- `vtl.conf` / 环境变量中的 `iscsi_iqn`、`iscsi_portals` 可与运维脚本一起使用，**不会**被 `vtladm-iscsi` 自动读取（避免隐式魔法）；需要时在 shell 中展开即可。

## Web 管理页（`vtladm serve`）

登录后台打开 **`/admin/iscsi`**（**iSCSI / LUN 映射**）可在 **target 主机**上：

1. **从 vtl.conf 加载推荐值**：调用 `GET /api/manage/iscsi/config`，返回 `tape_dir`、`transport`、`iscsi_iqn`、`iscsi_portals`（原文）、由 `iscsi_portals` **首项**解析的 `portal_ip_suggested` / `portal_port_suggested`（**不支持 IPv6** 字面量）、`vtladm_iscsi_path`、`allow_iscsi_exec`、`non_unix_build`、**`kernel_reload_on_db_change`**（**默认 `false`**：ioctl 失败时**不**自动跑整模块重载脚本；**`kernel_geom_prefer_ioctl` 为 `true`（默认）时改库后仍会先试 ioctl**，见 `SCSI.md` §1c/§1e）、**`kernel_geom_prefer_ioctl`**（`false` 时不尝试 `/dev/vtl` ioctl，见 `SCSI.md` §1e）。**若 ioctl 失败且需自动跑脚本回退**：设 **`kernel_reload_on_db_change=true`** 并配置 **`kernel_vtl_reload_script`**，且尽量使用带 ioctl 的 **`vtl.ko`** 以减少整模块重载。  
2. **检测环境**：调用 `POST /api/manage/iscsi/check`（可选 `{"sudo":true}`），执行 **`vtladm-iscsi check`**（不修改 configfs，**不需要** `allow_iscsi_exec`）。  
3. **`library-export` / `library-unexport`**（属于 Web **③ 传输链路**，见 [WEB-WORKFLOW.md](WEB-WORKFLOW.md)）：页面选择在线库后自动加载；若该库**已有成功导出记录**（SQLite 表 `iscsi_library_exports`），则回填 IQN、后端前缀、门户、`/dev/sg` 与 LUN；否则生成新 IQN（`iqn.年-月.com.marstor:库名-时间戳`）。**`POST …/library-export` 成功且非 dry-run** 会写入/更新该记录；**`POST …/library-unexport` 传 `library` 库名** 可从库记录一键卸除（无须再记 IQN/前缀）。**「重新加载」** 对应 `GET …/library-export-defaults?library=…&regenerate=1`（强制新 IQN）。可 **「扫描 lsscsi」** 按库内驱动器台数匹配 `lsscsi -g`（多 host 时用 `prefer_scsi_host`）。**`lun_map` 仅允许 0,1,2…**。真正执行（非 dry-run）须 **`allow_iscsi_exec: true`**。  
4. **`quick-export` / `quick-unexport`**：仍可通过 **HTTP API**（`POST /api/manage/iscsi/quick-export` 等）或 CLI 调用；管理页默认以 **带库 pscsi 导出** 为主。

**门户与 IPv6**：Web 与 `parse_first_iscsi_portal` 仅按「逗号分隔取首段、最后一个 `:` 左侧为 host」解析；**不支持**标准 IPv6 门户写法（如 `[fe80::1]:3260`）。请在 `iscsi_portals` 中使用 **IPv4** 或可解析为 A/AAAA 的**主机名**；若需纯 IPv6 门户，请自行在 shell / `targetcli` 中配置，勿依赖本页的「推荐 IP/端口」回填。

若 `vtladm serve` 以非 root 运行，「检测」时可勾选 **--sudo**，以便 `which targetcli` 等子命令在 sudo 环境下成功。

### 自动化测试

与本节 API 及 `/admin/iscsi` 页面对应的用例见仓库根目录 **[TEST.md](../../TEST.md)** §**3.2**（表 **78a、78a-2、78a-3、78b–78e、78c-2**）与 **§3.1 K1–K2**（`vtl_instances` 规格串）。快速只跑相关用例：

```bash
cd userspace
cargo test test_web_html_admin_iscsi
cargo test test_parse_first_iscsi_portal
cargo test test_web_http_iscsi_
cargo test test_extract_vtl_session_cookie_
cargo test iscsi_validate_tests
cargo test test_build_vtl_instances_kernel_spec_
```

## 故障排除

- **`targetcli` 找不到**：安装 targetcli 并确认在 root 的 PATH 中（`sudo which targetcli`）。  
- **`WWN not valid as: iqn, naa, eui` / `No such path /iscsi/iqn...`**：多为 **IQN 字符串不合法**。新版 LIO 对 **IQN 全串**校验较严，**勿在 IQN 中使用下划线 `_`**（默认已改为冒号后用连字符 `-` 连接库名与时间戳）；手工填写时请同样避免 `_`，或重新「加载默认」。若 **phase1 已成功**（`pscsi` 已有对象）而 phase2 失败，请先 **`library-unexport`**（或 `targetcli` 删除该 IQN 与 pscsi）再导出，以免残留 `…_ch` / `…_dr*`。  
- **portal 已存在**：删除旧 TPG 门户或换端口；可用 `vtladm-iscsi shell` 手工调整。  
- **`quick-unexport` 失败**：可能 LUN 名不是 `lun0` 或 IQN/FILEIO 名不一致；请 `vtladm-iscsi ls` 后用手工 `batch` 或 `shell` 清理。
