//! Echo full-channel SCSI rescan on each host whose `proc_name` is **`vtl`**.
//! Used when the DB-derived `vtl_instances` spec matches the last applied spec
//! (see `maybe_reload_kernel_vtl_after_db_change` and `docs/SCSI.md` §1f).
//! Does **not** add or remove SCSI hosts — instance count / geometry changes still need
//! `VTL_IOCTL_SET_INSTANCES` or `rmmod`/`insmod`.

use std::path::{Path, PathBuf};

/// sysfs base for `scsi_host` entries.
const SYS_SCSI_HOST: &str = "/sys/class/scsi_host";

/// Same payload as `echo '- - -' > scan` and **`vtl-scsi-rescan.sh`** (`printf '%s\n' '- - -'`).
const SCAN_LINE: &[u8] = b"- - -\n";

/// After SET_INSTANCES / RESIZE_GEOMETRY / rmmod+insmod: enumerate new LUNs on each vtl host.
/// Skip with **`VTL_NO_SCSI_RESCAN_AFTER_GEOM=1`**. Returns `ok` / `skipped` / `failed`.
pub(crate) fn try_scsi_rescan_after_geom_change(context: &str) -> &'static str {
    if std::env::var("VTL_NO_SCSI_RESCAN_AFTER_GEOM")
        .ok()
        .as_deref()
        == Some("1")
    {
        return "skipped";
    }
    match scsi_rescan_vtl_hosts() {
        Ok(()) => {
            crate::log_message(&format!(
                "scsi_rescan: {} — all vtl hosts scanned (lsscsi should show 1 changer + N drives per library)",
                context
            ));
            "ok"
        }
        Err(e) => {
            crate::log_error(
                "scsi_rescan_vtl",
                &format!(
                    "{}: {} — run: sh /opt/vtladm/scripts/vtl-scsi-scan-all-hosts.sh 5",
                    context, e
                ),
            );
            "failed"
        }
    }
}

