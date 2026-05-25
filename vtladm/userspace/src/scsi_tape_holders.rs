//! Probe open handles on VTL SCSI device nodes (`lsscsi -g` paths) before `rmmod vtl`.
//! Kylin 4.19: unloading the module while tape/sg nodes are open can kdump.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VtlHoldersProbe {
    /// No fuser holders on VTL nodes (or no VTL nodes enumerated).
    Clear,
    /// At least one VTL node has a fuser holder.
    Busy,
    /// `fuser` missing — treat as unsafe to rmmod (fail-closed).
    FuserUnavailable,
}

/// Enumerate `/dev/*` paths for `lsscsi -g` rows whose vendor/product contain `VTL`.
pub(crate) fn vtl_scsi_device_paths() -> Vec<PathBuf> {
    let out = match Command::new("lsscsi").arg("-g").output() {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    parse_vtl_paths_from_lsscsi(&text)
}

fn parse_vtl_paths_from_lsscsi(text: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in text.lines() {
        if !line.contains("VTL") {
            continue;
        }
        for token in line.split_whitespace() {
            if let Some(rest) = token.strip_prefix("/dev/") {
                if !rest.is_empty() {
                    paths.push(PathBuf::from(token));
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

pub(crate) fn probe_vtl_device_holders() -> VtlHoldersProbe {
    if !fuser_available() {
        return VtlHoldersProbe::FuserUnavailable;
    }
    let paths = vtl_scsi_device_paths();
    if paths.is_empty() {
        return VtlHoldersProbe::Clear;
    }
    for path in &paths {
        if device_has_fuser_holder(path) {
            return VtlHoldersProbe::Busy;
        }
    }
    VtlHoldersProbe::Clear
}

/// True when full module reload must not run (holders or cannot verify).
pub(crate) fn refuse_rmmod_for_safety() -> bool {
    if std::env::var("VTL_FORCE_RMMOD").ok().as_deref() == Some("1") {
        return false;
    }
    matches!(
        probe_vtl_device_holders(),
        VtlHoldersProbe::Busy | VtlHoldersProbe::FuserUnavailable
    )
}

fn fuser_available() -> bool {
    for p in ["/usr/sbin/fuser", "/bin/fuser", "/sbin/fuser"] {
        if Path::new(p).is_file() {
            return true;
        }
    }
    Command::new("sh")
        .args(["-c", "command -v fuser >/dev/null 2>&1"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn device_has_fuser_holder(path: &Path) -> bool {
    match Command::new("fuser").arg(path).output() {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lsscsi_vtl_paths() {
        let sample = "\
[33:0:0:0]   mediumx VTL      VTL CHANGER      1.00  /dev/sch0  /dev/sg2
[33:0:0:1]   tape    VTL      VTL TAPE DRV     1.00  /dev/st0   /dev/sg3
[0:0:1:0]    disk    ATA      DISK             1.00  /dev/sda   /dev/sg0
";
        let p = parse_vtl_paths_from_lsscsi(sample);
        assert_eq!(p.len(), 4);
        assert!(p.iter().any(|x| x == Path::new("/dev/st0")));
        assert!(p.iter().any(|x| x == Path::new("/dev/sg2")));
        assert!(!p.iter().any(|x| x == Path::new("/dev/sg0")));
    }
}
