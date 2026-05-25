//! DB ↔ kernel medium-changer sync via `/dev/vtl` ioctl (mhVTL-style: load makes tape LUN ready).
//!
//! Requires `vtl.ko` with `VTL_IOCTL_LOAD_TAPE` / `SLOT_PLACE` (not `EOPNOTSUPP`).
//! Library `instance` index matches `vtl_instances` segment order (`ORDER BY id` among online libs).
//!
//! SCSI element addresses: data slot `0..N-1`, drive `1000+i`, import/export `2000+i`
//! (DB mailslot `slot_id` = `MAILSLOT_OFFSET + i`, default offset 100).

use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::{get_config, log_error, log_message, VtlError, OFFLINE_LIBRARY_NAME};

/// DB `slots.slot_id` for `mail0`..`mail3` (see `main.rs`).
pub const MAILSLOT_OFFSET: i32 = 100;
/// Kernel SCSI element address for tape drives (must match `kernel/include/vtl.h`).
pub const VTL_ELEM_DRIVE_BASE: i32 = 1000;
/// Kernel SCSI element address for import/export (IE) slots.
pub const VTL_ELEM_IE_BASE: i32 = 2000;

#[repr(C)]
struct VtlLoadReq {
    instance: i32,
    slot: i32,
    drive: i32,
    tape_name: [u8; 64],
    barcode: [u8; 16],
}

#[repr(C)]
struct VtlUnloadReq {
    instance: i32,
    drive: i32,
    slot: i32,
    tape_name: [u8; 64],
}

#[repr(C)]
struct VtlSlotPlaceReq {
    instance: i32,
    slot: i32,
    tape_name: [u8; 64],
    barcode: [u8; 16],
}

#[repr(C)]
struct VtlMoveReq {
    instance: i32,
    src: i32,
    dst: i32,
}

#[repr(C)]
struct VtlElemRemoveReq {
    instance: i32,
    element: i32,
}

/// `_IOW('V', 2, struct vtl_load_req)` — must match `kernel/src/vtl_misc.c`.
const VTL_IOCTL_LOAD_TAPE: libc::c_ulong = 0x405C_5602;
const VTL_IOCTL_UNLOAD_TAPE: libc::c_ulong = 0x404C_5603;
const VTL_IOCTL_SLOT_PLACE: libc::c_ulong = 0x4058_5606;
const VTL_IOCTL_MOVE_MEDIUM: libc::c_ulong = 0x400C_5607;
/// `_IOW('V', 9, struct vtl_elem_remove_req)` — must match `kernel/src/vtl_misc.c`.
const VTL_IOCTL_ELEM_REMOVE: libc::c_ulong = 0x4008_5609;

pub const VTL_INV_MAX_ITEMS: usize = 128;

#[repr(C)]
#[derive(Clone, Copy)]
struct VtlInvItem {
    element: i32,
    tape_name: [u8; 64],
}

#[repr(C)]
struct VtlInventoryIoctl {
    instance: i32,
    num_drives: i32,
    num_slots: i32,
    num_mailslots: i32,
    count: i32,
    truncated: i32,
    items: [VtlInvItem; VTL_INV_MAX_ITEMS],
}

/// Kernel changer snapshot from `VTL_IOCTL_GET_INVENTORY`.
#[derive(Debug, Clone)]
pub struct KernelInventory {
    pub locations: HashMap<String, MediumLocation>,
    pub truncated: bool,
}

const VTL_INV_IOCTL_SZ: u32 = std::mem::size_of::<VtlInventoryIoctl>() as u32;
/// `_IOWR('V', 8, struct vtl_inventory_ioctl)` — must match `kernel/src/vtl_misc.c`.
const VTL_IOCTL_GET_INVENTORY: libc::c_ulong =
    ((3u32 << 30) | (VTL_INV_IOCTL_SZ << 16) | ((b'V' as u32) << 8) | 8) as libc::c_ulong;

/// Where a tape sits in the medium changer (DB or kernel view).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MediumLocation {
    DataSlot(i32),
    Drive(i32),
    MailSlot(i32),
}

