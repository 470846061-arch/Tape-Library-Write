//! DB ↔ kernel medium-changer reconciliation (inventory ioctl + fix paths).

use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};

use crate::robot_sync::{
    self, db_inventory, ensure_inventory_complete, evacuate_shelved_tapes_from_kernel,
    evacuate_tape_from_changer, kernel_inventory_snapshot, robot_sync_enabled,
    warn_kernel_sync_failed, MediumLocation,
};
use crate::{get_config, init_db, log_error, log_message, resolve_library_id, VtlError};

#[derive(Debug, Clone)]
pub struct DriftItem {
    pub tape: String,
    pub db: Option<MediumLocation>,
    pub kernel: Option<MediumLocation>,
}

#[derive(Debug, Default)]
pub struct ReconcileReport {
    pub drifts: Vec<DriftItem>,
    pub fixes_applied: usize,
    pub pull_updates: usize,
    pub inventory_truncated: bool,
}

pub fn find_drifts(
    db: &HashMap<String, MediumLocation>,
    kernel: &HashMap<String, MediumLocation>,
) -> Vec<DriftItem> {
    let mut tapes: HashSet<String> = HashSet::new();
    tapes.extend(db.keys().cloned());
    tapes.extend(kernel.keys().cloned());
    let mut out = Vec::new();
    for tape in tapes {
        let d = db.get(&tape).cloned();
        let k = kernel.get(&tape).cloned();
        if d != k {
            out.push(DriftItem {
                tape,
                db: d,
                kernel: k,
            });
        }
    }
    out.sort_by(|a, b| a.tape.cmp(&b.tape));
    out
}

fn tape_id_by_name(conn: &Connection, library_id: i64, name: &str) -> Result<i64, VtlError> {
    conn.query_row(
        "SELECT id FROM tapes WHERE library_id = ?1 AND name = ?2",
        params![library_id, name],
        |r| r.get(0),
    )
    .map_err(|_| VtlError::TapeNotFound(name.to_string()))
}

/// Write kernel changer inventory into DB (backup software is authoritative).
pub(crate) fn pull_library_inventory_from_kernel(library_id: i64) -> Result<usize, VtlError> {
    if !robot_sync_enabled() {
        return Ok(0);
    }
    let mut conn = init_db()?;
    let kernel_snap = kernel_inventory_snapshot(&conn, library_id)?;
    ensure_inventory_complete(&kernel_snap)?;
    apply_kernel_to_db(&mut conn, library_id, &kernel_snap.locations)
}

/// Apply kernel inventory to DB (backup-software MOVE without vtladm).
fn apply_kernel_to_db(
    conn: &mut Connection,
    library_id: i64,
    kernel: &HashMap<String, MediumLocation>,
) -> Result<usize, VtlError> {
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE slots SET tape_id = NULL WHERE library_id = ?1",
        params![library_id],
    )?;
    tx.execute(
        "UPDATE drives SET tape_id = NULL WHERE library_id = ?1",
        params![library_id],
    )?;
    let mut n = 0usize;
    for (tape_name, loc) in kernel {
        let tape_id = match tape_id_by_name(&tx, library_id, tape_name) {
            Ok(id) => id,
            Err(VtlError::TapeNotFound(_)) => {
                log_message(&format!(
                    "reconcile pull: skip unknown tape '{}' in kernel",
                    tape_name
                ));
                continue;
            }
            Err(e) => return Err(e),
        };
        match loc {
            MediumLocation::DataSlot(slot) => {
                tx.execute(
                    "UPDATE slots SET tape_id = ?1 WHERE library_id = ?2 AND slot_id = ?3",
                    params![tape_id, library_id, slot],
                )?;
                tx.execute(
                    "UPDATE tapes SET slot = ?1, shelf_id = NULL WHERE id = ?2",
                    params![slot, tape_id],
                )?;
            }
            MediumLocation::Drive(drive) => {
                tx.execute(
                    "UPDATE drives SET tape_id = ?1 WHERE library_id = ?2 AND drive_id = ?3",
                    params![tape_id, library_id, drive],
                )?;
                tx.execute(
                    "UPDATE tapes SET slot = NULL, shelf_id = NULL WHERE id = ?1",
                    params![tape_id],
                )?;
            }
            MediumLocation::MailSlot(slot) => {
                tx.execute(
                    "UPDATE slots SET tape_id = ?1 WHERE library_id = ?2 AND slot_id = ?3",
                    params![tape_id, library_id, slot],
                )?;
                tx.execute(
                    "UPDATE tapes SET slot = ?1, shelf_id = NULL WHERE id = ?2",
                    params![slot, tape_id],
                )?;
            }
        }
        n += 1;
    }
    tx.execute(
        "UPDATE tapes SET slot = NULL WHERE library_id = ?1 AND shelf_id IS NULL AND id NOT IN (
            SELECT tape_id FROM slots WHERE library_id = ?1 AND tape_id IS NOT NULL
         ) AND id NOT IN (
            SELECT tape_id FROM drives WHERE library_id = ?1 AND tape_id IS NOT NULL
         )",
        params![library_id],
    )?;
    tx.commit()?;
    Ok(n)
}

