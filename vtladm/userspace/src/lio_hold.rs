//! Detect LIO pscsi backstores still referencing VTL `/dev/sg*` (configfs / targetcli).
//! Hot `SET_INSTANCES` ioctl while exported can corrupt kernel state (see `docs/SCSI.md`).

use std::path::Path;
use std::process::Command;

use crate::scsi_tape_holders;

/// True when any VTL SCSI node path appears under LIO configfs or `targetcli` pscsi listing.
pub(crate) fn lio_pscsi_references_vtl_sg() -> bool {
    let paths = scsi_tape_holders::vtl_scsi_device_paths();
    if paths.is_empty() {
        return false;
    }
    let configfs = Path::new("/sys/kernel/config/target");
    if configfs.is_dir() {
        for path in &paths {
            if configfs_references_path(configfs, path) {
                return true;
            }
        }
    }
    targetcli_lists_vtl_path(&paths)
}

fn configfs_references_path(root: &Path, dev_path: &Path) -> bool {
    let needle = dev_path.to_string_lossy();
    walk_configfs_for_needle(root, needle.as_ref())
}

fn walk_configfs_for_needle(dir: &Path, needle: &str) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for ent in entries.flatten() {
        let p = ent.path();
        if p.is_dir() {
            if walk_configfs_for_needle(&p, needle) {
                return true;
            }
            continue;
        }
        if p.is_file() {
            if let Ok(s) = std::fs::read_to_string(&p) {
                if s.contains(needle) {
                    return true;
                }
            }
        }
    }
    false
}

fn targetcli_lists_vtl_path(paths: &[std::path::PathBuf]) -> bool {
    let Some(out) = Command::new("sh")
        .arg("-c")
        .arg("timeout 20 targetcli ls /backstores/pscsi 2>/dev/null")
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    paths
        .iter()
        .any(|p| text.contains(&p.to_string_lossy().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_paths_never_lio_busy() {
        assert!(!targetcli_lists_vtl_path(&[]));
    }
}