impl MediumLocation {
    pub fn from_element(elem: i32) -> Option<Self> {
        if elem >= 0 && elem < VTL_ELEM_IE_BASE {
            Some(MediumLocation::DataSlot(elem))
        } else if elem >= VTL_ELEM_DRIVE_BASE && elem < VTL_ELEM_IE_BASE {
            Some(MediumLocation::Drive(elem - VTL_ELEM_DRIVE_BASE))
        } else if elem >= VTL_ELEM_IE_BASE {
            Some(MediumLocation::MailSlot(
                MAILSLOT_OFFSET + (elem - VTL_ELEM_IE_BASE),
            ))
        } else {
            None
        }
    }

    pub fn to_element(&self) -> i32 {
        match self {
            MediumLocation::DataSlot(s) => *s,
            MediumLocation::Drive(d) => VTL_ELEM_DRIVE_BASE + d,
            MediumLocation::MailSlot(m) => db_mailslot_to_element(*m),
        }
    }
}

fn cstr_to_string(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// True when vtladm may issue per-tape robot ioctl (assign-slot, load, unload, eject).
pub(crate) fn robot_ioctl_enabled() -> bool {
    robot_sync_enabled()
}

pub(crate) fn robot_sync_enabled() -> bool {
    if let Ok(s) = std::env::var("VTL_ROBOT_SYNC") {
        let t = s.trim().to_ascii_lowercase();
        if matches!(t.as_str(), "0" | "false" | "no" | "off") {
            return false;
        }
        if matches!(t.as_str(), "1" | "true" | "yes" | "on") {
            return true;
        }
    }
    get_config().robot_sync
}

fn copy_name_field(dst: &mut [u8; 64], name: &str) -> io::Result<()> {
    if name.len() >= 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tape name exceeds 63 bytes",
        ));
    }
    dst.fill(0);
    dst[..name.len()].copy_from_slice(name.as_bytes());
    Ok(())
}

fn copy_barcode_field(dst: &mut [u8; 16], barcode: Option<&str>) -> io::Result<()> {
    dst.fill(0);
    let Some(bc) = barcode.filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if bc.len() >= 16 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "barcode exceeds 15 bytes",
        ));
    }
    dst[..bc.len()].copy_from_slice(bc.as_bytes());
    Ok(())
}

/// Map DB mailslot `slot_id` (e.g. 100 = mail0) to kernel element address 2000+i.
pub fn db_mailslot_to_element(db_slot_id: i32) -> i32 {
    VTL_ELEM_IE_BASE + (db_slot_id - MAILSLOT_OFFSET)
}

/// Align kernel `tape_dir` module parameter with `vtl.conf` (best-effort; needs root).
fn sync_module_tape_dir_from_config() {
    let dir = get_config().tape_dir;
    let trimmed = dir.to_string_lossy().trim_end_matches('/').to_string();
    if trimmed.is_empty() {
        return;
    }
    let sysfs = Path::new("/sys/module/vtl/parameters/tape_dir");
    if sysfs.exists() {
        if let Err(e) = fs::write(sysfs, trimmed.as_bytes()) {
            log_error("robot_sync", &format!("write {}: {}", sysfs.display(), e));
        }
    }
}

fn prepare_kernel_tape_dir_for_ioctl() -> Result<(), VtlError> {
    let rep = crate::link_kernel_tapes()?;
    if rep.linked > 0 || rep.removed_stale > 0 || rep.relocated_flat > 0 {
        log_message(&format!(
            "link_kernel_tapes: linked={} removed_stale={} relocated_flat={}",
            rep.linked, rep.removed_stale, rep.relocated_flat
        ));
    }
    sync_module_tape_dir_from_config();
    Ok(())
}