const DB_TO_KERNEL_DISABLED: &str =
    "DB→kernel mechanical sync removed: runtime robot is always vtl.ko; use assign-slot/load/unload ioctl or backup SCSI MOVE; catalog: robot sync-db or reconcile --pull";

/// Push DB inventory into kernel (removed; mhVTL-style single runtime in vtl.ko).
pub(crate) fn push_db_to_kernel(_conn: &Connection, _library_id: i64) -> Result<usize, VtlError> {
    Err(VtlError::InvalidParameter(DB_TO_KERNEL_DISABLED.into()))
}

pub fn reconcile_library(
    library_id: i64,
    apply_db: bool,
    pull_kernel: bool,
) -> Result<ReconcileReport, VtlError> {
    if pull_kernel && apply_db {
        return Err(VtlError::InvalidParameter(
            "use either --apply (DB→kernel) or --pull (kernel→DB), not both".into(),
        ));
    }
    if apply_db {
        return Err(VtlError::InvalidParameter(DB_TO_KERNEL_DISABLED.into()));
    }
    if !robot_sync_enabled() {
        return Err(VtlError::InvalidParameter(
            "robot_sync is disabled in vtl.conf".into(),
        ));
    }

    let mut conn = init_db()?;
    let db = db_inventory(&conn, library_id)?;
    let kernel_snap = kernel_inventory_snapshot(&conn, library_id)?;
    let kernel = kernel_snap.locations.clone();
    let drifts = find_drifts(&db, &kernel);
    let mut report = ReconcileReport {
        drifts: drifts.clone(),
        fixes_applied: 0,
        pull_updates: 0,
        inventory_truncated: kernel_snap.truncated,
    };

    if kernel_snap.truncated {
        eprintln!(
            "Warning: kernel inventory truncated (>= {} items); drift list may be incomplete",
            robot_sync::VTL_INV_MAX_ITEMS
        );
        if apply_db || pull_kernel {
            return Err(VtlError::InvalidParameter(format!(
                "kernel GET_INVENTORY truncated (>= {} elements); refuse --apply/--pull",
                robot_sync::VTL_INV_MAX_ITEMS
            )));
        }
    }

    if drifts.is_empty() {
        log_message(&format!(
            "reconcile: library_id={} DB and kernel inventory match ({} tapes)",
            library_id,
            db.len()
        ));
        return Ok(report);
    }

    if pull_kernel {
        ensure_inventory_complete(&kernel_snap)?;
        report.pull_updates = apply_kernel_to_db(&mut conn, library_id, &kernel)?;
        log_message(&format!(
            "reconcile: pulled {} tape location(s) from kernel to DB for library_id={}",
            report.pull_updates, library_id
        ));
        return Ok(report);
    }

    if apply_db {
        report.fixes_applied = push_db_to_kernel(&conn, library_id)?;
        let kernel_after = kernel_inventory_snapshot(&conn, library_id)?;
        report.inventory_truncated |= kernel_after.truncated;
        if kernel_after.truncated {
            return Err(VtlError::InvalidParameter(format!(
                "kernel inventory still truncated after apply (>= {} items)",
                robot_sync::VTL_INV_MAX_ITEMS
            )));
        }
        let remaining = find_drifts(&db_inventory(&conn, library_id)?, &kernel_after.locations);
        log_message(&format!(
            "reconcile: applied {} fix(es) for library_id={}; {} drift(s) remain",
            report.fixes_applied,
            library_id,
            remaining.len()
        ));
        if !remaining.is_empty() {
            report.drifts = remaining;
        } else {
            report.drifts.clear();
        }
        return Ok(report);
    }

    Ok(report)
}

