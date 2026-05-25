//! VTL stack patrol module: kernel ↔ SCSI ↔ DB/UI automation.
//!
//! Used by `vtladm patrol`, `vtl-patrol.timer`, and the thin `vtl-patrol.sh` wrapper.
//! Exit codes: `0` OK, `1` warning, `2` critical (matches monitoring hooks).

use std::fs;
use std::path::Path;
use std::process::Command;

use chrono::Local;

use crate::fab_transport::FabTransport;
use crate::iscsi_export::{
    delete_iscsi_library_export_by_name, list_iscsi_library_exports, IscsiLibraryExportRecord,
};
use crate::reconcile::{auto_sync_db_from_kernel_enabled, sync_db_from_kernel_all_libraries};
use crate::robot_sync::robot_sync_enabled;
use crate::{
    build_vtl_instances_kernel_spec, get_config, init_db, lio_hold, primary_vtl_conf_path,
    primary_vtl_statedir, scsi_tape_holders, VtlError,
};

/// Patrol finished successfully.
pub const PATROL_EXIT_OK: i32 = 0;
/// One or more warnings (or strict mode).
pub const PATROL_EXIT_WARN: i32 = 1;
/// Critical failure (e.g. module not loaded).
pub const PATROL_EXIT_CRIT: i32 = 2;

#[derive(Debug, Default)]
pub struct PatrolReport {
    pub warn_count: usize,
    pub crit_count: usize,
}

struct PatrolRunner {
    report: PatrolReport,
    strict: bool,
}

impl PatrolRunner {
    fn new() -> Self {
        let strict = std::env::var("VTL_PATROL_STRICT")
            .ok()
            .map(|s| {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        Self {
            report: PatrolReport::default(),
            strict,
        }
    }

    fn ok(&self, msg: impl AsRef<str>) {
        println!("OK  {}", msg.as_ref());
    }

    fn warn(&mut self, msg: impl AsRef<str>) {
        println!("WARN {}", msg.as_ref());
        self.report.warn_count += 1;
    }

    fn crit(&mut self, msg: impl AsRef<str>) {
        println!("CRIT {}", msg.as_ref());
        self.report.crit_count += 1;
    }

    fn exit_code(&self) -> i32 {
        if self.report.crit_count > 0 {
            return PATROL_EXIT_CRIT;
        }
        if self.report.warn_count > 0 && self.strict {
            return PATROL_EXIT_WARN;
        }
        PATROL_EXIT_OK
    }
}

/// Run full stack patrol; returns process exit code for systemd / monitoring.
pub fn run_patrol() -> i32 {
    match run_patrol_inner() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("CRIT patrol failed: {}", e);
            PATROL_EXIT_CRIT
        }
    }
}

fn run_patrol_inner() -> Result<i32, VtlError> {
    let mut pr = PatrolRunner::new();
    let now = Local::now().format("%Y-%m-%dT%H:%M:%S%z");
    println!("=== vtl patrol {} ===", now);

    check_kernel_module(&mut pr);
    check_sysfs_and_ioctl(&mut pr);
    check_scsi_bus(&mut pr);
    check_userspace_config(&mut pr);
    check_database_integrity(&mut pr);
    check_db_kernel_geometry(&mut pr);
    check_kernel_geom_safety(&mut pr);
    check_transport_links(&mut pr);
    check_iscsi_db_lio_alignment(&mut pr);
    check_robot_inventory_automation(&mut pr);
    check_web_optional(&mut pr);

    println!(
        "=== patrol summary: crit={} warn={} strict={} ===",
        pr.report.crit_count, pr.report.warn_count, pr.strict
    );
    Ok(pr.exit_code())
}