fn ioctl_vtl<T>(cmd: libc::c_ulong, req: &mut T) -> io::Result<()> {
    prepare_kernel_tape_dir_for_ioctl().map_err(|e| {
        io::Error::new(
            io::ErrorKind::Other,
            format!("prepare_kernel_tape_dir: {}", e),
        )
    })?;
    let f = OpenOptions::new().read(true).write(true).open("/dev/vtl")?;
    let r = unsafe { libc::ioctl(f.as_raw_fd(), cmd, req as *mut T as *mut libc::c_void) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Online library ids that have a kernel `vtl_instances` segment (skip 0 drive / 0 slot shells).
pub(crate) fn online_library_ids_for_kernel_sync(conn: &Connection) -> Result<Vec<i64>, VtlError> {
    let mut stmt =
        conn.prepare("SELECT id FROM vtl_libraries WHERE name NOT IN (?1, ?2) ORDER BY id ASC")?;
    let ids: Vec<i64> = stmt
        .query_map(
            params![OFFLINE_LIBRARY_NAME, crate::LEGACY_DEFAULT_LIBRARY_NAME],
            |r| r.get(0),
        )?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for id in ids {
        let drives: i64 = conn.query_row(
            "SELECT COUNT(*) FROM drives WHERE library_id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        let slots: i64 = conn.query_row(
            "SELECT COUNT(*) FROM slots WHERE library_id = ?1 AND is_import_export = 0",
            params![id],
            |r| r.get(0),
        )?;
        if drives == 0 && slots == 0 {
            continue;
        }
        out.push(id);
    }
    Ok(out)
}

/// Kernel SCSI host index for this online library (same order as `build_vtl_instances_kernel_spec`).
pub(crate) fn library_kernel_instance_index(
    conn: &Connection,
    library_id: i64,
) -> Result<usize, VtlError> {
    let libs = online_library_ids_for_kernel_sync(conn)?;
    for (idx, id) in libs.iter().enumerate() {
        if *id == library_id {
            return Ok(idx);
        }
    }
    Err(VtlError::LibraryNotFound(
        "library has no kernel instance (empty or not in vtl_instances order)".into(),
    ))
}

fn library_instance_i32(conn: &Connection, library_id: i64) -> io::Result<i32> {
    library_kernel_instance_index(conn, library_id)
        .map(|i| i as i32)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
}

pub(crate) fn kernel_slot_place(
    conn: &Connection,
    library_id: i64,
    slot: i32,
    tape_name: &str,
    barcode: Option<&str>,
) -> io::Result<()> {
    let inst = library_instance_i32(conn, library_id)?;
    let mut req = VtlSlotPlaceReq {
        instance: inst,
        slot,
        tape_name: [0u8; 64],
        barcode: [0u8; 16],
    };
    copy_name_field(&mut req.tape_name, tape_name)?;
    copy_barcode_field(&mut req.barcode, barcode)?;
    ioctl_vtl(VTL_IOCTL_SLOT_PLACE, &mut req)
}

pub(crate) fn kernel_load(
    conn: &Connection,
    library_id: i64,
    slot: i32,
    drive: i32,
    tape_name: &str,
    barcode: Option<&str>,
) -> io::Result<()> {
    let inst = library_instance_i32(conn, library_id)?;
    let mut req = VtlLoadReq {
        instance: inst,
        slot,
        drive,
        tape_name: [0u8; 64],
        barcode: [0u8; 16],
    };
    copy_name_field(&mut req.tape_name, tape_name)?;
    copy_barcode_field(&mut req.barcode, barcode)?;
    ioctl_vtl(VTL_IOCTL_LOAD_TAPE, &mut req)
}

pub(crate) fn kernel_unload(
    conn: &Connection,
    library_id: i64,
    drive: i32,
    slot: i32,
) -> io::Result<()> {
    let inst = library_instance_i32(conn, library_id)?;
    let mut req = VtlUnloadReq {
        instance: inst,
        drive,
        slot,
        tape_name: [0u8; 64],
    };
    ioctl_vtl(VTL_IOCTL_UNLOAD_TAPE, &mut req)
}

pub(crate) fn tape_barcode_for_name(
    conn: &Connection,
    library_id: i64,
    tape_name: &str,
) -> Option<String> {
    conn.query_row(
        "SELECT barcode FROM tapes WHERE library_id = ?1 AND name = ?2",
        params![library_id, tape_name],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

pub(crate) fn map_ioctl_error(e: io::Error) -> crate::VtlError {
    let msg = e.to_string();
    if e.raw_os_error() == Some(libc::ENOTTY) || e.kind() == io::ErrorKind::NotFound {
        return crate::VtlError::InvalidParameter(format!("RECONCILE_IOCTL_ERROR: {}", msg));
    }
    crate::VtlError::IoError(e)
}

/// Kernel changer occupancy: tape name → SCSI element location.
pub(crate) fn kernel_inventory_snapshot(
    conn: &Connection,
    library_id: i64,
) -> Result<KernelInventory, crate::VtlError> {
    let inst = library_instance_i32(conn, library_id).map_err(map_ioctl_error)?;
    let mut req = VtlInventoryIoctl {
        instance: inst,
        num_drives: 0,
        num_slots: 0,
        num_mailslots: 0,
        count: 0,
        truncated: 0,
        items: [VtlInvItem {
            element: -1,
            tape_name: [0u8; 64],
        }; VTL_INV_MAX_ITEMS],
    };
    ioctl_vtl(VTL_IOCTL_GET_INVENTORY, &mut req).map_err(map_ioctl_error)?;
    let mut locations = HashMap::new();
    let n = req.count.max(0) as usize;
    for item in req.items.iter().take(n.min(VTL_INV_MAX_ITEMS)) {
        let name = cstr_to_string(&item.tape_name);
        if name.is_empty() {
            continue;
        }
        if let Some(loc) = MediumLocation::from_element(item.element) {
            if locations.contains_key(&name) {
                return Err(crate::VtlError::InvalidParameter(format!(
                    "kernel inventory: duplicate tape name '{}'",
                    name
                )));
            }
            locations.insert(name, loc);
        }
    }
    Ok(KernelInventory {
        locations,
        truncated: req.truncated != 0,
    })
}

#[allow(dead_code)]
pub(crate) fn kernel_inventory(
    conn: &Connection,
    library_id: i64,
) -> Result<HashMap<String, MediumLocation>, crate::VtlError> {
    Ok(kernel_inventory_snapshot(conn, library_id)?.locations)
}

/// DB occupancy: tape name → slot / drive / mailslot.
pub(crate) fn db_inventory(
    conn: &Connection,
    library_id: i64,
) -> Result<HashMap<String, MediumLocation>, crate::VtlError> {
    let mut out = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT t.name, s.slot_id FROM slots s
         JOIN tapes t ON t.id = s.tape_id
         WHERE s.library_id = ?1 AND s.is_import_export = 0",
    )?;
    let rows = stmt.query_map(params![library_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)?))
    })?;
    for row in rows {
        let (name, slot) = row?;
        if out.contains_key(&name) {
            return Err(VtlError::InvalidParameter(format!(
                "DB inventory: tape '{}' assigned to multiple locations",
                name
            )));
        }
        out.insert(name, MediumLocation::DataSlot(slot));
    }
    let mut dstmt = conn.prepare(
        "SELECT t.name, d.drive_id FROM drives d
         JOIN tapes t ON t.id = d.tape_id WHERE d.library_id = ?1",
    )?;
    let drows = dstmt.query_map(params![library_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)?))
    })?;
    for row in drows {
        let (name, drive) = row?;
        if out.contains_key(&name) {
            return Err(VtlError::InvalidParameter(format!(
                "DB inventory: tape '{}' assigned to multiple locations",
                name
            )));
        }
        out.insert(name, MediumLocation::Drive(drive));
    }
    let mut mstmt = conn.prepare(
        "SELECT t.name, s.slot_id FROM slots s
         JOIN tapes t ON t.id = s.tape_id
         WHERE s.library_id = ?1 AND s.is_import_export != 0",
    )?;
    let mrows = mstmt.query_map(params![library_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)?))
    })?;
    for row in mrows {
        let (name, slot) = row?;
        if out.contains_key(&name) {
            return Err(VtlError::InvalidParameter(format!(
                "DB inventory: tape '{}' assigned to multiple locations",
                name
            )));
        }
        out.insert(name, MediumLocation::MailSlot(slot));
    }
    Ok(out)
}