#[allow(dead_code)] // reserved for future CLI / patrol hooks
pub fn reconcile_current_library(
    apply_db: bool,
    pull_kernel: bool,
) -> Result<ReconcileReport, VtlError> {
    let conn = init_db()?;
    let lib = crate::current_library_name();
    let library_id = resolve_library_id(&conn, &lib)?;
    reconcile_library(library_id, apply_db, pull_kernel)
}

pub fn print_reconcile_report(lib_name: &str, report: &ReconcileReport) {
    if report.inventory_truncated {
        eprintln!("Warning: kernel GET_INVENTORY was truncated; results may be incomplete.");
    }
    if report.drifts.is_empty() {
        println!("Library '{}': DB and kernel inventory match.", lib_name);
        return;
    }
    println!(
        "Library '{}': {} drift(s) between DB and kernel:",
        lib_name,
        report.drifts.len()
    );
    for d in &report.drifts {
        println!("  tape '{}': DB {:?}  kernel {:?}", d.tape, d.db, d.kernel);
    }
    if report.fixes_applied > 0 {
        println!("Applied {} fix(es) (DB → kernel).", report.fixes_applied);
    }
    if report.pull_updates > 0 {
        println!("Updated {} tape row(s) (kernel → DB).", report.pull_updates);
    }
    if report.fixes_applied == 0 && report.pull_updates == 0 {
        println!("Dry-run only. Use: vtladm -L <lib> robot reconcile --apply | --pull");
    }
}

pub fn drift_error_message(report: &ReconcileReport) -> String {
    format!(
        "RECONCILE_DRIFT: {} inventory drift(s); use robot reconcile --pull or robot auto-align",
        report.drifts.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::robot_sync::MediumLocation;

    #[test]
    fn find_drifts_detects_mismatch() {
        let mut db = HashMap::new();
        db.insert("T1".into(), MediumLocation::DataSlot(0));
        let mut k = HashMap::new();
        k.insert("T1".into(), MediumLocation::Drive(0));
        let d = find_drifts(&db, &k);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].tape, "T1");
    }

    /// Regression: repeated `?1` in subqueries is one bind; extra params caused sync-db to fail.
    #[test]
    fn apply_kernel_to_db_orphan_slot_cleanup_sql_bind_count() {
        let conn = Connection::open_in_memory().expect("mem db");
        conn.execute_batch(
            "CREATE TABLE tapes (id INTEGER PRIMARY KEY, library_id INTEGER, slot INTEGER, shelf_id INTEGER);
             CREATE TABLE slots (library_id INTEGER, slot_id INTEGER, tape_id INTEGER);
             CREATE TABLE drives (library_id INTEGER, drive_id INTEGER, tape_id INTEGER);
             INSERT INTO tapes (id, library_id, slot, shelf_id) VALUES (1, 1, 3, NULL);
             INSERT INTO slots (library_id, slot_id, tape_id) VALUES (1, 0, NULL);",
        )
        .expect("schema");
        let sql = "UPDATE tapes SET slot = NULL WHERE library_id = ?1 AND shelf_id IS NULL AND id NOT IN (
            SELECT tape_id FROM slots WHERE library_id = ?1 AND tape_id IS NOT NULL
         ) AND id NOT IN (
            SELECT tape_id FROM drives WHERE library_id = ?1 AND tape_id IS NOT NULL
         )";
        conn.execute(sql, params![1i64])
            .expect("single library_id bind for repeated ?1");
        let slot: Option<i32> = conn
            .query_row("SELECT slot FROM tapes WHERE id = 1", [], |r| r.get(0))
            .expect("row");
        assert!(slot.is_none());
    }

    #[test]
    fn apply_kernel_to_db_empty_kernel_updates_db() {
        let conn = Connection::open_in_memory().expect("mem db");
        conn.execute_batch(
            "CREATE TABLE tapes (id INTEGER PRIMARY KEY, library_id INTEGER, name TEXT, slot INTEGER, shelf_id INTEGER);
             CREATE TABLE slots (library_id INTEGER, slot_id INTEGER, tape_id INTEGER);
             CREATE TABLE drives (library_id INTEGER, drive_id INTEGER, tape_id INTEGER);
             INSERT INTO tapes (id, library_id, name, slot, shelf_id) VALUES (1, 1, 'T1', 0, NULL);
             INSERT INTO slots (library_id, slot_id, tape_id) VALUES (1, 0, 1);",
        )
        .expect("schema");
        let mut conn = conn;
        let kernel = HashMap::new();
        let n = apply_kernel_to_db(&mut conn, 1, &kernel).expect("empty kernel pull");
        assert_eq!(n, 0);
        let slot_tape: Option<i64> = conn
            .query_row(
                "SELECT tape_id FROM slots WHERE library_id = 1 AND slot_id = 0",
                [],
                |r| r.get(0),
            )
            .expect("slot");
        assert!(slot_tape.is_none());
        let slot: Option<i32> = conn
            .query_row("SELECT slot FROM tapes WHERE id = 1", [], |r| r.get(0))
            .expect("tape");
        assert!(slot.is_none());
    }
}

