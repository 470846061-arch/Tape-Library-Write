//! `vtladm-iscsi` — 基于 `targetcli`（Linux-IO）的 iSCSI target 辅助工具。
//! 将多步 targetcli 操作合并为少量子命令；复杂场景仍可用 `batch` / `shell`。
//!
//! **须 root**（或具备写 configfs 的权限）。默认使用「演示」TPG 属性，生产环境请自行收紧 ACL。
//!
//! **`library-export`**：将本机 `vtl` 的 `/dev/sg*` 以 **pscsi 多 LUN** 挂到 iSCSI，使 initiator 侧可枚举 **介质转换器 + 多台磁带机**（依赖内核/LIO 对 pscsi 的支持）。

use clap::{Parser, Subcommand};
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Parser)]
#[command(name = "vtladm-iscsi")]
#[command(about = "Simplify Linux-IO / targetcli iSCSI target setup (wrapper around targetcli)")]
struct Cli {
    /// 调用 `sudo targetcli`（若本机 targetcli 需要 root）
    #[arg(long, global = true)]
    sudo: bool,
    /// 只打印将发送给 targetcli 的脚本，不执行
    #[arg(long, global = true)]
    dry_run: bool,
    /// `tpg1`（默认）：TPG 相关行使用路径前缀 `/iscsi/<iqn>/tpg1/...`（避免 IQN 中 **`:`** 与相对 `cd` 在部分 **Datera targetcli 2.1.x** 下解析失败）。`merged`：仅当 **`ls` 下该 IQN 无 `tpg1` 子项** 的旧 shell 使用（前缀为 `/iscsi/<iqn>/…`，无 `/tpg1`）。环境变量 **`VTL_ISCSI_SHELL_PATH`**（同取值）。
    #[arg(long = "iscsi-shell-path", global = true, value_name = "tpg1|merged")]
    iscsi_shell_path: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 检查 configfs、`targetcli` 是否在 PATH 中
    Check,
    /// 等价于向 targetcli 发送 `cd /` + `ls`（树状列出）
    Ls,
    /// 写入持久化配置（`saveconfig`）
    Save,
    /// 从文件读取一批 targetcli 命令并执行（每行一条，空行与 # 开头行忽略）
    Batch {
        /// 批处理文件路径
        path: PathBuf,
    },
    /// 交互式 targetcli（stdin/stdout 直连终端）
    Shell,
    /// 一键：FILEIO 后端 + iSCSI Target + TPG1（demo 模式）+ 单 LUN0 + 门户
    QuickExport {
        /// 现有镜像文件（如 .vtltape）；大小自动取文件长度
        #[arg(long)]
        file: PathBuf,
        /// 目标 IQN，例如 iqn.2026-05.org.vtladm:export1
        #[arg(long)]
        iqn: String,
        /// FILEIO 后端在 target 中的名称（仅字母数字与下划线）
        #[arg(long, default_value = "vtl_fileio0")]
        fileio_name: String,
        /// 监听地址（传给 portals create）
        #[arg(long, default_value = "0.0.0.0")]
        portal_ip: String,
        #[arg(long, default_value = "3260")]
        portal_port: u16,
    },
    /// 一键删除：`quick-export` 创建的 lun0、IQN、FILEIO 后端（顺序与创建相反）
    QuickUnexport {
        #[arg(long)]
        iqn: String,
        #[arg(long, default_value = "vtl_fileio0")]
        fileio_name: String,
    },
    /// 将本机 VTL 的 `/dev/sg*` 以 **pscsi 多 LUN** 导出：默认 LUN 为 lun0（机械手）、lun1…（磁带机）；`--lun-map` 仅允许与默认相同的 **0,1,2,…** 连续编号（与 targetcli 批处理 `luns/` 自动分配一致）
    LibraryExport {
        /// 后端名前缀（仅字母数字与下划线），将生成 `{id}_ch`、`{id}_dr0`…（LIO pscsi 对象名，作用类似「后端 / fileio 名」）
        #[arg(long)]
        id: String,
        #[arg(long)]
        iqn: String,
        /// 机械手（Medium changer）对应的 **字符设备**，如 `/dev/sg3`
        #[arg(long)]
        changer_sg: PathBuf,
        /// 每台磁带驱动器对应的 **字符设备**；按 **与 --lun-map 一致的顺序** 重复指定
        #[arg(long = "drive-sg", action = clap::ArgAction::Append)]
        drive_sg: Vec<PathBuf>,
        /// 逗号分隔 LUN：须为 **0,1,…,N**（与 `--drive-sg` 个数一致：1 个机械手 + 各磁带机）；用于显式写出与默认相同的映射。非连续编号不支持
        #[arg(long)]
        lun_map: Option<String>,
        #[arg(long, default_value = "0.0.0.0")]
        portal_ip: String,
        #[arg(long, default_value = "3260")]
        portal_port: u16,
    },
    /// 删除 `library-export` 创建的 LUN、IQN、pscsi 后端
    LibraryUnexport {
        #[arg(long)]
        id: String,
        #[arg(long)]
        iqn: String,
        /// 导出时 `--drive-sg` 的个数（不含机械手）；与未使用 `--lun-map` 的导出配对（LUN 为 lun0…lunN）
        #[arg(long)]
        drives: Option<u32>,
        /// 与导出时 `--lun-map` 相同的逗号分隔 LUN 列表（含机械手）；若指定则忽略 `--drives`
        #[arg(long)]
        lun_map: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IscsiShellPathStyle {
    /// 含 `tpg1`：命令前缀为 `/iscsi/<iqn>/tpg1`（与 path-prefixed `… set attribute` / `…/luns/` 一致）。
    Tpg1,
    /// 极少数旧版 shell：`ls` 下 IQN 无 `tpg1`，`luns/` 挂在 IQN 节点上；前缀为 `/iscsi/<iqn>`（无 `/tpg1`）。
    Merged,
}

fn resolve_iscsi_shell_path_style(cli: &Cli) -> Result<IscsiShellPathStyle, String> {
    let owned = cli
        .iscsi_shell_path
        .clone()
        .or_else(|| std::env::var("VTL_ISCSI_SHELL_PATH").ok());
    let raw = owned.as_deref().map(str::trim).filter(|s| !s.is_empty());
    match raw {
        None | Some("") => Ok(IscsiShellPathStyle::Tpg1),
        Some("tpg1") | Some("fb") => Ok(IscsiShellPathStyle::Tpg1),
        Some("merged") | Some("datera") => Ok(IscsiShellPathStyle::Merged),
        Some(other) => Err(format!(
            "invalid --iscsi-shell-path / VTL_ISCSI_SHELL_PATH={:?} (use tpg1 or merged)",
            other
        )),
    }
}

/// Base path for TPG-scoped commands: `/iscsi/<iqn>/tpg1` or `/iscsi/<iqn>` (`merged`).
/// Use **path-prefixed** lines like `{base}/luns/ create …` so IQN colons never break `cd` parsing
/// (configshell `pathstd` requires `/`; relative `cd <iqn>` is fragile across targetcli builds).
fn iscsi_export_tpg_base(style: IscsiShellPathStyle, iqn: &str) -> String {
    let mut s = String::from("/iscsi/");
    s.push_str(iqn);
    if matches!(style, IscsiShellPathStyle::Tpg1) {
        s.push_str("/tpg1");
    }
    s
}

fn run(cli: Cli) -> Result<(), String> {
    match &cli.command {
        Commands::Check => cmd_check(),
        Commands::Ls => run_targetcli_batch(&cli, "cd /\nls\n", "ls"),
        Commands::Save => run_targetcli_batch(&cli, "cd /\nsaveconfig\n", "saveconfig"),
        Commands::Batch { path } => {
            let raw =
                fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
            let body: String = raw
                .lines()
                .filter(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.starts_with('#')
                })
                .collect::<Vec<_>>()
                .join("\n");
            if body.is_empty() {
                return Err("batch file has no non-comment lines".into());
            }
            run_targetcli_batch(&cli, &ensure_trailing_exit(&body), "batch")
        }
        Commands::Shell => run_targetcli_shell(&cli),
        Commands::QuickExport {
            file,
            iqn,
            fileio_name,
            portal_ip,
            portal_port,
        } => cmd_quick_export(&cli, file, iqn, fileio_name, portal_ip, *portal_port),
        Commands::QuickUnexport { iqn, fileio_name } => cmd_quick_unexport(&cli, iqn, fileio_name),
        Commands::LibraryExport {
            id,
            iqn,
            changer_sg,
            drive_sg,
            lun_map,
            portal_ip,
            portal_port,
        } => cmd_library_export(
            &cli,
            id,
            iqn,
            changer_sg,
            drive_sg,
            lun_map.as_deref(),
            portal_ip,
            *portal_port,
        ),
        Commands::LibraryUnexport {
            id,
            iqn,
            drives,
            lun_map,
        } => cmd_library_unexport(&cli, id, iqn, drives.as_ref().copied(), lun_map.as_deref()),
    }
}

fn cmd_check() -> Result<(), String> {
    let configfs = Path::new("/sys/kernel/config/target");
    println!(
        "configfs /sys/kernel/config/target: {}",
        if configfs.is_dir() {
            "ok"
        } else {
            "missing (load target_core_mod / LIO modules?)"
        }
    );
    let ok = Command::new("which")
        .arg("targetcli")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    println!(
        "targetcli in PATH: {}",
        if ok {
            "yes"
        } else {
            "no (install targetcli / python3-rtslib-fb)"
        }
    );
    println!(
        "iSCSI: TPG commands use path-prefixed `/iscsi/<iqn>/tpg1/...` (IQN may contain `:`). Use `merged` / VTL_ISCSI_SHELL_PATH=merged only if `ls` under the IQN shows no `tpg1` child."
    );
    if !configfs.is_dir() || !ok {
        return Err("environment not ready for targetcli".into());
    }
    Ok(())
}

fn ensure_trailing_exit(script: &str) -> String {
    let t = script.trim();
    let mut s = t.to_string();
    if !s.lines().any(|l| {
        let x = l.trim();
        x == "exit" || x.starts_with("exit ")
    }) {
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("exit\n");
    } else if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

fn targetcli_cmd(sudo: bool) -> Command {
    let mut c = if sudo {
        let mut c = Command::new("sudo");
        c.arg("targetcli");
        c
    } else {
        Command::new("targetcli")
    };
    c.stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    c
}

fn targetcli_cmd_capture(sudo: bool) -> Command {
    let mut c = if sudo {
        let mut c = Command::new("sudo");
        c.arg("targetcli");
        c
    } else {
        Command::new("targetcli")
    };
    c.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

fn library_export_phase1_stderr_fatal(stderr: &str) -> bool {
    let l = stderr.to_ascii_lowercase();
    l.contains("cannot configure storageobject") || l.contains("already in use")
}

fn library_export_phase2_stderr_fatal(stderr: &str) -> bool {
    let l = stderr.to_ascii_lowercase();
    l.contains("cannot configure storageobject")
        || l.contains("already in use")
        || l.contains("unknown configuration group")
        || l.contains("no such path")
        || l.contains("no storage object named")
        || l.contains("wwn not valid")
}

fn run_targetcli_batch_captured(
    cli: &Cli,
    script: &str,
    label: &str,
    stderr_fatal: Option<fn(&str) -> bool>,
) -> Result<std::process::Output, String> {
    if cli.dry_run {
        println!("--- targetcli script ({}) ---\n{}", label, script);
        return Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
    }
    let mut child = targetcli_cmd_capture(cli.sudo)
        .spawn()
        .map_err(|e| format!("spawn targetcli: {} (try --sudo)", e))?;
    let stdin = child
        .stdin
        .as_mut()
        .ok_or_else(|| "targetcli: no stdin".to_string())?;
    stdin
        .write_all(script.as_bytes())
        .map_err(|e| format!("write stdin: {}", e))?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "targetcli ({}) exited {:?} stderr:\n{}",
            label,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let err = String::from_utf8_lossy(&output.stderr);
    if let Some(fatal) = stderr_fatal {
        if fatal(&err) {
            return Err(format!(
                "targetcli ({}) exited 0 but stderr indicates failure:\n{}",
                label, err
            ));
        }
    }
    Ok(output)
}

fn run_targetcli_batch(cli: &Cli, script: &str, label: &str) -> Result<(), String> {
    if cli.dry_run {
        println!("--- targetcli script ({}) ---\n{}", label, script);
        return Ok(());
    }
    let mut child = targetcli_cmd(cli.sudo)
        .spawn()
        .map_err(|e| format!("spawn targetcli: {} (try --sudo)", e))?;
    let stdin = child
        .stdin
        .as_mut()
        .ok_or_else(|| "targetcli: no stdin".to_string())?;
    stdin
        .write_all(script.as_bytes())
        .map_err(|e| format!("write stdin: {}", e))?;
    let st = child.wait().map_err(|e| format!("wait: {}", e))?;
    if !st.success() {
        return Err(format!("targetcli ({}) exited {:?}", label, st.code()));
    }
    Ok(())
}

fn run_targetcli_shell(cli: &Cli) -> Result<(), String> {
    if cli.dry_run {
        return Err("--dry-run is not compatible with shell".into());
    }
    let st = targetcli_cmd(cli.sudo)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("spawn targetcli shell: {}", e))?;
    if !st.success() {
        return Err(format!("targetcli shell exited {:?}", st.code()));
    }
    Ok(())
}

fn validate_export_id(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("export id empty".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("id: use only ASCII letters, digits, underscore".into());
    }
    Ok(())
}

fn validate_fileio_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("fileio_name empty".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("fileio_name: use only ASCII letters, digits, underscore".into());
    }
    Ok(())
}

fn validate_iqn(iqn: &str) -> Result<(), String> {
    let i = iqn.trim();
    if i.len() < 10 || !i.to_ascii_lowercase().starts_with("iqn.") {
        return Err("iqn must look like iqn.YYYY-MM.com.vendor:id".into());
    }
    if i.contains('_') {
        return Err(
            "iqn must not contain underscore _ (LIO/rtslib: WWN not valid); use hyphen - after the colon"
                .into(),
        );
    }
    Ok(())
}

/// 规范化 `/dev/sgN` 路径并校验为字符设备（Linux）；非 Unix 构建仅检查路径非空。
fn validate_sg_path(p: &Path) -> Result<String, String> {
    let meta = fs::metadata(p).map_err(|e| format!("stat {}: {}", p.display(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_char_device() {
            return Err(format!(
                "{} is not a character device (expected /dev/sgN from lsscsi -g)",
                p.display()
            ));
        }
    }
    let canon = p
        .canonicalize()
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned();
    Ok(canon)
}

/// 解析逗号分隔 LUN 列表；须恰好 `expected_len` 项、互不重复。
fn parse_comma_lun_map(s: &str, expected_len: usize) -> Result<Vec<u32>, String> {
    let parts: Vec<u32> = s
        .split(',')
        .map(|x| x.trim().parse::<u32>())
        .collect::<Result<_, _>>()
        .map_err(|_| "lun-map: invalid integer".to_string())?;
    if parts.len() != expected_len {
        return Err(format!(
            "lun-map: need {} comma-separated values (changer + each drive), got {}",
            expected_len,
            parts.len()
        ));
    }
    const MAX_LUN: u32 = 255;
    for n in &parts {
        if *n > MAX_LUN {
            return Err(format!("lun-map: LUN {} exceeds max {}", n, MAX_LUN));
        }
    }
    let mut chk = parts.clone();
    chk.sort_unstable();
    for w in chk.windows(2) {
        if w[0] == w[1] {
            return Err("lun-map: duplicate LUN".into());
        }
    }
    Ok(parts)
}

/// `lun-map` 经校验须为自 0 起的连续编号（与 targetcli `luns/ create` 自动分配一致）；非连续编号不支持批处理。
fn lun_map_must_be_consecutive_from_zero(luns: &[u32]) -> Result<(), String> {
    for (i, &v) in luns.iter().enumerate() {
        if v != i as u32 {
            return Err(format!(
                "lun-map must be consecutive from 0 (expected LUN {} at index {}, got {}); non-consecutive maps are not supported in targetcli batch mode",
                i, i, v
            ));
        }
    }
    Ok(())
}

/// Phase 1: pscsi backstores only (separate targetcli invocation so `/dev/sg` failures do not cascade).
fn library_export_phase1_targetcli_script_with_exit(
    id: &str,
    changer_path: &str,
    drive_paths: &[String],
) -> String {
    let mut s = library_export_phase1_targetcli_script_body(id, changer_path, drive_paths);
    s.push_str("exit\n");
    s
}

fn library_export_phase1_targetcli_script_body(
    id: &str,
    changer_path: &str,
    drive_paths: &[String],
) -> String {
    let ch_bs = format!("{}_ch", id);
    let mut s = String::from("cd /\n");
    s.push_str(&format!(
        "/backstores/pscsi create {} {}\n",
        ch_bs, changer_path
    ));
    for (i, p) in drive_paths.iter().enumerate() {
        let dr_bs = format!("{}_dr{}", id, i);
        s.push_str(&format!("/backstores/pscsi create {} {}\n", dr_bs, p));
    }
    s
}

/// Phase 2: iSCSI target, TPG attributes, LUNs, portals (assumes phase 1 backstores exist).
fn library_export_phase2_targetcli_script(
    id: &str,
    iqn: &str,
    drive_paths: &[String],
    portal_ip: &str,
    portal_port: u16,
    path_style: IscsiShellPathStyle,
) -> String {
    let ch_bs = format!("{}_ch", id);
    let base = iscsi_export_tpg_base(path_style, iqn);
    let mut s = String::from("cd /\n");
    s.push_str(&format!("/iscsi create {}\n", iqn));
    // Space before `set` — `…/tpg1/set` would wrongly treat `set` as a path component.
    s.push_str(&format!("{} set attribute authentication=0\n", base));
    s.push_str(&format!(
        "{} set attribute demo_mode_write_protect=0\n",
        base
    ));
    s.push_str(&format!("{} set attribute generate_node_acls=1\n", base));
    s.push_str(&format!(
        "{}/luns/ create /backstores/pscsi/{}\n",
        base, ch_bs
    ));
    for i in 0..drive_paths.len() {
        let dr_bs = format!("{}_dr{}", id, i);
        s.push_str(&format!(
            "{}/luns/ create /backstores/pscsi/{}\n",
            base, dr_bs
        ));
    }
    s.push_str(&format!(
        "{}/portals/ create {} {}\n",
        base, portal_ip, portal_port
    ));
    s.push_str("cd /\nsaveconfig\nexit\n");
    s
}

/// 生成 `library-export` 的 targetcli 脚本（单元测试与人工对照）：两阶段合并为一次批处理时的连续内容（无阶段间 `exit`）。
/// 实际执行使用 `library_export_phase1_targetcli_script_with_exit` + `library_export_phase2_targetcli_script` 两次调用。
#[cfg(test)]
fn library_export_targetcli_script(
    id: &str,
    iqn: &str,
    changer_path: &str,
    drive_paths: &[String],
    portal_ip: &str,
    portal_port: u16,
    path_style: IscsiShellPathStyle,
) -> String {
    let mut s = library_export_phase1_targetcli_script_body(id, changer_path, drive_paths);
    s.push_str(&library_export_phase2_targetcli_script(
        id,
        iqn,
        drive_paths,
        portal_ip,
        portal_port,
        path_style,
    ));
    s
}

fn library_unexport_targetcli_script(
    id: &str,
    iqn: &str,
    lun_numbers: &[u32],
    path_style: IscsiShellPathStyle,
) -> String {
    let base = iscsi_export_tpg_base(path_style, iqn);
    let mut luns: Vec<u32> = lun_numbers.to_vec();
    luns.sort_unstable();
    luns.reverse();
    let mut s = String::from("cd /\n");
    for lun in luns {
        s.push_str(&format!("{}/luns/ delete lun{}\n", base, lun));
    }
    s.push_str(&format!("/iscsi delete {}\n", iqn));
    let drives = lun_numbers.len().saturating_sub(1);
    for i in (0..drives).rev() {
        let dr_bs = format!("{}_dr{}", id, i);
        s.push_str(&format!("/backstores/pscsi delete {}\n", dr_bs));
    }
    let ch_bs = format!("{}_ch", id);
    s.push_str(&format!("/backstores/pscsi delete {}\n", ch_bs));
    s.push_str("cd /\nsaveconfig\nexit\n");
    s
}

fn cmd_library_export(
    cli: &Cli,
    id: &str,
    iqn: &str,
    changer_sg: &Path,
    drive_sg: &[PathBuf],
    lun_map: Option<&str>,
    portal_ip: &str,
    portal_port: u16,
) -> Result<(), String> {
    validate_export_id(id)?;
    validate_iqn(iqn)?;
    if drive_sg.is_empty() {
        return Err("need at least one --drive-sg (tape drives); use lsscsi -g to map LUNs".into());
    }

    if let Some(s) = lun_map {
        let v = parse_comma_lun_map(s, 1 + drive_sg.len())?;
        lun_map_must_be_consecutive_from_zero(&v)?;
    }

    let ch_path = validate_sg_path(changer_sg)?;
    let mut dr_paths: Vec<String> = Vec::with_capacity(drive_sg.len());
    for p in drive_sg {
        dr_paths.push(validate_sg_path(p)?);
    }

    eprintln!(
        "Note: pscsi passthrough requires kernel/LIO support for the given /dev/sg nodes. \
         Close other programs using these sg devices. TPG uses demo_mode_write_protect=0 and \
         generate_node_acls=1 — tighten for production."
    );

    let path_style = resolve_iscsi_shell_path_style(cli)?;
    let phase1 = library_export_phase1_targetcli_script_with_exit(id, &ch_path, &dr_paths);
    run_targetcli_batch_captured(
        cli,
        &phase1,
        "library-export-pscsi",
        Some(library_export_phase1_stderr_fatal),
    )?;
    let phase2 = library_export_phase2_targetcli_script(
        id,
        iqn,
        &dr_paths,
        portal_ip,
        portal_port,
        path_style,
    );
    run_targetcli_batch_captured(
        cli,
        &phase2,
        "library-export-iscsi",
        Some(library_export_phase2_stderr_fatal),
    )?;
    Ok(())
}

fn cmd_library_unexport(
    cli: &Cli,
    id: &str,
    iqn: &str,
    drives: Option<u32>,
    lun_map: Option<&str>,
) -> Result<(), String> {
    validate_export_id(id)?;
    validate_iqn(iqn)?;
    let lun_numbers: Vec<u32> = if let Some(s) = lun_map {
        const MAX_LUN: u32 = 255;
        let v: Vec<u32> = s
            .split(',')
            .map(|x| x.trim().parse::<u32>())
            .collect::<Result<_, _>>()
            .map_err(|_| "lun-map: invalid integer".to_string())?;
        if v.len() < 1 {
            return Err("lun-map: empty".into());
        }
        for n in &v {
            if *n > MAX_LUN {
                return Err(format!("lun-map: LUN {} exceeds max {}", n, MAX_LUN));
            }
        }
        let mut chk = v.clone();
        chk.sort_unstable();
        for w in chk.windows(2) {
            if w[0] == w[1] {
                return Err("lun-map: duplicate LUN".into());
            }
        }
        v
    } else if let Some(d) = drives {
        (0..=d).collect()
    } else {
        return Err(
            "library-unexport: specify --drives (with sequential lun0..) or --lun-map".into(),
        );
    };
    let path_style = resolve_iscsi_shell_path_style(cli)?;
    let script = library_unexport_targetcli_script(id, iqn, &lun_numbers, path_style);
    run_targetcli_batch(cli, &script, "library-unexport")
}

fn cmd_quick_export(
    cli: &Cli,
    file: &Path,
    iqn: &str,
    fileio_name: &str,
    portal_ip: &str,
    portal_port: u16,
) -> Result<(), String> {
    validate_fileio_name(fileio_name)?;
    validate_iqn(iqn)?;
    let meta = fs::metadata(file).map_err(|e| format!("stat {}: {}", file.display(), e))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", file.display()));
    }
    let size = meta.len();
    if size == 0 {
        return Err("backing file is empty".into());
    }

    let path_str = file
        .canonicalize()
        .unwrap_or_else(|_| file.to_path_buf())
        .to_string_lossy()
        .to_string();

    eprintln!(
        "Note: TPG uses demo_mode_write_protect=0 and generate_node_acls=1 — any initiator that can reach the portal may access this LUN. Tighten ACLs for production."
    );

    let path_style = resolve_iscsi_shell_path_style(cli)?;
    let base = iscsi_export_tpg_base(path_style, iqn);

    // 与手工 targetcli 等价的一组最短命令；路径中的反斜杠在 Linux 上不应出现。
    let script = format!(
        r#"cd /
/backstores/fileio create {fileio} {path} {size}
/iscsi create {iqn}
{base} set attribute authentication=0
{base} set attribute demo_mode_write_protect=0
{base} set attribute generate_node_acls=1
{base}/luns/ create /backstores/fileio/{fileio}
{base}/portals/ create {ip} {port}
cd /
saveconfig
exit
"#,
        fileio = fileio_name,
        path = path_str,
        size = size,
        iqn = iqn,
        base = base,
        ip = portal_ip,
        port = portal_port,
    );

    run_targetcli_batch(cli, &script, "quick-export")
}

fn cmd_quick_unexport(cli: &Cli, iqn: &str, fileio_name: &str) -> Result<(), String> {
    validate_fileio_name(fileio_name)?;
    validate_iqn(iqn)?;

    let path_style = resolve_iscsi_shell_path_style(cli)?;
    let base = iscsi_export_tpg_base(path_style, iqn);

    // 与多数 targetcli 版本兼容：先删 LUN，再删 target，再删 FILEIO
    let script = format!(
        r#"cd /
{base}/luns/ delete lun0
/iscsi delete {iqn}
/backstores/fileio delete {fileio}
cd /
saveconfig
exit
"#,
        iqn = iqn,
        fileio = fileio_name,
        base = base,
    );

    run_targetcli_batch(cli, &script, "quick-unexport")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_iqn_accepts_and_rejects() {
        assert!(validate_iqn("iqn.2026-05.org.example:vtl1").is_ok());
        assert!(validate_iqn("  iqn.2026-05.org.example:x  ").is_ok());
        assert!(validate_iqn("bad").is_err());
        assert!(validate_iqn("iqn.x").is_err());
        assert!(validate_iqn("iqn.2026-05.com.marstor:lib_bad").is_err());
        assert!(validate_iqn("iqn.2026-05.com.marstor:lib-good").is_ok());
    }

    #[test]
    fn test_validate_fileio_name() {
        assert!(validate_fileio_name("vtl0").is_ok());
        assert!(validate_fileio_name("Tape_01").is_ok());
        assert!(validate_fileio_name("a-b").is_err());
        assert!(validate_fileio_name("").is_err());
    }

    #[test]
    fn test_ensure_trailing_exit_appends_exit() {
        let s = ensure_trailing_exit("cd /\nls");
        assert!(s.lines().any(|l| l.trim() == "exit"));
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn test_ensure_trailing_exit_keeps_single_exit() {
        let s = ensure_trailing_exit("cd /\nexit");
        assert_eq!(s.matches("exit").count(), 1);
    }

    #[test]
    fn test_library_export_script_order() {
        let s = library_export_targetcli_script(
            "vtl1",
            "iqn.2026-05.org.example:lib",
            "/dev/sg10",
            &["/dev/sg11".into(), "/dev/sg12".into()],
            "0.0.0.0",
            3260,
            IscsiShellPathStyle::Tpg1,
        );
        let base = "/iscsi/iqn.2026-05.org.example:lib/tpg1";
        assert!(s.contains("/iscsi create iqn.2026-05.org.example:lib\n"));
        assert!(!s.contains("cd /iscsi\n"));
        assert!(!s.contains("cd iqn.2026-05.org.example:lib\n"));
        assert!(!s.contains("cd tpg1\n"));
        assert!(s.contains(&format!("{} set attribute authentication=0\n", base)));
        assert!(s.contains(&format!(
            "{} set attribute demo_mode_write_protect=0\n",
            base
        )));
        assert!(s.contains(&format!("{} set attribute generate_node_acls=1\n", base)));
        assert!(s.contains("/backstores/pscsi create vtl1_ch /dev/sg10"));
        assert!(s.contains("create vtl1_dr0 /dev/sg11"));
        assert!(s.contains("create vtl1_dr1 /dev/sg12"));
        assert!(s.contains(&format!(
            "{}/luns/ create /backstores/pscsi/vtl1_ch\n",
            base
        )));
        assert!(s.contains(&format!(
            "{}/luns/ create /backstores/pscsi/vtl1_dr0\n",
            base
        )));
        assert!(s.contains(&format!(
            "{}/luns/ create /backstores/pscsi/vtl1_dr1\n",
            base
        )));
        assert!(s.contains(&format!("{}/portals/ create 0.0.0.0 3260\n", base)));
        assert!(!s.contains("set parameter"));
        assert!(
            !s.lines().any(|line| {
                let t = line.trim();
                if !t.contains("/luns/ create /backstores/pscsi/") {
                    return false;
                }
                let n = t.split_whitespace().count();
                n > 3
            }),
            "luns/ create must receive only the backstore path (auto lun), got:\n{}",
            s
        );
    }

    #[test]
    fn test_library_export_script_merged_path_no_tpg1_segment() {
        let s = library_export_targetcli_script(
            "mm1",
            "iqn.2026-05.com.marstor:lib",
            "/dev/sg2",
            &["/dev/sg3".into()],
            "0.0.0.0",
            3260,
            IscsiShellPathStyle::Merged,
        );
        let base = "/iscsi/iqn.2026-05.com.marstor:lib";
        assert!(s.contains("/iscsi create iqn.2026-05.com.marstor:lib\n"));
        assert!(!s.contains("/tpg1"));
        assert!(s.contains(&format!("{}/luns/ create /backstores/pscsi/mm1_ch\n", base)));
    }

    #[test]
    fn test_library_unexport_script_merged_luns_path() {
        let s = library_unexport_targetcli_script(
            "vtl1",
            "iqn.2026-05.org.example:lib",
            &[0, 1],
            IscsiShellPathStyle::Merged,
        );
        let base = "/iscsi/iqn.2026-05.org.example:lib";
        assert!(s.contains(&format!("{}/luns/ delete lun1\n", base)));
        assert!(s.contains(&format!("{}/luns/ delete lun0\n", base)));
        assert!(!s.contains("tpg1"));
    }

    #[test]
    fn test_library_unexport_script_deletes_luns_high_to_low() {
        let s = library_unexport_targetcli_script(
            "vtl1",
            "iqn.2026-05.org.example:lib",
            &[0, 1, 2],
            IscsiShellPathStyle::Tpg1,
        );
        let base = "/iscsi/iqn.2026-05.org.example:lib/tpg1";
        assert!(s.contains(&format!("{}/luns/ delete lun2\n", base)));
        assert!(s.contains(&format!("{}/luns/ delete lun1\n", base)));
        assert!(s.contains(&format!("{}/luns/ delete lun0\n", base)));
        assert!(s.contains("/backstores/pscsi delete vtl1_dr1"));
        assert!(s.contains("/backstores/pscsi delete vtl1_dr0"));
        assert!(s.contains("/backstores/pscsi delete vtl1_ch"));
    }

    #[test]
    fn test_lun_map_must_reject_non_consecutive() {
        assert!(lun_map_must_be_consecutive_from_zero(&[0, 1, 2]).is_ok());
        assert!(lun_map_must_be_consecutive_from_zero(&[1, 2, 3]).is_err());
        assert!(lun_map_must_be_consecutive_from_zero(&[0, 2, 3]).is_err());
    }

    #[test]
    fn test_parse_comma_lun_map() {
        assert_eq!(parse_comma_lun_map("3,4,5", 3).unwrap(), vec![3, 4, 5]);
        assert!(parse_comma_lun_map("3,3,5", 3).is_err());
        assert!(parse_comma_lun_map("3,4", 3).is_err());
        assert!(parse_comma_lun_map("255,0", 2).is_ok());
        assert!(parse_comma_lun_map("0,256", 2).is_err());
    }

    #[test]
    fn test_validate_export_id() {
        assert!(validate_export_id("vtl_pt1").is_ok());
        assert!(validate_export_id("bad-id").is_err());
    }
}