fn first_empty_mailslot(conn: &Connection, library_id: i64) -> Option<i32> {
    conn.query_row(
        "SELECT slot_id FROM slots WHERE library_id = ?1 AND tape_id IS NULL AND is_import_export != 0 ORDER BY slot_id",
        params![library_id],
        |r| r.get(0),
    )
    .ok()
}

/// Refuse sync/repair when kernel inventory was truncated (incomplete snapshot).
pub(crate) fn ensure_inventory_complete(snap: &KernelInventory) -> Result<(), VtlError> {
    if snap.truncated {
        return Err(VtlError::InvalidParameter(format!(
            "kernel GET_INVENTORY truncated (>= {} occupied elements); \
             cannot sync or reconcile safely — reduce in-changer media or raise VTL_INV_MAX_ITEMS in kernel",
            VTL_INV_MAX_ITEMS
        )));
    }
    Ok(())
}

pub(crate) fn kernel_move_medium(
    conn: &Connection,
    library_id: i64,
    src: i32,
    dst: i32,
) -> io::Result<()> {
    let inst = library_instance_i32(conn, library_id)?;
    let mut req = VtlMoveReq {
        instance: inst,
        src,
        dst,
    };
    ioctl_vtl(VTL_IOCTL_MOVE_MEDIUM, &mut req)
}