/// Whether periodic `vtl-robot-sync` should mirror kernel slot hints into SQLite catalog.
pub(crate) fn auto_sync_db_from_kernel_enabled() -> bool {
    if let Ok(s) = std::env::var("VTL_AUTO_SYNC_DB_FROM_KERNEL") {
        let t = s.trim().to_ascii_lowercase();
        if matches!(t.as_str(), "0" | "false" | "no" | "off") {
            return false;
        }
        if matches!(t.as_str(), "1" | "true" | "yes" | "on") {
            return true;
        }
    }
    get_config().auto_sync_db_from_kernel
}

pub(crate) fn auto_reconcile_pull_enabled() -> bool {
    if let Ok(s) = std::env::var("VTL_AUTO_RECONCILE_PULL") {
        let t = s.trim().to_ascii_lowercase();
        if matches!(t.as_str(), "0" | "false" | "no" | "off") {
            return false;
        }
        if matches!(t.as_str(), "1" | "true" | "yes" | "on") {
            return true;
        }
    }
    get_config().auto_reconcile_pull
}

#[derive(Debug, Default)]
pub struct AutoAlignReport {
    pub evacuated: usize,
    pub fixes_applied: usize,
    pub pull_updates: usize,
    pub drifts_remaining: usize,
}

fn drifts_safe_for_pull(drifts: &[DriftItem]) -> bool {
    drifts
        .iter()
        .all(|d| d.db.is_some() && d.kernel.is_some() && d.db != d.kernel)
}

/// Evacuate shelved media from kernel, then optionally pull kernel→DB when drifts are safe.
pub fn auto_align_library(library_id: i64) -> Result<AutoAlignReport, VtlError> {
    auto_align_library_inner(library_id, true)
}

fn auto_align_library_inner(
    library_id: i64,
    allow_pull: bool,
) -> Result<AutoAlignReport, VtlError> {
    if !robot_sync_enabled() {
        return Ok(AutoAlignReport::default());
    }
    let mut conn = init_db()?;
    let mut report = AutoAlignReport::default();
    report.evacuated = evacuate_shelved_tapes_from_kernel(&conn, library_id)?;

    let kernel_snap = kernel_inventory_snapshot(&conn, library_id)?;
    if kernel_snap.truncated {
        return Err(VtlError::InvalidParameter(format!(
            "kernel GET_INVENTORY truncated (>= {} elements); refuse auto-align",
            robot_sync::VTL_INV_MAX_ITEMS
        )));
    }

    let db = db_inventory(&conn, library_id)?;
    let mut drifts = find_drifts(&db, &kernel_snap.locations);

    for d in drifts.clone() {
        if d.db.is_none() && d.kernel.is_some() {
            if evacuate_tape_from_changer(&conn, library_id, &d.tape).is_ok() {
                report.evacuated += 1;
            }
        }
    }
    if report.evacuated > 0 {
        let kernel_after = kernel_inventory_snapshot(&conn, library_id)?;
        drifts = find_drifts(&db, &kernel_after.locations);
    }

    if drifts.is_empty() {
        return Ok(report);
    }

    let want_pull = allow_pull && auto_reconcile_pull_enabled() && drifts_safe_for_pull(&drifts);

    if want_pull {
        ensure_inventory_complete(&kernel_snap)?;
        let fresh = kernel_inventory_snapshot(&conn, library_id)?;
        ensure_inventory_complete(&fresh)?;
        report.pull_updates = apply_kernel_to_db(&mut conn, library_id, &fresh.locations)?;
        drifts.clear();
        log_message(&format!(
            "auto-align: library_id={} pulled {} tape location(s) from kernel",
            library_id, report.pull_updates
        ));
    }

    report.drifts_remaining = drifts.len();
    Ok(report)
}

