//! 声明式「传输 /  fabric」模式：本地 SCSI、iSCSI、FC 意图与运维指引（不单独启动网络 target）。

use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FabTransport {
    /// 本机内核 VTL 模块枚举的 /dev/st*、/dev/sg* 等
    LocalScsi,
    /// 计划通过 iSCSI 暴露（需 LIO/TGT 等，见 docs/TRANSPORT.md）
    Iscsi,
    /// 计划通过 FC target 暴露（需硬件与 target 模式）
    Fc,
}

impl Default for FabTransport {
    fn default() -> Self {
        Self::LocalScsi
    }
}

impl FabTransport {
    pub(crate) fn as_conf_str(&self) -> &'static str {
        match self {
            FabTransport::LocalScsi => "local",
            FabTransport::Iscsi => "iscsi",
            FabTransport::Fc => "fc",
        }
    }

    pub(crate) fn describe(&self) -> &'static str {
        match self {
            FabTransport::LocalScsi => {
                "本机 SCSI：加载内核模块 vtl.ko 后使用 /dev/st*、机械手 /dev/sg*。"
            }
            FabTransport::Iscsi => {
                "iSCSI：vtladm 仅管理元数据与镜像路径；请在系统上部署 Linux-IO/TGT 等 iSCSI target。"
            }
            FabTransport::Fc => {
                "FC：vtladm 不配置 HBA target 模式；请在支持 FC target 的平台上单独配置。"
            }
        }
    }
}

pub(crate) fn parse_fab_transport(s: &str) -> Option<FabTransport> {
    match s.trim().to_ascii_lowercase().as_str() {
        "local" | "scsi" | "local_scsi" | "localscsi" => Some(FabTransport::LocalScsi),
        "iscsi" => Some(FabTransport::Iscsi),
        "fc" | "fibre" | "fiber" | "fcp" => Some(FabTransport::Fc),
        _ => None,
    }
}

pub(crate) fn transport_show() -> Result<(), super::VtlError> {
    let c = super::get_config();
    println!(
        "Fabric transport (configured): {}",
        c.transport.as_conf_str()
    );
    println!("{}", c.transport.describe());
    println!("  db_path:    {}", c.db_path.display());
    println!("  tape_dir:   {}", c.tape_dir.display());
    println!("  log_dir:    {}", c.log_dir.display());
    println!("  log_rotate: max {} bytes per file", c.log_max_bytes);
    if let Some(ref iqn) = c.iscsi_iqn {
        println!("  iscsi_iqn:  {}", iqn);
    }
    if let Some(ref p) = c.iscsi_portals {
        println!("  iscsi_portals: {}", p);
    }
    if let Some(ref w) = c.fc_wwpn {
        println!("  fc_wwpn:    {}", w);
    }
    println!();
    println!("详细说明: vtladm transport guide");
    Ok(())
}

pub(crate) fn transport_guide() -> Result<(), super::VtlError> {
    const MD: &str = include_str!("../docs/TRANSPORT.md");
    print!("{}", MD);
    Ok(())
}

fn command_in_path(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub(crate) fn transport_check() -> Result<(), super::VtlError> {
    let c = super::get_config();
    println!(
        "Transport check (configured: {})",
        c.transport.as_conf_str()
    );
    println!();

    let configfs = std::path::Path::new("/sys/kernel/config/target");
    println!(
        "  Linux-IO configfs target dir: {}",
        if configfs.is_dir() {
            "present (/sys/kernel/config/target)"
        } else {
            "not mounted (load target modules / start target service if using LIO)"
        }
    );

    println!(
        "  targetcli in PATH: {}",
        if command_in_path("targetcli") {
            "yes"
        } else {
            "no (install targetcli-fb / python3-rtslib on RHEL family, etc.)"
        }
    );

    let vtl_sysfs = std::path::Path::new("/sys/kernel/vtl");
    println!(
        "  /sys/kernel/vtl: {}",
        if vtl_sysfs.exists() {
            "present (kernel vtl may be loaded)"
        } else {
            "absent (kernel module may not be loaded)"
        }
    );

    println!();
    println!("本工具当前不内置 iSCSI/FC target 进程；以上为环境线索。");
    Ok(())
}