/// Remove medium from a changer element (shelf / off-line); SCSI inventory no longer lists it there.
pub(crate) fn kernel_elem_remove(
    conn: &Connection,
    library_id: i64,
    element: i32,
) -> io::Result<()> {
    let inst = library_instance_i32(conn, library_id)?;
    let mut req = VtlElemRemoveReq {
        instance: inst,
        element,
    };
    ioctl_vtl(VTL_IOCTL_ELEM_REMOVE, &mut req)
}

/// Best-effort: `ELEM_REMOVE` on new kernels; empty mailslot park on older `vtl.ko`.
fn evacuate_element(conn: &Connection, library_id: i64, element: i32) -> io::Result<()> {
    match kernel_elem_remove(conn, library_id, element) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::ENODEV) => Ok(()),
        Err(e)
            if e.raw_os_error() == Some(libc::ENOTTY) || e.raw_os_error() == Some(libc::EINVAL) =>
        {
            if let Some(mail) = first_empty_mailslot(conn, library_id) {
                kernel_move_medium(conn, library_id, element, db_mailslot_to_element(mail))
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// Remove a tape from kernel changer inventory when DB says it is on shelf or out of robot.
pub(crate) fn evacuate_tape_from_changer(
    conn: &Connection,
    library_id: i64,
    tape_name: &str,
) -> Result<(), VtlError> {
    let kernel = kernel_inventory_snapshot(conn, library_id)?;
    if let Some(loc) = kernel.locations.get(tape_name) {
        evacuate_element(conn, library_id, loc.to_element()).map_err(VtlError::IoError)?;
    }
    Ok(())
}

/// Evacuate all shelved tapes that still appear in kernel GET_INVENTORY.
pub(crate) fn evacuate_shelved_tapes_from_kernel(
    conn: &Connection,
    library_id: i64,
) -> Result<usize, VtlError> {
    let mut stmt =
        conn.prepare("SELECT name FROM tapes WHERE library_id = ?1 AND shelf_id IS NOT NULL")?;
    let names: Vec<String> = stmt
        .query_map(params![library_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let kernel_before = kernel_inventory_snapshot(conn, library_id)?;
    let mut n = 0usize;
    for name in names {
        if !kernel_before.locations.contains_key(&name) {
            continue;
        }
        if evacuate_tape_from_changer(conn, library_id, &name).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

/// After kernel geometry change: pull kernel inventory into DB catalog (runtime robot stays in vtl.ko).
pub(crate) fn sync_all_online_libraries_after_geom() {
    crate::reconcile::pull_all_online_libraries_from_kernel();
}

pub(crate) fn warn_kernel_sync_failed(op: &str, e: &io::Error) {
    eprintln!(
        "Warning: database updated but kernel robot sync failed on {}: {} (run: vtladm -L <lib> robot sync-db or reconcile --pull)",
        op, e
    );
    log_error("robot_sync", &format!("{}: {}", op, e));
}

/// One row for CLI/Web changer inventory (slot / drive / mailslot).
#[derive(Debug, Clone)]
pub struct ChangerRow {
    pub label: String,
    pub tape_name: Option<String>,
    pub barcode: Option<String>,
}

/// mhVTL-style: runtime robot view comes from vtl.ko GET_INVENTORY.
#[derive(Debug, Clone)]
pub struct ChangerInventoryDisplay {
    pub source: &'static str,
    pub data_slots: Vec<ChangerRow>,
    pub drives: Vec<ChangerRow>,
    pub mailslots: Vec<ChangerRow>,
}

fn tape_barcode_map(
    conn: &Connection,
    library_id: i64,
) -> Result<HashMap<String, String>, VtlError> {
    let mut out = HashMap::new();
    let mut stmt = conn.prepare("SELECT name, barcode FROM tapes WHERE library_id = ?1")?;
    let rows = stmt.query_map(params![library_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (name, bc) = row?;
        out.insert(name, bc);
    }
    Ok(out)
}

fn row_barcode(map: &HashMap<String, String>, tape: &str) -> Option<String> {
    map.get(tape).cloned()
}

/// Inventory source for changer slots/drives: kernel GET_INVENTORY when robot_sync is on.
pub(crate) fn changer_inventory_uses_kernel() -> bool {
    robot_sync_enabled()
}

fn changer_inventory_from_db(
    conn: &Connection,
    library_id: i64,
) -> Result<ChangerInventoryDisplay, VtlError> {
    let mut data_slots = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT s.slot_id, t.name, t.barcode
         FROM slots s
         LEFT JOIN tapes t ON s.tape_id = t.id
         WHERE s.library_id = ?1 AND s.is_import_export = 0
         ORDER BY s.slot_id",
    )?;
    let rows = stmt.query_map(params![library_id], |r| {
        Ok((
            r.get::<_, i32>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in rows {
        let (id, name, bc) = row?;
        data_slots.push(ChangerRow {
            label: format!("slot{}", id),
            tape_name: name,
            barcode: bc,
        });
    }

    let mut drives = Vec::new();
    let mut dstmt = conn.prepare(
        "SELECT d.drive_id, t.name, t.barcode
         FROM drives d
         LEFT JOIN tapes t ON d.tape_id = t.id
         WHERE d.library_id = ?1
         ORDER BY d.drive_id",
    )?;
    let drows = dstmt.query_map(params![library_id], |r| {
        Ok((
            r.get::<_, i32>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in drows {
        let (id, name, bc) = row?;
        drives.push(ChangerRow {
            label: format!("drive{}", id),
            tape_name: name,
            barcode: bc,
        });
    }

    let mut mailslots = Vec::new();
    let mut mstmt = conn.prepare(
        "SELECT s.slot_id, t.name, t.barcode
         FROM slots s
         LEFT JOIN tapes t ON s.tape_id = t.id
         WHERE s.library_id = ?1 AND s.is_import_export != 0
         ORDER BY s.slot_id",
    )?;
    let mrows = mstmt.query_map(params![library_id], |r| {
        Ok((
            r.get::<_, i32>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in mrows {
        let (id, name, bc) = row?;
        mailslots.push(ChangerRow {
            label: format!("mail{}", id - MAILSLOT_OFFSET),
            tape_name: name,
            barcode: bc,
        });
    }

    Ok(ChangerInventoryDisplay {
        source: "db",
        data_slots,
        drives,
        mailslots,
    })
}

fn changer_inventory_from_kernel(
    conn: &Connection,
    library_id: i64,
) -> Result<ChangerInventoryDisplay, VtlError> {
    let snap = kernel_inventory_snapshot(conn, library_id)?;
    let barcodes = tape_barcode_map(conn, library_id)?;

    let mut by_slot: HashMap<i32, String> = HashMap::new();
    let mut by_drive: HashMap<i32, String> = HashMap::new();
    let mut by_mail: HashMap<i32, String> = HashMap::new();
    for (name, loc) in &snap.locations {
        match loc {
            MediumLocation::DataSlot(s) => {
                by_slot.insert(*s, name.clone());
            }
            MediumLocation::Drive(d) => {
                by_drive.insert(*d, name.clone());
            }
            MediumLocation::MailSlot(m) => {
                by_mail.insert(*m, name.clone());
            }
        }
    }

    let mut data_slots = Vec::new();
    let mut sstmt = conn.prepare(
        "SELECT slot_id FROM slots WHERE library_id = ?1 AND is_import_export = 0 ORDER BY slot_id",
    )?;
    let slot_ids = sstmt.query_map(params![library_id], |r| r.get::<_, i32>(0))?;
    for sid in slot_ids {
        let id = sid?;
        let tape = by_slot.get(&id).cloned();
        data_slots.push(ChangerRow {
            label: format!("slot{}", id),
            tape_name: tape.clone(),
            barcode: tape.as_ref().and_then(|t| row_barcode(&barcodes, t)),
        });
    }

    let mut drives = Vec::new();
    let mut dstmt =
        conn.prepare("SELECT drive_id FROM drives WHERE library_id = ?1 ORDER BY drive_id")?;
    let dids = dstmt.query_map(params![library_id], |r| r.get::<_, i32>(0))?;
    for did in dids {
        let id = did?;
        let tape = by_drive.get(&id).cloned();
        drives.push(ChangerRow {
            label: format!("drive{}", id),
            tape_name: tape.clone(),
            barcode: tape.as_ref().and_then(|t| row_barcode(&barcodes, t)),
        });
    }

    let mut mailslots = Vec::new();
    let mut mstmt = conn.prepare(
        "SELECT slot_id FROM slots WHERE library_id = ?1 AND is_import_export != 0 ORDER BY slot_id",
    )?;
    let mids = mstmt.query_map(params![library_id], |r| r.get::<_, i32>(0))?;
    for mid in mids {
        let id = mid?;
        let tape = by_mail.get(&id).cloned();
        mailslots.push(ChangerRow {
            label: format!("mail{}", id - MAILSLOT_OFFSET),
            tape_name: tape.clone(),
            barcode: tape.as_ref().and_then(|t| row_barcode(&barcodes, t)),
        });
    }

    Ok(ChangerInventoryDisplay {
        source: "kernel",
        data_slots,
        drives,
        mailslots,
    })
}

/// Changer inventory for CLI `inventory` and Web library status (mhVTL: kernel is runtime truth in backup mode).
pub fn changer_inventory_display(
    conn: &Connection,
    library_id: i64,
) -> Result<ChangerInventoryDisplay, VtlError> {
    if changer_inventory_uses_kernel() {
        match changer_inventory_from_kernel(conn, library_id) {
            Ok(d) => return Ok(d),
            Err(e) => {
                log_message(&format!(
                    "changer inventory: kernel GET_INVENTORY failed ({}); falling back to DB",
                    e
                ));
            }
        }
    }
    changer_inventory_from_db(conn, library_id)
}

#[cfg(test)]
mod tests {
    #[test]
    fn ioctl_numbers_match_kernel_ioc_macro() {
        const DIR_SHIFT: u32 = 30;
        const SIZE_SHIFT: u32 = 16;
        const TYPE_SHIFT: u32 = 8;
        let load_sz = std::mem::size_of::<super::VtlLoadReq>() as u32;
        let unload_sz = std::mem::size_of::<super::VtlUnloadReq>() as u32;
        let place_sz = std::mem::size_of::<super::VtlSlotPlaceReq>() as u32;
        let move_sz = std::mem::size_of::<super::VtlMoveReq>() as u32;
        let mk = |nr: u32, sz: u32| {
            (1u32 << DIR_SHIFT) | (sz << SIZE_SHIFT) | ((b'V' as u32) << TYPE_SHIFT) | nr
        };
        assert_eq!(mk(2, load_sz) as libc::c_ulong, super::VTL_IOCTL_LOAD_TAPE);
        assert_eq!(
            mk(3, unload_sz) as libc::c_ulong,
            super::VTL_IOCTL_UNLOAD_TAPE
        );
        assert_eq!(
            mk(6, place_sz) as libc::c_ulong,
            super::VTL_IOCTL_SLOT_PLACE
        );
        assert_eq!(
            mk(7, move_sz) as libc::c_ulong,
            super::VTL_IOCTL_MOVE_MEDIUM
        );
        let remove_sz = std::mem::size_of::<super::VtlElemRemoveReq>() as u32;
        assert_eq!(
            mk(9, remove_sz) as libc::c_ulong,
            super::VTL_IOCTL_ELEM_REMOVE
        );
        let inv_sz = std::mem::size_of::<super::VtlInventoryIoctl>() as u32;
        let mkwr = |nr: u32, sz: u32| {
            (3u32 << DIR_SHIFT) | (sz << SIZE_SHIFT) | ((b'V' as u32) << TYPE_SHIFT) | nr
        };
        assert_eq!(
            mkwr(8, inv_sz) as libc::c_ulong,
            super::VTL_IOCTL_GET_INVENTORY
        );
    }

    #[test]
    fn mailslot_element_map() {
        assert_eq!(super::db_mailslot_to_element(100), 2000);
        assert_eq!(super::db_mailslot_to_element(103), 2003);
    }
}