/// Evacuate shelved / orphan kernel media only (no apply/pull).
pub fn try_post_op_evacuate_only(library_id: i64) {
    if !robot_sync_enabled() {
        return;
    }
    let conn = match init_db() {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = evacuate_shelved_tapes_from_kernel(&conn, library_id);
    if let Ok(kernel_snap) = kernel_inventory_snapshot(&conn, library_id) {
        if !kernel_snap.truncated {
            let db = match db_inventory(&conn, library_id) {
                Ok(d) => d,
                Err(_) => return,
            };
            for (tape, kloc) in &kernel_snap.locations {
                if db.get(tape).is_none() {
                    let _ = evacuate_tape_from_changer(&conn, library_id, tape);
                    let _ = (tape, kloc);
                }
            }
        }
    }
}

/// Best-effort evacuate + optional pull after vtladm catalog/robot ioctl (warn only on failure).
pub fn try_post_op_auto_align(library_id: i64) {
    if !robot_sync_enabled() {
        return;
    }
    try_post_op_evacuate_only(library_id);
    if !auto_reconcile_pull_enabled() {
        return;
    }
    match auto_align_library_inner(library_id, false) {
        Ok(r) if r.drifts_remaining == 0 => {}
        Ok(r) => {
            eprintln!(
                "Warning: auto-align: library_id={} still has {} drift(s) (evacuated={}, applied={}, pulled={})",
                library_id,
                r.drifts_remaining,
                r.evacuated,
                r.fixes_applied,
                r.pull_updates
            );
        }
        Err(VtlError::InvalidParameter(ref m)) if m.contains("truncated") => {
            eprintln!("Warning: auto-align skipped: {}", m);
        }
        Err(VtlError::InvalidParameter(ref m)) if m.starts_with("RECONCILE_IOCTL") => {}
        Err(e) => {
            if let VtlError::IoError(ref io) = e {
                warn_kernel_sync_failed("auto-align", io);
            } else {
                eprintln!("Warning: auto-align failed: {}", e);
            }
        }
    }
}

/// Report for `sync_db_from_kernel` / `robot sync-db`.
#[derive(Debug, Default)]
pub struct SyncDbFromKernelReport {
    pub libraries: usize,
    pub tapes_updated: usize,
}

/// mhVTL-style catalog hint: mirror kernel data-slot numbers into `tapes.slot` only (no slots/drives table wipe).
pub(crate) fn mirror_kernel_catalog_hints_only(
    conn: &mut Connection,
    library_id: i64,
) -> Result<usize, VtlError> {
    use robot_sync::MediumLocation;
    let kernel_snap = kernel_inventory_snapshot(conn, library_id)?;
    ensure_inventory_complete(&kernel_snap)?;
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE tapes SET slot = NULL WHERE library_id = ?1",
        params![library_id],
    )?;
    let mut n = 0usize;
    for (tape_name, loc) in &kernel_snap.locations {
        if let MediumLocation::DataSlot(slot) = loc {
            if let Ok(tape_id) = tape_id_by_name(&tx, library_id, tape_name) {
                tx.execute(
                    "UPDATE tapes SET slot = ?1, shelf_id = NULL WHERE id = ?2",
                    params![slot, tape_id],
                )?;
                n += 1;
            }
        }
    }
    tx.commit()?;
    Ok(n)
}

/// Backup-software mode: periodic sync — update catalog hints from kernel; UI reads kernel via GET_INVENTORY.
pub fn sync_db_from_kernel_all_libraries() -> Result<SyncDbFromKernelReport, VtlError> {
    if !robot_sync_enabled() {
        return Ok(SyncDbFromKernelReport::default());
    }
    let mut conn = match init_db() {
        Ok(c) => c,
        Err(e) => return Err(e),
    };
    let libs = robot_sync::online_library_ids_for_kernel_sync(&conn)?;
    let mut report = SyncDbFromKernelReport::default();
    report.libraries = libs.len();
    for lib_id in libs {
        match mirror_kernel_catalog_hints_only(&mut conn, lib_id) {
            Ok(n) => {
                report.tapes_updated += n;
                log_message(&format!(
                    "sync-db: library_id={} mirrored {} kernel data-slot hint(s) into tapes.slot",
                    lib_id, n
                ));
            }
            Err(e) => {
                log_message(&format!("sync-db: library_id={} failed: {}", lib_id, e));
                return Err(e);
            }
        }
    }
    Ok(report)
}

