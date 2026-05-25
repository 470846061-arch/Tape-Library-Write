# 存储网络传输（SCSI / FC / iSCSI）说明

Web 侧按 **建库 → 磁带/槽位 → 传输链路 → 备份软件** 分层操作，见 [WEB-WORKFLOW.md](WEB-WORKFLOW.md)。

## 产品定位：目标端 + 模仿物理带库

虚拟磁带库（VTL）作为 **目标端（Target）** 使用时，备份服务器或带库客户端把数据 **写入虚拟磁带**（顺序介质、槽位/机械手/驱动器模型与物理带库一致）。要做到与物理环境对齐，协议上必然涉及：

| 层次 | 作用 |
|------|------|
| **SCSI** | 磁带机、介质、机械手等在操作系统里呈现的 **设备类型与命令**（本机常见为 `/dev/st*`、`/dev/sg*`）。这是 **「像物理带库」的核心语义**。 |
| **iSCSI** | 在以太网上 **承载 SCSI**（Initiator 发 CDB，Target 执行）；用于 **网络到达** 同一套磁带语义（能否呈现为「网络 SCSI 磁带」取决于内核与 target 实现，见下文）。 |
| **FC（光纤通道）** | 在 FC 链路上 **承载 SCSI**；用于 **SAN 到达** 同一套语义，需 FC HBA **目标模式** 与相应 target 栈。 |

三者不是三套并列的「独立功能」，而是：**SCSI 是磁带/带库的设备模型；iSCSI 与 FC 是两种可选的网络承载方式**。

## 职责划分（本仓库 vs 系统）

| 组件 | 职责 |
|------|------|
| **内核 VTL 模块（如 `vtl.ko`）** | 在本机 SCSI 子系统中枚举 **磁带机与机械手**；处理与磁带相关的 **SCSI 命令路径**（与物理带库类比）。 |
| **`vtladm` 用户态** | 元数据（库、槽位、磁带名、镜像路径等）、**`.vtltape` 镜像**、CLI/Web 管理；`transport` / IQN / 门户 / WWPN 用于 **部署意图声明** 与 **`transport show\|check\|guide`**；**不**在内核内实现 FC/iSCSI 帧级协议，**不**单独常驻 iSCSI/FC Target 进程。 |
| **Linux-IO / TGT / SCST、FC target 等** | 若要把存储 **暴露到网络**，在 **系统级** 配置 iSCSI Target 或 FC Target；与 `vtl.ko` 的衔接方式（块 LUN、passthrough、厂商方案等）需按目标平台与备份软件要求 **自行选型与验证**。 |
| **`vtladm-iscsi`（可选）** | 在 **有 `targetcli` 的 Linux** 上，将多步 Linux-IO 操作封装为 **`quick-export` / `quick-unexport`** 等（常见为 **FILEIO + iSCSI + LUN** 演示路径）；详见 [VTLADM-ISCSI.md](VTLADM-ISCSI.md)。 |

本仓库中的 **内核 VTL 模块** 在 **本机** 上枚举为传统 SCSI 磁带机与机械手设备（如 `/dev/st*`、`/dev/sg*`）。这是 **本地 SCSI 协议栈** 路径。**不包含**下列开箱即用的能力（需在系统或网关上另行建设）：

- **光纤通道 (FCP)** 目标模式端口（需 FC HBA、驱动与 target 子系统）
- **iSCSI** 目标门户（需 Linux-IO、SCST、TGT 等 target 框架）
- **将本虚拟磁带设备直接暴露为网络上的 SCSI tape**（视内核与 target 能力，可能需 **专用磁带网关** 或厂商方案）

## `vtladm` 中的「传输模式」配置

`vtl.conf` 中的 `transport` 用于 **声明部署意图** 与 **生成检查/集成指引**，便于运维与自动化脚本统一读取；**不会**单独启动 FC 或 iSCSI 目标。

| 取值 | 含义 |
|------|------|
| `local`（默认） | 仅本机内核 SCSI 设备；加载 `vtl.ko` 后使用 `mt`/`tar` 等访问 |
| `iscsi` | 计划通过 iSCSI 暴露存储；需自行部署 LIO/TGT 等，并配置 IQN、门户 |
| `fc` | 计划通过 FC target 暴露；需 FC 硬件与 target 模式支持 |

可选键（供文档化与后续工具使用）：

- `iscsi_iqn` — 目标 IQN 字符串
- `iscsi_portals` — 逗号分隔的 `host:port`
- `fc_wwpn` — 十六进制 WWPN（若有）

## 集成思路（概要）

1. **本地 SCSI（当前默认）**  
   加载内核模块 → 扫描 SCSI 总线 → 使用 `/dev/st*`；机械手等通过 `/dev/sg*`。此路径最贴近 **「目标端 + 模仿物理带库」** 的 SCSI 语义。

2. **iSCSI**  
   在 **另一层** 使用 Linux-IO (`targetcli`) 或 `tgtd` 创建 **iSCSI Target**。  
   注意：常见 iSCSI LUN 为 **块设备**；若备份软件严格要求 **网络上的 SCSI tape** 与物理带库完全一致，需验证 **target 与内核是否支持磁带类 LUN**，或采用 **专用磁带网关**（视发行版与内核能力而定）。  
   **本仓库**：除 **FILEIO** 的 `quick-export` 外，可选用 **`vtladm-iscsi library-export`**，将本机 **`vtl` 对应的 `/dev/sg*`** 以 **pscsi 多 LUN** 挂到同一 IQN（LUN0=机械手，LUN1…=磁带机），使 initiator 侧有机会按 **完整 SCSI 带库** 枚举与控制；是否成功取决于 **LIO/内核** 对给定 `sg` 节点的支持（详见 [VTLADM-ISCSI.md](VTLADM-ISCSI.md)）。

3. **FC**  
   需要支持 target 模式的 FC 卡与相应内核/用户态配置；不在本用户态工具范围内实现。

## 命令

```bash
vtladm transport show    # 当前配置与简要说明
vtladm transport check   # 本机线索：configfs、targetcli、内核 vtl 等
vtladm transport guide   # 打印本文件要点
```

将 `transport` 设为 `iscsi` 或 `fc` 后，请结合贵方存储架构完成 target 侧配置；`vtladm` 负责元数据与 `.vtltape` 镜像路径管理。

### 使用 `vtladm-iscsi` 简化 targetcli

同仓库构建的 **`vtladm-iscsi`** 二进制封装常见 `targetcli` 批处理（如 `quick-export` / `quick-unexport`），详见 [VTLADM-ISCSI.md](VTLADM-ISCSI.md)。

## 延伸阅读

- [TAPE-LIBRARY.md](TAPE-LIBRARY.md) — 物理磁带库的典型能力、合规与气隙等背景，便于对照本仓库 VTL 的语义边界。
- [SCSI.md](SCSI.md) — 内核 `vtl.ko` 的 **LUN 划分**、元素地址与 **CDB 命令矩阵**（与备份软件对接时优先阅读）。
