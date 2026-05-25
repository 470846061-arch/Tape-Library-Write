//! Per-library iSCSI (LIO / targetcli) export records in SQLite for Web auto-fill and one-click unexport.

use std::convert::TryFrom;

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{resolve_library_id, VtlError};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct IscsiLibraryExportRecord {
    pub iqn: String,
    pub export_id: String,
    pub changer_sg: String,
    pub drive_sg: Vec<String>,
    pub lun_map: Vec<u32>,
    pub portal_ip: String,
    pub portal_port: u16,
    pub exported_at: String,
}

pub(crate) fn ensure_iscsi_exports_table(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS iscsi_library_exports (
            library_id INTEGER NOT NULL PRIMARY KEY
                REFERENCES vtl_libraries(id) ON DELETE CASCADE,
            iqn TEXT NOT NULL,
            export_id TEXT NOT NULL,
            changer_sg TEXT NOT NULL,
            drive_sg_json TEXT NOT NULL,
            lun_map_json TEXT NOT NULL,
            portal_ip TEXT NOT NULL,
            portal_port INTEGER NOT NULL,
            exported_at TEXT NOT NULL
        );
        "#,
    )
}

pub(crate) fn save_iscsi_library_export(
    conn: &Connection,
    library_id: i64,
    record: &IscsiLibraryExportRecord,
) -> Result<(), VtlError> {
    let drive_sg_json = serde_json::to_string(&record.drive_sg)
        .map_err(|e| VtlError::InvalidParameter(e.to_string()))?;
    let lun_map_json = serde_json::to_string(&record.lun_map)
        .map_err(|e| VtlError::InvalidParameter(e.to_string()))?;
    conn.execute(
        r#"
        INSERT INTO iscsi_library_exports (
            library_id, iqn, export_id, changer_sg, drive_sg_json, lun_map_json,
            portal_ip, portal_port, exported_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ON CONFLICT(library_id) DO UPDATE SET
            iqn = excluded.iqn,
            export_id = excluded.export_id,
            changer_sg = excluded.changer_sg,
            drive_sg_json = excluded.drive_sg_json,
            lun_map_json = excluded.lun_map_json,
            portal_ip = excluded.portal_ip,
            portal_port = excluded.portal_port,
            exported_at = excluded.exported_at
        "#,
        params![
            library_id,
            record.iqn,
            record.export_id,
            record.changer_sg,
            drive_sg_json,
            lun_map_json,
            record.portal_ip,
            i64::from(record.portal_port),
            record.exported_at,
        ],
    )?;
    Ok(())
}

pub(crate) fn load_iscsi_library_export(
    conn: &Connection,
    library_id: i64,
) -> Result<Option<IscsiLibraryExportRecord>, VtlError> {
    let row = conn.query_row(
        r#"
        SELECT iqn, export_id, changer_sg, drive_sg_json, lun_map_json,
               portal_ip, portal_port, exported_at
        FROM iscsi_library_exports WHERE library_id = ?1
        "#,
        params![library_id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, String>(7)?,
            ))
        },
    );
    match row {
        Ok((
            iqn,
            export_id,
            changer_sg,
            drive_sg_json,
            lun_map_json,
            portal_ip,
            portal_port,
            exported_at,
        )) => {
            let drive_sg: Vec<String> = serde_json::from_str(&drive_sg_json)
                .map_err(|e| VtlError::InvalidParameter(format!("drive_sg_json: {}", e)))?;
            let lun_map: Vec<u32> = serde_json::from_str(&lun_map_json)
                .map_err(|e| VtlError::InvalidParameter(format!("lun_map_json: {}", e)))?;
            let portal_port = u16::try_from(portal_port).map_err(|_| {
                VtlError::InvalidParameter(format!("invalid portal_port in DB: {}", portal_port))
            })?;
            Ok(Some(IscsiLibraryExportRecord {
                iqn,
                export_id,
                changer_sg,
                drive_sg,
                lun_map,
                portal_ip,
                portal_port,
                exported_at,
            }))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn load_iscsi_library_export_by_name(
    conn: &Connection,
    library_name: &str,
) -> Result<Option<IscsiLibraryExportRecord>, VtlError> {
    let library_id = resolve_library_id(conn, library_name)?;
    load_iscsi_library_export(conn, library_id)
}

pub(crate) fn delete_iscsi_library_export(
    conn: &Connection,
    library_id: i64,
) -> Result<(), VtlError> {
    conn.execute(
        "DELETE FROM iscsi_library_exports WHERE library_id = ?1",
        params![library_id],
    )?;
    Ok(())
}

pub(crate) fn delete_iscsi_library_export_by_name(
    conn: &Connection,
    library_name: &str,
) -> Result<(), VtlError> {
    let library_id = resolve_library_id(conn, library_name)?;
    delete_iscsi_library_export(conn, library_id)
}

/// True when targetcli exited 0 but export likely failed — do not persist to DB.
pub(crate) fn targetcli_stderr_blocks_export_save(stderr: &str) -> bool {
    if stderr.trim().is_empty() {
        return false;
    }
    let low = stderr.to_ascii_lowercase();
    stderr.contains("Cannot configure")
        || stderr.contains("Unknown configuration")
        || stderr.contains("No such path")
        || stderr.contains("No storage object named")
        || low.contains("already in use")
        || low.contains("wwn not valid")
        || low.contains("not valid as:")
}

pub(crate) fn list_iscsi_library_exports(
    conn: &Connection,
) -> Result<Vec<(String, IscsiLibraryExportRecord)>, VtlError> {
    let mut stmt = conn.prepare(
        r#"
        SELECT l.name, e.iqn, e.export_id, e.changer_sg, e.drive_sg_json, e.lun_map_json,
               e.portal_ip, e.portal_port, e.exported_at
        FROM iscsi_library_exports e
        INNER JOIN vtl_libraries l ON l.id = e.library_id
        ORDER BY l.name
        "#,
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, String>(8)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (
            name,
            iqn,
            export_id,
            changer_sg,
            drive_sg_json,
            lun_map_json,
            portal_ip,
            portal_port,
            exported_at,
        ) = row?;
        let drive_sg: Vec<String> = serde_json::from_str(&drive_sg_json)
            .map_err(|e| VtlError::InvalidParameter(format!("drive_sg_json: {}", e)))?;
        let lun_map: Vec<u32> = serde_json::from_str(&lun_map_json)
            .map_err(|e| VtlError::InvalidParameter(format!("lun_map_json: {}", e)))?;
        let portal_port = u16::try_from(portal_port).map_err(|_| {
            VtlError::InvalidParameter(format!("invalid portal_port in DB: {}", portal_port))
        })?;
        out.push((
            name,
            IscsiLibraryExportRecord {
                iqn,
                export_id,
                changer_sg,
                drive_sg,
                lun_map,
                portal_ip,
                portal_port,
                exported_at,
            },
        ));
    }
    Ok(out)
}

/// Resolve library name for advanced unexport (manual IQN/export_id) when it matches a DB row.
pub(crate) fn find_library_name_by_export_credentials(
    conn: &Connection,
    export_id: &str,
    iqn: &str,
) -> Result<Option<String>, VtlError> {
    let name: Option<String> = conn
        .query_row(
            r#"
            SELECT l.name FROM iscsi_library_exports e
            INNER JOIN vtl_libraries l ON l.id = e.library_id
            WHERE e.export_id = ?1 AND e.iqn = ?2
            "#,
            params![export_id, iqn],
            |r| r.get(0),
        )
        .optional()?;
    Ok(name)
}

pub(crate) fn new_iscsi_export_record(
    iqn: String,
    export_id: String,
    changer_sg: String,
    drive_sg: Vec<String>,
    lun_map: Vec<u32>,
    portal_ip: String,
    portal_port: u16,
) -> IscsiLibraryExportRecord {
    IscsiLibraryExportRecord {
        iqn,
        export_id,
        changer_sg,
        drive_sg,
        lun_map,
        portal_ip,
        portal_port,
        exported_at: Utc::now().to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils;

    #[test]
    fn targetcli_stderr_blocks_export_save_detects_failure_noise() {
        assert!(super::targetcli_stderr_blocks_export_save(
            "No such path /iscsi/iqn.foo/tpg1/luns"
        ));
        assert!(!super::targetcli_stderr_blocks_export_save(""));
    }

    #[test]
    fn iscsi_export_save_load_delete_roundtrip() {
        let dir = test_utils::prepare_temp_vtl("iscsi_export_db");
        let _ = crate::create_named_library("marstor", 2, 10);
        let conn = crate::init_db().expect("db");
        let lib_id: i64 = conn
            .query_row(
                "SELECT id FROM vtl_libraries WHERE name = ?1",
                params!["marstor"],
                |r| r.get(0),
            )
            .expect("marstor");
        let rec = new_iscsi_export_record(
            "iqn.2025-01.com.marstor:marstor-1".into(),
            "mmarstor_1".into(),
            "/dev/sg5".into(),
            vec!["/dev/sg6".into(), "/dev/sg7".into()],
            vec![0, 1, 2],
            "0.0.0.0".into(),
            3260,
        );
        save_iscsi_library_export(&conn, lib_id, &rec).expect("save");
        let loaded = load_iscsi_library_export(&conn, lib_id)
            .expect("load")
            .expect("some");
        assert_eq!(loaded.iqn, rec.iqn);
        assert_eq!(loaded.drive_sg, rec.drive_sg);
        assert_eq!(loaded.lun_map, rec.lun_map);
        delete_iscsi_library_export(&conn, lib_id).expect("del");
        assert!(load_iscsi_library_export(&conn, lib_id)
            .expect("load2")
            .is_none());
        let _ = dir;
    }
}