/// Pull kernel inventory into DB for every online library (after geom change in kernel-authority mode).
pub fn pull_all_online_libraries_from_kernel() {
    if !robot_sync_enabled() {
        return;
    }
    let conn = match init_db() {
        Ok(c) => c,
        Err(e) => {
            log_error("reconcile", &format!("init_db: {}", e));
            return;
        }
    };
    let libs = match robot_sync::online_library_ids_for_kernel_sync(&conn) {
        Ok(v) => v,
        Err(e) => {
            log_error("reconcile", &format!("list libraries: {}", e));
            return;
        }
    };
    for lib_id in libs {
        match pull_library_inventory_from_kernel(lib_id) {
            Ok(n) => log_message(&format!(
                "reconcile: pulled {} tape location(s) from kernel for library_id={}",
                n, lib_id
            )),
            Err(e) => log_message(&format!(
                "reconcile: pull from kernel failed for library_id={}: {}",
                lib_id, e
            )),
        }
    }
}

pub fn auto_align_all_online_libraries() -> Result<(), VtlError> {
    let conn = init_db()?;
    let libs = robot_sync::online_library_ids_for_kernel_sync(&conn)?;
    for lib_id in libs {
        let name: String = conn
            .query_row(
                "SELECT name FROM vtl_libraries WHERE id = ?1",
                rusqlite::params![lib_id],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| format!("id{}", lib_id));
        match auto_align_library(lib_id) {
            Ok(r) if r.drifts_remaining > 0 => {
                eprintln!(
                    "Warning: library '{}': {} drift(s) after auto-align",
                    name, r.drifts_remaining
                );
            }
            Err(e) => eprintln!("Warning: library '{}': auto-align: {}", name, e),
            _ => {}
        }
    }
    Ok(())
}

#[allow(dead_code)] // reserved for post-op drift logging
pub fn warn_if_reconcile_drift(library_id: i64, lib_name: &str) -> bool {
    if !robot_sync_enabled() {
        return false;
    }
    let conn = match init_db() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let db = match db_inventory(&conn, library_id) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Warning: library '{}': {}", lib_name, e);
            return false;
        }
    };
    let kernel_snap = match kernel_inventory_snapshot(&conn, library_id) {
        Ok(v) => v,
        Err(e) => {
            if let VtlError::InvalidParameter(ref m) = e {
                if m.starts_with("RECONCILE_IOCTL") {
                    eprintln!("Warning: library '{}': {}", lib_name, e);
                    return false;
                }
            }
            if let VtlError::IoError(ref io) = e {
                warn_kernel_sync_failed("reconcile-inventory", io);
            }
            return false;
        }
    };
    if kernel_snap.truncated {
        eprintln!(
            "Warning: library '{}': kernel inventory truncated (>= {} items); dry-run only",
            lib_name,
            robot_sync::VTL_INV_MAX_ITEMS
        );
    }
    let drifts = find_drifts(&db, &kernel_snap.locations);
    if drifts.is_empty() {
        return false;
    }
    if auto_reconcile_pull_enabled() {
        match auto_align_library(library_id) {
            Ok(r) if r.drifts_remaining == 0 => {
                log_message(&format!(
                    "auto-align: library '{}' aligned (evacuated={}, applied={}, pulled={})",
                    lib_name, r.evacuated, r.fixes_applied, r.pull_updates
                ));
                return false;
            }
            Ok(r) => {
                eprintln!(
                    "Warning: library '{}' still has {} drift(s) after auto-align",
                    lib_name, r.drifts_remaining
                );
                return true;
            }
            Err(e) => {
                eprintln!("Warning: library '{}': auto-align failed: {}", lib_name, e);
                return true;
            }
        }
    }
    eprintln!(
        "Warning: library '{}' has {} DB/kernel drift(s); run: vtladm -L {} robot reconcile --pull or robot auto-align",
        lib_name,
        drifts.len(),
        lib_name
    );
    true
}