/// Trigger kernel SCSI scan on every `vtl` host (same as `echo "- - -" > .../scan`).
pub(crate) fn scsi_rescan_vtl_hosts() -> Result<(), String> {
    let stagger_ms: u64 = std::env::var("VTL_SCSI_RESCAN_STAGGER_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    scsi_rescan_vtl_hosts_under(Path::new(SYS_SCSI_HOST), stagger_ms)
}

/// Parsed `hostN` directory name for stable ordering (`host2` before `host10`).
fn scsi_host_dir_index(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    name.strip_prefix("host")?.parse().ok()
}

/// Rescan all `vtl` SCSI hosts under `sys_scsi_host` (normally `/sys/class/scsi_host`).
/// `stagger_ms` is applied **between** writes only (not after the last host).
pub(crate) fn scsi_rescan_vtl_hosts_under(
    sys_scsi_host: &Path,
    stagger_ms: u64,
) -> Result<(), String> {
    use std::thread;
    use std::time::Duration;

    let entries = std::fs::read_dir(sys_scsi_host)
        .map_err(|e| format!("read {}: {}", sys_scsi_host.display(), e))?;

    let mut scan_targets: Vec<PathBuf> = Vec::new();
    for ent in entries {
        let ent = ent.map_err(|e| format!("readdir {}: {}", sys_scsi_host.display(), e))?;
        let path = ent.path();
        let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !fname.starts_with("host") {
            continue;
        }
        let proc_path = path.join("proc_name");
        let scan_path = path.join("scan");
        if !scan_path.exists() {
            continue;
        }
        let proc_name = std::fs::read_to_string(&proc_path).unwrap_or_default();
        if proc_name.trim() != "vtl" {
            continue;
        }
        scan_targets.push(scan_path);
    }

    scan_targets.sort_by_key(|p| p.parent().and_then(scsi_host_dir_index).unwrap_or(u32::MAX));

    let n = scan_targets.len();
    if n == 0 {
        return Err(format!(
            "no {}/*/proc_name == vtl (is vtl.ko loaded and scsi_add_host completed?)",
            sys_scsi_host.display()
        ));
    }

    for (i, scan_path) in scan_targets.iter().enumerate() {
        std::fs::write(scan_path, SCAN_LINE)
            .map_err(|e| format!("write {} (need root?): {}", scan_path.display(), e))?;
        if stagger_ms > 0 && i + 1 < n {
            thread::sleep(Duration::from_millis(stagger_ms));
        }
    }
    Ok(())
}

/// Rescan one SCSI host (e.g. `host33` from `lsscsi`) when `proc_name` is **`vtl`**.
pub(crate) fn scsi_rescan_scsi_host(host: u32) -> Result<(), String> {
    let path = Path::new(SYS_SCSI_HOST).join(format!("host{}", host));
    let scan_path = path.join("scan");
    if !scan_path.exists() {
        return Err(format!(
            "no {} (SCSI host {} missing?)",
            scan_path.display(),
            host
        ));
    }
    let proc_path = path.join("proc_name");
    let proc_name = std::fs::read_to_string(&proc_path).unwrap_or_default();
    if proc_name.trim() != "vtl" {
        return Err(format!(
            "host{} proc_name={:?} (not vtl)",
            host,
            proc_name.trim()
        ));
    }
    std::fs::write(&scan_path, SCAN_LINE)
        .map_err(|e| format!("write {} (need root?): {}", scan_path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn temp_scsi_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("vtladm_scsi_rescan_{}", Uuid::new_v4()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn rescan_writes_only_vtl_hosts_deterministic_order() {
        let root = temp_scsi_root();
        let scsi = root.join("scsi_host");
        fs::create_dir_all(scsi.join("host10")).expect("mkdir");
        fs::create_dir_all(scsi.join("host2")).expect("mkdir");
        fs::create_dir_all(scsi.join("host_other")).expect("mkdir");
        fs::write(scsi.join("host10/proc_name"), "vtl\n").unwrap();
        fs::write(scsi.join("host10/scan"), "").unwrap();
        fs::write(scsi.join("host2/proc_name"), "vtl\n").unwrap();
        fs::write(scsi.join("host2/scan"), "").unwrap();
        fs::write(scsi.join("host_other/proc_name"), "megaraid_sas\n").unwrap();
        fs::write(scsi.join("host_other/scan"), "").unwrap();

        scsi_rescan_vtl_hosts_under(&scsi, 0).expect("ok");

        assert_eq!(
            fs::read_to_string(scsi.join("host2/scan")).unwrap(),
            std::str::from_utf8(SCAN_LINE).unwrap()
        );
        assert_eq!(
            fs::read_to_string(scsi.join("host10/scan")).unwrap(),
            std::str::from_utf8(SCAN_LINE).unwrap()
        );
        assert_eq!(
            fs::read_to_string(scsi.join("host_other/scan")).unwrap(),
            ""
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rescan_errors_when_no_vtl_host() {
        let root = temp_scsi_root();
        let scsi = root.join("scsi_host");
        fs::create_dir_all(scsi.join("host0")).expect("mkdir");
        fs::write(scsi.join("host0/proc_name"), "qla2xxx\n").unwrap();
        fs::write(scsi.join("host0/scan"), "").unwrap();

        let e = scsi_rescan_vtl_hosts_under(&scsi, 0).expect_err("no vtl");
        assert!(e.contains("no "), "{}", e);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rescan_errors_on_empty_scsi_host_dir() {
        let root = temp_scsi_root();
        let scsi = root.join("empty");
        fs::create_dir_all(&scsi).expect("mkdir");

        let e = scsi_rescan_vtl_hosts_under(&scsi, 0).expect_err("empty");
        assert!(e.contains("no "), "{}", e);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_line_matches_echo_minus_minus_minus() {
        assert_eq!(SCAN_LINE, b"- - -\n");
    }
}