fn vtl_module_loaded() -> bool {
    if let Ok(m) = fs::read_to_string("/proc/modules") {
        return m.lines().any(|l| l.starts_with("vtl "));
    }
    Command::new("sh")
        .args(["-c", "lsmod 2>/dev/null | awk '{print $1}' | grep -qx vtl"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn check_kernel_module(pr: &mut PatrolRunner) {
    if vtl_module_loaded() {
        pr.ok("vtl.ko loaded");
    } else {
        pr.crit("vtl.ko not loaded (UI/DB cannot align SCSI without module)");
    }
}

fn check_sysfs_and_ioctl(pr: &mut PatrolRunner) {
    let sysfs = Path::new("/sys/module/vtl/parameters/vtl_instances");
    if sysfs.exists() {
        let spec = fs::read_to_string(sysfs)
            .unwrap_or_else(|_| "(unreadable)".into())
            .trim()
            .to_string();
        pr.ok(format!("sysfs vtl_instances={}", spec));
    } else if vtl_module_loaded() {
        pr.warn("sysfs vtl_instances unreadable (old ko?)");
    }

    if Path::new("/dev/vtl").exists() {
        pr.ok("/dev/vtl present (robot ioctl available)");
    } else if vtl_module_loaded() {
        pr.warn("/dev/vtl missing (upgrade vtl.ko for robot ioctl)");
    }
}

fn check_scsi_bus(pr: &mut PatrolRunner) {
    let lsscsi = Command::new("sh")
        .arg("-c")
        .arg("command -v lsscsi")
        .status();
    if lsscsi.ok().map_or(true, |s| !s.success()) {
        pr.warn("lsscsi not installed");
        return;
    }
    let out = match Command::new("lsscsi").arg("-g").output() {
        Ok(o) => o,
        Err(_) => {
            pr.warn("lsscsi execution failed");
            return;
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().filter(|l| l.contains("VTL")).collect();
    if lines.is_empty() {
        if vtl_module_loaded() {
            pr.crit("lsscsi: no VTL SCSI rows (wait scan or vtl-scsi-scan-all-hosts.sh)");
        }
    } else {
        pr.ok(format!("lsscsi: {} vtl SCSI device line(s)", lines.len()));
        for line in lines {
            println!("    {}", line);
        }
    }
}

fn check_userspace_config(pr: &mut PatrolRunner) {
    let conf = primary_vtl_conf_path();
    if conf.is_file() {
        pr.ok(format!("vtl.conf present ({})", conf.display()));
    } else {
        pr.warn("vtl.conf missing — run vtladm init-config or Web setup wizard");
    }

    let cache = primary_vtl_statedir().join(".last_vtl_instances_spec");
    if cache.is_file() {
        let cached = fs::read_to_string(&cache)
            .unwrap_or_default()
            .trim()
            .to_string();
        pr.ok(format!("last applied spec (vtladm cache): {}", cached));
        let sysfs = Path::new("/sys/module/vtl/parameters/vtl_instances");
        if sysfs.is_file() {
            let sys = fs::read_to_string(sysfs).unwrap_or_default();
            let sys = sys.trim();
            if !sys.is_empty() && sys != "(null)" && sys != cached {
                pr.warn(
                    "sysfs vtl_instances differs from .last_vtl_instances_spec (manual insmod or drift?)",
                );
            }
        }
    } else if vtl_module_loaded() {
        pr.warn("no .last_vtl_instances_spec (DB geometry may never have been applied to kernel)");
    }

    let db = get_config().db_path.clone();
    if db.is_file() {
        pr.ok(format!("vtl.db present ({})", db.display()));
    } else if vtl_module_loaded() {
        pr.warn("vtl.db missing — create library via Web or CLI");
    }
}

/// Periodic kernel→DB catalog hints (runtime robot display stays on vtl.ko GET_INVENTORY).
fn check_robot_inventory_automation(pr: &mut PatrolRunner) {
    if !robot_sync_enabled() {
        return;
    }
    if !Path::new("/dev/vtl").exists() {
        pr.warn("robot_sync enabled but /dev/vtl missing — skip inventory automation");
        return;
    }
    let db_path = get_config().db_path.clone();
    if !db_path.is_file() {
        return;
    }

    if !auto_sync_db_from_kernel_enabled() {
        pr.ok("auto_sync_db_from_kernel=false (use vtl-robot-sync.timer or sync-db)");
        return;
    }
    match sync_db_from_kernel_all_libraries() {
        Ok(r) => {
            pr.ok(format!(
                "kernel→DB sync-db ({} librar{}, {} catalog slot hint(s))",
                r.libraries,
                if r.libraries == 1 { "y" } else { "ies" },
                r.tapes_updated
            ));
        }
        Err(e) => {
            if let VtlError::InvalidParameter(ref m) = e {
                if m.contains("truncated") {
                    pr.warn(format!("sync-db skipped: {}", m));
                    return;
                }
            }
            pr.warn(format!("sync-db failed: {}", e));
        }
    }
}

fn check_database_integrity(pr: &mut PatrolRunner) {
    match init_db() {
        Ok(conn) => {
            pr.ok("SQLite database opens");
            let libs: i64 = conn
                .query_row("SELECT COUNT(*) FROM vtl_libraries", [], |r| r.get(0))
                .unwrap_or(0);
            let online: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM vtl_libraries WHERE name NOT IN ('__offline__', 'default')",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if online == 0 {
                pr.warn("no online libraries in DB — create one via Web /admin/library");
            } else {
                pr.ok(format!("DB: {} library row(s), {} online", libs, online));
            }
            let exports = list_iscsi_library_exports(&conn).unwrap_or_default();
            if exports.is_empty() {
                pr.ok("DB: no iscsi_library_exports rows");
            } else {
                pr.ok(format!(
                    "DB: {} iSCSI export record(s) persisted",
                    exports.len()
                ));
            }
        }
        Err(e) => pr.crit(format!("SQLite init_db failed: {}", e)),
    }
}

fn read_vtl_module_param(name: &str) -> Option<String> {
    let p = format!("/sys/module/vtl/parameters/{}", name);
    if !Path::new(&p).is_file() {
        return None;
    }
    fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().trim_end_matches('\n').to_string())
}

fn check_kernel_geom_safety(pr: &mut PatrolRunner) {
    if !vtl_module_loaded() {
        return;
    }
    let allow = read_vtl_module_param("allow_hot_geom").unwrap_or_default();
    if matches!(allow.as_str(), "Y" | "y" | "1") {
        pr.warn(
            "kernel allow_hot_geom=Y — hot SET_INSTANCES is risky; use allow_hot_geom=N and vtl-kernelctl reload",
        );
    } else {
        pr.ok("kernel allow_hot_geom=N (SET_INSTANCES disabled by default)");
    }
    if lio_hold::lio_pscsi_references_vtl_sg() {
        pr.warn(
            "LIO pscsi still references VTL /dev/sg — unexport before vtl-kernelctl reload or geometry change",
        );
    }
    use scsi_tape_holders::VtlHoldersProbe;
    match scsi_tape_holders::probe_vtl_device_holders() {
        VtlHoldersProbe::Busy => {
            pr.warn("VTL st/sg nodes have open handles — stop backup before module reload");
        }
        VtlHoldersProbe::FuserUnavailable => {
            pr.warn("fuser missing — cannot verify VTL device holders before reload");
        }
        VtlHoldersProbe::Clear => {}
    }
    if let Some(script) = get_config().kernel_vtl_reload_script.as_ref() {
        if script.is_file() {
            pr.ok(format!(
                "kernel_vtl_reload_script configured: {}",
                script.display()
            ));
        } else {
            pr.warn(format!(
                "kernel_vtl_reload_script missing: {}",
                script.display()
            ));
        }
    } else if read_vtl_module_param("allow_hot_geom")
        .map(|s| !matches!(s.as_str(), "Y" | "y" | "1"))
        .unwrap_or(true)
    {
        pr.warn(
            "no kernel_vtl_reload_script — DB geometry changes need manual vtl-kernelctl reload",
        );
    }
}

fn check_db_kernel_geometry(pr: &mut PatrolRunner) {
    if !vtl_module_loaded() {
        return;
    }
    let db_path = get_config().db_path.clone();
    if !db_path.is_file() {
        return;
    }
    let sysfs_path = Path::new("/sys/module/vtl/parameters/vtl_instances");
    let sysfs = if sysfs_path.is_file() {
        fs::read_to_string(sysfs_path)
            .unwrap_or_default()
            .trim()
            .to_string()
    } else {
        String::new()
    };
    match build_vtl_instances_kernel_spec() {
        Ok(spec) => {
            if sysfs.is_empty() || sysfs == "(null)" {
                pr.warn(format!(
                    "DB expects vtl_instances={} but sysfs empty — apply ioctl or vtl-kernelctl reload",
                    spec
                ));
            } else if sysfs != spec {
                pr.warn(format!(
                    "kernel vtl_instances={} != DB spec {} — align via library create/config or ioctl",
                    sysfs, spec
                ));
            } else {
                pr.ok(format!("DB/kernel geometry match: {}", spec));
            }
        }
        Err(e) => pr.warn(format!("cannot build DB vtl_instances spec: {}", e)),
    }
}

fn check_transport_links(pr: &mut PatrolRunner) {
    let c = get_config();
    let mode = c.transport.as_conf_str();
    pr.ok(format!("vtl.conf transport={}", mode));
    match c.transport {
        FabTransport::LocalScsi => {
            if vtl_module_loaded() {
                pr.ok("local SCSI: vtl.ko provides /dev/st* /dev/sg*");
            }
        }
        FabTransport::Iscsi => {
            if Path::new("/sys/kernel/config/target").exists() {
                pr.ok("iSCSI: configfs /sys/kernel/config/target present");
            } else {
                pr.warn(
                    "transport=iscsi but LIO configfs missing — install targetcli / LIO modules",
                );
            }
            if !command_exists("targetcli") {
                pr.warn("transport=iscsi but targetcli not in PATH");
            }
        }
        FabTransport::Fc => {
            if Path::new("/sys/class/fc_host").exists() {
                pr.ok("FC: /sys/class/fc_host present (configure FC target on OS)");
            } else {
                pr.warn("transport=fc but no FC host in sysfs — check HBA / target mode");
            }
            if c.fc_wwpn.is_none() {
                pr.warn("transport=fc but fc_wwpn unset in vtl.conf");
            }
        }
    }
}

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {} >/dev/null 2>&1", name))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Best-effort LIO tree text for IQN / pscsi name checks.
fn probe_lio_tree_text() -> Option<String> {
    if !Path::new("/sys/kernel/config/target").exists() {
        return None;
    }
    if command_exists("targetcli") {
        let out = Command::new("sh")
            .arg("-c")
            .arg("timeout 25 targetcli ls / 2>/dev/null")
            .output()
            .ok()?;
        if out.status.success() || !out.stdout.is_empty() {
            return Some(String::from_utf8_lossy(&out.stdout).into_owned());
        }
    }
    let mut buf = String::from("--- configfs iscsi ---\n");
    let iscsi = Path::new("/sys/kernel/config/target/iscsi");
    if iscsi.is_dir() {
        if let Ok(rd) = fs::read_dir(iscsi) {
            for e in rd.flatten() {
                buf.push_str(&e.file_name().to_string_lossy());
                buf.push('\n');
            }
        }
    }
    if buf.lines().count() > 2 {
        Some(buf)
    } else {
        None
    }
}

fn lsscsi_vtl_text() -> String {
    Command::new("lsscsi")
        .args(["-g"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn patrol_validate_iscsi_record(
    pr: &mut PatrolRunner,
    library: &str,
    rec: &IscsiLibraryExportRecord,
    lio: Option<&str>,
    lsscsi_text: &str,
    conn: &rusqlite::Connection,
) {
    let lio_text = match lio {
        Some(t) => t,
        None => {
            pr.warn(format!(
                "library '{}': DB has iSCSI export but LIO cannot be probed (targetcli/configfs)",
                library
            ));
            return;
        }
    };
    if !lio_text.contains(&rec.iqn) {
        pr.warn(format!(
            "library '{}': DB IQN '{}' not in LIO — stale record or manual targetcli change; run library-unexport or re-export",
            library, rec.iqn
        ));
    } else {
        pr.ok(format!("library '{}': IQN present in LIO", library));
    }
    let ch_bs = format!("{}_ch", rec.export_id);
    if !lio_text.contains(&ch_bs) {
        pr.warn(format!(
            "library '{}': pscsi backstore '{}' not in LIO",
            library, ch_bs
        ));
    }
    if !Path::new(&rec.changer_sg).exists() {
        pr.warn(format!(
            "library '{}': changer_sg {} not on filesystem (rescan or update DB)",
            library, rec.changer_sg
        ));
    }
    for (i, sg) in rec.drive_sg.iter().enumerate() {
        if !Path::new(sg).exists() {
            pr.warn(format!(
                "library '{}': drive_sg[{}] {} missing",
                library, i, sg
            ));
        }
        if !lsscsi_text.contains(sg) {
            pr.warn(format!(
                "library '{}': drive_sg[{}] {} not in lsscsi (vtl host)",
                library, i, sg
            ));
        }
    }
    let library_id = match crate::resolve_library_id(conn, library) {
        Ok(id) => id,
        Err(_) => return,
    };
    let drive_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM drives WHERE library_id = ?1",
            rusqlite::params![library_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if rec.drive_sg.len() as i64 != drive_count {
        pr.warn(format!(
            "library '{}': export record has {} drive sg path(s) but DB has {} drive row(s) — rescan sg and re-export",
            library,
            rec.drive_sg.len(),
            drive_count
        ));
    }
}

fn check_iscsi_db_lio_alignment(pr: &mut PatrolRunner) {
    let conn = match init_db() {
        Ok(c) => c,
        Err(_) => return,
    };
    let exports = match list_iscsi_library_exports(&conn) {
        Ok(v) => v,
        Err(e) => {
            pr.warn(format!("list iscsi_library_exports: {}", e));
            return;
        }
    };
    if exports.is_empty() {
        return;
    }
    let lio = probe_lio_tree_text();
    let lsscsi_text = lsscsi_vtl_text();
    let clear_stale = std::env::var("VTL_PATROL_CLEAR_STALE_ISCSI_DB")
        .ok()
        .map(|s| {
            matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);

    for (library, rec) in &exports {
        patrol_validate_iscsi_record(pr, library, rec, lio.as_deref(), &lsscsi_text, &conn);
        if clear_stale {
            if let Some(ref text) = lio {
                if !text.contains(&rec.iqn) {
                    if delete_iscsi_library_export_by_name(&conn, library).is_ok() {
                        pr.ok(format!(
                            "library '{}': cleared stale iSCSI DB record (IQN absent in LIO)",
                            library
                        ));
                    }
                }
            }
        }
    }

    if let Some(ref text) = lio {
        if text.contains("pscsi") && exports.is_empty() {
            pr.warn("LIO has pscsi objects but no iscsi_library_exports in DB — export via Web or record will not auto-unexport");
        }
    }
}

fn check_web_optional(pr: &mut PatrolRunner) {
    let host = std::env::var("VTLADM_WEB_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("VTLADM_WEB_PORT").unwrap_or_else(|_| "8765".to_string());
    let url = format!("http://{}:{}/api/setup/status", host, port);
    let curl = Command::new("curl")
        .arg("-fsS")
        .arg("-m")
        .arg("3")
        .arg(&url)
        .status();
    match curl {
        Ok(s) if s.success() => pr.ok(format!("Web UI responds on {}:{}", host, port)),
        Ok(_) => pr.warn(format!(
            "Web UI not reachable on {}:{} (service stopped or bound elsewhere)",
            host, port
        )),
        Err(_) => pr.warn(format!(
            "Web UI probe skipped (curl missing?) on {}:{}",
            host, port
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patrol_exit_code_crit_over_warn() {
        let mut pr = PatrolRunner::new();
        pr.warn("w");
        pr.crit("c");
        assert_eq!(pr.exit_code(), PATROL_EXIT_CRIT);
    }

    #[test]
    fn patrol_exit_code_warn_only_default_lenient() {
        let mut pr = PatrolRunner::new();
        pr.warn("w");
        assert_eq!(pr.exit_code(), PATROL_EXIT_OK);
    }

    #[test]
    fn patrol_exit_code_warn_only_when_strict() {
        std::env::set_var("VTL_PATROL_STRICT", "1");
        let mut pr = PatrolRunner::new();
        assert!(pr.strict);
        pr.warn("w");
        assert_eq!(pr.exit_code(), PATROL_EXIT_WARN);
        std::env::remove_var("VTL_PATROL_STRICT");
    }
}
