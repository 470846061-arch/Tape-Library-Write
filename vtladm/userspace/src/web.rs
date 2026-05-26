//! Web：全站需登录（算术验证码登录、HttpOnly cookie）；数据与写操作 API 均在校验会话后可用。

use axum::body::Body;
use axum::extract::Query;
use axum::extract::Request;
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::{extract::State, middleware, Router};
use axum_extra::extract::cookie::CookieJar;
use chrono::{Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
#[cfg(unix)]
use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

#[derive(serde::Deserialize)]
struct LibraryQuery {
    library: Option<String>,
}

/// `/api/tapes` 支持分页。
#[derive(serde::Deserialize)]
struct TapesQuery {
    library: Option<String>,
    /// 起始偏移量（默认 0）。
    #[serde(default)]
    offset: Option<i64>,
    /// 每页条数上限（默认 5000，最大 50000）。
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/manage/iscsi/library-export-defaults`：可选 `regenerate=1` 忽略库内已存记录并生成新 IQN。
#[derive(serde::Deserialize)]
struct IscsiExportDefaultsQuery {
    library: Option<String>,
    #[serde(default)]
    regenerate: bool,
}

/// `GET /api/manage/transport/scan-sg`（及兼容的 `iscsi/scan-sg`）：按库 drives 表台数选取 VTL SCSI 节点。
#[derive(serde::Deserialize)]
struct TransportScanQuery {
    library: Option<String>,
    #[serde(default)]
    prefer_scsi_host: Option<u32>,
    /// `local` | `iscsi` | `fc` — 仅影响响应中的 `transport_hint` 文案。
    #[serde(default)]
    transport: Option<String>,
}

/// RAII guard：在 `spawn_blocking` 中安全地临时切换 `CURRENT_LIBRARY`。
/// Drop 时自动恢复原值，即使闭包 panic / 提前 `?` 返回也不会泄漏。
struct LibraryGuard {
    prev: String,
}

impl LibraryGuard {
    fn new(lib: &str) -> Self {
        let prev = super::current_library_name();
        super::set_current_library(lib);
        LibraryGuard { prev }
    }
}

impl Drop for LibraryGuard {
    fn drop(&mut self) {
        super::set_current_library(&self.prev);
    }
}

#[derive(serde::Serialize)]
struct LibraryRow {
    id: i64,
    name: String,
    created_at: String,
    is_offline_storage: bool,
}

#[derive(serde::Serialize)]
struct TapeRow {
    name: String,
    barcode: String,
    capacity_bytes: u64,
    used_bytes: u64,
    slot: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shelf_name: Option<String>,
    in_drive: bool,
}

fn resolve_library_from_query(q: &LibraryQuery) -> Result<String, String> {
    super::resolve_active_library_name(q.library.as_deref()).map_err(|e| e.to_string())
}

fn count_lsscsi_vtl_lines() -> u32 {
    use std::process::Command;
    let out = match Command::new("lsscsi").arg("-g").output() {
        Ok(o) if o.status.success() => o,
        _ => return 0,
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("VTL"))
        .count() as u32
}

fn product_limits_json() -> serde_json::Value {
    json!({
        "max_online_libraries": super::VTL_KERNEL_MAX_ONLINE_LIBRARIES,
        "max_drives_per_library": super::VTL_KERNEL_MAX_DRIVES_PER_LIB,
        "max_data_slots_per_library": super::VTL_KERNEL_MAX_DATA_SLOTS_PER_LIB,
    })
}

async fn api_libraries() -> impl IntoResponse {
    let db_path = super::get_config().db_path.display().to_string();
    let res: Result<(Vec<LibraryRow>, u32), String> = tokio::task::spawn_blocking(|| {
        let conn = super::init_db().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT id, name, created_at FROM vtl_libraries ORDER BY name")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                let name: String = r.get(1)?;
                Ok(LibraryRow {
                    id: r.get(0)?,
                    name: name.clone(),
                    created_at: r.get(2)?,
                    is_offline_storage: name == super::OFFLINE_LIBRARY_NAME,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for row in rows {
            let row = row.map_err(|e| e.to_string())?;
            if super::is_test_only_library_name(&row.name) {
                continue;
            }
            v.push(row);
        }
        let vtl_scsi = count_lsscsi_vtl_lines();
        Ok((v, vtl_scsi))
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok((libs, vtl_scsi_lines)) => {
            let online_count = libs.iter().filter(|r| !r.is_offline_storage).count();
            let mut hint = String::new();
            if online_count == 0 {
                hint = if vtl_scsi_lines > 0 {
                    "数据库中无在线库，但 lsscsi 已看到 vtl.ko SCSI 设备（proc_name=VTL）：请打开「磁带库」页创建库（如 marstor），以写入数据库并与内核几何对齐。".into()
                } else {
                    "数据库中无在线库，且未检测到 vtl.ko SCSI 设备：请先 vtl-kernelctl start / insmod vtl，再在「磁带库」页创建库。".into()
                };
            }
            (
                StatusCode::OK,
                Json(json!({
                    "libraries": libs,
                    "db_path": db_path,
                    "online_count": online_count,
                    "vtl_scsi_lines": vtl_scsi_lines,
                    "hint": if hint.is_empty() { serde_json::Value::Null } else { json!(hint) },
                    "product_limits": product_limits_json(),
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e, "db_path": db_path })),
        )
            .into_response(),
    }
}

async fn api_tapes(Query(q): Query<TapesQuery>) -> impl IntoResponse {
    let lib = match super::resolve_active_library_name(q.library.as_deref()) {
        Ok(l) => l,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let offset = q.offset.unwrap_or(0).max(0);
    let limit = q.limit.unwrap_or(5000).clamp(1, 50000);
    let lib_for_db = lib.clone();
    let res: Result<(Vec<TapeRow>, i64), String> = tokio::task::spawn_blocking(move || {
        let conn = super::init_db().map_err(|e| e.to_string())?;
        let library_id =
            super::resolve_library_id(&conn, &lib_for_db).map_err(|e| e.to_string())?;
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tapes WHERE library_id = ?1",
                rusqlite::params![library_id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT t.name, t.barcode, t.capacity_bytes, t.used_bytes, t.slot, sh.name,
                 EXISTS(SELECT 1 FROM drives d WHERE d.library_id = t.library_id AND d.tape_id = t.id)
                 FROM tapes t
                 LEFT JOIN shelves sh ON t.shelf_id = sh.id
                 WHERE t.library_id = ?1 ORDER BY t.name LIMIT ?2 OFFSET ?3",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![library_id, limit, offset], |r| {
                Ok(TapeRow {
                    name: r.get(0)?,
                    barcode: r.get(1)?,
                    capacity_bytes: r.get(2)?,
                    used_bytes: r.get(3)?,
                    slot: r.get(4)?,
                    shelf_name: r.get(5)?,
                    in_drive: r.get::<_, i64>(6)? != 0,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for row in rows {
            v.push(row.map_err(|e| e.to_string())?);
        }
        Ok((v, total))
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok((v, total)) => (
            StatusCode::OK,
            Json(json!({
                "library": lib,
                "tapes": v,
                "total": total,
                "offset": offset,
                "limit": limit,
                "truncated": (offset + limit) < total,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

async fn api_status(Query(q): Query<LibraryQuery>) -> impl IntoResponse {
    let lib = match resolve_library_from_query(&q) {
        Ok(l) => l,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
    };
    let res: Result<serde_json::Value, String> = tokio::task::spawn_blocking(move || {
        let conn = super::init_db().map_err(|e| e.to_string())?;
        let library_id = super::resolve_library_id(&conn, &lib).map_err(|e| e.to_string())?;
        let tape_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tapes WHERE library_id = ?1",
                rusqlite::params![library_id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let loaded_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM drives WHERE library_id = ?1 AND tape_id IS NOT NULL",
                rusqlite::params![library_id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let drive_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM drives WHERE library_id = ?1",
                rusqlite::params![library_id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let slot_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM slots WHERE library_id = ?1 AND is_import_export = 0",
                rusqlite::params![library_id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "library": lib,
            "tape_count": tape_count,
            "loaded_in_drives": loaded_count,
            "drives": drive_count,
            "data_slots": slot_count,
        }))
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

async fn api_fabric() -> impl IntoResponse {
    let c = super::get_config();
    let iscsi_exports: Vec<serde_json::Value> = match super::init_db() {
        Ok(conn) => super::iscsi_export::list_iscsi_library_exports(&conn)
            .unwrap_or_default()
            .into_iter()
            .map(|(name, rec)| {
                json!({
                    "library": name,
                    "iqn": rec.iqn,
                    "export_id": rec.export_id,
                    "portal": format!("{}:{}", rec.portal_ip, rec.portal_port),
                    "exported_at": rec.exported_at,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    let body = json!({
        "transport": c.transport.as_conf_str(),
        "iscsi_iqn": c.iscsi_iqn,
        "iscsi_portals": c.iscsi_portals,
        "fc_wwpn": c.fc_wwpn,
        "kernel_reload_on_db_change": c.kernel_reload_on_db_change,
        "kernel_geom_prefer_ioctl": c.kernel_geom_prefer_ioctl,
        "vtl_reload_scan_delay_ms": c.vtl_reload_scan_delay_ms,
        "log_max_bytes": c.log_max_bytes,
        "iscsi_exports_in_db": iscsi_exports,
        "patrol_hint": "run: vtladm patrol  (or GET /api/patrol with session)",
        "product_limits": product_limits_json(),
    });
    (StatusCode::OK, Json(body)).into_response()
}

async fn api_patrol_run(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let res = tokio::task::spawn_blocking(|| -> Result<serde_json::Value, String> {
        let exe = trusted_vtladm_binary()?;
        let out = std::process::Command::new(&exe)
            .arg("patrol")
            .output()
            .map_err(|e| e.to_string())?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let code = out.status.code().unwrap_or(-1);
        Ok(json!({
            "exit_code": code,
            "stdout": stdout,
            "stderr": stderr,
            "ok": code == super::patrol::PATROL_EXIT_OK,
            "warn": code == super::patrol::PATROL_EXIT_WARN,
            "crit": code == super::patrol::PATROL_EXIT_CRIT,
        }))
    })
    .await;
    match res {
        Ok(Ok(j)) => (StatusCode::OK, Json(j)).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("后台任务失败: {}", e) })),
        )
            .into_response(),
    }
}

fn trusted_vtladm_binary() -> Result<PathBuf, String> {
    let configured = std::env::var("VTLADM_WEB_PATROL_BIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/opt/vtladm/bin/vtladm".to_string());
    let path = PathBuf::from(configured);
    if !path.is_absolute() {
        return Err("patrol binary path must be absolute".into());
    }
    let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        return Err("patrol binary path must not be a symlink".into());
    }
    Ok(path)
}

#[derive(Serialize)]
struct ShelfRow {
    id: i64,
    name: String,
    is_default_unused: i64,
}

async fn api_shelves(Query(q): Query<LibraryQuery>) -> impl IntoResponse {
    let lib = match resolve_library_from_query(&q) {
        Ok(l) => l,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
    };
    let lib_for_db = lib.clone();
    let res: Result<Vec<ShelfRow>, String> = tokio::task::spawn_blocking(move || {
        let conn = super::init_db().map_err(|e| e.to_string())?;
        let library_id =
            super::resolve_library_id(&conn, &lib_for_db).map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT id, name, is_default_unused FROM shelves WHERE library_id = ?1 ORDER BY is_default_unused DESC, name",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![library_id], |r| {
                Ok(ShelfRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    is_default_unused: r.get(2)?,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for row in rows {
            v.push(row.map_err(|e| e.to_string())?);
        }
        Ok(v)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(v) => (
            StatusCode::OK,
            Json(json!({ "library": lib, "shelves": v })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn api_offline_shelves() -> impl IntoResponse {
    let res: Result<Vec<ShelfRow>, String> = tokio::task::spawn_blocking(|| {
        let conn = super::init_db().map_err(|e| e.to_string())?;
        let offline_id =
            super::ensure_offline_library(&conn).map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT id, name, is_default_unused FROM shelves WHERE library_id = ?1 AND is_default_unused = 0 ORDER BY name",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![offline_id], |r| {
                Ok(ShelfRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    is_default_unused: r.get(2)?,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for row in rows {
            v.push(row.map_err(|e| e.to_string())?);
        }
        Ok(v)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(v) => (StatusCode::OK, Json(json!({ "shelves": v }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

async fn api_empty_slots(Query(q): Query<LibraryQuery>) -> impl IntoResponse {
    let lib = match resolve_library_from_query(&q) {
        Ok(l) => l,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
    };
    let lib_for_db = lib.clone();
    let res: Result<Vec<i32>, String> = tokio::task::spawn_blocking(move || {
        let conn = super::init_db().map_err(|e| e.to_string())?;
        let library_id =
            super::resolve_library_id(&conn, &lib_for_db).map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT slot_id FROM slots WHERE library_id = ?1 AND tape_id IS NULL AND is_import_export = 0 ORDER BY slot_id",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![library_id], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for row in rows {
            v.push(row.map_err(|e| e.to_string())?);
        }
        Ok(v)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(v) => {
            let n = v.len();
            (
                StatusCode::OK,
                Json(json!({ "library": lib, "empty_slots": v, "empty_slot_count": n })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Serialize)]
struct LibraryStatusRow {
    library: String,
    tape_count: i64,
    loaded_in_drives: i64,
    drives: i64,
    data_slots: i64,
}

async fn api_libraries_status() -> impl IntoResponse {
    let res: Result<Vec<serde_json::Value>, String> = tokio::task::spawn_blocking(|| {
        let conn = super::init_db().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT l.name,
                 (SELECT COUNT(*) FROM tapes WHERE library_id = l.id),
                 (SELECT COUNT(*) FROM drives WHERE library_id = l.id AND tape_id IS NOT NULL),
                 (SELECT COUNT(*) FROM drives WHERE library_id = l.id),
                 (SELECT COUNT(*) FROM slots WHERE library_id = l.id AND is_import_export = 0)
                 FROM vtl_libraries l ORDER BY l.name",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok(LibraryStatusRow {
                    library: r.get(0)?,
                    tape_count: r.get(1)?,
                    loaded_in_drives: r.get(2)?,
                    drives: r.get(3)?,
                    data_slots: r.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for row in rows {
            let row = row.map_err(|e| e.to_string())?;
            if super::is_test_only_library_name(&row.library) {
                continue;
            }
            v.push(row);
        }
        let mut libs_json: Vec<serde_json::Value> = Vec::new();
        for row in v {
            let mut obj = serde_json::Map::new();
            obj.insert("library".into(), json!(row.library));
            obj.insert("tape_count".into(), json!(row.tape_count));
            obj.insert("loaded_in_drives".into(), json!(row.loaded_in_drives));
            obj.insert("drives".into(), json!(row.drives));
            obj.insert("data_slots".into(), json!(row.data_slots));
            match super::iscsi_export::load_iscsi_library_export_by_name(&conn, &row.library) {
                Ok(Some(rec)) => {
                    obj.insert("iscsi_exported".into(), json!(true));
                    obj.insert("iscsi_iqn".into(), json!(rec.iqn));
                    obj.insert("iscsi_export_id".into(), json!(rec.export_id));
                    obj.insert("iscsi_exported_at".into(), json!(rec.exported_at));
                }
                Ok(None) => {
                    obj.insert("iscsi_exported".into(), json!(false));
                }
                Err(e) => {
                    obj.insert("iscsi_exported".into(), json!(null));
                    obj.insert("iscsi_export_error".into(), json!(e.to_string()));
                }
            }
            libs_json.push(serde_json::Value::Object(obj));
        }
        Ok(libs_json)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(v) => (StatusCode::OK, Json(json!({ "libraries": v }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// 单库详情：基本信息计数、驱动器行、磁带行（供磁带库主区三块表）。
async fn api_library_detail(Query(q): Query<LibraryQuery>) -> impl IntoResponse {
    let lib_for = match resolve_library_from_query(&q) {
        Ok(l) => l,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
    };
    if super::is_test_only_library_name(&lib_for) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "未找到库" }))).into_response();
    }
    let res = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, (StatusCode, String)> {
        let conn = super::init_db().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
            )
        })?;
        let library_id = match super::resolve_library_id(&conn, &lib_for) {
            Ok(id) => id,
            Err(super::VtlError::LibraryNotFound(name)) => {
                return Err((StatusCode::NOT_FOUND, format!("未找到库: {}", name)));
            }
            Err(e) => {
                return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
            }
        };
        let (lid, name, created_at): (i64, String, String) = conn
            .query_row(
                "SELECT id, name, created_at FROM vtl_libraries WHERE id = ?1",
                rusqlite::params![library_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let is_offline = name == super::OFFLINE_LIBRARY_NAME;

        let online_libs = super::count_exported_online_libraries(&conn)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let can_delete_online =
            !is_offline && super::is_kernel_exported_library_name(&name) && online_libs > 1;

        let tape_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tapes WHERE library_id = ?1",
                rusqlite::params![library_id],
                |r| r.get(0),
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let changer = super::robot_sync::changer_inventory_display(&conn, library_id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let loaded_in_drives: i64 = if changer.source == "kernel" {
            changer
                .drives
                .iter()
                .filter(|d| d.tape_name.is_some())
                .count() as i64
        } else {
            conn.query_row(
                "SELECT COUNT(*) FROM drives WHERE library_id = ?1 AND tape_id IS NOT NULL",
                rusqlite::params![library_id],
                |r| r.get(0),
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        };
        let drive_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM drives WHERE library_id = ?1",
                rusqlite::params![library_id],
                |r| r.get(0),
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let data_slots: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM slots WHERE library_id = ?1 AND is_import_export = 0",
                rusqlite::params![library_id],
                |r| r.get(0),
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mail_slots: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM slots WHERE library_id = ?1 AND is_import_export != 0",
                rusqlite::params![library_id],
                |r| r.get(0),
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let max_drives: String = conn
            .query_row(
                "SELECT value FROM library_config WHERE library_id = ?1 AND key = 'max_drives'",
                rusqlite::params![library_id],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| "2".to_string());
        let slots: String = conn
            .query_row(
                "SELECT value FROM library_config WHERE library_id = ?1 AND key = 'slots'",
                rusqlite::params![library_id],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| "10".to_string());

        let mut dstmt = conn
            .prepare(
                "SELECT d.drive_id, t.name, t.barcode FROM drives d LEFT JOIN tapes t ON t.id = d.tape_id WHERE d.library_id = ?1 ORDER BY d.drive_id",
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let drives: Vec<serde_json::Value> = dstmt
            .query_map(rusqlite::params![library_id], |r| {
                Ok(json!({
                    "drive_id": r.get::<_, i64>(0)?,
                    "tape_name": r.get::<_, Option<String>>(1)?,
                    "tape_barcode": r.get::<_, Option<String>>(2)?,
                }))
            })
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let mut tstmt = conn
            .prepare(
                "SELECT t.name, t.barcode, t.capacity_bytes, t.used_bytes, t.slot, sh.name,
                 EXISTS(SELECT 1 FROM drives d WHERE d.library_id = t.library_id AND d.tape_id = t.id)
                 FROM tapes t
                 LEFT JOIN shelves sh ON t.shelf_id = sh.id
                 WHERE t.library_id = ?1 ORDER BY t.name",
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let tapes: Vec<serde_json::Value> = tstmt
            .query_map(rusqlite::params![library_id], |r| {
                Ok(json!({
                    "name": r.get::<_, String>(0)?,
                    "barcode": r.get::<_, String>(1)?,
                    "capacity_bytes": r.get::<_, i64>(2)?,
                    "used_bytes": r.get::<_, i64>(3)?,
                    "slot": r.get::<_, Option<i32>>(4)?,
                    "shelf_name": r.get::<_, Option<String>>(5)?,
                    "in_drive": r.get::<_, i64>(6)? != 0,
                }))
            })
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let changer_json = |rows: &[super::robot_sync::ChangerRow]| -> Vec<serde_json::Value> {
            rows.iter()
                .map(|r| {
                    json!({
                        "label": r.label,
                        "tape_name": r.tape_name,
                        "barcode": r.barcode,
                    })
                })
                .collect()
        };

        Ok(json!({
            "library": {
                "id": lid,
                "name": name,
                "created_at": created_at,
                "is_offline_storage": is_offline,
                "tape_count": tape_count,
                "loaded_in_drives": loaded_in_drives,
                "drive_count": drive_count,
                "data_slots": data_slots,
                "mail_slots": mail_slots,
                "max_drives": max_drives,
                "slots": slots,
                "can_delete_online": can_delete_online,
                "inventory_source": changer.source,
            },
            "drives": drives,
            "tapes": tapes,
            "changer": {
                "source": changer.source,
                "data_slots": changer_json(&changer.data_slots),
                "drives": changer_json(&changer.drives),
                "mailslots": changer_json(&changer.mailslots),
            },
        }))
    })
    .await;

    match res {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)).into_response(),
        Ok(Err((status, msg))) => (status, Json(json!({ "error": msg }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

fn session_token(jar: &CookieJar) -> Option<String> {
    jar.get("vtl_session")
        .map(|c| c.value().trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Cookie 头中的 `vtl_session`（供中间件使用；与 [`session_token`] 语义一致）。
fn session_token_from_cookie_header(headers: &axum::http::HeaderMap) -> Option<String> {
    let hv = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in hv.split(';') {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix("vtl_session=") {
            let t = rest.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn session_authenticated(st: &super::web_auth::WebState, headers: &axum::http::HeaderMap) -> bool {
    session_token_from_cookie_header(headers)
        .as_deref()
        .and_then(|tok| st.session_username(Some(tok)))
        .is_some()
}

fn csrf_header_ok(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-vtl-csrf")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// 主配置未完成时，仅允许访问初始化向导相关路径（仍须已登录）。
fn path_allowed_during_setup(method: &Method, path: &str) -> bool {
    matches!(
        (method, path),
        (&Method::GET, "/admin/setup-init")
            | (&Method::GET, "/api/setup/status")
            | (&Method::POST, "/api/setup/complete")
    )
}

/// 无需会话即可访问的路由（登录页、验证码、登录接口）。
fn is_public_route(method: &Method, path: &str) -> bool {
    matches!(
        (method, path),
        (&Method::GET, "/login") | (&Method::GET, "/api/captcha") | (&Method::POST, "/api/login")
    )
}

async fn require_authenticated(
    State(st): State<Arc<super::web_auth::WebState>>,
    request: Request,
    next: Next,
) -> Response {
    let path_owned = request.uri().path().to_string();
    let method = request.method().clone();
    let path = path_owned.as_str();
    if is_public_route(request.method(), path) {
        if path == "/login" && session_authenticated(&st, request.headers()) {
            return Redirect::to("/").into_response();
        }
        return next.run(request).await;
    }
    if session_authenticated(&st, request.headers()) {
        if method == Method::POST && !csrf_header_ok(request.headers()) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "缺少 CSRF 请求头" })),
            )
                .into_response();
        }
        if super::setup_gate_active() && !path_allowed_during_setup(&method, path) {
            if path.starts_with("/api/") {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "error": "请先完成初始化配置（数据库与目录路径）",
                        "code": "setup_required"
                    })),
                )
                    .into_response();
            }
            return Redirect::to("/admin/setup-init").into_response();
        }
        return next.run(request).await;
    }
    if path.starts_with("/api/") {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "未登录" }))).into_response();
    }
    Redirect::to("/login").into_response()
}

fn require_session(st: &super::web_auth::WebState, jar: &CookieJar) -> Result<(), Response> {
    let tok = session_token(jar).ok_or_else(|| {
        (StatusCode::UNAUTHORIZED, Json(json!({ "error": "未登录" }))).into_response()
    })?;
    st.session_username(Some(&tok)).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "会话无效或已过期" })),
        )
            .into_response()
    })?;
    Ok(())
}

fn json_cookie_response(
    status: StatusCode,
    body: serde_json::Value,
    set_cookie: Option<&str>,
) -> Response {
    let mut res = Response::builder()
        .status(status)
        .header("content-type", "application/json");
    if let Some(c) = set_cookie {
        if let Ok(hv) = HeaderValue::from_str(c) {
            res = res.header(SET_COOKIE, hv);
        }
    }
    let body = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    res.body(Body::from(body))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "build response").into_response())
}

fn login_rate_key(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
        })
        .unwrap_or("unknown")
        .to_string()
}

fn cookie_secure_enabled() -> bool {
    if let Ok(v) = std::env::var("VTLADM_WEB_COOKIE_SECURE") {
        return !matches!(v.trim(), "0" | "false" | "FALSE" | "no" | "NO");
    }
    std::env::var("VTLADM_WEB_COOKIE_SECURE_EFFECTIVE")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

fn session_cookie(token: &str, max_age: u64) -> String {
    let secure = if cookie_secure_enabled() {
        "; Secure"
    } else {
        ""
    };
    format!(
        "vtl_session={}; Path=/; HttpOnly; SameSite=Lax{}; Max-Age={}",
        token, secure, max_age
    )
}

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
    captcha_id: String,
    captcha_answer: String,
}

async fn api_captcha(State(st): State<Arc<super::web_auth::WebState>>) -> impl IntoResponse {
    let (id, q) = st.new_captcha();
    Json(json!({ "captcha_id": id, "question": q })).into_response()
}

async fn api_login(
    State(st): State<Arc<super::web_auth::WebState>>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> impl IntoResponse {
    let login_key = login_rate_key(&headers);
    if let Err(wait) = st.login_allowed(&login_key) {
        super::log_error(
            "api_login",
            &format!(
                "登录频率限制：IP={} 剩余锁定 {}s",
                login_key,
                wait.as_secs()
            ),
        );
        return json_cookie_response(
            StatusCode::TOO_MANY_REQUESTS,
            json!({ "error": "登录尝试过多，请稍后再试", "retry_after_secs": wait.as_secs().max(1) }),
            None,
        );
    }
    if !st.verify_captcha(&body.captcha_id, body.captcha_answer.trim()) {
        st.record_login_failure(&login_key);
        super::log_error(
            "api_login",
            &format!(
                "验证码校验失败：IP={} user={}",
                login_key,
                body.username.trim()
            ),
        );
        return json_cookie_response(
            StatusCode::UNAUTHORIZED,
            json!({ "error": "验证码错误或已过期" }),
            None,
        );
    }
    let username = body.username.trim();
    // 密码不 trim：与 web_auth::change_password 一致；保留前后空格的合法密码字符。
    // 旧版 .trim() 会让 "mypass " 被改成 "mypass"，与 bcrypt 写入路径不一致导致永远登录失败。
    let password = body.password.as_str();
    match st.login(username, password) {
        Ok(tok) => {
            st.record_login_success(&login_key);
            super::log_message(&format!("Web 登录成功：user={} IP={}", username, login_key));
            let c = session_cookie(&tok, super::web_auth::SESSION_SECS);
            json_cookie_response(StatusCode::OK, json!({ "ok": true }), Some(&c))
        }
        Err(super::web_auth::LoginFail::AuthFile) => {
            super::log_error(
                "api_login",
                &format!(
                    "无法读取认证文件 web_admin.json：IP={} user={} path={}",
                    login_key,
                    username,
                    st.auth_file.display()
                ),
            );
            json_cookie_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({
                    "error": "无法读取 Web 认证文件（web_admin.json），请在服务器执行: vtladm reset-web-auth"
                }),
                None,
            )
        }
        Err(super::web_auth::LoginFail::BadCredentials) => {
            st.record_login_failure(&login_key);
            super::log_error(
                "api_login",
                &format!("用户名或密码错误：IP={} user={}", login_key, username),
            );
            json_cookie_response(
                StatusCode::UNAUTHORIZED,
                json!({
                    "error": "用户名或密码错误",
                    "hint": format!(
                        "默认用户 {}，若忘记密码请在服务器执行: vtladm reset-web-auth",
                        super::web_auth::DEFAULT_WEB_USER
                    )
                }),
                None,
            )
        }
    }
}

async fn api_logout(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
) -> impl IntoResponse {
    if let Some(t) = session_token(&jar) {
        st.logout_token(&t);
    }
    json_cookie_response(
        StatusCode::OK,
        json!({ "ok": true }),
        Some(&session_cookie("", 0)),
    )
}

#[derive(Deserialize)]
struct ChangePasswordBody {
    old_password: String,
    new_password: String,
}

async fn api_change_password(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ChangePasswordBody>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    match st.change_password(&body.old_password, &body.new_password) {
        Ok(()) => json_cookie_response(
            StatusCode::OK,
            json!({ "ok": true }),
            Some(&session_cookie("", 0)),
        ),
        Err(e) => json_cookie_response(StatusCode::BAD_REQUEST, json!({ "error": e }), None),
    }
}

async fn api_setup_status() -> impl IntoResponse {
    let setup_required = super::setup_gate_active();
    (
        StatusCode::OK,
        Json(json!({
            "setup_required": setup_required,
            "defaults": {
                "db_path": super::DEFAULT_INIT_DB_PATH,
                "tape_dir": super::DEFAULT_INIT_TAPE_DIR,
                "log_dir": super::DEFAULT_INIT_LOG_DIR,
                "kernel_vtl_reload_script": super::DEFAULT_INIT_KERNEL_RELOAD_SCRIPT,
                "vtl_ko": super::DEFAULT_INIT_VTL_KO,
                "vtl_reload_scan_delay_ms": "",
            }
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct SetupCompleteBody {
    db_path: String,
    tape_dir: String,
    log_dir: String,
    kernel_vtl_reload_script: String,
    vtl_ko: String,
    #[serde(default)]
    vtl_reload_scan_delay_ms: String,
    /// 为 true 时向导提交成功后立即执行 `maybe_reload_kernel_vtl_after_db_change`（ioctl / 脚本；默认 false）。
    #[serde(default)]
    run_kernel_reload_now: bool,
}

async fn api_setup_complete(Json(body): Json<SetupCompleteBody>) -> impl IntoResponse {
    let db_path = body.db_path;
    let tape_dir = body.tape_dir;
    let log_dir = body.log_dir;
    let kernel_vtl_reload_script = body.kernel_vtl_reload_script;
    let vtl_ko = body.vtl_ko;
    let vtl_reload_scan_delay_ms = body.vtl_reload_scan_delay_ms;
    let run_kernel_reload_now = body.run_kernel_reload_now;
    let res = tokio::task::spawn_blocking(move || {
        super::try_complete_primary_vtl_setup_from_web(
            &db_path,
            &tape_dir,
            &log_dir,
            &kernel_vtl_reload_script,
            &vtl_ko,
            &vtl_reload_scan_delay_ms,
            run_kernel_reload_now,
        )
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    match res {
        Ok(geom) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "kernel_geom": geom.kernel_geom,
                "kernel_geom_detail": geom.kernel_geom_detail,
                "scsi_rescan": geom.scsi_rescan,
            })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageTapeCreate {
    library: String,
    name: String,
    size: String,
    shelf: Option<String>,
}

async fn api_manage_tape_create(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageTapeCreate>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let name = body.name.clone();
    let size_s = body.size.clone();
    let shelf = body.shelf.clone();
    let res = tokio::task::spawn_blocking(move || {
        let size = super::parse_size(&size_s).map_err(|e| e.to_string())?;
        let _guard = LibraryGuard::new(&lib);
        super::create_tape(&name, size, shelf.as_deref()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageLibraryCreate {
    name: String,
    drives: i32,
    slots: i32,
}

async fn api_manage_library_create(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageLibraryCreate>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let name = body.name.clone();
    let drives = body.drives;
    let slots = body.slots;
    let res = tokio::task::spawn_blocking(move || {
        super::create_named_library(&name, drives, slots).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(geom) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "kernel_geom": geom.kernel_geom,
                "kernel_geom_detail": geom.kernel_geom_detail,
                "scsi_rescan": geom.scsi_rescan,
            })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageLibraryDelete {
    name: String,
}

async fn api_manage_library_delete(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageLibraryDelete>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let name = body.name.clone();
    let res = tokio::task::spawn_blocking(move || {
        super::delete_named_library(&name).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok((warnings, geom)) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "file_warnings": warnings,
                "kernel_geom": geom.kernel_geom,
                "kernel_geom_detail": geom.kernel_geom_detail,
            })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageShelfDelete {
    library: String,
    name: String,
}

async fn api_manage_shelf_delete(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageShelfDelete>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let name = body.name.clone();
    let res = tokio::task::spawn_blocking(move || {
        super::delete_shelf_in_library(&lib, &name).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageTapeDelete {
    library: String,
    name: String,
}

async fn api_manage_tape_delete(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageTapeDelete>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let name = body.name.clone();
    let res = tokio::task::spawn_blocking(move || {
        super::delete_tape_in_library(&lib, &name).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(warning) => {
            let mut resp = json!({ "ok": true });
            if let Some(w) = warning {
                resp["warning"] = json!(w);
            }
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageTapeInit {
    library: String,
    name: String,
}

async fn api_manage_tape_init(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageTapeInit>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let name = body.name.clone();
    let res = tokio::task::spawn_blocking(move || {
        super::init_tape_in_library(&lib, &name).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageShelfCreate {
    library: String,
    name: String,
}

async fn api_manage_shelf_create(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageShelfCreate>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let name = body.name.clone();
    let res = tokio::task::spawn_blocking(move || {
        let _guard = LibraryGuard::new(&lib);
        super::create_shelf(&name).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageAssign {
    library: String,
    tape: String,
    slot: i32,
    /// 磁带在 `__offline__` 离线货架上时设为 true（与批量入槽一致）。
    #[serde(default)]
    from_offline: bool,
}

const WEB_RECONCILE_APPLY_FORBIDDEN: &str =
    "Web 不支持 reconcile --apply（DB→内核机械手已移除）；请用 --pull 或 vtladm load/unload/assign-slot ioctl";

/// 单条入槽（UI 批量页优先用 `assign-slot-batch`；保留本 API 供脚本/兼容）。
async fn api_manage_assign(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageAssign>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let tape = body.tape.clone();
    let slot = body.slot;
    let from_offline = body.from_offline;
    let res = tokio::task::spawn_blocking(move || {
        super::assign_tapes_to_slots_batch(&lib, &[(tape, slot, from_offline)])
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageChangerLibrary {
    library: String,
}

#[derive(Deserialize)]
#[allow(dead_code)] // JSON fields for API symmetry; handlers use query/session library
struct ManageChangerLoad {
    library: String,
    slot: i32,
    drive: i32,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ManageChangerUnload {
    library: String,
    drive: i32,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ManageChangerEject {
    library: String,
    slot: i32,
}

async fn api_manage_changer_load(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageChangerLoad>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let slot = body.slot;
    let drive = body.drive;
    let res = tokio::task::spawn_blocking(move || {
        super::load_tape_in_library(&lib, slot, drive).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn api_manage_changer_unload(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageChangerUnload>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let drive = body.drive;
    let res = tokio::task::spawn_blocking(move || {
        super::unload_tape_in_library(&lib, drive)
            .map(|slot| json!({ "ok": true, "slot": slot }))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn api_manage_changer_eject(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageChangerEject>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let slot = body.slot;
    let res = tokio::task::spawn_blocking(move || {
        super::eject_tape_in_library(&lib, slot)
            .map(|mailslot| json!({ "ok": true, "mailslot": mailslot }))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageRobotReconcile {
    library: String,
    #[serde(default)]
    apply: bool,
    #[serde(default)]
    pull: bool,
}

async fn api_manage_robot_reconcile(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageRobotReconcile>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    if body.apply {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": WEB_RECONCILE_APPLY_FORBIDDEN })),
        )
            .into_response();
    }
    let lib = body.library.clone();
    let apply = body.apply;
    let pull = body.pull;
    let res = tokio::task::spawn_blocking(move || {
        let conn = super::init_db().map_err(|e| e.to_string())?;
        let library_id = super::resolve_library_id(&conn, &lib).map_err(|e| e.to_string())?;
        let report = super::reconcile::reconcile_library(library_id, apply, pull)
            .map_err(|e| e.to_string())?;
        let drifts: Vec<serde_json::Value> = report
            .drifts
            .iter()
            .map(|d| {
                json!({
                    "tape": d.tape,
                    "db": format!("{:?}", d.db),
                    "kernel": format!("{:?}", d.kernel),
                })
            })
            .collect();
        Ok::<_, String>(json!({
            "ok": true,
            "drift_count": report.drifts.len(),
            "fixes_applied": report.fixes_applied,
            "pull_updates": report.pull_updates,
            "inventory_truncated": report.inventory_truncated,
            "drifts": drifts,
        }))
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn api_manage_robot_auto_align(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageChangerLibrary>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let res = tokio::task::spawn_blocking(move || {
        let conn = super::init_db().map_err(|e| e.to_string())?;
        let library_id = super::resolve_library_id(&conn, &lib).map_err(|e| e.to_string())?;
        let report = super::reconcile::auto_align_library(library_id).map_err(|e| e.to_string())?;
        Ok::<_, String>(json!({
            "ok": true,
            "evacuated": report.evacuated,
            "fixes_applied": report.fixes_applied,
            "pull_updates": report.pull_updates,
            "drifts_remaining": report.drifts_remaining,
        }))
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn api_manage_robot_sync(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageChangerLibrary>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let res = tokio::task::spawn_blocking(move || {
        let n = super::sync_db_from_kernel_library(&lib).map_err(|e| e.to_string())?;
        Ok::<_, String>(json!({ "ok": true, "tapes_updated": n }))
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageShelfPlace {
    library: String,
    tape: String,
    shelf: Option<String>,
}

async fn api_manage_shelf_place(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageShelfPlace>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let tape = body.tape.clone();
    let shelf = body.shelf.clone();
    let res = tokio::task::spawn_blocking(move || {
        let _guard = LibraryGuard::new(&lib);
        super::shelf_place_tape(&tape, shelf.as_deref()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageShelfOfflineCreate {
    name: String,
}

async fn api_manage_shelf_create_offline(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageShelfOfflineCreate>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let name = body.name.clone();
    let res = tokio::task::spawn_blocking(move || {
        super::create_offline_shelf(&name).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageTapeShelfPlaceBatch {
    library: String,
    tapes: Vec<String>,
    shelf: String,
}

async fn api_manage_shelf_place_batch(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageTapeShelfPlaceBatch>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let tapes = body.tapes.clone();
    let shelf = body.shelf.clone();
    let res = tokio::task::spawn_blocking(move || {
        super::move_tapes_to_offline_shelf(&lib, &tapes, &shelf).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct AssignPair {
    tape: String,
    slot: i32,
    /// 为 true 时表示磁带当前在离线保管库 `__offline__` 的货架上（离架入在线库槽位）。
    #[serde(default)]
    from_offline: bool,
}

#[derive(Deserialize)]
struct ManageTapeAssignBatch {
    library: String,
    pairs: Vec<AssignPair>,
}

async fn api_manage_assign_batch(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageTapeAssignBatch>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let pairs: Vec<(String, i32, bool)> = body
        .pairs
        .iter()
        .map(|p| (p.tape.clone(), p.slot, p.from_offline))
        .collect();
    let res = tokio::task::spawn_blocking(move || {
        super::assign_tapes_to_slots_batch(&lib, &pairs).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct TapeBatchItem {
    name: String,
    size: String,
}

#[derive(Deserialize)]
struct ManageTapeCreateBatch {
    library: String,
    shelf: Option<String>,
    items: Vec<TapeBatchItem>,
}

async fn api_manage_tape_create_batch(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageTapeCreateBatch>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let shelf = body.shelf.clone();
    let items: Vec<(String, String)> = body.items.into_iter().map(|x| (x.name, x.size)).collect();
    let res = tokio::task::spawn_blocking(move || {
        super::create_tapes_batch(&lib, shelf.as_deref(), &items).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageTapeCreateAutoBatch {
    library: String,
    shelf: Option<String>,
    count: u32,
    size: String,
}

async fn api_manage_tape_create_auto_batch(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageTapeCreateAutoBatch>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let shelf = body.shelf.clone();
    let count = body.count as usize;
    let size_s = body.size.clone();
    let res = tokio::task::spawn_blocking(move || {
        let size = super::parse_size(&size_s).map_err(|e| e.to_string())?;
        super::create_auto_named_tapes_batch(&lib, shelf.as_deref(), count, size)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(names) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "names": names, "count": names.len() })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ManageTapeMigrateShelves {
    library: String,
    from_shelf: String,
    to_shelf: String,
    tapes: Vec<String>,
}

async fn api_manage_tape_migrate_shelves_batch(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<ManageTapeMigrateShelves>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = body.library.clone();
    let from = body.from_shelf.clone();
    let to = body.to_shelf.clone();
    let tapes = body.tapes.clone();
    let res = tokio::task::spawn_blocking(move || {
        super::migrate_tapes_between_shelves(&lib, &from, &to, &tapes).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    match res {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// IQN 写入 targetcli 脚本前校验：长度、前缀，并拒绝易注入 targetcli/ shell 的字符。
fn validate_iqn(s: &str) -> Result<(), String> {
    if s.len() < 10 || s.len() > 223 {
        return Err("IQN 长度无效".into());
    }
    if !s.starts_with("iqn.") {
        return Err("IQN 应以 iqn. 开头".into());
    }
    if s.contains('_') {
        return Err(
            "IQN 不得含下划线 _（新版 LIO/rtslib 会报 WWN not valid；冒号后请用连字符 -，或点 .）"
                .into(),
        );
    }
    if s.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                ';' | '|' | '&' | '`' | '$' | '<' | '>' | '\n' | '\r' | '\'' | '"' | '\\'
            )
    }) {
        return Err("IQN 含非法字符（禁止控制字符、引号、反斜杠及 shell 元字符）".into());
    }
    Ok(())
}

/// `targetcli` portals create 的第一个参数：仅 IPv4 或主机名（与配置解析一致，**不支持 IPv6 字面量**）。
fn validate_iscsi_portal_host(host: &str) -> Result<(), String> {
    let t = host.trim();
    if t.is_empty() {
        return Err("门户监听地址不能为空".into());
    }
    if t != host {
        return Err("门户监听地址首尾不得含空白".into());
    }
    if t.contains(':') {
        return Err(
            "门户监听地址不可含冒号（不支持 IPv6 字面量；请用 IPv4 或可解析主机名）".into(),
        );
    }
    if t.chars().any(|c| {
        c.is_whitespace()
            || c.is_control()
            || matches!(
                c,
                ';' | '|' | '&' | '`' | '$' | '<' | '>' | '\'' | '"' | '\\'
            )
    }) {
        return Err("门户监听地址含非法字符".into());
    }
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err("门户监听地址仅允许 ASCII 字母、数字、点、连字符".into());
    }
    Ok(())
}

const ISCSI_LUN_MAX: u32 = 255;

fn validate_lun_map_values(lun_map: &[u32]) -> Result<(), String> {
    for &n in lun_map {
        if n > ISCSI_LUN_MAX {
            return Err(format!("LUN {} 超过上限 {}", n, ISCSI_LUN_MAX));
        }
    }
    Ok(())
}

/// 与 `vtladm-iscsi library-export` 一致：`lun_map` 须为自 0 起的连续编号（targetcli `luns/` 批处理仅支持自动 LUN）。
fn validate_lun_map_consecutive_from_zero(luns: &[u32]) -> Result<(), String> {
    for (i, &n) in luns.iter().enumerate() {
        let want = i as u32;
        if n != want {
            return Err(format!(
                "lun_map 须为自 0 起的连续编号（第 {} 项应为 {}，实际为 {}）；非连续映射在 targetcli 批处理模式下不支持",
                i + 1,
                want,
                n
            ));
        }
    }
    Ok(())
}

/// 可选 `hint`：`library-export` / `library-unexport` 调用 `vtladm-iscsi` 后的统一说明。
///
/// `library-unexport` 在未导出过或已卸净时，`targetcli` 常向 stderr 打印 `No such path` / `No storage object named` 等
/// 而仍退出 0；此类不当作「导出异常」提示（仅对 **`library-export` 且 `ok`**）给出 stderr 警示 hint。
fn iscsi_library_exec_hint(
    dry_run: bool,
    ok: bool,
    stderr: &str,
    is_export: bool,
) -> Option<serde_json::Value> {
    let verb = if is_export { "导出" } else { "卸载" };
    if dry_run {
        Some(json!(format!(
            "dry_run 为 true：仅由 vtladm-iscsi 打印 targetcli 脚本，未调用 targetcli，LIO 不会写入 /etc/target/saveconfig.json；取消勾选「仅 dry-run」才会真正{}。",
            verb
        )))
    } else if ok
        && is_export
        && (stderr.contains("Cannot configure")
            || stderr.contains("Unknown configuration")
            || stderr.contains("No such path")
            || stderr.contains("No storage object named"))
    {
        Some(json!(
            "进程退出码为 0 但 stderr 含错误：请阅读 stderr。常见原因：/dev/sg 已被其它 LIO pscsi 占用需先 library-unexport；IQN 含冒号时勿用错误的 `VTL_ISCSI_SHELL_PATH=merged`（若 `targetcli` 下 IQN 下仍有 `tpg1` 请取消 merged、保持默认 tpg1）；或 targetcli 报错行需对照版本。"
        ))
    } else if !ok {
        let low = stderr.to_ascii_lowercase();
        let msg = if low.contains("already in use") {
            "vtladm-iscsi 非零退出：stderr 含 already in use：对应 /dev/sg 已被 LIO 其它 pscsi 对象占用，或仍有进程打开。请先 library-unexport（与上次相同的 export_id、iqn、drives/lun_map），或在 targetcli 中删除仍引用这些 sg 的 backstore；关闭备份、sg 类工具或其它 target；可用 lsof、fuser -v 核对。然后再试导出。"
        } else if low.contains("wwn not valid") || low.contains("not valid as:") {
            "vtladm-iscsi 非零退出：stderr 含 WWN/IQN 校验失败。常见原因：IQN 冒号后含下划线 _（新版 targetcli 不接受）；请改用连字符 - 或重新「加载默认」IQN；并确认 iqn 符合 iqn.年-月.反向域名:唯一标识。"
        } else {
            "vtladm-iscsi 非零退出：请阅读 stdout/stderr；请核对 iqn、export_id、与导出时一致的 drives 或 lun_map，以及 targetcli 是否可用。"
        };
        Some(json!(msg))
    } else {
        None
    }
}

fn validate_fileio_name(s: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > 64 {
        return Err("fileio_name 长度无效".into());
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("fileio_name 仅允许字母数字与下划线".into());
    }
    Ok(())
}

/// 与 `vtladm-iscsi library-export --id` 相同规则（LIO pscsi 后端名前缀，可理解为自动「fileio 名」式前缀）。
fn validate_export_id_for_iscsi(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("export_id 不能为空".into());
    }
    if name.len() > 48 {
        return Err("export_id 过长（≤48）".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("export_id 仅允许 ASCII 字母、数字与下划线".into());
    }
    Ok(())
}

/// 默认 IQN：`iqn.YYYY-MM.com.marstor:<库名ASCII>-<时间戳>`（冒号后**不用** `_`，避免 LIO 报 WWN not valid）
fn iscsi_default_iqn_and_export_id(library: &str) -> (String, String) {
    let now = Utc::now();
    let ts = now.format("%Y%m%d%H%M%S").to_string();
    let y = now.year();
    let m = now.month();
    let lib_slug: String = library
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(40)
        .collect();
    let slug = if lib_slug.is_empty() {
        "lib".to_string()
    } else {
        lib_slug
    };
    let iqn = format!("iqn.{:04}-{:02}.com.marstor:{}-{}", y, m, slug, ts);
    let mut export_id = String::from("m");
    for c in library.chars() {
        if c.is_ascii_alphanumeric() {
            export_id.push(c);
        } else if c == '_' || c == '-' {
            export_id.push('_');
        }
        if export_id.len() >= 26 {
            break;
        }
    }
    if export_id == "m" {
        export_id.push_str("lib");
    }
    export_id.push('_');
    export_id.push_str(&ts);
    let export_id: String = export_id.chars().take(48).collect();
    (iqn, export_id)
}

#[cfg(unix)]
fn scsi_host_from_lsscsi_line(line: &str) -> Option<u32> {
    let t = line.trim_start();
    if !t.starts_with('[') {
        return None;
    }
    let end = t.find(']')?;
    t[1..end].split(':').next()?.parse().ok()
}

#[cfg(unix)]
fn scsi_lun_from_lsscsi_line(line: &str) -> Option<u32> {
    let t = line.trim_start();
    if !t.starts_with('[') {
        return None;
    }
    let end = t.find(']')?;
    let parts: Vec<&str> = t[1..end].split(':').collect();
    parts.get(3)?.parse().ok()
}

#[cfg(unix)]
fn lsscsi_line_is_vtl(line: &str) -> bool {
    line.contains("VTL")
}

#[cfg(unix)]
fn lsscsi_line_is_changer(line: &str) -> bool {
    lsscsi_line_is_vtl(line) && line.split_whitespace().nth(1) == Some("mediumx")
}

#[cfg(unix)]
fn lsscsi_line_is_tape_drive(line: &str) -> bool {
    lsscsi_line_is_vtl(line) && line.split_whitespace().nth(1) == Some("tape")
}

#[cfg(unix)]
#[derive(Default)]
struct VtlHostSg {
    changer: Option<(u32, String)>,
    drives: Vec<(u32, String)>,
}

/// 按 SCSI host 分组；避免把多个内核 VTL host 的 `/dev/sg` 混成一组（会导致导出路径错误甚至内核风险）。
#[cfg(unix)]
fn parse_lsscsi_vtl_grouped(stdout: &str) -> BTreeMap<u32, VtlHostSg> {
    let mut map: BTreeMap<u32, VtlHostSg> = BTreeMap::new();
    for line in stdout.lines() {
        if !lsscsi_line_is_vtl(line) {
            continue;
        }
        let Some(host) = scsi_host_from_lsscsi_line(line) else {
            continue;
        };
        let sgs: Vec<&str> = line
            .split_whitespace()
            .filter(|t| t.starts_with("/dev/sg"))
            .collect();
        let Some(sg) = sgs.last().copied() else {
            continue;
        };
        let lun = scsi_lun_from_lsscsi_line(line).unwrap_or(999);
        let g = map.entry(host).or_default();
        if lsscsi_line_is_changer(line) {
            match &g.changer {
                None => g.changer = Some((lun, sg.to_string())),
                Some((ol, _)) if lun < *ol => g.changer = Some((lun, sg.to_string())),
                _ => {}
            }
        } else if lsscsi_line_is_tape_drive(line) {
            g.drives.push((lun, sg.to_string()));
        }
    }
    for g in map.values_mut() {
        g.drives.sort_by_key(|(l, _)| *l);
        g.drives.dedup_by(|a, b| a.1 == b.1);
    }
    map
}

/// `expected_drives`: `None` 时取最小 host 号上「有机械手且至少一台磁带机」的一组；`Some(n)` 时要求该 host 上磁带机行数恰好为 n。
#[cfg(unix)]
fn pick_vtl_host_for_scan(
    groups: &BTreeMap<u32, VtlHostSg>,
    expected_drives: Option<i64>,
    prefer_scsi_host: Option<u32>,
) -> Result<(u32, String, Vec<String>), String> {
    if groups.is_empty() {
        return Err("lsscsi 输出中未发现 VTL SCSI 设备（请确认已加载 vtl.ko 且已 scan）".into());
    }
    match expected_drives {
        None => {
            for (&h, g) in groups.iter() {
                if let Some((_, ref cs)) = g.changer {
                    if !g.drives.is_empty() {
                        let dr: Vec<String> = g.drives.iter().map(|(_, s)| s.clone()).collect();
                        return Ok((h, cs.clone(), dr));
                    }
                }
            }
            Err("未发现同时含机械手(LUN0 mediumx)与磁带机(LUN≥1)的 vtl SCSI host".into())
        }
        Some(0) => Err(
            "当前库在数据库中驱动器数为 0，无法匹配磁带机 /dev/sg（请先「加载默认」确认库几何或增加驱动器）"
                .into(),
        ),
        Some(n) => {
            let n = n as usize;
            // 方案 B 等场景下内核可能 scan 出多于 DB 配置的磁带 LUN；仅取 LUN 序前 n 台与库一致。
            let mut hits: Vec<(u32, &VtlHostSg)> = groups
                .iter()
                .filter(|(_, g)| g.changer.is_some() && g.drives.len() >= n)
                .map(|(&h, g)| (h, g))
                .collect();
            if hits.is_empty() {
                let summary: Vec<String> = groups
                    .iter()
                    .map(|(h, g)| {
                        format!(
                            "host {}: {} tape LUN(s){}",
                            h,
                            g.drives.len(),
                            g.changer
                                .as_ref()
                                .map(|_| ", changer OK")
                                .unwrap_or(", no changer")
                        )
                    })
                    .collect();
                return Err(format!(
                    "未找到至少 {} 台磁带机的 VTL SCSI host（与当前库 drives 表一致）。各 host：{}。{}",
                    n,
                    summary.join("; "),
                    scan_vtl_remediation_hint(n as i64)
                ));
            }
            if let Some(want) = prefer_scsi_host {
                hits.retain(|(h, _)| *h == want);
                if hits.is_empty() {
                    let hosts: Vec<u32> = groups
                        .iter()
                        .filter(|(_, g)| g.changer.is_some() && g.drives.len() >= n)
                        .map(|(&h, _)| h)
                        .collect();
                    return Err(format!(
                        "prefer_scsi_host={} 与至少 {} 台磁带机的候选不匹配。候选 host：{:?}",
                        want, n, hosts
                    ));
                }
            } else if hits.len() > 1 {
                let hosts: Vec<u32> = hits.iter().map(|(h, _)| *h).collect();
                return Err(format!(
                    "多个 SCSI host 均至少有 {} 台磁带机，无法唯一定位。请在 scan-sg 上增加 prefer_scsi_host=其一（候选 {:?}）。",
                    n, hosts
                ));
            }
            hits.sort_by_key(|(h, _)| *h);
            let (h, g) = hits[0];
            let ch = g.changer.as_ref().unwrap().1.clone();
            let dr: Vec<String> = g
                .drives
                .iter()
                .take(n)
                .map(|(_, s)| s.clone())
                .collect();
            Ok((h, ch, dr))
        }
    }
}

/// 从 `lsscsi -g` 行中查找与 `sg` 同行的 `/dev/st*`（本机 SCSI 备份常用）。
fn st_path_for_sg_in_lsscsi(stdout: &str, sg: &str) -> Option<String> {
    for line in stdout.lines() {
        if line.contains(sg) && lsscsi_line_is_vtl(line) {
            return line
                .split_whitespace()
                .find(|t| t.starts_with("/dev/st"))
                .map(str::to_string);
        }
    }
    None
}

fn sch_path_for_sg_in_lsscsi(stdout: &str, sg: &str) -> Option<String> {
    for line in stdout.lines() {
        if line.contains(sg) && lsscsi_line_is_vtl(line) {
            return line
                .split_whitespace()
                .find(|t| t.starts_with("/dev/sch"))
                .map(str::to_string);
        }
    }
    None
}

fn transport_scan_hint(transport: Option<&str>) -> &'static str {
    match transport
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("fc") => {
            "FC：在 SAN 上呈现与下表相同的 SCSI 磁带/机械手语义；本页仅列出本机应对应该库的 /dev 节点（须由 FC target 映射）。"
        }
        Some("iscsi") => {
            "iSCSI：下表为本库应对的 /dev/sg（及 /dev/st）；经 LIO pscsi 导出见「iSCSI / LUN 映射」页。"
        }
        Some("local" | "scsi") => {
            "本机 SCSI：备份软件与本机同台时使用下表 /dev/st*、/dev/sg*；内核可见的额外磁带 LUN 已按库驱动器数隐藏。"
        }
        _ => {
            "按库 drives 表台数选取 VTL 节点；内核可见 LUN 可能更多，仅显示前 N 台磁带机。"
        }
    }
}

fn build_transport_scan_response(
    lib_name: &str,
    stdout: &str,
    picked_host: u32,
    changer_sg: String,
    drive_sg: Vec<String>,
    transport: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let n = drive_sg.len();
    let changer_sch = sch_path_for_sg_in_lsscsi(stdout, &changer_sg);
    let mut devices: Vec<serde_json::Value> = Vec::with_capacity(1 + n);
    devices.push(json!({
        "role": "changer",
        "lun": 0,
        "sg": changer_sg,
        "sch": changer_sch,
    }));
    for (i, sg) in drive_sg.iter().enumerate() {
        devices.push(json!({
            "role": "drive",
            "index": i,
            "lun": i + 1,
            "sg": sg,
            "st": st_path_for_sg_in_lsscsi(stdout, sg),
        }));
    }
    let mut m = serde_json::Map::new();
    m.insert("library".into(), json!(lib_name));
    m.insert("picked_scsi_host".into(), json!(picked_host));
    m.insert("changer_sg".into(), json!(changer_sg));
    m.insert("drive_sg".into(), json!(drive_sg));
    m.insert("devices".into(), json!(devices));
    m.insert("drive_count".into(), json!(n));
    m.insert("note".into(), json!(transport_scan_hint(transport)));
    m.insert("product_limits".into(), product_limits_json());
    m.insert(
        "raw_tail".into(),
        json!(stdout
            .lines()
            .filter(|l| lsscsi_line_is_vtl(l))
            .take(40)
            .collect::<Vec<_>>()
            .join("\n")),
    );
    m
}

#[cfg(unix)]
fn scan_vtl_remediation_hint(expected_drives: i64) -> &'static str {
    let _ = expected_drives;
    "建议：① /opt/vtladm/bin/vtladm kernel-align  ② sh /opt/vtladm/scripts/vtl-scsi-scan-all-hosts.sh 5  ③ 再点「扫描 lsscsi」；核对 cat /sys/module/vtl/parameters/vtl_instances 与库 drives 数"
}

#[cfg(unix)]
fn lsscsi_g_stdout() -> Result<String, String> {
    let out = Command::new("lsscsi")
        .args(["-g"])
        .output()
        .map_err(|e| format!("无法执行 lsscsi: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "lsscsi 退出 {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(unix)]
fn infer_prefer_scsi_host(
    groups: &BTreeMap<u32, VtlHostSg>,
    library_host_ordinal: Option<usize>,
    prefer_host: Option<u32>,
) -> Option<u32> {
    if prefer_host.is_some() {
        return prefer_host;
    }
    let mut vtl_hosts: Vec<u32> = groups.keys().copied().collect();
    vtl_hosts.sort();
    library_host_ordinal.and_then(|idx| vtl_hosts.get(idx).copied())
}

#[cfg(unix)]
fn rescan_vtl_for_library_scan(stdout: &str, library_host_ordinal: Option<usize>) {
    let groups = parse_lsscsi_vtl_grouped(stdout);
    let mut vtl_hosts: Vec<u32> = groups.keys().copied().collect();
    vtl_hosts.sort();
    if let Some(idx) = library_host_ordinal {
        if let Some(&h) = vtl_hosts.get(idx) {
            let _ = super::scsi_rescan_vtl::scsi_rescan_scsi_host(h);
            return;
        }
    }
    let _ = super::scsi_rescan_vtl::scsi_rescan_vtl_hosts();
}

#[cfg(unix)]
fn pick_vtl_scan_from_stdout(
    stdout: &str,
    drive_count: i64,
    prefer_host: Option<u32>,
    library_host_ordinal: Option<usize>,
) -> Result<(u32, String, Vec<String>), String> {
    let groups = parse_lsscsi_vtl_grouped(stdout);
    let use_prefer = infer_prefer_scsi_host(&groups, library_host_ordinal, prefer_host);
    pick_vtl_host_for_scan(&groups, Some(drive_count), use_prefer)
}

fn scan_vtl_library_blocking(
    lib_for_task: &str,
    prefer_host: Option<u32>,
) -> Result<(String, u32, String, Vec<String>), String> {
    let conn = super::init_db().map_err(|e| e.to_string())?;
    if let Err(e) = super::build_vtl_instances_kernel_spec() {
        return Err(format!(
            "无已导出在线库：{}；请先 library create 并 vtl-kernelctl reload",
            e
        ));
    }
    let library_id = match super::resolve_library_id(&conn, lib_for_task) {
        Ok(id) => id,
        Err(super::VtlError::LibraryNotFound(_)) => return Err(format!("LIB404:{}", lib_for_task)),
        Err(e) => return Err(e.to_string()),
    };
    let drive_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM drives WHERE library_id = ?1",
            rusqlite::params![library_id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    let library_ordinal = super::online_library_export_index(&conn, library_id)
        .ok()
        .map(|i| {
            if i >= super::VTL_KERNEL_MAX_ONLINE_LIBRARIES {
                super::VTL_KERNEL_MAX_ONLINE_LIBRARIES - 1
            } else {
                i
            }
        });

    #[cfg(unix)]
    {
        let mut stdout = lsscsi_g_stdout()?;
        match pick_vtl_scan_from_stdout(&stdout, drive_count, prefer_host, library_ordinal) {
            Ok((host, ch, dr)) => return Ok((stdout, host, ch, dr)),
            Err(e) if e.contains("未找到至少") && drive_count > 0 => {
                rescan_vtl_for_library_scan(&stdout, library_ordinal);
                stdout = lsscsi_g_stdout()?;
                pick_vtl_scan_from_stdout(&stdout, drive_count, prefer_host, library_ordinal)
                    .map(|(host, ch, dr)| (stdout, host, ch, dr))
                    .map_err(|e2| {
                        if e2.contains("未找到至少") {
                            format!(
                                "{}\n（已自动对该 host 写 SCSI scan 仍不足；若 drives 刚增加，请先 kernel-align）",
                                e2
                            )
                        } else {
                            e2
                        }
                    })
            }
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (lib_for_task, prefer_host, library_ordinal, drive_count);
        Err("scan-sg 仅支持 Linux".into())
    }
}

#[cfg(all(test, unix))]
mod lsscsi_vtl_pick_tests {
    use super::{parse_lsscsi_vtl_grouped, pick_vtl_host_for_scan};

    const SAMPLE: &str = r#"[33:0:0:0]   mediumx    VTL      VTL CHANGER  1.00  /dev/sch0  /dev/sg2
[33:0:0:1]   tape       VTL      VTL TAPE DRV 1.00  /dev/st0    /dev/sg3
[34:0:0:0]   mediumx    VTL      VTL CHANGER  1.00  /dev/sch1  /dev/sg4
[34:0:0:1]   tape       VTL      VTL TAPE DRV 1.00  /dev/st1    /dev/sg5
[34:0:0:2]   tape       VTL      VTL TAPE DRV 1.00  /dev/st2    /dev/sg6
"#;

    #[test]
    fn pick_matches_drive_count_two() {
        let g = parse_lsscsi_vtl_grouped(SAMPLE);
        let (h, ch, dr) = pick_vtl_host_for_scan(&g, Some(2), None).unwrap();
        assert_eq!(h, 34);
        assert_eq!(ch, "/dev/sg4");
        assert_eq!(dr, vec!["/dev/sg5".to_string(), "/dev/sg6".to_string()]);
    }

    #[test]
    fn pick_matches_drive_count_one() {
        let g = parse_lsscsi_vtl_grouped(SAMPLE);
        let (h, ch, dr) = pick_vtl_host_for_scan(&g, Some(1), Some(33)).unwrap();
        assert_eq!(h, 33);
        assert_eq!(ch, "/dev/sg2");
        assert_eq!(dr, vec!["/dev/sg3".to_string()]);
    }

    #[test]
    fn st_path_lookup_from_lsscsi_line() {
        const OUT: &str =
            "[33:0:0:1]   tape       VTL      VTL TAPE DRV 1.00  /dev/st0    /dev/sg3\n";
        assert_eq!(
            super::st_path_for_sg_in_lsscsi(OUT, "/dev/sg3").as_deref(),
            Some("/dev/st0")
        );
    }

    #[test]
    fn pick_truncates_when_kernel_has_more_tape_luns_than_db() {
        const MANY: &str = r#"[40:0:0:0]   mediumx    VTL      VTL CHANGER  1.00  /dev/sch0  /dev/sg2
[40:0:0:1]   tape       VTL      VTL TAPE DRV 1.00  /dev/st0    /dev/sg3
[40:0:0:2]   tape       VTL      VTL TAPE DRV 1.00  /dev/st1    /dev/sg4
[40:0:0:3]   tape       VTL      VTL TAPE DRV 1.00  /dev/st2    /dev/sg5
[40:0:0:4]   tape       VTL      VTL TAPE DRV 1.00  /dev/st3    /dev/sg6
"#;
        let g = parse_lsscsi_vtl_grouped(MANY);
        let (_, ch, dr) = pick_vtl_host_for_scan(&g, Some(2), None).unwrap();
        assert_eq!(ch, "/dev/sg2");
        assert_eq!(dr, vec!["/dev/sg3".to_string(), "/dev/sg4".to_string()]);
    }

    #[test]
    fn pick_ambiguous_two_hosts_requires_prefer() {
        const AMB: &str = r#"[33:0:0:0]   mediumx    VTL      VTL CHANGER  1.00  /dev/sch0  /dev/sg2
[33:0:0:1]   tape       VTL      VTL TAPE DRV 1.00  /dev/st0    /dev/sg3
[33:0:0:2]   tape       VTL      VTL TAPE DRV 1.00  /dev/st0b   /dev/sg10
[34:0:0:0]   mediumx    VTL      VTL CHANGER  1.00  /dev/sch1  /dev/sg4
[34:0:0:1]   tape       VTL      VTL TAPE DRV 1.00  /dev/st1    /dev/sg5
[34:0:0:2]   tape       VTL      VTL TAPE DRV 1.00  /dev/st2    /dev/sg6
"#;
        let g = parse_lsscsi_vtl_grouped(AMB);
        let e = pick_vtl_host_for_scan(&g, Some(2), None).unwrap_err();
        assert!(e.contains("prefer_scsi_host"), "{}", e);
        let (h, ch, dr) = pick_vtl_host_for_scan(&g, Some(2), Some(34)).unwrap();
        assert_eq!(h, 34);
        assert_eq!(ch, "/dev/sg4");
        assert_eq!(dr, vec!["/dev/sg5".to_string(), "/dev/sg6".to_string()]);
        assert!(pick_vtl_host_for_scan(&g, Some(2), Some(99)).is_err());
    }
}

fn ensure_tape_file_under_tape_dir(path_str: &str) -> Result<PathBuf, String> {
    let p = Path::new(path_str);
    if !p.is_absolute() {
        return Err("file 须为绝对路径".into());
    }
    let meta = std::fs::metadata(p).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("file 不是普通文件".into());
    }
    let canon_file = std::fs::canonicalize(p).map_err(|e| e.to_string())?;
    let root = super::get_config().tape_dir;
    let canon_root =
        std::fs::canonicalize(&root).map_err(|e| format!("无法 canonicalize tape_dir: {}", e))?;
    if !canon_file.starts_with(&canon_root) {
        return Err("file 必须在配置的 tape_dir 之下".into());
    }
    Ok(canon_file)
}

fn vtladm_iscsi_program() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|ex| ex.parent().map(|d| d.join("vtladm-iscsi")))
        .unwrap_or_else(|| PathBuf::from("vtladm-iscsi"))
}

/// 从 `iscsi_portals` 取第一个 `host:port`（逗号分隔）；不解析 IPv6 字面量。
pub(crate) fn parse_first_iscsi_portal(portals: &str) -> Option<(String, u16)> {
    let first = portals.split(',').next()?.trim();
    if first.is_empty() {
        return None;
    }
    let (host, port_s) = first.rsplit_once(':')?;
    let port = port_s.parse().ok()?;
    Some((host.to_string(), port))
}

async fn api_manage_iscsi_config(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
) -> Response {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let c = super::get_config();
    let (portal_ip, portal_port) = c
        .iscsi_portals
        .as_deref()
        .and_then(parse_first_iscsi_portal)
        .unwrap_or_else(|| ("0.0.0.0".to_string(), 3260u16));
    Json(json!({
        "tape_dir": c.tape_dir.to_string_lossy(),
        "transport": c.transport.as_conf_str(),
        "iscsi_iqn": c.iscsi_iqn,
        "iscsi_portals": c.iscsi_portals,
        "portal_ip_suggested": portal_ip,
        "portal_port_suggested": portal_port,
        "vtladm_iscsi_path": vtladm_iscsi_program().to_string_lossy(),
        "allow_iscsi_exec": st.allow_iscsi_exec(),
        "non_unix_build": false,
        "kernel_reload_on_db_change": c.kernel_reload_on_db_change,
        "kernel_geom_prefer_ioctl": c.kernel_geom_prefer_ioctl,
    }))
    .into_response()
}

async fn api_manage_iscsi_check(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let sudo = body.get("sudo").and_then(|v| v.as_bool()).unwrap_or(false);
    let prog = vtladm_iscsi_program();
    let res = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(&prog);
        if sudo {
            cmd.arg("--sudo");
        }
        cmd.arg("check");
        cmd.output()
    })
    .await;

    match res {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let ok = out.status.success();
            let status = if ok {
                StatusCode::OK
            } else {
                StatusCode::BAD_GATEWAY
            };
            (
                status,
                Json(json!({
                    "ok": ok,
                    "stdout": stdout,
                    "stderr": stderr,
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct IscsiAllowExecBody {
    allow: bool,
}

async fn api_manage_iscsi_allow_exec(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<IscsiAllowExecBody>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let allow = body.allow;
    match st.set_allow_iscsi_exec(allow) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "allow_iscsi_exec": allow })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct IscsiQuickExportBody {
    file: String,
    iqn: String,
    #[serde(default = "default_fileio")]
    fileio_name: String,
    #[serde(default = "default_portal_ip")]
    portal_ip: String,
    #[serde(default = "default_portal_port")]
    portal_port: u16,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    sudo: bool,
}

fn default_fileio() -> String {
    "vtl_fileio0".into()
}
fn default_portal_ip() -> String {
    "0.0.0.0".into()
}
fn default_portal_port() -> u16 {
    3260
}

async fn api_manage_iscsi_quick_export(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<IscsiQuickExportBody>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    if let Err(e) = validate_iqn(&body.iqn) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    if let Err(e) = validate_fileio_name(&body.fileio_name) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    if let Err(e) = validate_iscsi_portal_host(&body.portal_ip) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    if body.portal_port == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "门户端口须在 1–65535" })),
        )
            .into_response();
    }
    let canon = match ensure_tape_file_under_tape_dir(&body.file) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    };
    if !body.dry_run && !st.allow_iscsi_exec() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "未允许执行 vtladm-iscsi：请在本页开启「允许执行 vtladm-iscsi」，或在 web_admin.json 中设置 allow_iscsi_exec: true，或仅使用 dry_run"
            })),
        )
            .into_response();
    }

    let file_str = canon.to_string_lossy().to_string();
    let iqn = body.iqn.clone();
    let fileio = body.fileio_name.clone();
    let portal_ip = body.portal_ip.clone();
    let portal_port = body.portal_port;
    let dry_run = body.dry_run;
    let sudo = body.sudo;
    let prog = vtladm_iscsi_program();

    let res = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(&prog);
        if sudo {
            cmd.arg("--sudo");
        }
        if dry_run {
            cmd.arg("--dry-run");
        }
        cmd.arg("quick-export");
        cmd.arg("--file").arg(&file_str);
        cmd.arg("--iqn").arg(&iqn);
        cmd.arg("--fileio-name").arg(&fileio);
        cmd.arg("--portal-ip").arg(&portal_ip);
        cmd.arg("--portal-port").arg(portal_port.to_string());
        cmd.output().map_err(|e| e.to_string())
    })
    .await;

    match res {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let ok = out.status.success();
            let status = if ok {
                StatusCode::OK
            } else {
                StatusCode::BAD_GATEWAY
            };
            (
                status,
                Json(json!({
                    "ok": ok,
                    "stdout": stdout,
                    "stderr": stderr,
                    "dry_run": dry_run,
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("后台任务失败: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct IscsiQuickUnexportBody {
    iqn: String,
    #[serde(default = "default_fileio")]
    fileio_name: String,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    sudo: bool,
}

async fn api_manage_iscsi_quick_unexport(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<IscsiQuickUnexportBody>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    if let Err(e) = validate_iqn(&body.iqn) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    if let Err(e) = validate_fileio_name(&body.fileio_name) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    if !body.dry_run && !st.allow_iscsi_exec() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "未允许执行 vtladm-iscsi：请在本页开启「允许执行」或设置 allow_iscsi_exec" })),
        )
            .into_response();
    }
    let iqn = body.iqn.clone();
    let fileio = body.fileio_name.clone();
    let dry_run = body.dry_run;
    let sudo = body.sudo;
    let prog = vtladm_iscsi_program();

    let res = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(&prog);
        if sudo {
            cmd.arg("--sudo");
        }
        if dry_run {
            cmd.arg("--dry-run");
        }
        cmd.arg("quick-unexport");
        cmd.arg("--iqn").arg(&iqn);
        cmd.arg("--fileio-name").arg(&fileio);
        cmd.output().map_err(|e| e.to_string())
    })
    .await;

    match res {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let ok = out.status.success();
            let status = if ok {
                StatusCode::OK
            } else {
                StatusCode::BAD_GATEWAY
            };
            (
                status,
                Json(json!({
                    "ok": ok,
                    "stdout": stdout,
                    "stderr": stderr,
                    "dry_run": dry_run,
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct IscsiLibraryExportBody {
    library: String,
    #[serde(default)]
    iqn: Option<String>,
    #[serde(default)]
    export_id: Option<String>,
    changer_sg: String,
    drive_sg: Vec<String>,
    #[serde(default)]
    lun_map: Option<Vec<u32>>,
    #[serde(default = "default_portal_ip")]
    portal_ip: String,
    #[serde(default = "default_portal_port")]
    portal_port: u16,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    sudo: bool,
}

#[derive(Deserialize)]
struct IscsiLibraryUnexportBody {
    /// 按库名从数据库读取上次成功导出的 IQN / export_id / lun_map（推荐，一键卸除）。
    #[serde(default)]
    library: Option<String>,
    #[serde(default)]
    iqn: Option<String>,
    #[serde(default)]
    export_id: Option<String>,
    #[serde(default)]
    drives: Option<u32>,
    #[serde(default)]
    lun_map: Option<Vec<u32>>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    sudo: bool,
}

/// 解析 unexport 参数：优先 `library` + DB 记录；否则须显式 `iqn` + `export_id`。
fn resolve_iscsi_unexport_params(
    body: &IscsiLibraryUnexportBody,
) -> Result<
    (
        String,
        String,
        Option<Vec<u32>>,
        Option<u32>,
        Option<String>,
    ),
    String,
> {
    if let Some(ref lib) = body
        .library
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        if *lib == super::OFFLINE_LIBRARY_NAME {
            return Err("离线库不可用于 iSCSI 卸除".into());
        }
        if *lib == super::LEGACY_DEFAULT_LIBRARY_NAME {
            return Err("测试库 default 无 iSCSI 导出记录".into());
        }
        let conn = super::init_db().map_err(|e| e.to_string())?;
        let rec = super::iscsi_export::load_iscsi_library_export_by_name(&conn, lib)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("库「{}」无已保存的 iSCSI 导出记录；请先执行 library-export 或手工填写 IQN/前缀", lib))?;
        let iqn = body
            .iqn
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or(rec.iqn);
        let export_id = body
            .export_id
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or(rec.export_id);
        let drive_sg_count = rec.drive_sg.len();
        let lun_map = body.lun_map.clone().or(Some(rec.lun_map));
        let drives = body.drives.or_else(|| {
            if drive_sg_count > 0 {
                Some(drive_sg_count as u32)
            } else {
                None
            }
        });
        return Ok((iqn, export_id, lun_map, drives, Some(lib.to_string())));
    }
    let iqn = body
        .iqn
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "须指定 library（推荐）或填写 iqn".to_string())?;
    let export_id = body
        .export_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "须指定 library（推荐）或填写 export_id".to_string())?;
    if body.lun_map.is_none() && body.drives.is_none() {
        return Err("未指定 library 时须填写 lun_map 或 drives".into());
    }
    Ok((iqn, export_id, body.lun_map.clone(), body.drives, None))
}

async fn api_manage_iscsi_library_export_defaults(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Query(q): Query<IscsiExportDefaultsQuery>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib = match super::resolve_active_library_name(q.library.as_deref()) {
        Ok(l) => l,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    if lib == super::OFFLINE_LIBRARY_NAME {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "离线库不可用于 iSCSI 导出" })),
        )
            .into_response();
    }
    if lib == super::LEGACY_DEFAULT_LIBRARY_NAME {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "测试库 default 不导出 SCSI" })),
        )
            .into_response();
    }
    let lib_c = lib.clone();
    let res = tokio::task::spawn_blocking(
        move || -> Result<(i64, Option<super::iscsi_export::IscsiLibraryExportRecord>), String> {
            let conn = super::init_db().map_err(|e| e.to_string())?;
            let library_id = match super::resolve_library_id(&conn, &lib_c) {
                Ok(id) => id,
                Err(super::VtlError::LibraryNotFound(n)) => return Err(format!("LIB404:{}", n)),
                Err(e) => return Err(e.to_string()),
            };
            let drive_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM drives WHERE library_id = ?1",
                    rusqlite::params![library_id],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            let saved = super::iscsi_export::load_iscsi_library_export(&conn, library_id)
                .map_err(|e| e.to_string())?;
            Ok((drive_count, saved))
        },
    )
    .await;

    let (drive_count, saved) = match res {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) if e.starts_with("LIB404:") => {
            let name = e.trim_start_matches("LIB404:");
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("未找到库: {}", name) })),
            )
                .into_response();
        }
        Ok(Err(e)) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("后台任务失败: {}", e) })),
            )
                .into_response();
        }
    };

    let c = super::get_config();
    let (conf_portal_ip, conf_portal_port) = c
        .iscsi_portals
        .as_deref()
        .and_then(parse_first_iscsi_portal)
        .unwrap_or_else(|| ("0.0.0.0".to_string(), 3260u16));

    let (
        iqn,
        export_id,
        portal_ip,
        portal_port,
        default_lun_map,
        changer_sg,
        drive_sg,
        has_saved_export,
        exported_at,
        saved_drive_mismatch,
    ) = if !q.regenerate {
        if let Some(ref rec) = saved {
            let saved_sg: Vec<String> = rec
                .drive_sg
                .iter()
                .take(drive_count as usize)
                .cloned()
                .collect();
            let mismatch = rec.drive_sg.len() as i64 != drive_count;
            let (iqn, export_id) = if rec.iqn.trim().is_empty() || rec.export_id.trim().is_empty() {
                let (gen_iqn, gen_id) = iscsi_default_iqn_and_export_id(&lib);
                (
                    if rec.iqn.trim().is_empty() {
                        gen_iqn
                    } else {
                        rec.iqn.clone()
                    },
                    if rec.export_id.trim().is_empty() {
                        gen_id
                    } else {
                        rec.export_id.clone()
                    },
                )
            } else {
                (rec.iqn.clone(), rec.export_id.clone())
            };
            (
                iqn,
                export_id,
                rec.portal_ip.clone(),
                rec.portal_port,
                rec.lun_map.clone(),
                Some(rec.changer_sg.clone()),
                Some(saved_sg),
                true,
                Some(rec.exported_at.clone()),
                mismatch,
            )
        } else {
            let (iqn, export_id) = iscsi_default_iqn_and_export_id(&lib);
            let mut default_lun_map: Vec<u32> = Vec::new();
            for i in 0..=drive_count as u32 {
                default_lun_map.push(i);
            }
            (
                iqn,
                export_id,
                conf_portal_ip.clone(),
                conf_portal_port,
                default_lun_map,
                None,
                None,
                false,
                None,
                false,
            )
        }
    } else {
        let (iqn, export_id) = iscsi_default_iqn_and_export_id(&lib);
        let mut default_lun_map: Vec<u32> = Vec::new();
        for i in 0..=drive_count as u32 {
            default_lun_map.push(i);
        }
        (
            iqn,
            export_id,
            conf_portal_ip,
            conf_portal_port,
            default_lun_map,
            None,
            None,
            false,
            None,
            false,
        )
    };

    let backend_ch = format!("{}_ch", export_id);
    let backend_drives: Vec<String> = (0..drive_count)
        .map(|i| format!("{}_dr{}", export_id, i))
        .collect();

    let can_export = drive_count > 0;
    let export_blocked_reason: serde_json::Value = if can_export {
        serde_json::Value::Null
    } else {
        json!(
            "当前库驱动器数为 0，无法执行 library-export（内核 vtl 须至少暴露一台磁带机 /dev/sg）"
        )
    };

    (
        StatusCode::OK,
        Json(json!({
            "library": lib,
            "iqn": iqn,
            "export_id": export_id,
            "backend_ch": backend_ch,
            "backend_drives": backend_drives,
            "portal_ip": portal_ip,
            "portal_port": portal_port,
            "drive_count": drive_count,
            "default_lun_map": default_lun_map,
            "changer_sg": changer_sg,
            "drive_sg": drive_sg,
            "has_saved_export": has_saved_export,
            "exported_at": exported_at,
            "saved_drive_mismatch": saved_drive_mismatch,
            "can_export": can_export,
            "export_blocked_reason": export_blocked_reason,
            "product_limits": product_limits_json(),
        })),
    )
        .into_response()
}

async fn api_manage_iscsi_scan_sg(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Query(q): Query<TransportScanQuery>,
) -> impl IntoResponse {
    api_manage_transport_scan_sg(State(st), jar, Query(q)).await
}

async fn api_manage_transport_scan_sg(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Query(q): Query<TransportScanQuery>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let lib_name = match q
        .library
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "缺少查询参数 library（扫描结果须与所选在线库的驱动器数对齐；请先在下拉框选库）"
                })),
            )
                .into_response();
        }
        Some(s) if s == super::OFFLINE_LIBRARY_NAME => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "离线库不可用于传输层设备扫描" })),
            )
                .into_response();
        }
        Some(s) if s == super::LEGACY_DEFAULT_LIBRARY_NAME => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "测试库 default 不可用于传输层设备扫描" })),
            )
                .into_response();
        }
        Some(s) => s.to_string(),
    };
    let prefer_host = q.prefer_scsi_host;
    let transport = q.transport.clone();
    let lib_for_task = lib_name.clone();
    let res =
        tokio::task::spawn_blocking(move || scan_vtl_library_blocking(&lib_for_task, prefer_host))
            .await;

    match res {
        Ok(Ok((stdout, picked_host, changer_sg, drive_sg))) => {
            let m = build_transport_scan_response(
                &lib_name,
                &stdout,
                picked_host,
                changer_sg,
                drive_sg,
                transport.as_deref(),
            );
            (StatusCode::OK, Json(serde_json::Value::Object(m))).into_response()
        }
        Ok(Err(e)) if e.starts_with("LIB404:") => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("未找到库: {}", lib_name) })),
        )
            .into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("后台任务失败: {}", e) })),
        )
            .into_response(),
    }
}

async fn api_manage_iscsi_library_export(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<IscsiLibraryExportBody>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    if body.drive_sg.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "drive_sg 至少一项" })),
        )
            .into_response();
    }
    if body.drive_sg.len() > super::VTL_KERNEL_MAX_DRIVES_PER_LIB as usize {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "drive_sg 最多 {} 项（产品上限每库 {} 台驱动器）",
                    super::VTL_KERNEL_MAX_DRIVES_PER_LIB,
                    super::VTL_KERNEL_MAX_DRIVES_PER_LIB
                )
            })),
        )
            .into_response();
    }
    let lib_for_drv = body.library.clone();
    let drv_n = body.drive_sg.len();
    let drv_check = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let conn = super::init_db().map_err(|e| e.to_string())?;
        let library_id =
            super::resolve_library_id(&conn, &lib_for_drv).map_err(|e| e.to_string())?;
        let drive_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM drives WHERE library_id = ?1",
                rusqlite::params![library_id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if drive_count <= 0 {
            return Err("当前库驱动器数为 0".into());
        }
        if drv_n != drive_count as usize {
            return Err(format!(
                "drive_sg 为 {} 项，但库「{}」在数据库中配置 {} 台驱动器（须一致）",
                drv_n, lib_for_drv, drive_count
            ));
        }
        Ok(())
    })
    .await;
    match drv_check {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("后台任务失败: {}", e) })),
            )
                .into_response();
        }
    }
    let (def_iqn, def_id) = iscsi_default_iqn_and_export_id(&body.library);
    let iqn = body
        .iqn
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or(def_iqn);
    let export_id = body
        .export_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or(def_id);
    if let Err(e) = validate_iqn(&iqn) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    if let Err(e) = validate_export_id_for_iscsi(&export_id) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    if let Err(e) = validate_iscsi_portal_host(&body.portal_ip) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    if body.portal_port == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "门户端口须在 1–65535" })),
        )
            .into_response();
    }
    if !body.dry_run && !st.allow_iscsi_exec() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "未允许执行 vtladm-iscsi：请开启「允许执行」或 dry_run，或设置 allow_iscsi_exec"
            })),
        )
            .into_response();
    }

    let lun_map_str = body.lun_map.as_ref().map(|v| {
        v.iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",")
    });
    let expected_luns = 1 + body.drive_sg.len();
    if let Some(ref s) = lun_map_str {
        let parts: Vec<&str> = s.split(',').collect();
        if parts.len() != expected_luns {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("lun_map 须含 {} 个 LUN 编号（机械手+各驱动）", expected_luns) })),
            )
                .into_response();
        }
    }
    if let Some(ref lm) = body.lun_map {
        if let Err(e) = validate_lun_map_values(lm) {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
        if let Err(e) = validate_lun_map_consecutive_from_zero(lm) {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
    }

    let changer_sg = body.changer_sg.clone();
    let drive_sg = body.drive_sg.clone();
    let portal_ip = body.portal_ip.clone();
    let portal_port = body.portal_port;
    let dry_run = body.dry_run;
    let sudo = body.sudo;
    let prog = vtladm_iscsi_program();

    let iqn_for_sp = iqn.clone();
    let export_id_for_sp = export_id.clone();
    let library_for_save = body.library.clone();
    let lun_map_for_save = body.lun_map.clone();
    let res =
        tokio::task::spawn_blocking(move || -> Result<(std::process::Output, bool), String> {
            let mut cmd = Command::new(&prog);
            if sudo {
                cmd.arg("--sudo");
            }
            if dry_run {
                cmd.arg("--dry-run");
            }
            cmd.arg("library-export");
            cmd.arg("--id").arg(&export_id_for_sp);
            cmd.arg("--iqn").arg(&iqn_for_sp);
            cmd.arg("--changer-sg").arg(&changer_sg);
            for d in &drive_sg {
                cmd.arg("--drive-sg").arg(d);
            }
            if let Some(ref lm) = lun_map_str {
                cmd.arg("--lun-map").arg(lm);
            }
            cmd.arg("--portal-ip").arg(&portal_ip);
            cmd.arg("--portal-port").arg(portal_port.to_string());
            let out = cmd.output().map_err(|e| e.to_string())?;
            let ok = out.status.success();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let blocks_db = super::iscsi_export::targetcli_stderr_blocks_export_save(&stderr);
            if ok && !dry_run && !blocks_db {
                let conn = super::init_db().map_err(|e| e.to_string())?;
                let library_id = super::resolve_library_id(&conn, &library_for_save)
                    .map_err(|e| e.to_string())?;
                let lun_map =
                    lun_map_for_save.unwrap_or_else(|| (0..=drive_sg.len() as u32).collect());
                let rec = super::iscsi_export::new_iscsi_export_record(
                    iqn_for_sp.clone(),
                    export_id_for_sp.clone(),
                    changer_sg.clone(),
                    drive_sg.clone(),
                    lun_map,
                    portal_ip.clone(),
                    portal_port,
                );
                super::iscsi_export::save_iscsi_library_export(&conn, library_id, &rec)
                    .map_err(|e| e.to_string())?;
            }
            Ok((out, blocks_db))
        })
        .await;

    match res {
        Ok(Ok((out, blocks_db))) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let ok = out.status.success();
            let status = if ok {
                StatusCode::OK
            } else {
                StatusCode::BAD_GATEWAY
            };
            let saved = ok && !dry_run && !blocks_db;
            let mut body = serde_json::Map::new();
            body.insert("ok".into(), json!(ok));
            body.insert("stdout".into(), json!(stdout));
            body.insert("stderr".into(), json!(stderr));
            body.insert("dry_run".into(), json!(dry_run));
            body.insert("iqn".into(), json!(iqn));
            body.insert("export_id".into(), json!(export_id));
            body.insert("saved_to_db".into(), json!(saved));
            if blocks_db {
                body.insert(
                    "save_blocked_reason".into(),
                    json!("targetcli stderr 表明导出未完全成功，未写入 iscsi_library_exports"),
                );
            }
            if let Some(h) = iscsi_library_exec_hint(dry_run, ok, &stderr, true) {
                body.insert("hint".into(), h);
            }
            (status, Json(serde_json::Value::Object(body))).into_response()
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("后台任务失败: {}", e) })),
        )
            .into_response(),
    }
}

async fn api_manage_iscsi_library_unexport(
    State(st): State<Arc<super::web_auth::WebState>>,
    jar: CookieJar,
    Json(body): Json<IscsiLibraryUnexportBody>,
) -> impl IntoResponse {
    if let Err(resp) = require_session(&st, &jar) {
        return resp;
    }
    let resolved = match resolve_iscsi_unexport_params(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
    };
    let (iqn, export_id, lun_map, drives, library_for_db) = resolved;
    if let Err(e) = validate_iqn(&iqn) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    if let Err(e) = validate_export_id_for_iscsi(&export_id) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    if lun_map.is_none() && drives.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "须指定 drives 或 lun_map" })),
        )
            .into_response();
    }
    if let Some(ref lm) = lun_map {
        if let Err(e) = validate_lun_map_values(lm) {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
    }
    if !body.dry_run && !st.allow_iscsi_exec() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "未允许执行 vtladm-iscsi" })),
        )
            .into_response();
    }

    let lun_map_str = lun_map.as_ref().map(|v| {
        v.iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",")
    });
    let dry_run = body.dry_run;
    let sudo = body.sudo;
    let prog = vtladm_iscsi_program();
    let iqn_for_response = iqn.clone();
    let export_id_for_response = export_id.clone();

    let res =
        tokio::task::spawn_blocking(
            move || -> Result<(std::process::Output, Option<String>), String> {
                let mut cmd = Command::new(&prog);
                if sudo {
                    cmd.arg("--sudo");
                }
                if dry_run {
                    cmd.arg("--dry-run");
                }
                cmd.arg("library-unexport");
                cmd.arg("--id").arg(&export_id);
                cmd.arg("--iqn").arg(&iqn);
                if let Some(ref lm) = lun_map_str {
                    cmd.arg("--lun-map").arg(lm);
                } else if let Some(d) = drives {
                    cmd.arg("--drives").arg(d.to_string());
                }
                let out = cmd.output().map_err(|e| e.to_string())?;
                let ok = out.status.success();
                let mut lib_to_clear = library_for_db.clone();
                if ok && !dry_run && lib_to_clear.is_none() {
                    let conn = super::init_db().map_err(|e| e.to_string())?;
                    if let Ok(Some(name)) =
                        super::iscsi_export::find_library_name_by_export_credentials(
                            &conn, &export_id, &iqn,
                        )
                    {
                        lib_to_clear = Some(name);
                    }
                }
                if ok && !dry_run {
                    if let Some(ref lib) = lib_to_clear {
                        let conn = super::init_db().map_err(|e| e.to_string())?;
                        super::iscsi_export::delete_iscsi_library_export_by_name(&conn, lib)
                            .map_err(|e| e.to_string())?;
                    }
                }
                Ok((out, lib_to_clear))
            },
        )
        .await;

    match res {
        Ok(Ok((out, lib_cleared))) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let ok = out.status.success();
            let status = if ok {
                StatusCode::OK
            } else {
                StatusCode::BAD_GATEWAY
            };
            let mut j = serde_json::Map::new();
            j.insert("ok".into(), json!(ok));
            j.insert("stdout".into(), json!(stdout));
            j.insert("stderr".into(), json!(stderr));
            j.insert("dry_run".into(), json!(dry_run));
            j.insert("iqn".into(), json!(iqn_for_response));
            j.insert("export_id".into(), json!(export_id_for_response));
            j.insert(
                "removed_from_db".into(),
                json!(ok && !dry_run && lib_cleared.is_some()),
            );
            if let Some(ref lib) = lib_cleared {
                j.insert("library".into(), json!(lib));
            }
            if let Some(h) = iscsi_library_exec_hint(dry_run, ok, &stderr, false) {
                j.insert("hint".into(), h);
            }
            (status, Json(serde_json::Value::Object(j))).into_response()
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("后台任务失败: {}", e) })),
        )
            .into_response(),
    }
}

async fn page_login() -> Html<&'static str> {
    Html(LOGIN_HTML)
}

async fn page_home() -> Html<&'static str> {
    Html(HOME_HTML)
}

async fn page_browse_tapes() -> Html<&'static str> {
    Html(BROWSE_TAPES_HTML)
}

async fn page_browse_status() -> Html<&'static str> {
    Html(BROWSE_STATUS_HTML)
}

async fn page_browse_fabric() -> Html<&'static str> {
    Html(BROWSE_FABRIC_HTML)
}

async fn redirect_admin_to_overview() -> Redirect {
    Redirect::to("/admin/overview")
}

async fn page_admin_overview() -> Html<&'static str> {
    Html(ADMIN_OVERVIEW_HTML)
}

async fn page_admin_account() -> Html<&'static str> {
    Html(ADMIN_ACCOUNT_HTML)
}

async fn page_admin_tapes() -> Html<&'static str> {
    Html(ADMIN_TAPES_HTML)
}

async fn redirect_admin_libraries() -> Redirect {
    Redirect::to("/admin/library")
}

async fn redirect_admin_slots() -> Redirect {
    Redirect::to("/admin/tapes?tab=create")
}

async fn page_admin_library() -> Html<&'static str> {
    Html(ADMIN_LIBRARY_HTML)
}

async fn page_admin_shelf() -> Html<&'static str> {
    Html(ADMIN_SHELF_HTML)
}

async fn page_admin_assign_slot() -> Html<&'static str> {
    Html(ADMIN_ASSIGN_SLOT_HTML)
}

async fn page_admin_changer() -> Html<&'static str> {
    Html(ADMIN_CHANGER_HTML)
}

async fn page_admin_shelf_place() -> Html<&'static str> {
    Html(ADMIN_SHELF_PLACE_HTML)
}

async fn page_admin_iscsi() -> Html<&'static str> {
    Html(ADMIN_ISCSI_HTML)
}

async fn page_admin_transport() -> Html<&'static str> {
    Html(ADMIN_TRANSPORT_HTML)
}

async fn page_admin_setup_init() -> Html<&'static str> {
    Html(ADMIN_SETUP_INIT_HTML)
}

/// 与 `run_web_ui` 内 `serve` 使用的路由、状态与鉴权中间件一致（供 `tower::ServiceExt::oneshot` 等测试）。
/// 返回 **`Router<()>`**：`with_state` 已注入 `Arc<WebState>` 后，axum 0.7 中不再「缺状态」，故实现 `Service` 与 `oneshot`。
pub(crate) fn build_web_router(auth: Arc<super::web_auth::WebState>) -> Router<()> {
    let auth_layer = auth.clone();
    Router::new()
        .route("/", get(page_home))
        .route("/browse/tapes", get(page_browse_tapes))
        .route("/browse/status", get(page_browse_status))
        .route("/browse/fabric", get(page_browse_fabric))
        .route("/login", get(page_login))
        .route("/admin/setup-init", get(page_admin_setup_init))
        .route("/api/setup/status", get(api_setup_status))
        .route("/api/setup/complete", post(api_setup_complete))
        .route("/admin", get(redirect_admin_to_overview))
        .route("/admin/overview", get(page_admin_overview))
        .route("/admin/account", get(page_admin_account))
        .route("/admin/tapes", get(page_admin_tapes))
        .route("/admin/libraries", get(redirect_admin_libraries))
        .route("/admin/slots", get(redirect_admin_slots))
        .route("/admin/library", get(page_admin_library))
        .route("/admin/shelf", get(page_admin_shelf))
        .route("/admin/assign-slot", get(page_admin_assign_slot))
        .route("/admin/changer", get(page_admin_changer))
        .route("/admin/shelf-place", get(page_admin_shelf_place))
        .route("/admin/iscsi", get(page_admin_iscsi))
        .route("/admin/transport", get(page_admin_transport))
        .route("/api/libraries", get(api_libraries))
        .route("/api/libraries-status", get(api_libraries_status))
        .route("/api/library/detail", get(api_library_detail))
        .route("/api/shelves", get(api_shelves))
        .route("/api/offline-shelves", get(api_offline_shelves))
        .route("/api/empty-slots", get(api_empty_slots))
        .route("/api/tapes", get(api_tapes))
        .route("/api/status", get(api_status))
        .route("/api/fabric", get(api_fabric))
        .route("/api/patrol", get(api_patrol_run))
        .route("/api/captcha", get(api_captcha))
        .route("/api/login", post(api_login))
        .route("/api/logout", post(api_logout))
        .route("/api/change-password", post(api_change_password))
        .route("/api/manage/tape/create", post(api_manage_tape_create))
        .route(
            "/api/manage/library/create",
            post(api_manage_library_create),
        )
        .route(
            "/api/manage/library/delete",
            post(api_manage_library_delete),
        )
        .route("/api/manage/shelf/create", post(api_manage_shelf_create))
        .route("/api/manage/shelf/delete", post(api_manage_shelf_delete))
        .route("/api/manage/tape/assign-slot", post(api_manage_assign))
        .route("/api/manage/tape/load", post(api_manage_changer_load))
        .route("/api/manage/tape/unload", post(api_manage_changer_unload))
        .route("/api/manage/tape/eject", post(api_manage_changer_eject))
        .route("/api/manage/robot/sync", post(api_manage_robot_sync))
        .route(
            "/api/manage/robot/auto-align",
            post(api_manage_robot_auto_align),
        )
        .route(
            "/api/manage/robot/reconcile",
            post(api_manage_robot_reconcile),
        )
        .route("/api/manage/tape/shelf-place", post(api_manage_shelf_place))
        .route(
            "/api/manage/tape/shelf-place-batch",
            post(api_manage_shelf_place_batch),
        )
        .route(
            "/api/manage/shelf/create-offline",
            post(api_manage_shelf_create_offline),
        )
        .route(
            "/api/manage/tape/assign-slot-batch",
            post(api_manage_assign_batch),
        )
        .route(
            "/api/manage/tape/create-batch",
            post(api_manage_tape_create_batch),
        )
        .route(
            "/api/manage/tape/create-auto-batch",
            post(api_manage_tape_create_auto_batch),
        )
        .route("/api/manage/tape/delete", post(api_manage_tape_delete))
        .route("/api/manage/tape/init", post(api_manage_tape_init))
        .route(
            "/api/manage/tape/migrate-shelves-batch",
            post(api_manage_tape_migrate_shelves_batch),
        )
        .route(
            "/api/manage/iscsi/quick-export",
            post(api_manage_iscsi_quick_export),
        )
        .route(
            "/api/manage/iscsi/quick-unexport",
            post(api_manage_iscsi_quick_unexport),
        )
        .route(
            "/api/manage/iscsi/library-export-defaults",
            get(api_manage_iscsi_library_export_defaults),
        )
        .route(
            "/api/manage/transport/scan-sg",
            get(api_manage_transport_scan_sg),
        )
        .route("/api/manage/iscsi/scan-sg", get(api_manage_iscsi_scan_sg))
        .route(
            "/api/manage/iscsi/library-export",
            post(api_manage_iscsi_library_export),
        )
        .route(
            "/api/manage/iscsi/library-unexport",
            post(api_manage_iscsi_library_unexport),
        )
        .route("/api/manage/iscsi/config", get(api_manage_iscsi_config))
        .route("/api/manage/iscsi/check", post(api_manage_iscsi_check))
        .route(
            "/api/manage/iscsi/allow-exec",
            post(api_manage_iscsi_allow_exec),
        )
        .with_state(auth)
        .layer(middleware::from_fn_with_state(
            auth_layer,
            require_authenticated,
        ))
}

pub(crate) fn run_web_ui(host: &str, port: u16) -> Result<(), super::VtlError> {
    let addr: SocketAddr = format!("{}:{}", host, port).parse().map_err(|_| {
        super::VtlError::InvalidParameter(format!("Invalid web bind address {}:{}", host, port))
    })?;

    if std::env::var("VTLADM_WEB_COOKIE_SECURE").is_err() {
        let secure =
            !(addr.ip().is_loopback() || addr.ip().is_unspecified() && host == "127.0.0.1");
        std::env::set_var(
            "VTLADM_WEB_COOKIE_SECURE_EFFECTIVE",
            if secure { "1" } else { "0" },
        );
    }

    let auth_path = super::get_config().log_dir.join("web_admin.json");
    let auth = Arc::new(super::web_auth::WebState::new(auth_path));
    auth.init_auth_file().map_err(super::VtlError::IoError)?;

    eprintln!(
        "VTL web UI: http://{}:{}  （须登录后访问页面与 API；默认用户 {}；认证文件 {})",
        addr.ip(),
        addr.port(),
        super::web_auth::DEFAULT_WEB_USER,
        super::get_config().log_dir.join("web_admin.json").display()
    );
    eprintln!("若无法登录：在服务器执行 vtladm reset-web-auth 后 systemctl restart vtladm-web");
    eprintln!(
        "提示：若仅在本机 SSH 会话里前台运行，断开终端可能导致进程被挂断而浏览器报「连接中断」。生产环境请用 systemd、nohup 或 tmux/screen 常驻。"
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(super::VtlError::IoError)?;

    let auth_clone = auth.clone();
    rt.block_on(async move {
        let app = build_web_router(auth_clone);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(super::VtlError::IoError)?;

        axum::serve(listener, app).await.map_err(|e| {
            super::VtlError::IoError(std::io::Error::new(std::io::ErrorKind::Other, e))
        })
    })
}

const HOME_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — 首页</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
.card-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(220px,1fr));gap:1rem;}
.card{background:var(--card);border:1px solid var(--border);border-radius:10px;padding:1rem;text-decoration:none;color:inherit;display:block;box-shadow:0 1px 2px rgba(0,0,0,.04);}
.card:hover{box-shadow:0 4px 14px rgba(0,0,0,.08);} .card h3{margin:0 0 .35rem;font-size:1rem;} .card p{margin:0;font-size:.84rem;color:var(--muted);line-height:1.4;}
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_vp_side_inner.html"),
    r#"</aside>
<main class="vp-main vp-workspace">
<div class="breadcrumb">存储 · 虚拟磁带库 <b>›</b> <b>控制台</b></div>
<div class="topbar"><div><h1 style="margin:0 0 .35rem;font-size:1.28rem;">VTL 控制台</h1>
<p style="margin:0;color:var(--muted);font-size:.9rem;max-width:40rem;">为备份软件提供虚拟磁带库存储层：Web 负责建库、建磁带、SCSI/iSCSI/FC 链路；备份侧发现带库并读写磁带。</p></div></div>
<div class="card-grid">
<a class="card" href="/admin/library"><h3>① 磁带库</h3><p>创建在线库、驱动器/槽位几何，对齐内核 vtl。</p></a>
<a class="card" href="/admin/tapes"><h3>② 磁带与槽位</h3><p>批量建带、入槽、出库、inventory 对账。</p></a>
<a class="card" href="/admin/transport"><h3>③ 传输链路</h3><p>local / iSCSI / FC 向导与配置检查。</p></a>
<a class="card" href="/browse/tapes"><h3>磁带目录</h3><p>只读浏览条码、容量与位置。</p></a>
<a class="card" href="/browse/status"><h3>库状态</h3><p>全部库汇总与按库 JSON。</p></a>
<a class="card" href="/admin/overview"><h3>后台概览</h3><p>分层入口与运维快捷链接。</p></a>
</div>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"
document.getElementById('lo').onclick=async(ev)=>{ev.preventDefault();await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';};
</script></body></html>
"#
);

const BROWSE_TAPES_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — 磁带目录</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_vp_side_inner.html"),
    r#"</aside>
<main class="vp-main vp-workspace">
<div class="breadcrumb">存储功能 <b>›</b> 虚拟磁带库 <b>›</b> <b>磁带目录</b></div>
<div class="topbar"><h1 style="margin:0;font-size:1.22rem;">磁带目录</h1><span class="hint" style="margin:0">条码 · 容量 · 已用 · 位置</span></div>
<div class="toolbar">
<span class="inline"><label style="display:inline;margin:0">在线库</label> <select id="lib"></select>
<button type="button" id="reload">刷新</button></span>
</div>
<section class="panel"><div id="tapes"></div></section>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"function fmtBytes(n){
  n=Number(n)||0;if(n===0)return'0 B';
  const u=['B','KB','MB','GB','TB'];let i=0,x=n;
  while(x>=1024&&i<u.length-1){x/=1024;i++;}
  return (x>=100||i===0?x.toFixed(0):x.toFixed(1))+' '+u[i];
}
function escapeHtml(s){return String(s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
function loc(x){
  if(x.in_drive)return'驱动器';
  if(x.slot!=null)return'槽位 '+x.slot;
  if(x.shelf_name)return'架: '+escapeHtml(x.shelf_name);
  return'货架';
}
async function loadLibs(){
  const r=await fetch('/api/libraries',{credentials:'include'});
  const j=await r.json();
  if(!r.ok){document.getElementById('tapes').innerHTML='<p class="err">'+(j.error||r.status)+'</p>';return;}
  const sel=document.getElementById('lib');
  sel.innerHTML='';
  (j.libraries||[]).filter(l=>!l.is_offline_storage&&l.name).forEach(l=>{const o=document.createElement('option');o.value=l.name;o.textContent=l.name+' (#'+l.id+')';sel.appendChild(o);});
  if(sel.options.length) await refresh();
}
async function refresh(){
  const lib=document.getElementById('lib').value;
  const t=await fetch('/api/tapes?library='+encodeURIComponent(lib),{credentials:'include'});
  const tj=await t.json();
  if(!t.ok){document.getElementById('tapes').innerHTML='<p class="err">'+(tj.error||t.status)+'</p>';return;}
  const rows=tj.tapes||[];
  if(!rows.length){document.getElementById('tapes').innerHTML='<div class="empty">当前库暂无磁带</div>';return;}
  let h='<table class="data-table"><thead><tr><th>名称</th><th>条码</th><th class="num">容量</th><th class="num">已用</th><th>位置</th><th>货架</th><th>在驱动</th></tr></thead><tbody>';
  rows.forEach(x=>{h+='<tr><td>'+escapeHtml(x.name)+'</td><td>'+escapeHtml(x.barcode)+'</td><td class="num">'+fmtBytes(x.capacity_bytes)+'</td><td class="num">'+fmtBytes(x.used_bytes||0)+'</td><td>'+loc(x)+'</td><td>'+(x.shelf_name?escapeHtml(x.shelf_name):'—')+'</td><td>'+(x.in_drive?'是':'否')+'</td></tr>';});
  if(tj.truncated){h+='<tr><td colspan="7" style="text-align:center;color:#888">（已显示前 '+rows.length+' 条，共 '+tj.total+' 条）</td></tr>';}
  h+='</tbody></table>';document.getElementById('tapes').innerHTML=h;
}
document.getElementById('reload').onclick=refresh;
document.getElementById('lib').onchange=refresh;
loadLibs();
document.getElementById('lo').onclick=async(ev)=>{ev.preventDefault();await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';};
</script></body></html>
"#
);

const BROWSE_STATUS_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — 库状态</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_vp_side_inner.html"),
    r#"</aside>
<main class="vp-main vp-workspace">
<div class="breadcrumb">存储功能 <b>›</b> 虚拟磁带库 <b>›</b> <b>库状态</b></div>
<div class="topbar"><h1 style="margin:0;font-size:1.22rem;">库状态</h1></div>
<section class="panel">
<h2 style="margin:0 0 .6rem;font-size:1rem;border:0;padding:0">全部库汇总</h2>
<div id="sum-wrap"></div>
<button type="button" id="reload-sum">刷新汇总</button>
</section>
<section class="panel">
<label>当前库（详细 JSON）</label> <select id="lib"></select>
<button type="button" id="reload">刷新详情</button>
</section>
<section class="panel"><pre id="status"></pre></section>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"function escapeHtml(s){return String(s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
async function loadSummary(){
  const r=await fetch('/api/libraries-status',{credentials:'include'});
  const j=await r.json();
  const el=document.getElementById('sum-wrap');
  if(!r.ok){el.innerHTML='<p class="err">'+(j.error||r.status)+'</p>';return;}
  const rows=j.libraries||[];
  if(!rows.length){el.innerHTML='<div class="empty">暂无库</div>';return;}
  let h='<table class="data-table"><thead><tr><th>库</th><th class="num">磁带数</th><th class="num">已加载驱动</th><th class="num">驱动数</th><th class="num">数据槽位</th><th>iSCSI</th></tr></thead><tbody>';
  rows.forEach(x=>{
    const isc=x.iscsi_exported===true?('是 '+escapeHtml(x.iscsi_iqn||'')):(x.iscsi_exported===false?'否':'—');
    h+='<tr><td>'+escapeHtml(x.library)+'</td><td class="num">'+x.tape_count+'</td><td class="num">'+x.loaded_in_drives+'</td><td class="num">'+x.drives+'</td><td class="num">'+x.data_slots+'</td><td>'+isc+'</td></tr>';
  });
  h+='</tbody></table>';el.innerHTML=h;
}
async function loadLibs(){
  const r=await fetch('/api/libraries',{credentials:'include'});
  const j=await r.json();
  if(!r.ok){document.getElementById('status').textContent=(j.error||r.status);return;}
  const sel=document.getElementById('lib');sel.innerHTML='';
  (j.libraries||[]).forEach(l=>{const o=document.createElement('option');o.value=l.name;o.textContent=l.name+' (#'+l.id+')';sel.appendChild(o);});
  if(sel.options.length) await refresh();
}
async function refresh(){
  const lib=document.getElementById('lib').value;
  const s=await fetch('/api/status?library='+encodeURIComponent(lib),{credentials:'include'});
  const sj=await s.json();
  document.getElementById('status').textContent=JSON.stringify(sj,null,2);
}
document.getElementById('reload-sum').onclick=loadSummary;
document.getElementById('reload').onclick=refresh;
document.getElementById('lib').onchange=refresh;
loadSummary();
loadLibs();
document.getElementById('lo').onclick=async(ev)=>{ev.preventDefault();await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';};
</script></body></html>
"#
);

const BROWSE_FABRIC_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — 传输与路径</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_vp_side_inner.html"),
    r#"</aside>
<main class="vp-main vp-workspace">
<div class="breadcrumb">存储功能 <b>›</b> 虚拟磁带库 <b>›</b> <b>传输配置</b></div>
<div class="topbar"><h1 style="margin:0;font-size:1.22rem;">传输配置（JSON）</h1>
<p class="hint" style="margin:.35rem 0 0">运维只读；分层说明与 iSCSI 操作请用 <a href="/admin/transport">传输向导</a>、<a href="/admin/iscsi">iSCSI 映射</a>。</p></div>
<section class="panel"><button type="button" id="reload">刷新</button><pre id="out"></pre></section>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"async function refresh(){
  const r=await fetch('/api/fabric',{credentials:'include'});
  const j=await r.json();
  document.getElementById('out').textContent=r.ok?JSON.stringify(j,null,2):JSON.stringify(j,null,2);
}
document.getElementById('reload').onclick=refresh;
refresh();
document.getElementById('lo').onclick=async(ev)=>{ev.preventDefault();await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';};
</script></body></html>
"#
);

const ADMIN_SETUP_INIT_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL 初始化配置</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
body.login-page{display:flex;min-height:100vh;align-items:flex-start;justify-content:center;padding:1.25rem;margin:0;}
.setup-card{width:100%;max-width:560px;background:var(--card);border:1px solid var(--border);border-radius:10px;padding:1.25rem 1.35rem;box-shadow:0 4px 20px rgba(15,23,42,.08);margin-top:1rem;}
.setup-card h1{margin:0 0 .35rem;font-size:1.15rem;color:var(--accent);}
.setup-hint{font-size:.84rem;color:var(--muted);line-height:1.55;margin:0 0 1rem;}
.setup-card label{display:block;margin:.45rem 0 .15rem;font-size:.88rem;}
.setup-card input[type=text]{width:100%;box-sizing:border-box;}
.setup-card button{margin-top:1rem;padding:.55rem 1rem;font-weight:600;}
.setup-card .err{margin:.5rem 0 0;color:#b91c1c;}
.setup-done{display:none;margin-top:.75rem;}
</style>
</head>
<body class="login-page">
<div class="setup-card">
<h1>首次初始化配置</h1>
<p class="setup-hint">未检测到 <code>/opt/vtladm/var/vtl.conf</code>。路径须为<strong>绝对路径</strong>；留空则使用 <code>/opt/vtladm/var/</code> 下默认路径（库、磁带镜像、日志）。提交后将创建目录并写入主配置。默认 <strong>不在提交瞬间</strong> 重载内核；改库后默认走 <code>vtl-kernelctl reload</code>（内核 <code>allow_hot_geom=N</code>，不在线热改几何）。若配置了 <code>kernel_vtl_reload_script</code> 或 ioctl 可用则自动对齐。<strong><code>kernel_reload_on_db_change</code> 默认为 <code>false</code></strong>。勾选下方可在保存后立即 ioctl 对齐；整模块 <code>rmmod</code> 仅当显式开启 <code>kernel_reload_on_db_change</code> 且 ioctl 失败时才会尝试，并在磁带设备仍被占用时拒绝。详见 <code>docs/SCSI.md</code> §1c。</p>
<div id="done" class="setup-done setup-hint">主配置已存在。<a href="/admin/overview">进入后台总览</a></div>
<form id="sf" style="display:none">
<label>db_path（SQLite 文件）</label><input name="db_path" type="text" autocomplete="off"/>
<label>tape_dir（磁带镜像目录）</label><input name="tape_dir" type="text" autocomplete="off"/>
<label>log_dir（运行日志目录）</label><input name="log_dir" type="text" autocomplete="off"/>
<label>kernel_vtl_reload_script（可选）</label><input name="kernel_vtl_reload_script" type="text" autocomplete="off"/>
<label>vtl_ko（可选，写入 vtl.conf；vtladm 调用重载脚本时注入环境变量 VTL_KO）</label><input name="vtl_ko" type="text" autocomplete="off"/>
<label>vtl_reload_scan_delay_ms（可选，毫秒；写入 vtl.conf；重载脚本收到 VTL_SCAN_DELAY_MS，对应 insmod 的 scan_delay_ms）</label><input name="vtl_reload_scan_delay_ms" type="text" inputmode="numeric" autocomplete="off"/>
<label style="margin-top:.65rem"><input name="run_kernel_reload_now" type="checkbox" value="1"/> 提交成功后<strong>立即</strong>尝试对齐内核几何（默认仅 <code>/dev/vtl</code> ioctl，<strong>不</strong>跑 <code>rmmod</code>；仅当 <code>kernel_reload_on_db_change=true</code> 且 ioctl 失败时才可能执行 <code>kernel_vtl_reload_script</code>，有磁带占用时会拒绝 <code>rmmod</code>）</label>
<button type="submit">保存并创建配置</button>
<p class="err" id="se"></p>
</form>
</div>
<script>
async function boot(){
  const r=await fetch('/api/setup/status',{credentials:'include'});
  const j=await r.json().catch(()=>({}));
  if(!j.setup_required){
    document.getElementById('done').style.display='block';
    return;
  }
  const d=j.defaults||{};
  const f=document.getElementById('sf');
  f.style.display='block';
  f.querySelector('[name=db_path]').placeholder=d.db_path||'';
  f.querySelector('[name=tape_dir]').placeholder=d.tape_dir||'';
  f.querySelector('[name=log_dir]').placeholder=d.log_dir||'';
  f.querySelector('[name=kernel_vtl_reload_script]').placeholder=d.kernel_vtl_reload_script||'/opt/vtladm/scripts/vtl-kernel-reload.sh';
  f.querySelector('[name=vtl_ko]').placeholder=d.vtl_ko||'/opt/vtladm/ko/vtl.ko';
  f.querySelector('[name=vtl_reload_scan_delay_ms]').placeholder='留空则脚本默认 500；仍不稳可 vtl.conf 写大或 export VTL_POST_ADD_SCAN_DELAY_MS=1000';
}
boot();
document.getElementById('sf').onsubmit=async(ev)=>{
  ev.preventDefault();
  document.getElementById('se').textContent='';
  const fd=new FormData(ev.target);
  const body={
    db_path:(fd.get('db_path')||'').toString().trim(),
    tape_dir:(fd.get('tape_dir')||'').toString().trim(),
    log_dir:(fd.get('log_dir')||'').toString().trim(),
    kernel_vtl_reload_script:(fd.get('kernel_vtl_reload_script')||'').toString().trim(),
    vtl_ko:(fd.get('vtl_ko')||'').toString().trim(),
    vtl_reload_scan_delay_ms:(fd.get('vtl_reload_scan_delay_ms')||'').toString().trim(),
    run_kernel_reload_now:!!fd.get('run_kernel_reload_now')
  };
  const r=await fetch('/api/setup/complete',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body),credentials:'include'});
  const j=await r.json().catch(()=>({}));
  if(r.ok){location.href='/admin/overview';return;}
  document.getElementById('se').textContent=j.error||('HTTP '+r.status);
};
</script>
</body></html>
"#
);

// 登录页：**不使用 <form>**。
//
// 历史 bug：旧版用 <form id="f"> 但未设 action，靠 onsubmit 里的 ev.preventDefault()
// 阻止默认提交。一旦 JS 因任何原因（绑定时机、autofill 提前提交、双击、CSP）
// 没拦截住，浏览器就把表单按默认行为 POST 到当前 URL `/login`——而 `/login`
// 是 GET 路由，结果是渲染同一个登录页，用户看到的就是"只刷新就结束"。
//
// 重写方案：完全用 <div> 容器 + <button type="button">，物理上消除"默认表单提交"
// 这条故障路径。所有交互都走显式 addEventListener；fetch 失败、网络异常、JSON 解析
// 失败均有可见错误提示，不会静默回退。
const LOGIN_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width,initial-scale=1"/>
<meta name="referrer" content="no-referrer"/>
<title>VTL 登录</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
body.login-page{display:flex;min-height:100vh;align-items:center;justify-content:center;padding:1.25rem;margin:0;}
.login-card{width:100%;max-width:420px;background:var(--card);border:1px solid var(--border);border-radius:10px;padding:1.25rem 1.35rem;box-shadow:0 4px 20px rgba(15,23,42,.08);}
.login-card h1{margin:0 0 .5rem;font-size:1.2rem;color:var(--accent);}
.login-hint{font-size:.84rem;color:var(--muted);line-height:1.55;margin:0 0 1rem;}
.login-hint code{font-size:.8rem;background:#f1f5f9;padding:.1rem .35rem;border-radius:4px;border:1px solid #e2e8f0;}
.login-card label{display:block;margin:.6rem 0 .2rem;font-size:.92rem;}
.login-card input{width:100%;box-sizing:border-box;padding:.45rem .55rem;font-size:.95rem;}
.login-card .captcha-row{display:flex;align-items:center;gap:.6rem;}
.login-card .captcha-q{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:1rem;letter-spacing:.05em;}
.login-card .btn-link{background:none;border:none;color:var(--accent);cursor:pointer;font-size:.84rem;padding:0;text-decoration:underline;}
.login-card .btn-link:disabled{color:var(--muted);cursor:wait;text-decoration:none;}
.login-card #submitBtn{margin-top:1.1rem;width:100%;padding:.6rem;font-weight:600;font-size:.98rem;cursor:pointer;}
.login-card #submitBtn:disabled{opacity:.65;cursor:wait;}
.login-card #msg{margin:.7rem 0 0;font-size:.88rem;min-height:1.2em;line-height:1.45;word-break:break-word;}
.login-card #msg.err{color:#b91c1c;}
.login-card #msg.ok{color:#047857;}
.login-card .login-foot{margin:.85rem 0 0;font-size:.84rem;color:var(--muted);}
.login-card .login-foot a{color:var(--accent);}
</style>
</head>
<body class="login-page">
<div class="login-card" id="loginCard">
  <h1>VTL Web 登录</h1>
  <p class="login-hint">默认用户 <code>admin</code>。首次运行会在日志目录生成 <code>web_admin.json</code>（bcrypt 哈希）；初始密码请从安装输出或服务器端 <code>vtladm reset-web-auth</code> 获取，登录后请尽快修改。</p>

  <label for="u">用户名</label>
  <input id="u" type="text" autocomplete="username" autocapitalize="none" spellcheck="false" required/>

  <label for="p">密码</label>
  <input id="p" type="password" autocomplete="current-password" required/>

  <label for="a">验证码</label>
  <div class="captcha-row">
    <span id="q" class="captcha-q">加载中…</span>
    <button id="captchaReload" type="button" class="btn-link" title="换一题">换一题</button>
  </div>
  <input id="a" type="text" inputmode="numeric" autocomplete="off" spellcheck="false" required/>

  <button id="submitBtn" type="button">登录</button>

  <p id="msg" role="status" aria-live="polite"></p>
  <p class="login-foot"><a href="/">返回首页</a>（须先登录后访问）。</p>
</div>

<script>
(function () {
  "use strict";

  var state = { captchaId: "", busy: false };

  function $(id) { return document.getElementById(id); }

  function setMsg(text, kind) {
    var el = $("msg");
    if (!el) return;
    el.textContent = text || "";
    el.className = kind || "";
  }

  function setBusy(b) {
    state.busy = b;
    var btn = $("submitBtn");
    if (btn) {
      btn.disabled = b;
      btn.textContent = b ? "登录中…" : "登录";
    }
    var rl = $("captchaReload");
    if (rl) rl.disabled = b;
  }

  function getInput(id) {
    var el = $(id);
    return el ? String(el.value || "") : "";
  }

  async function loadCaptcha() {
    var q = $("q");
    var rl = $("captchaReload");
    if (q) q.textContent = "加载中…";
    if (rl) rl.disabled = true;
    state.captchaId = "";
    try {
      var r = await fetch("/api/captcha", { credentials: "include", cache: "no-store" });
      if (!r.ok) throw new Error("HTTP " + r.status);
      var j = await r.json();
      state.captchaId = j.captcha_id || "";
      if (q) q.textContent = (j.question || "(空)").trim();
      var ans = $("a");
      if (ans) ans.value = "";
    } catch (e) {
      if (q) q.textContent = "加载失败";
      setMsg("无法获取验证码：" + (e && e.message ? e.message : e) + "（请检查与服务器的连接，然后点「换一题」）", "err");
    } finally {
      if (rl && !state.busy) rl.disabled = false;
    }
  }

  async function detectSetupTarget() {
    try {
      var s = await fetch("/api/setup/status", { credentials: "include", cache: "no-store" });
      if (!s.ok) return "/admin/library";
      var sj = await s.json();
      if (sj && sj.setup_required) return "/admin/setup-init";
    } catch (_) { /* 忽略：跳到 library 作为默认 */ }
    return "/admin/library";
  }

  async function doLogin() {
    if (state.busy) return;
    setMsg("", "");
    var u = getInput("u").trim();
    var p = getInput("p");          // 密码不 trim：保留前后空格的合法字符
    var a = getInput("a").trim();
    if (!u || !p || !a) { setMsg("请填写用户名、密码和验证码", "err"); return; }
    if (!state.captchaId) { setMsg("验证码未加载完成，请稍候或点「换一题」", "err"); return; }

    setBusy(true);
    try {
      var resp;
      try {
        resp = await fetch("/api/login", {
          method: "POST",
          headers: { "Content-Type": "application/json", "Accept": "application/json" },
          body: JSON.stringify({
            username: u,
            password: p,
            captcha_id: state.captchaId,
            captcha_answer: a
          }),
          credentials: "include",
          cache: "no-store"
        });
      } catch (netErr) {
        setMsg("网络错误：无法连接服务器（" + (netErr && netErr.message ? netErr.message : netErr) + "）", "err");
        return;
      }

      var body = {};
      try { body = await resp.json(); } catch (_) { /* 非 JSON 响应：保持 body={} */ }

      if (resp.ok && body && body.ok) {
        setMsg("登录成功，正在跳转…", "ok");
        var target = await detectSetupTarget();
        window.location.assign(target);
        return;
      }

      var msg = (body && body.error) ? body.error : ("登录失败（HTTP " + resp.status + "）");
      if (body && body.hint) msg += " — " + body.hint;
      if (body && typeof body.retry_after_secs === "number") {
        msg += "（约 " + body.retry_after_secs + " 秒后可重试）";
      }
      setMsg(msg, "err");
      // 验证码单次消费，无论成败都换一题
      await loadCaptcha();
    } finally {
      setBusy(false);
    }
  }

  function onKey(ev) {
    if (ev.key === "Enter" || ev.keyCode === 13) {
      ev.preventDefault();
      doLogin();
    }
  }

  function init() {
    var btn = $("submitBtn");
    if (btn) btn.addEventListener("click", function (ev) { ev.preventDefault(); doLogin(); });
    var rl = $("captchaReload");
    if (rl) rl.addEventListener("click", function (ev) { ev.preventDefault(); loadCaptcha(); });
    ["u", "p", "a"].forEach(function (id) {
      var el = $(id);
      if (el) el.addEventListener("keydown", onKey);
    });
    var first = $("u");
    if (first) { try { first.focus(); } catch (_) {} }
    loadCaptcha();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
</script>
</body>
</html>
"#
);

const ADMIN_OVERVIEW_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — 后台概览</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
.grid-minis{display:grid;gap:.65rem;grid-template-columns:repeat(auto-fill,minmax(260px,1fr));}
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_admin_side_inner.html"),
    r#"</aside>
<main class="adm-main adm-workspace">
<div class="breadcrumb">存储 · 虚拟磁带库 <b>›</b> <b>概览</b></div>
<div class="topbar">
<div><h1 style="margin:0;font-size:1.28rem;">后台概览</h1>
<p style="margin:.35rem 0 0;color:var(--muted);font-size:.9rem;max-width:44rem;">按 <strong>① 建库 → ② 磁带/槽位 → ③ 传输 → ④ 备份软件</strong> 使用；详见 <a href="/admin/transport">传输向导</a> 与 <code>docs/WEB-WORKFLOW.md</code>。</p></div>
<div><span class="badge-w">已登录</span> <button type="button" id="btn-logout">登出</button></div>
</div>
<div class="grid-minis">
<a class="mini" href="/admin/library"><strong>① 磁带库</strong><span>建库、几何、删库</span></a>
<a class="mini" href="/admin/tapes"><strong>② 磁带与货架</strong><span>批量建带、迁移、删带</span></a>
<a class="mini" href="/admin/assign-slot"><strong>② 磁带入槽</strong><span>货架 → 在线库槽位</span></a>
<a class="mini" href="/admin/changer"><strong>② inventory 对账</strong><span>备份搬带后 DB↔内核</span></a>
<a class="mini" href="/admin/transport"><strong>③ 传输向导</strong><span>SCSI / iSCSI / FC</span></a>
<a class="mini" href="/admin/iscsi"><strong>③ iSCSI 映射</strong><span>library-export，记录入库</span></a>
<a class="mini" href="/admin/shelf"><strong>货架</strong><span>在线/离线货架</span></a>
<a class="mini" href="/admin/shelf-place"><strong>磁带出库</strong><span>在线库 → 离线</span></a>
<a class="mini" href="/admin/account"><strong>账户与安全</strong><span>修改登录密码</span></a>
</div>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"document.getElementById('btn-logout').onclick=async()=>{
  await fetch('/api/logout',{method:'POST',credentials:'include'});
  location.href='/login';
};
</script></body></html>
"#
);

const ADMIN_TRANSPORT_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — 传输向导</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
.workflow{display:grid;gap:.75rem;grid-template-columns:repeat(auto-fill,minmax(280px,1fr));margin:1rem 0;}
.wstep{border:1px solid var(--border,#ddd);border-radius:8px;padding:.85rem 1rem;background:var(--panel,#fafafa);}
.wstep h3{margin:0 0 .35rem;font-size:1rem;}
.wstep p{margin:0;font-size:.88rem;color:var(--muted,#555);line-height:1.45;}
.wstep a{display:inline-block;margin-top:.5rem;font-size:.88rem;}
.wstep-num{color:var(--accent,#2980b9);font-weight:600;}
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_admin_side_inner.html"),
    r#"</aside>
<main class="adm-main adm-workspace">
<div class="breadcrumb">存储功能 <b>›</b> 虚拟磁带库 <b>›</b> <b>传输向导</b></div>
<div class="topbar" style="justify-content:space-between;flex-wrap:wrap;gap:.75rem">
<div><h1 style="margin:0;font-size:1.22rem;">传输链路（SCSI / iSCSI / FC）</h1>
<p class="hint" style="margin:.35rem 0 0;max-width:44rem">本页汇总<strong>③ 传输层</strong>：内核在本机提供 SCSI 带库语义；可选 iSCSI（Web 一键 export）或 FC（系统级 target）供<strong>备份软件</strong>作为存储层连接。完整分层见仓库 <code>userspace/docs/WEB-WORKFLOW.md</code>。</p></div>
<button type="button" id="btn-logout">登出</button>
</div>
<section class="panel">
<h2 style="margin:0 0 .6rem;font-size:1rem">推荐顺序</h2>
<div class="workflow">
<div class="wstep"><h3><span class="wstep-num">①</span> 磁带库</h3><p>先建库并对齐 <code>vtl.ko</code> 几何（驱动器数、槽位数）。</p><a href="/admin/library">建库与库属性 →</a></div>
<div class="wstep"><h3><span class="wstep-num">②</span> 磁带与槽位</h3><p>创建 <code>.vtltape</code> 镜像，必要时入槽；DB 记录槽位/货架。</p><a href="/admin/tapes">磁带与货架 →</a></div>
<div class="wstep"><h3><span class="wstep-num">③</span> 传输（本页）</h3><p>按部署选择 local / iSCSI / FC，使备份机可见 SCSI 带库。</p></div>
<div class="wstep"><h3><span class="wstep-num">④</span> 备份软件</h3><p>在备份侧添加存储单元、扫描带库；换带与备份任务在备份侧完成（非 Web）。</p></div>
</div>
</section>
<section class="panel">
<h2 style="margin:0 0 .6rem;font-size:1rem">三种承载方式</h2>
<table class="data-table"><thead><tr><th>方式</th><th>适用</th><th>Web / 工具</th></tr></thead><tbody>
<tr><td><strong>SCSI（local）</strong></td><td>备份软件与本机同台</td><td>下方按库扫描 <code>/dev/sg*</code>、<code>/dev/st*</code>（仅显示建库时配置的驱动器数）</td></tr>
<tr><td><strong>iSCSI</strong></td><td>备份机经以太网</td><td>先扫描核对节点，再在 <a href="/admin/iscsi">iSCSI / LUN 映射</a> 执行 library-export</td></tr>
<tr><td><strong>FC</strong></td><td>SAN / 光纤</td><td>下方扫描本机应对节点；FC target 由系统配置，见 <code>docs/TRANSPORT.md</code></td></tr>
</tbody></table>
<p class="hint" id="transport-limits-hint" style="margin-top:.75rem">产品上限：在线库最多 <strong>8</strong> 个；每库最多 <strong>8</strong> 台驱动器、<strong>256</strong> 个数据槽。内核可能枚举更多磁带 LUN，界面与导出<strong>仅使用前 N 台</strong>（N = 建库驱动数）。</p>
<p class="hint" style="margin-top:.5rem">SCSI 是磁带/机械手<strong>设备模型</strong>；iSCSI 与 FC 是在网络上<strong>承载同一套 SCSI</strong>，并非三套独立产品功能。</p>
</section>
<section class="panel">
<h2 style="margin:0 0 .6rem;font-size:1rem">按库核对 SCSI 设备（local / iSCSI / FC 共用）</h2>
<p class="hint">选择在线库后扫描；结果行数 = 1 机械手 + 库内磁带机数（与「磁带库」页建库时填写一致）。</p>
<label>当前在线库</label><select id="tselib" style="max-width:16rem"></select>
<label style="margin-left:.75rem">承载方式（仅影响说明文案）</label>
<select id="tmode"><option value="local">SCSI（本机）</option><option value="iscsi">iSCSI</option><option value="fc">FC</option></select>
<button type="button" id="btn-scan-dev">扫描 lsscsi（VTL）</button>
<p class="err" id="tscan-err"></p>
<p class="hint" id="tscan-note"></p>
<table class="data-table" style="margin-top:.5rem"><thead><tr><th>角色</th><th>LUN</th><th>/dev/sg</th><th>/dev/st 或 sch</th></tr></thead><tbody id="tscan-body"><tr><td colspan="4" class="muted">尚未扫描</td></tr></tbody></table>
<pre id="tscan-raw" style="max-height:8rem;overflow:auto;font-size:.78rem;margin-top:.5rem"></pre>
</section>
<section class="panel">
<h2 style="margin:0 0 .6rem;font-size:1rem">当前配置</h2>
<button type="button" id="btn-fabric">刷新</button>
<pre id="fabric-out" style="max-height:14rem;overflow:auto;margin-top:.5rem"></pre>
<p class="hint">CLI：<code>vtladm transport show|check|guide</code> · 定时：<code>vtl-patrol.timer</code></p>
<p><button type="button" id="btn-patrol">运行巡检</button></p>
<pre id="patrol-out" style="max-height:14rem;overflow:auto;margin-top:.5rem"></pre>
</section>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"let VTL_LIMITS={max_online_libraries:8,max_drives_per_library:8,max_data_slots_per_library:256};
function applyProductLimitsFromApi(j){
  if(!j||!j.product_limits)return;
  const L=j.product_limits;
  VTL_LIMITS={max_online_libraries:Number(L.max_online_libraries)||8,max_drives_per_library:Number(L.max_drives_per_library)||8,max_data_slots_per_library:Number(L.max_data_slots_per_library)||256};
  const h=document.getElementById('transport-limits-hint');
  if(h){h.innerHTML='产品上限：在线库最多 <strong>'+VTL_LIMITS.max_online_libraries+'</strong> 个；每库最多 <strong>'+VTL_LIMITS.max_drives_per_library+'</strong> 台驱动器、<strong>'+VTL_LIMITS.max_data_slots_per_library+'</strong> 个数据槽。内核可能枚举更多磁带 LUN，界面与导出<strong>仅使用前 N 台</strong>（N = 建库驱动数）。';}
}
function escapeHtml(s){return String(s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
function onlineLibs(j){return (j.libraries||[]).filter(l=>{const n=l.name||'';if(n==='__offline__')return false;if(l.is_offline_storage)return false;return !!n;});}
async function loadTransportLibs(){
  const sel=document.getElementById('tselib');
  const r=await fetch('/api/libraries',{credentials:'include'});
  const j=await r.json();
  if(!r.ok)return;
  applyProductLimitsFromApi(j);
  sel.innerHTML='';
  onlineLibs(j).forEach(l=>{const o=document.createElement('option');o.value=l.name;o.textContent=l.name;sel.appendChild(o);});
  const q=new URLSearchParams(location.search).get('library');
  if(q&&[...sel.options].some(o=>o.value===q))sel.value=q;
}
function renderScanDevices(j){
  const tb=document.getElementById('tscan-body');
  const devs=j.devices||[];
  if(!devs.length){tb.innerHTML='<tr><td colspan="4" class="muted">无设备</td></tr>';return;}
  tb.innerHTML=devs.map(d=>{
    const role=d.role==='changer'?'机械手':'磁带机 '+(d.index!=null?d.index:'');
    const aux=d.role==='changer'?(d.sch||'—'):(d.st||'—');
    return '<tr><td>'+escapeHtml(role)+'</td><td class="num">'+(d.lun!=null?d.lun:'')+'</td><td><code>'+escapeHtml(d.sg||'—')+'</code></td><td><code>'+escapeHtml(aux)+'</code></td></tr>';
  }).join('');
}
document.getElementById('btn-logout').onclick=async()=>{await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';};
document.getElementById('btn-scan-dev').onclick=async()=>{
  document.getElementById('tscan-err').textContent='';
  const lib=document.getElementById('tselib').value;
  const mode=document.getElementById('tmode').value;
  if(!lib){document.getElementById('tscan-err').textContent='请选择在线库';return;}
  const url='/api/manage/transport/scan-sg?library='+encodeURIComponent(lib)+'&transport='+encodeURIComponent(mode);
  const r=await fetch(url,{credentials:'include'});
  const j=await r.json();
  if(!r.ok){document.getElementById('tscan-err').textContent=j.error||r.status;document.getElementById('tscan-note').textContent='';return;}
  document.getElementById('tscan-note').textContent=(j.note||'')+' SCSI host '+j.picked_scsi_host+'，驱动器 '+j.drive_count+' 台。';
  renderScanDevices(j);
  document.getElementById('tscan-raw').textContent=j.raw_tail||'';
};
async function loadFabric(){
  const r=await fetch('/api/fabric',{credentials:'include'});
  const j=await r.json();
  applyProductLimitsFromApi(j);
  document.getElementById('fabric-out').textContent=JSON.stringify(j,null,2);
  const mode=(j.transport||'local').toLowerCase();
  const sel=document.getElementById('tmode');
  if(sel&&[...sel.options].some(o=>o.value===mode))sel.value=mode;
}
document.getElementById('btn-fabric').onclick=loadFabric;
document.getElementById('btn-patrol').onclick=async()=>{
  const el=document.getElementById('patrol-out');
  el.textContent='…';
  const r=await fetch('/api/patrol',{credentials:'include'});
  const j=await r.json();
  el.textContent=(j.stdout||'')+(j.stderr?'\n'+j.stderr:'')+'\nexit_code='+j.exit_code;
};
(async()=>{await loadTransportLibs();await loadFabric();})();
</script></body></html>
"#
);

const ADMIN_ACCOUNT_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — 账户与安全</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
main.adm-main{max-width:36rem;}
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_admin_side_inner.html"),
    r#"</aside>
<main class="adm-main adm-workspace">
<div class="breadcrumb">存储 · 虚拟磁带库 <b>›</b> <b>系统</b> <b>›</b> <b>账户与安全</b></div>
<div class="topbar"><h1 style="margin:0;font-size:1.2rem;">账户与安全</h1><button type="button" id="btn-logout">登出</button></div>
<section class="panel"><h2>修改密码</h2>
<label>原密码</label><input id="op" type="password"/>
<label>新密码（至少 8 字符）</label><input id="np" type="password"/>
<button type="button" id="chgpw">保存</button><p class="err" id="pe"></p>
</section>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"async function jpost(url,body){
  const r=await fetch(url,{method:'POST',headers:{'Content-Type':'application/json'},credentials:'include',body:JSON.stringify(body)});
  const t=await r.text();let j;try{j=JSON.parse(t);}catch{j={raw:t};}return{r,j};
}
document.getElementById('btn-logout').onclick=async()=>{await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';};
document.getElementById('chgpw').onclick=async()=>{
  document.getElementById('pe').textContent='';
  const {r,j}=await jpost('/api/change-password',{old_password:document.getElementById('op').value,new_password:document.getElementById('np').value});
  if(!r.ok){document.getElementById('pe').textContent=j.error||r.status;return;}
  showToast('已修改密码');
};
</script></body></html>
"#
);

const ADMIN_TAPES_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — 磁带与货架</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
input,select{max-width:36rem;}
.btn-del{color:#c0392b!important;font-weight:600;}
.btn-del:hover{background:#c0392b;color:#fff!important;}
.btn-init{color:#2980b9!important;}
.loading-mask{pointer-events:none;opacity:.5;}
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_admin_side_inner.html"),
    r#"</aside>
<main class="adm-main adm-workspace">
<div class="breadcrumb">存储 · 虚拟磁带库 <b>›</b> <b>磁带与货架</b></div>
<div class="topbar"><h1 style="margin:0;font-size:1.2rem;">磁带与货架</h1><div><span class="badge-w">已登录</span> <button type="button" id="btn-logout">登出</button></div></div>
<p class="hint" style="margin-top:0">先选择<strong>当前在线库</strong>；下方按步骤切换标签，避免创建、迁移与删除混在同一屏。</p>
<div class="ctx-strip">
<div class="ctx-strip-inner">
<label class="ctx-label" for="tlib">当前在线库</label>
<select id="tlib"></select>
</div>
</div>
<nav class="panel-tabs" role="tablist" aria-label="磁带操作">
<button type="button" class="ptab active" role="tab" id="tabbtn-create" data-tab="create" aria-selected="true">批量创建</button>
<button type="button" class="ptab" role="tab" id="tabbtn-migrate" data-tab="migrate" aria-selected="false">货架迁移</button>
<button type="button" class="ptab" role="tab" id="tabbtn-maintain" data-tab="maintain" aria-selected="false">初始化 / 删除</button>
</nav>
<div id="tab-create" class="tab-panel active" role="tabpanel" aria-labelledby="tabbtn-create">
<section class="panel"><h2>批量创建磁带</h2>
<p class="hint">名称由程序按 <code>{库名}_tape</code> + 数字自动生成（如 <code>marstor_tape01</code>）；<strong>磁带名在所有库间须全局唯一</strong>（内核扁平 <code>tape_dir</code>）。条码自动随机。<code>robot sync</code> 前会自动链接镜像；链接失败则 sync 中止。只需选<strong>货架</strong>、<strong>数量</strong>、<strong>容量</strong>。CLI/API 手动建带亦须遵守全局唯一命名。</p>
<label>货架</label><select id="tshelf"><option value="">默认货架</option></select>
<label>数量（1–10000）</label><input type="number" id="tcnt" min="1" max="10000" value="10"/>
<label>容量（如 500M、2G）</label><input id="tsize" placeholder="500M"/>
<button type="button" id="btauto">创建磁带</button><p class="err" id="te"></p>
</section>
</div>
<div id="tab-migrate" class="tab-panel" role="tabpanel" aria-labelledby="tabbtn-migrate">
<section class="panel"><h2>货架间批量迁移</h2>
<p class="hint">在同一在线库内，将磁带从<strong>源货架</strong>迁到<strong>目标货架</strong>。仅列出在源货架上、<strong>未入槽</strong>且<strong>未在驱动</strong>的磁带；不移动镜像文件。</p>
<label>源货架</label><select id="mfrom"><option value="">请选择源货架</option></select>
<label>目标货架</label><select id="mto"><option value="">请选择目标货架</option></select>
<div class="row-actions"><button type="button" id="mst">全选</button><button type="button" id="mclr">清除勾选</button></div>
<div id="mwrap"></div>
<button type="button" id="bmig">批量迁移</button><p class="err" id="me"></p>
</section>
</div>
<div id="tab-maintain" class="tab-panel" role="tabpanel" aria-labelledby="tabbtn-maintain">
<section class="panel"><h2>初始化与删除磁带</h2>
<p class="hint">初始化：将 <code>used_bytes</code> 置 0，并把镜像文件截断为标称容量（空白带）。磁带须<strong>在货架上</strong>（未入机械手槽）、<strong>不在驱动中</strong>。删除会移除数据库记录并删除镜像文件；若删除镜像失败会在服务端记错误日志，界面仍可能显示成功（需检查磁盘）。</p>
<div class="row-actions"><button type="button" id="treload">刷新列表</button></div>
<div id="tmwrap"></div><p class="err" id="tme"></p>
</section>
</div>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"function fmtBytes(n){n=Number(n)||0;if(n===0)return'0 B';const u=['B','KB','MB','GB','TB'];let i=0,x=n;while(x>=1024&&i<u.length-1){x/=1024;i++;}return (x>=100||i===0?x.toFixed(0):x.toFixed(1))+' '+u[i];}
function escapeHtml(s){return String(s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
async function jpost(url,body){
  const r=await fetch(url,{method:'POST',headers:{'Content-Type':'application/json'},credentials:'include',body:JSON.stringify(body)});
  const t=await r.text();let j;try{j=JSON.parse(t);}catch{j={raw:t};}return{r,j};
}
function onlineLibs(j){return (j.libraries||[]).filter(l=>!l.is_offline_storage&&l.name);}
document.getElementById('btn-logout').onclick=async()=>{await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';};
function initTapeTabs(){
  document.querySelectorAll('.ptab').forEach(btn=>{
    btn.addEventListener('click',()=>{
      const id=btn.getAttribute('data-tab');
      document.querySelectorAll('.ptab').forEach(b=>{
        const on=b===btn;
        b.classList.toggle('active',on);
        b.setAttribute('aria-selected',on?'true':'false');
      });
      document.querySelectorAll('.tab-panel').forEach(p=>{
        p.classList.toggle('active',p.id==='tab-'+id);
      });
    });
  });
}
initTapeTabs();
function appendShelfOptions(sel, shelves, withDefault){
  if(withDefault){
    const o=document.createElement('option');o.value='';o.textContent='默认货架';sel.appendChild(o);
  }
  (shelves||[]).forEach(s=>{
    const o=document.createElement('option');o.value=s.name;
    const tag=s.is_default_unused?'（未使用默认）':'';
    o.textContent=s.name+tag;sel.appendChild(o);
  });
}
async function loadLibs(){
  document.getElementById('te').textContent='';
  const r=await fetch('/api/libraries',{credentials:'include'});
  const j=await r.json();
  const sel=document.getElementById('tlib');
  sel.innerHTML='';
  if(!r.ok){document.getElementById('te').textContent=j.error||r.status;return;}
  onlineLibs(j).forEach(l=>{const o=document.createElement('option');o.value=l.name;o.textContent=l.name+' (#'+l.id+')';sel.appendChild(o);});
  const qt=new URLSearchParams(location.search).get('library');
  if(qt&&[...sel.options].some(o=>o.value===qt)) sel.value=qt;
  sel.onchange=async()=>{await loadShelves();await loadMigrateTable();await loadTapeMaintainTable();};
  if(sel.options.length) await loadShelves();
  if(sel.options.length) await loadMigrateTable();
  if(sel.options.length) await loadTapeMaintainTable();
  const tab=new URLSearchParams(location.search).get('tab');
  if(tab==='maintain'){const b=document.getElementById('tabbtn-maintain');if(b)b.click();}
  else if(tab==='migrate'){const b=document.getElementById('tabbtn-migrate');if(b)b.click();}
  else if(tab==='create'){const b=document.getElementById('tabbtn-create');if(b)b.click();}
}
async function loadTapeMaintainTable(){
  const lib=document.getElementById('tlib').value;
  const w=document.getElementById('tmwrap');
  const errEl=document.getElementById('tme');
  errEl.textContent='';
  w.innerHTML='';
  if(!lib)return;
  const tr=await fetch('/api/tapes?library='+encodeURIComponent(lib),{credentials:'include'});
  const tj=await tr.json();
  if(!tr.ok){errEl.textContent=tj.error||tr.status;return;}
  const rows=tj.tapes||[];
  if(!rows.length){w.innerHTML='<div class="empty">当前库无磁带</div>';return;}
  let h='<table class="data-table"><thead><tr><th>名称</th><th>条码</th><th class="num">容量</th><th class="num">已用</th><th>位置</th><th class="ops">操作</th></tr></thead><tbody>';
  rows.forEach(t=>{
    const loc=t.in_drive?'驱动中':(t.slot!=null?('槽位 '+t.slot):('架: '+(t.shelf_name?escapeHtml(t.shelf_name):'—')));
    const enc=encodeURIComponent(t.name);
    h+='<tr><td>'+escapeHtml(t.name)+'</td><td>'+escapeHtml(t.barcode)+'</td><td class="num">'+fmtBytes(t.capacity_bytes)+'</td><td class="num">'+fmtBytes(t.used_bytes||0)+'</td><td>'+loc+'</td><td class="ops"><button type="button" class="lnk btn-init" data-name="'+enc+'">初始化</button> <button type="button" class="lnk btn-del" data-name="'+enc+'" title="不可恢复！">⛔ 删除</button></td></tr>';
  });
  if(tj.truncated){h+='<tr><td colspan="6" style="text-align:center;color:var(--muted)">（已显示前 '+rows.length+' 条，共 '+tj.total+' 条；如需查看更多请使用 CLI）</td></tr>';}
  h+='</tbody></table>';w.innerHTML=h;
}
document.getElementById('treload').onclick=loadTapeMaintainTable;
document.getElementById('tmwrap').addEventListener('click',async (ev)=>{
  const t=ev.target;
  if(!t.classList.contains('btn-init')&&!t.classList.contains('btn-del'))return;
  const lib=document.getElementById('tlib').value;
  const name=decodeURIComponent(t.getAttribute('data-name'));
  const errEl=document.getElementById('tme');
  errEl.textContent='';
  if(t.classList.contains('btn-del')){
    if(!confirm('⚠️ 确定永久删除磁带「'+name+'」？\n此操作不可恢复，磁带数据将被清除。'))return;
    t.disabled=true;t.textContent='删除中…';
    const {r,j}=await jpost('/api/manage/tape/delete',{library:lib,name:name});
    t.disabled=false;t.textContent='⛔ 删除';
    if(!r.ok){errEl.textContent=j.error||r.status;return;}
    if(j.warning){errEl.textContent='⚠️ '+j.warning;}
  }else{
    if(!confirm('确定初始化磁带「'+name+'」？（已写数据将丢失）'))return;
    t.disabled=true;t.textContent='初始化中…';
    const {r,j}=await jpost('/api/manage/tape/init',{library:lib,name:name});
    t.disabled=false;t.textContent='初始化';
    if(!r.ok){errEl.textContent=j.error||r.status;return;}
  }
  await loadTapeMaintainTable();
  await loadMigrateTable();
});
async function loadShelves(){
  const lib=document.getElementById('tlib').value;
  const sh=document.getElementById('tshelf');
  const mfrom=document.getElementById('mfrom');
  const mto=document.getElementById('mto');
  sh.innerHTML='';
  mfrom.innerHTML='';
  mto.innerHTML='';
  const ph=document.createElement('option');ph.value='';ph.textContent='请选择源货架';mfrom.appendChild(ph);
  const ph2=document.createElement('option');ph2.value='';ph2.textContent='请选择目标货架';mto.appendChild(ph2);
  if(!lib)return;
  const r=await fetch('/api/shelves?library='+encodeURIComponent(lib),{credentials:'include'});
  const j=await r.json();
  if(!r.ok)return;
  const shelves=j.shelves||[];
  appendShelfOptions(sh, shelves, true);
  shelves.forEach(s=>{
    const o1=document.createElement('option');o1.value=s.name;o1.textContent=s.name+(s.is_default_unused?'（未使用默认）':'');mfrom.appendChild(o1);
    const o2=document.createElement('option');o2.value=s.name;o2.textContent=s.name+(s.is_default_unused?'（未使用默认）':'');mto.appendChild(o2);
  });
}
function tapesOnShelf(list, shelfName){
  return (list||[]).filter(t=>t.shelf_name===shelfName && t.slot==null && !t.in_drive);
}
async function loadMigrateTable(){
  const me=document.getElementById('me');
  me.textContent='';
  const lib=document.getElementById('tlib').value;
  const from=document.getElementById('mfrom').value;
  const w=document.getElementById('mwrap');
  w.innerHTML='';
  if(!lib||!from){return;}
  const tr=await fetch('/api/tapes?library='+encodeURIComponent(lib),{credentials:'include'});
  const tj=await tr.json();
  if(!tr.ok){me.textContent=tj.error||tr.status;return;}
  const rows=tapesOnShelf(tj.tapes, from);
  if(!rows.length){w.innerHTML='<div class="empty">当前源货架上无可迁移磁带</div>';return;}
  let h='<table class="data-table"><thead><tr><th style="width:2.5rem"></th><th>名称</th><th>条码</th><th class="num">容量</th></tr></thead><tbody>';
  rows.forEach(t=>{h+='<tr><td><input type="checkbox" class="cm" value="'+encodeURIComponent(t.name)+'"/></td><td>'+escapeHtml(t.name)+'</td><td>'+escapeHtml(t.barcode)+'</td><td class="num">'+fmtBytes(t.capacity_bytes)+'</td></tr>';});
  h+='</tbody></table>';w.innerHTML=h;
}
document.getElementById('mfrom').addEventListener('change', loadMigrateTable);
document.getElementById('btauto').onclick=async()=>{
  document.getElementById('te').textContent='';
  const cnt=parseInt(String(document.getElementById('tcnt').value),10);
  if(!(cnt>=1&&cnt<=10000)){document.getElementById('te').textContent='数量须在 1–10000 之间';return;}
  const size=document.getElementById('tsize').value.trim();
  if(!size){document.getElementById('te').textContent='请填写容量';return;}
  const shv=document.getElementById('tshelf').value;
  const lib=document.getElementById('tlib').value;
  const btn=document.getElementById('btauto');
  btn.disabled=true;btn.textContent='创建中（'+cnt+' 条）…';
  const {r,j}=await jpost('/api/manage/tape/create-auto-batch',{library:lib,shelf:shv?shv:null,count:cnt,size:size});
  btn.disabled=false;btn.textContent='创建磁带';
  if(!r.ok){document.getElementById('te').textContent=j.error||r.status;return;}
  const ns=j.names||[];
  const span=ns.length?('自 '+ns[0]+' 至 '+ns[ns.length-1]):'';
  showToast('已创建 '+ns.length+' 条 '+span);
  await loadTapeMaintainTable();
};
document.getElementById('mst').onclick=()=>{document.querySelectorAll('.cm').forEach(x=>{x.checked=true;});};
document.getElementById('mclr').onclick=()=>{document.querySelectorAll('.cm').forEach(x=>{x.checked=false;});};
document.getElementById('bmig').onclick=async()=>{
  const me=document.getElementById('me');
  me.textContent='';
  const lib=document.getElementById('tlib').value;
  const from=document.getElementById('mfrom').value;
  const to=document.getElementById('mto').value;
  if(!from){me.textContent='请选择源货架';return;}
  if(!to){me.textContent='请选择目标货架';return;}
  if(from===to){me.textContent='源货架与目标货架须不同';return;}
  const tapes=[...document.querySelectorAll('.cm:checked')].map(x=>decodeURIComponent(x.value));
  if(!tapes.length){me.textContent='请勾选要迁移的磁带';return;}
  const btn=document.getElementById('bmig');
  btn.disabled=true;btn.textContent='迁移中（'+tapes.length+' 条）…';
  document.getElementById('mwrap').classList.add('loading-mask');
  const {r,j}=await jpost('/api/manage/tape/migrate-shelves-batch',{library:lib,from_shelf:from,to_shelf:to,tapes:tapes});
  btn.disabled=false;btn.textContent='批量迁移';
  document.getElementById('mwrap').classList.remove('loading-mask');
  if(!r.ok){me.textContent=j.error||r.status;return;}
  showToast('已迁移 '+tapes.length+' 条');
  await loadMigrateTable();
};
loadLibs();
</script></body></html>
"#
);

const ADMIN_LIBRARY_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — 磁带库</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
main.adm-main.adm-lib-detail{max-width:1280px;}
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_admin_side_inner.html"),
    r#"</aside>
<main class="adm-main adm-workspace adm-lib-detail">
<div class="breadcrumb">存储功能 <b>›</b> 虚拟磁带库 <b>›</b> <b>磁带库</b></div>
<div class="topbar" style="justify-content:space-between;align-items:flex-start;flex-wrap:wrap;gap:.75rem">
<div>
<h1 id="lib-title" style="margin:0;font-size:1.22rem;">磁带库</h1>
<p class="hint" style="margin:.35rem 0 0"><strong>① 磁带库</strong>：建库与几何；② 建带/入槽见工具条；③ 暴露给备份机见 <a href="/admin/transport">传输向导</a> / <a href="/admin/iscsi">iSCSI</a>。</p>
</div>
<div style="display:flex;flex-wrap:wrap;gap:.5rem;align-items:center">
<label class="ctx-label" for="libsel">当前库</label>
<select id="libsel" style="min-width:12rem"></select>
<button type="button" id="btn-create-toggle" class="btn-create" title="展开/收起新建在线库">+ 创建</button>
<span class="badge-w">已登录</span>
<button type="button" id="btn-logout">登出</button>
</div>
</div>
<div class="lib-toolbar">
<button type="button" id="tb-import" title="货架/离线区磁带装入在线库槽位">磁带入槽</button>
<button type="button" id="tb-create-tape" title="打开「磁带与货架」页的批量创建">创建磁带</button>
<button type="button" id="tb-export" title="批量离库到离线货架（与入库相反）">磁带出库</button>
<button type="button" id="tb-reconcile" title="DB 与内核机械手 inventory 对账（reconcile / auto-align）">inventory 对账</button>
<button type="button" id="tb-lun">LUN映射</button>
<button type="button" id="tb-props">属性</button>
<button type="button" id="tb-delete" class="toolbar-danger">删除</button>
</div>
<p class="err" id="le"></p>
<details class="lib-fold" id="fold-create">
<summary>新建在线库</summary>
<div class="fold-body">
<label>库名</label><input id="lname" style="max-width:24rem" pattern="[A-Za-z0-9_-]+" title="仅字母、数字、-、_"/>
<p class="hint" style="font-size:.82rem;margin:.2rem 0 0">库名即 canonical 名（与磁带目录子文件夹一致）：仅 ASCII 字母、数字、<code>-</code>、<code>_</code>；磁带名须在<strong>全部库</strong>中全局唯一，批量建带为 <code>{库名}_tape01</code>…</p>
<label>驱动数</label><input id="ldrv" type="number" value="2" min="1" max="8" style="max-width:8rem"/>
<label>槽位数</label><input id="lslot" type="number" value="32" min="1" max="256" style="max-width:8rem"/>
<p class="hint" id="lib-limits-hint" style="font-size:.82rem;color:var(--muted);margin:.35rem 0 0">产品上限：在线库最多 <strong>8</strong> 个；每库最多 <strong>8</strong> 台驱动器、<strong>256</strong> 个数据槽（与内核 vtl 一致）。</p>
<p class="hint" style="font-size:.82rem;border-left:3px solid #2980b9;padding-left:.5rem;margin:.35rem 0 0">创建/删除库后，<code>vtladm</code> 用 <code>/dev/vtl</code> ioctl 对齐 SCSI 几何（无 <code>rmmod</code>）。机械手换带由<strong>备份软件</strong>经 iSCSI 完成；Web 仅管库/磁带/货架/iSCSI。详见 <code>docs/SCSI.md</code>、<code>docs/ROBOT-SYNC.md</code>。</p>
<button type="button" id="blib">创建库</button>
<p class="hint">删除库须先选库，并在下方「删除」确认；至少保留一个在线库。</p>
</div>
</details>
<details class="lib-fold" id="fold-basic" open>
<summary>基本信息</summary>
<div class="fold-body">
<table class="sum"><tbody id="basic-rows"></tbody></table>
</div>
</details>
<details class="lib-fold" open>
<summary>驱动器</summary>
<div class="fold-body">
<table class="data-table"><thead><tr><th>驱动器</th><th>磁带</th><th>条码</th></tr></thead><tbody id="drive-rows"></tbody></table>
</div>
</details>
<details class="lib-fold" open>
<summary>磁带</summary>
<div class="fold-body">
<table class="data-table"><thead><tr><th>名称</th><th>条码</th><th>容量</th><th>已用</th><th>位置</th></tr></thead><tbody id="tape-rows"></tbody></table>
</div>
</details>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"let VTL_LIMITS={max_online_libraries:8,max_drives_per_library:8,max_data_slots_per_library:256};
function applyProductLimitsFromApi(j){
  if(!j||!j.product_limits)return;
  const L=j.product_limits;
  VTL_LIMITS={max_online_libraries:Number(L.max_online_libraries)||8,max_drives_per_library:Number(L.max_drives_per_library)||8,max_data_slots_per_library:Number(L.max_data_slots_per_library)||256};
  const d=document.getElementById('ldrv'),s=document.getElementById('lslot'),h=document.getElementById('lib-limits-hint');
  if(d){d.max=VTL_LIMITS.max_drives_per_library;d.min=1;}
  if(s){s.max=VTL_LIMITS.max_data_slots_per_library;s.min=1;}
  if(h){h.innerHTML='产品上限：在线库最多 <strong>'+VTL_LIMITS.max_online_libraries+'</strong> 个；每库最多 <strong>'+VTL_LIMITS.max_drives_per_library+'</strong> 台驱动器、<strong>'+VTL_LIMITS.max_data_slots_per_library+'</strong> 个数据槽（与内核 vtl 一致）。';}
}
function fmtBytes(n){n=Number(n)||0;if(n===0)return'0 B';const u=['B','KB','MB','GB','TB'];let i=0,x=n;while(x>=1024&&i<u.length-1){x/=1024;i++;}return (x>=100||i===0?x.toFixed(0):x.toFixed(1))+' '+u[i];}
function escapeHtml(s){return String(s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
function locOf(t){if(t.in_drive)return'驱动器';if(t.slot!==null&&t.slot!==undefined)return'槽位 '+t.slot;if(t.shelf_name)return escapeHtml(t.shelf_name);return'—';}
async function jpost(url,body){
  const r=await fetch(url,{method:'POST',headers:{'Content-Type':'application/json'},credentials:'include',body:JSON.stringify(body)});
  const t=await r.text();let j;try{j=JSON.parse(t);}catch{j={raw:t};}return{r,j};
}
function onlineLibs(j){return (j.libraries||[]).filter(l=>!l.is_offline_storage&&l.name);}
let onlineLibCount=0;
function setUrlLib(name){
  const u=new URL(location.href);
  u.searchParams.set('library',name);
  history.replaceState({},'',u);
}
async function loadLibSelect(){
  document.getElementById('le').textContent='';
  const r=await fetch('/api/libraries',{credentials:'include'});
  let j;
  try{j=await r.json();}catch{document.getElementById('le').textContent='库列表接口返回非 JSON';return;}
  applyProductLimitsFromApi(j);
  onlineLibCount=onlineLibs(j).length;
  const sel=document.getElementById('libsel');
  sel.innerHTML='';
  if(!r.ok){document.getElementById('le').textContent=j.error||r.status;return;}
  const params=new URLSearchParams(location.search);
  let want=params.get('library')||'';
  (j.libraries||[]).forEach(l=>{
    const o=document.createElement('option');
    o.value=l.name;
    o.textContent=l.name+(l.is_offline_storage?'（离线保留库）':'');
    sel.appendChild(o);
  });
  const names=(j.libraries||[]).map(l=>l.name);
  if(want&&names.includes(want)) sel.value=want;
  else if(sel.options.length) sel.value=sel.options[0].value;
}
async function loadDetail(){
  document.getElementById('le').textContent='';
  const lib=document.getElementById('libsel').value;
  if(!lib)return;
  setUrlLib(lib);
  const r=await fetch('/api/library/detail?library='+encodeURIComponent(lib),{credentials:'include'});
  const j=await r.json();
  if(!r.ok){document.getElementById('le').textContent=j.error||r.status;return;}
  const L=j.library||{};
  document.getElementById('lib-title').textContent='磁带库 — '+escapeHtml(L.name||'');
  const br=document.getElementById('basic-rows');
  br.innerHTML=
    '<tr><th>库 ID</th><td>'+L.id+'</td></tr>'+
    '<tr><th>名称</th><td>'+escapeHtml(L.name||'')+'</td></tr>'+
    '<tr><th>创建时间</th><td>'+escapeHtml(L.created_at||'')+'</td></tr>'+
    '<tr><th>离线保留库</th><td>'+(L.is_offline_storage?'是':'否')+'</td></tr>'+
    '<tr><th>磁带总数</th><td>'+L.tape_count+'</td></tr>'+
    '<tr><th>已装入驱动器</th><td>'+L.loaded_in_drives+' / '+L.drive_count+'</td></tr>'+
    '<tr><th>数据槽位数</th><td>'+L.data_slots+'</td></tr>'+
    '<tr><th>I/O 槽（邮筒）</th><td>'+L.mail_slots+'</td></tr>'+
    '<tr><th>配置 max_drives</th><td>'+escapeHtml(String(L.max_drives||''))+'</td></tr>'+
    '<tr><th>配置 slots</th><td>'+escapeHtml(String(L.slots||''))+'</td></tr>';
  const dr=document.getElementById('drive-rows');
  if(!j.drives||!j.drives.length){
    dr.innerHTML='<tr><td colspan="3" class="muted">无驱动器行</td></tr>';
  }else{
    dr.innerHTML=j.drives.map(d=>'<tr><td class="num">drive '+d.drive_id+'</td><td>'+(d.tape_name?escapeHtml(d.tape_name):'—')+'</td><td>'+(d.tape_barcode?escapeHtml(d.tape_barcode):'—')+'</td></tr>').join('');
  }
  const tr=document.getElementById('tape-rows');
  if(!j.tapes||!j.tapes.length){
    tr.innerHTML='<tr><td colspan="5" class="muted">无磁带</td></tr>';
  }else{
    tr.innerHTML=j.tapes.map(t=>'<tr><td>'+escapeHtml(t.name)+'</td><td>'+escapeHtml(t.barcode)+'</td><td class="num">'+fmtBytes(t.capacity_bytes)+'</td><td class="num">'+fmtBytes(t.used_bytes)+'</td><td>'+locOf(t)+'</td></tr>').join('');
  }
  const off=L.is_offline_storage;
  const canDel=!off&&L.can_delete_online;
  document.getElementById('tb-import').disabled=off;
  document.getElementById('tb-create-tape').disabled=off;
  document.getElementById('tb-export').disabled=off;
  document.getElementById('tb-reconcile').disabled=off;
  document.getElementById('tb-lun').disabled=off;
  document.getElementById('tb-delete').disabled=!canDel;
}
document.getElementById('btn-logout').onclick=async()=>{await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';};
document.getElementById('libsel').onchange=loadDetail;
document.getElementById('btn-create-toggle').onclick=()=>{
  const d=document.getElementById('fold-create');
  d.open=!d.open;
};
document.getElementById('tb-import').onclick=()=>{
  const lib=document.getElementById('libsel').value;
  location.href='/admin/assign-slot?library='+encodeURIComponent(lib);
};
document.getElementById('tb-create-tape').onclick=()=>{
  const lib=document.getElementById('libsel').value;
  location.href='/admin/tapes?library='+encodeURIComponent(lib)+'&tab=create';
};
document.getElementById('tb-export').onclick=()=>{
  const lib=document.getElementById('libsel').value;
  location.href='/admin/shelf-place?library='+encodeURIComponent(lib);
};
document.getElementById('tb-reconcile').onclick=()=>{
  const lib=document.getElementById('libsel').value;
  location.href='/admin/changer?library='+encodeURIComponent(lib);
};
document.getElementById('tb-lun').onclick=()=>{location.href='/admin/iscsi?library='+encodeURIComponent(document.getElementById('libsel').value);};
document.getElementById('tb-props').onclick=()=>{
  const d=document.getElementById('fold-basic');
  if(d){d.open=true;d.scrollIntoView({behavior:'smooth'});}
};
document.getElementById('tb-delete').onclick=async()=>{
  const lib=document.getElementById('libsel').value;
  const r0=await fetch('/api/library/detail?library='+encodeURIComponent(lib),{credentials:'include'});
  if(!r0.ok){document.getElementById('le').textContent='无法读取库状态';return;}
  const L=(await r0.json()).library||{};
  if(L.is_offline_storage){showToast('不可删除保留库');return;}
  if(!L.can_delete_online){showToast('须至少保留一个在线库');return;}
  const c=prompt('删除在线库「'+lib+'」。请输入库名以确认：');
  if(c!==lib)return;
  const {r,j}=await jpost('/api/manage/library/delete',{name:lib});
  if(!r.ok){document.getElementById('le').textContent=j.error||r.status;return;}
  let msg='已删除';
  if(Array.isArray(j.file_warnings)&&j.file_warnings.length) msg+='（有文件清理警告，见 JSON）';
  if(j.kernel_geom&&j.kernel_geom!=='ioctl_ok'&&j.kernel_geom!=='rescan_only'&&j.kernel_geom!=='hot_geom_disabled'&&j.kernel_geom!=='script_ok'&&j.kernel_geom!=='reload_ok'){
    msg+=' [内核:'+j.kernel_geom+(j.kernel_geom_detail?(' '+j.kernel_geom_detail):'')+']';
  } else if(j.kernel_geom==='hot_geom_disabled'){
    msg+=' [请执行 vtl-kernelctl reload]';
  }
  showToast(msg);
  await loadLibSelect();
  await loadDetail();
};
document.getElementById('blib').onclick=async()=>{
  document.getElementById('le').textContent='';
  const nm=document.getElementById('lname').value.trim();
  const drives=parseInt(document.getElementById('ldrv').value,10);
  const slots=parseInt(document.getElementById('lslot').value,10);
  if(!Number.isFinite(drives)||drives<1||drives>VTL_LIMITS.max_drives_per_library){document.getElementById('le').textContent='驱动数须在 1..'+VTL_LIMITS.max_drives_per_library;return;}
  if(!Number.isFinite(slots)||slots<1||slots>VTL_LIMITS.max_data_slots_per_library){document.getElementById('le').textContent='槽位数须在 1..'+VTL_LIMITS.max_data_slots_per_library;return;}
  if(onlineLibCount>=VTL_LIMITS.max_online_libraries){document.getElementById('le').textContent='在线库已达上限 '+VTL_LIMITS.max_online_libraries+' 个';return;}
  const {r,j}=await jpost('/api/manage/library/create',{name:nm,drives:drives,slots:slots});
  if(!r.ok){document.getElementById('le').textContent=j.error||r.status;return;}
  let msg='已创建库';
  if(j.kernel_geom&&j.kernel_geom!=='ioctl_ok'&&j.kernel_geom!=='rescan_only'&&j.kernel_geom!=='hot_geom_disabled'&&j.kernel_geom!=='script_ok'&&j.kernel_geom!=='reload_ok'){
    msg+=' [内核:'+j.kernel_geom+(j.kernel_geom_detail?(' '+j.kernel_geom_detail):'')+']';
  } else if(j.kernel_geom==='hot_geom_disabled'){
    msg+=' [请执行 vtl-kernelctl reload]';
  } else if(j.scsi_rescan==='failed'){
    msg+=' [SCSI scan 失败，请运行 vtl-scsi-scan-all-hosts.sh]';
  } else {
    msg+='（本机 lsscsi 应见 1 机械手 + '+drives+' 磁带机；iSCSI 在「传输」页另做）';
  }
  showToast(msg);
  document.getElementById('lname').value='';
  await loadLibSelect();
  if(nm&&[...document.getElementById('libsel').options].some(o=>o.value===nm)) document.getElementById('libsel').value=nm;
  await loadDetail();
};
(async()=>{
  await loadLibSelect();
  await loadDetail();
})();
</script></body></html>
"#
);

const ADMIN_SHELF_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — 磁带架</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
main.adm-main{max-width:42rem;}
input,select{max-width:100%;}
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_admin_side_inner.html"),
    r#"</aside>
<main class="adm-main adm-workspace">
<div class="breadcrumb">存储 · 虚拟磁带库 <b>›</b> <b>磁带架</b></div>
<div class="topbar"><h1 style="margin:0;font-size:1.2rem;">磁带架（在线 / 离线）</h1><button type="button" id="btn-logout">登出</button></div>
<p class="hint" style="margin:0 0 .75rem">货架用于<strong>模拟离库后的手动保管</strong>，与在线虚拟库分离；磁带离库后进入保留库 <code>__offline__</code> 下对应货架。</p>
<section class="panel"><h2>新建离线货架</h2>
<label>货架名</label><input id="sname"/>
<button type="button" id="bshelf">创建离线货架</button><p class="err" id="se"></p>
</section>
<section class="panel"><h2>删除在线库中的货架</h2>
<p class="hint">不可删除默认「未使用」架；架上须无磁带。</p>
<label>在线库</label><select id="dblib"></select>
<label>货架</label><select id="dbsh"></select>
<button type="button" id="bdelsh">删除货架</button><p class="err" id="sde"></p>
</section>
<section class="panel"><h2>删除离线货架</h2>
<p class="hint">仅列出自建离线货架；须无磁带。</p>
<label>离线货架</label><select id="dosh"></select>
<button type="button" id="bdelosh">删除离线货架</button><p class="err" id="osde"></p>
</section>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"async function jpost(url,body){
  const r=await fetch(url,{method:'POST',headers:{'Content-Type':'application/json'},credentials:'include',body:JSON.stringify(body)});
  const t=await r.text();let j;try{j=JSON.parse(t);}catch{j={raw:t};}return{r,j};
}
function onlineLibs(j){return (j.libraries||[]).filter(l=>!l.is_offline_storage&&l.name);}
const OFFLINE_LIB='__offline__';
async function loadDelOnlineLibs(){
  const r=await fetch('/api/libraries',{credentials:'include'});
  const j=await r.json();
  const sel=document.getElementById('dblib');
  sel.innerHTML='';
  if(!r.ok)return;
  onlineLibs(j).forEach(l=>{const o=document.createElement('option');o.value=l.name;o.textContent=l.name;sel.appendChild(o);});
  sel.onchange=loadDelOnlineShelves;
  if(sel.options.length) await loadDelOnlineShelves();
}
async function loadDelOnlineShelves(){
  const lib=document.getElementById('dblib').value;
  const sh=document.getElementById('dbsh');
  sh.innerHTML='';
  if(!lib)return;
  const r=await fetch('/api/shelves?library='+encodeURIComponent(lib),{credentials:'include'});
  const j=await r.json();
  if(!r.ok)return;
  (j.shelves||[]).filter(s=>!s.is_default_unused).forEach(s=>{
    const o=document.createElement('option');o.value=s.name;o.textContent=s.name;sh.appendChild(o);
  });
}
async function loadOfflineShelvesDel(){
  const r=await fetch('/api/offline-shelves',{credentials:'include'});
  const j=await r.json();
  const sel=document.getElementById('dosh');
  sel.innerHTML='';
  if(!r.ok)return;
  (j.shelves||[]).forEach(s=>{const o=document.createElement('option');o.value=s.name;o.textContent=s.name;sel.appendChild(o);});
}
document.getElementById('btn-logout').onclick=async()=>{await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';};
document.getElementById('bshelf').onclick=async()=>{
  document.getElementById('se').textContent='';
  const {r,j}=await jpost('/api/manage/shelf/create-offline',{name:document.getElementById('sname').value});
  if(!r.ok){document.getElementById('se').textContent=j.error||r.status;return;}
  showToast('已创建离线货架');
  await loadOfflineShelvesDel();
};
document.getElementById('bdelsh').onclick=async()=>{
  document.getElementById('sde').textContent='';
  const lib=document.getElementById('dblib').value;
  const name=document.getElementById('dbsh').value;
  if(!name){document.getElementById('sde').textContent='没有可删除的非默认货架';return;}
  if(!confirm('确定删除货架 '+name+' ?'))return;
  const {r,j}=await jpost('/api/manage/shelf/delete',{library:lib,name:name});
  if(!r.ok){document.getElementById('sde').textContent=j.error||r.status;return;}
  showToast('已删除货架');
  await loadDelOnlineShelves();
};
document.getElementById('bdelosh').onclick=async()=>{
  document.getElementById('osde').textContent='';
  const name=document.getElementById('dosh').value;
  if(!name){document.getElementById('osde').textContent='无自建离线货架';return;}
  if(!confirm('确定删除离线货架 '+name+' ?'))return;
  const {r,j}=await jpost('/api/manage/shelf/delete',{library:OFFLINE_LIB,name:name});
  if(!r.ok){document.getElementById('osde').textContent=j.error||r.status;return;}
  showToast('已删除离线货架');
  await loadOfflineShelvesDel();
};
loadDelOnlineLibs();
loadOfflineShelvesDel();
</script></body></html>
"#
);

const ADMIN_CHANGER_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — inventory 对账</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
.row2{display:grid;gap:.75rem;grid-template-columns:repeat(auto-fill,minmax(14rem,1fr));}
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_admin_side_inner.html"),
    r#"</aside>
<main class="adm-main adm-workspace">
<div class="breadcrumb">存储 · 虚拟磁带库 <b>›</b> <b>inventory 对账</b></div>
<div class="topbar"><h1 style="margin:0;font-size:1.2rem;">inventory 对账</h1><button type="button" id="btn-logout">登出</button></div>
<p class="hint">运行时机械手真相在 <strong>vtl.ko</strong>（与 <code>mtx</code> 一致）。本页核对 SQLite 目录与内核 inventory：<code>reconcile --pull</code> / <code>auto-align</code>；<code>sync-db</code> 仅镜像内核数据槽号到 <code>tapes.slot</code>（API <code>/api/manage/robot/sync</code>）。DB→内核全量 <code>robot sync</code> 已移除。装/卸/弹出可用 CLI 或 API（ioctl，与 assign-slot 同类）。</p>
<section class="panel"><h2>库状态</h2>
<label>在线库</label><select id="alib"></select>
<button type="button" id="reload">刷新状态</button>
<pre id="st" class="mono" style="margin-top:.5rem;white-space:pre-wrap;font-size:.85rem;"></pre>
</section>
<section class="panel"><h2>DB ↔ 内核 inventory</h2>
<button type="button" id="btnRecon">对账（仅报告）</button>
<button type="button" id="btnReconPull">写回 DB（内核→DB，pull）</button>
<button type="button" id="btnAutoAlign">自动对齐（auto-align）</button>
<p class="hint" style="font-size:.82rem;margin-top:.5rem">「写回 DB」适用于备份软件搬带后；「自动对齐」会离架撤出并在配置允许时 pull/apply。勿在 initiator 活跃时对同一库做 DB→内核 apply。</p>
<p class="err" id="robotErr"></p>
</section>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"function escapeHtml(s){return String(s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
async function jpost(url,body){
  const r=await fetch(url,{method:'POST',headers:{'Content-Type':'application/json'},credentials:'include',body:JSON.stringify(body)});
  const t=await r.text();let j;try{j=JSON.parse(t);}catch{j={raw:t};}return{r,j};
}
function onlineLibs(j){return (j.libraries||[]).filter(l=>!l.is_offline_storage&&l.name);}
document.getElementById('btn-logout').onclick=async()=>{await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';};
async function loadLibs(){
  const r=await fetch('/api/libraries',{credentials:'include'});
  const j=await r.json();
  const sel=document.getElementById('alib');
  sel.innerHTML='';
  if(!r.ok){document.getElementById('st').textContent=j.error||r.status;return;}
  onlineLibs(j).forEach(l=>{const o=document.createElement('option');o.value=l.name;o.textContent=l.name;sel.appendChild(o);});
  const qlib=new URLSearchParams(location.search).get('library');
  if(qlib&&[...sel.options].some(o=>o.value===qlib)) sel.value=qlib;
  sel.onchange=refresh;
  if(sel.options.length) await refresh();
}
async function refresh(){
  const lib=document.getElementById('alib').value;
  if(lib){
    const u=new URL(location.href);
    u.searchParams.set('library',lib);
    history.replaceState({},'',u);
  }
  if(!lib){document.getElementById('st').textContent='无在线库';return;}
  const r=await fetch('/api/library/detail?library='+encodeURIComponent(lib),{credentials:'include'});
  const j=await r.json();
  if(!r.ok){document.getElementById('st').textContent=j.error||r.status;return;}
  const L=j.library||{};
  const C=j.changer||{};
  const src=C.source||L.inventory_source||'db';
  let s='机械手状态来源: '+src+' (kernel=与 mtx/备份软件一致)\n';
  s+='驱动器 '+L.loaded_in_drives+'/'+L.drive_count+' 已装带\n';
  (C.drives||j.drives||[]).forEach(d=>{
    const lab=d.label||(d.drive_id!=null?'drive'+d.drive_id:'drive');
    const tn=d.tape_name; const bc=d.tape_barcode||d.barcode||'';
    s+='  '+lab+': '+(tn?tn+' ['+bc+']':'(空)')+'\n';
  });
  s+='\n数据槽位:\n';
  (C.data_slots||[]).forEach(r=>{
    s+='  '+r.label+': '+(r.tape_name?r.tape_name+' ['+(r.barcode||'')+']':'(空)')+'\n';
  });
  if(!(C.data_slots||[]).length){
    s+='  (无 changer 明细；见磁带页或 vtladm inventory)\n';
  }
  document.getElementById('st').textContent=s;
}
document.getElementById('reload').onclick=refresh;
function libBody(extra){return Object.assign({library:document.getElementById('alib').value},extra);}
async function doReconcile(apply,pull){
  document.getElementById('robotErr').textContent='';
  const {r,j}=await jpost('/api/manage/robot/reconcile',libBody({apply,pull}));
  if(!r.ok){document.getElementById('robotErr').textContent=j.error||r.status;return;}
  let msg='漂移 '+j.drift_count+' 项';
  if(j.inventory_truncated) msg+='（inventory 截断，结果可能不全）';
  if(apply) msg+='，已修复 '+j.fixes_applied;
  if(pull) msg+='，已写回 DB '+j.pull_updates;
  document.getElementById('robotErr').textContent=msg;
  await refresh();
}
document.getElementById('btnRecon').onclick=()=>doReconcile(false,false);
document.getElementById('btnReconPull').onclick=()=>doReconcile(false,true);
document.getElementById('btnAutoAlign').onclick=async()=>{
  document.getElementById('robotErr').textContent='';
  const {r,j}=await jpost('/api/manage/robot/auto-align',libBody({}));
  if(!r.ok){document.getElementById('robotErr').textContent=j.error||r.status;return;}
  let msg='evac='+j.evacuated+' apply='+j.fixes_applied+' pull='+j.pull_updates;
  if(j.drifts_remaining) msg+='；仍剩漂移 '+j.drifts_remaining;
  document.getElementById('robotErr').textContent=msg;
  await refresh();
};
loadLibs();
</script></body></html>
"#
);

const ADMIN_ASSIGN_SLOT_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — 磁带入槽</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
#slotCount{font-weight:600;color:var(--accent);}
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_admin_side_inner.html"),
    r#"</aside>
<main class="adm-main adm-workspace">
<div class="breadcrumb">存储 · 虚拟磁带库 <b>›</b> <b>磁带入槽</b></div>
<div class="topbar"><h1 style="margin:0;font-size:1.2rem;">磁带入槽</h1><button type="button" id="btn-logout">登出</button></div>
<p class="hint">批量入槽会更新 SQLite 目录并通过 <code>/dev/vtl</code> ioctl 写入内核（与 mhVTL 的 <code>vtlcmd</code> 同类）；库状态/inventory 显示仍以内核为准。列出离线货架与在线库货架上、未在槽位内的磁带；提交按槽位号与磁带名升序配对。</p>
<section class="panel"><h2>批量分配到槽位</h2>
<label>在线库</label><select id="alib"></select>
<p class="hint" style="margin-top:.25rem;">当前库可用空数据槽：<span id="slotCount">—</span></p>
<div class="row-actions"><button type="button" id="sat">全选磁带</button><button type="button" id="sst">全选槽位</button><button type="button" id="clr">清除勾选</button></div>
<h3 style="font-size:.92rem;margin:.6rem 0 .35rem;">磁带</h3>
<div id="tapeWrap"></div>
<h3 style="font-size:.92rem;margin:.6rem 0 .35rem;">空槽位</h3>
<div id="slotWrap"></div>
<button type="button" id="bassign">批量入槽</button><p class="err" id="ae"></p>
</section>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"function escapeHtml(s){return String(s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
function slotCheckboxValue(s){
  if(typeof s==='number'&&Number.isFinite(s))return String(Math.trunc(s));
  var t=String(s).trim();
  if(/^\d+$/.test(t))return t;
  return '0';
}
async function jpost(url,body){
  const r=await fetch(url,{method:'POST',headers:{'Content-Type':'application/json'},credentials:'include',body:JSON.stringify(body)});
  const t=await r.text();let j;try{j=JSON.parse(t);}catch{j={raw:t};}return{r,j};
}
function onlineLibs(j){return (j.libraries||[]).filter(l=>!l.is_offline_storage&&l.name);}
function assignableTapes(list){return (list||[]).filter(t=>!t.in_drive && t.slot==null);}
document.getElementById('btn-logout').onclick=async()=>{await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';};
let lastTapes=[];let lastSlots=[];
const OFFLINE_LIB='__offline__';
async function loadLibs(){
  const r=await fetch('/api/libraries',{credentials:'include'});
  const j=await r.json();
  const sel=document.getElementById('alib');
  sel.innerHTML='';
  if(!r.ok){document.getElementById('ae').textContent=j.error||r.status;return;}
  onlineLibs(j).forEach(l=>{const o=document.createElement('option');o.value=l.name;o.textContent=l.name+' (#'+l.id+')';sel.appendChild(o);});
  const qlib=new URLSearchParams(location.search).get('library');
  if(qlib&&[...sel.options].some(o=>o.value===qlib)) sel.value=qlib;
  sel.onchange=loadDeps;
  if(sel.options.length) await loadDeps();
}
function renderTables(){
  const tw=document.getElementById('tapeWrap');
  const sw=document.getElementById('slotWrap');
  let th='<table class="data-table"><thead><tr><th style="width:2.5rem"></th><th>名称</th><th>货架 / 来源</th></tr></thead><tbody>';
  lastTapes.forEach(t=>{
    const sn=t.shelf_name?escapeHtml(t.shelf_name):'—';
    const src=t.from_offline?('离线 / '+sn):(sn+'（在线库货架）');
    const d=t.from_offline?'1':'0';
    th+='<tr><td><input type="checkbox" class="ct" data-offline="'+d+'" value="'+encodeURIComponent(t.name)+'"/></td><td>'+escapeHtml(t.name)+'</td><td>'+src+'</td></tr>';
  });
  th+='</tbody></table>';tw.innerHTML=th;
  let sh='<table class="data-table"><thead><tr><th style="width:2.5rem"></th><th>槽位号</th></tr></thead><tbody>';
  lastSlots.forEach(s=>{var disp=escapeHtml(String(s));var val=slotCheckboxValue(s);sh+='<tr><td><input type="checkbox" class="cs" value="'+val+'"/></td><td>'+disp+'</td></tr>';});
  sh+='</tbody></table>';sw.innerHTML=sh;
}
async function loadDeps(){
  document.getElementById('ae').textContent='';
  const lib=document.getElementById('alib').value;
  const [tr,er,orf]=await Promise.all([
    fetch('/api/tapes?library='+encodeURIComponent(lib),{credentials:'include'}),
    fetch('/api/empty-slots?library='+encodeURIComponent(lib),{credentials:'include'}),
    fetch('/api/tapes?library='+encodeURIComponent(OFFLINE_LIB),{credentials:'include'})
  ]);
  const tj=await tr.json();
  const ej=await er.json();
  const oj=await orf.json();
  if(!tr.ok){document.getElementById('ae').textContent=tj.error||tr.status;return;}
  if(!er.ok){document.getElementById('ae').textContent=ej.error||er.status;return;}
  if(!orf.ok){document.getElementById('ae').textContent=oj.error||orf.status;return;}
  const onlineAssignable=assignableTapes(tj.tapes);
  const offlineAssignable=assignableTapes(oj.tapes||[]);
  const offlineRows=offlineAssignable.map(t=>(Object.assign({},t,{from_offline:true})));
  const onlineRows=onlineAssignable.map(t=>(Object.assign({},t,{from_offline:false})));
  lastTapes=[...offlineRows,...onlineRows].sort((a,b)=>a.name.localeCompare(b.name,'zh'));
  lastSlots=ej.empty_slots||[];
  document.getElementById('slotCount').textContent=String(ej.empty_slot_count!=null?ej.empty_slot_count:lastSlots.length);
  renderTables();
  let warn='';
  if(!lastTapes.length) warn='没有可入槽的磁带（离线货架与当前在线库货架均无待装磁带）。';
  if(!lastSlots.length) warn=(warn?warn+' ':'')+'没有空槽位。';
  document.getElementById('ae').textContent=warn;
}
document.getElementById('sat').onclick=()=>{document.querySelectorAll('.ct').forEach(x=>x.checked=true);};
document.getElementById('sst').onclick=()=>{document.querySelectorAll('.cs').forEach(x=>x.checked=true);};
document.getElementById('clr').onclick=()=>{document.querySelectorAll('.ct,.cs').forEach(x=>x.checked=false);};
document.getElementById('bassign').onclick=async()=>{
  document.getElementById('ae').textContent='';
  const lib=document.getElementById('alib').value;
  const tapes=[...document.querySelectorAll('.ct:checked')].map(x=>({
    tape: decodeURIComponent(x.value),
    from_offline:x.getAttribute('data-offline')==='1'
  })).sort((a,b)=>a.tape.localeCompare(b.tape,'zh'));
  const slots=[...document.querySelectorAll('.cs:checked')].map(x=>parseInt(x.value,10)).sort((a,b)=>a-b);
  if(!tapes.length||!slots.length){document.getElementById('ae').textContent='请勾选磁带与空槽位';return;}
  if(tapes.length!==slots.length){document.getElementById('ae').textContent='勾选磁带数与槽位数须相同（当前 '+tapes.length+' / '+slots.length+'）';return;}
  const maxSlots=parseInt(String(document.getElementById('slotCount').textContent),10);
  if(tapes.length>maxSlots){document.getElementById('ae').textContent='超出可用空槽 '+maxSlots;return;}
  const pairs=tapes.map((t,i)=>({tape:t.tape,slot:slots[i],from_offline:t.from_offline}));
  const {r,j}=await jpost('/api/manage/tape/assign-slot-batch',{library:lib,pairs:pairs});
  if(!r.ok){document.getElementById('ae').textContent=j.error||r.status;return;}
  showToast('已批量入槽');
  await loadDeps();
};
loadLibs();
</script></body></html>
"#
);

const ADMIN_SHELF_PLACE_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — 磁带出库</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_admin_side_inner.html"),
    r#"</aside>
<main class="adm-main adm-workspace">
<div class="breadcrumb">存储 · 虚拟磁带库 <b>›</b> <b>磁带出库</b></div>
<div class="topbar"><h1 style="margin:0;font-size:1.2rem;">磁带出库</h1><button type="button" id="btn-logout">登出</button></div>
<p class="hint">将所选磁带<strong>从在线库离库</strong>，移至下方<strong>离线货架</strong>（镜像文件会迁入离线区目录）。仅列出不在驱动中的磁带。</p>
<section class="panel"><h2>批量离库到离线货架</h2>
<label>来源在线库</label><select id="plib"></select>
<label>目标离线货架</label><select id="psh"></select>
<div class="row-actions"><button type="button" id="st">全选磁带</button><button type="button" id="clr">清除勾选</button></div>
<div id="tapeWrap"></div>
<button type="button" id="bplace">批量离库</button><p class="err" id="pe2"></p>
</section>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"function escapeHtml(s){return String(s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
async function jpost(url,body){
  const r=await fetch(url,{method:'POST',headers:{'Content-Type':'application/json'},credentials:'include',body:JSON.stringify(body)});
  const t=await r.text();let j;try{j=JSON.parse(t);}catch{j={raw:t};}return{r,j};
}
function onlineLibs(j){return (j.libraries||[]).filter(l=>!l.is_offline_storage&&l.name);}
let lastTapes=[];
document.getElementById('btn-logout').onclick=async()=>{await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';};
async function loadSh(){
  const r=await fetch('/api/offline-shelves',{credentials:'include'});
  const j=await r.json();
  const sh=document.getElementById('psh');
  sh.innerHTML='';
  if(!r.ok){document.getElementById('pe2').textContent=j.error||r.status;return;}
  if(!(j.shelves||[]).length){document.getElementById('pe2').textContent='请先在「新建货架」页创建离线货架。';return;}
  (j.shelves||[]).forEach(s=>{const o=document.createElement('option');o.value=s.name;o.textContent=s.name;sh.appendChild(o);});
}
async function loadLibs(){
  const r=await fetch('/api/libraries',{credentials:'include'});
  const j=await r.json();
  const sel=document.getElementById('plib');
  sel.innerHTML='';
  if(!r.ok){document.getElementById('pe2').textContent=j.error||r.status;return;}
  onlineLibs(j).forEach(l=>{const o=document.createElement('option');o.value=l.name;o.textContent=l.name+' (#'+l.id+')';sel.appendChild(o);});
  const qlib=new URLSearchParams(location.search).get('library');
  if(qlib&&[...sel.options].some(o=>o.value===qlib)) sel.value=qlib;
  sel.onchange=loadDeps;
  await loadSh();
  if(sel.options.length) await loadDeps();
}
function renderTapes(){
  const tw=document.getElementById('tapeWrap');
  let h='<table class="data-table"><thead><tr><th style="width:2.5rem"></th><th>磁带</th><th>位置</th></tr></thead><tbody>';
  lastTapes.forEach(t=>{
    const loc=t.slot!=null?'槽位 '+t.slot:(t.shelf_name?'货架：'+escapeHtml(t.shelf_name):'货架');
    h+='<tr><td><input type="checkbox" class="cp" value="'+encodeURIComponent(t.name)+'"/></td><td>'+escapeHtml(t.name)+'</td><td>'+loc+'</td></tr>';
  });
  h+='</tbody></table>';tw.innerHTML=h;
}
async function loadDeps(){
  document.getElementById('pe2').textContent='';
  const lib=document.getElementById('plib').value;
  const tr=await fetch('/api/tapes?library='+encodeURIComponent(lib),{credentials:'include'});
  const tj=await tr.json();
  if(!tr.ok){document.getElementById('pe2').textContent=tj.error||tr.status;return;}
  lastTapes=(tj.tapes||[]).filter(t=>!t.in_drive);
  renderTapes();
  if(!lastTapes.length) document.getElementById('pe2').textContent='当前库没有可离库的磁带（均在驱动中或库为空）。';
}
document.getElementById('st').onclick=()=>{document.querySelectorAll('.cp').forEach(x=>x.checked=true);};
document.getElementById('clr').onclick=()=>{document.querySelectorAll('.cp').forEach(x=>x.checked=false);};
document.getElementById('bplace').onclick=async()=>{
  document.getElementById('pe2').textContent='';
  const shelf=document.getElementById('psh').value;
  if(!shelf){document.getElementById('pe2').textContent='请选择离线货架';return;}
  const tapes=[...document.querySelectorAll('.cp:checked')].map(x=>decodeURIComponent(x.value));
  if(!tapes.length){document.getElementById('pe2').textContent='请勾选磁带';return;}
  const {r,j}=await jpost('/api/manage/tape/shelf-place-batch',{library:document.getElementById('plib').value,tapes:tapes,shelf:shelf});
  if(!r.ok){document.getElementById('pe2').textContent=j.error||r.status;return;}
  showToast('已离库到离线货架');
  await loadDeps();
};
loadLibs();
</script></body></html>
"#
);

const ADMIN_ISCSI_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VTL — iSCSI / LUN 映射</title>
<style>
"#,
    include_str!("web_shell.css"),
    r#"
main.adm-main{max-width:56rem;}
.data-table select{min-width:4.5rem;}
</style>
</head>
<body><div class="app">
<aside class="adm-side">"#,
    include_str!("web_admin_side_inner.html"),
    r#"</aside>
<main class="adm-main adm-workspace">
<div class="breadcrumb">存储功能 <b>›</b> 虚拟磁带库 <b>›</b> <b>iSCSI / LUN 映射</b></div>
<div class="topbar"><div><h1 style="margin:0;font-size:1.2rem;">library-export（pscsi 多 LUN）</h1>
<p class="hint" style="margin:0.4rem 0 0;">将本机内核 <code>vtl</code> 的 <code>/dev/sg*</code> 经 LIO <strong>pscsi</strong> 导出为 iSCSI 带库（机械手 + 磁带机）。IQN 与 LIO 后端名前缀默认自动生成；下方表格可调整各 LUN 号。须先 <code>insmod vtl</code> 且 <code>lsscsi -g</code> 能看到设备。</p></div><button type="button" id="btn-logout">登出</button></div>
<section class="panel"><h2>环境与权限</h2>
<p class="hint" style="border-left:3px solid #c0392b;padding-left:.6rem;margin-bottom:.75rem"><strong>内核与主机风险：</strong>改库几何默认走 <code>/dev/vtl</code> ioctl（不卸模块）。<code>library-export</code>（LIO pscsi）与可选的 <code>kernel_vtl_reload_script</code>（<code>rmmod</code>/<code>insmod</code>，默认关闭）会触及内核；在 <code>/dev/st*</code>、<code>/dev/sg*</code> 仍被占用时 <strong>rmmod</strong> 可能导致 <strong>整机重启</strong>（麒麟 4.19 实测）。请先 <strong>dry-run</strong>、停备份；详见 <code>userspace/docs/SCSI.md</code> §1c。</p>
<p class="hint"><strong>加载配置</strong>与<strong>检测环境</strong>不修改 LIO。<code>library-export</code> / <code>library-unexport</code> 非 dry-run 须在下方<strong>开启并保存</strong>「允许执行」。成功导出会<strong>写入数据库</strong>，下次打开本页自动回填；卸除可按<strong>当前库名</strong>一键执行。门户默认取自 <code>vtl.conf</code> 的 <code>iscsi_portals</code> 首项（<strong>不支持 IPv6</strong> 字面量）。</p>
<p>
<label><input id="iallow" type="checkbox"/> 允许本页执行 <code>vtladm-iscsi</code>（非 dry-run）</label>
<button type="button" id="btn-iallow-save">保存开关</button>
<span id="iallow-msg" class="hint" style="margin-left:0.5rem;"></span>
</p>
<div id="icfg-bar" class="hint" style="white-space:pre-wrap;"></div>
<p><button type="button" id="btn-icfg">从 vtl.conf 加载推荐值</button>
<button type="button" id="btn-ichk">检测 targetcli 环境</button></p>
<label><input id="ichksudo" type="checkbox"/> 检测时使用 <code>--sudo</code></label>
<pre id="io0" style="max-height:10rem;overflow:auto;"></pre>
</section>
<section class="panel"><h2>LUN 映射与导出</h2>
<p class="hint" id="iscsi-limits-hint">产品上限：在线库最多 <strong>8</strong> 个；每库最多 <strong>8</strong> 台驱动器、<strong>256</strong> 个数据槽。导出与扫描<strong>仅使用当前库配置的驱动器台数</strong>（内核可能可见更多磁带 LUN，多余的不显示、不导出）。</p>
<p class="hint">选择<strong>在线库</strong>后点「加载默认」或「扫描 lsscsi」：均会生成 <strong>IQN</strong> 与 <strong>LIO 后端前缀</strong>，并填充 LUN 表（1 机械手 + N 台磁带机）。扫描在同一 SCSI host 上取前 N 个 <code>/dev/sg</code>（N = 库驱动器数）。</p>
<label>当前在线库</label><select id="iselib"></select>
<button type="button" id="brefreshlibs">刷新库列表</button>
<button type="button" id="bloaddef">加载默认 IQN / 门户 / LUN 表</button>
<button type="button" id="bscansg">扫描 lsscsi（VTL）</button>
<p class="err" id="iscsi-err"></p>
<p class="hint" id="iscsi-warn" style="display:none"></p>
<p class="hint" id="ilib-hint"></p>
<p class="hint" id="iexport-status" style="display:none"></p>
<label>IQN</label><input id="iiqn" style="max-width:100%"/>
<label>LIO 后端名前缀（自动生成，可改）</label><input id="iexpid" style="max-width:100%"/>
<p class="hint" style="margin-top:0">实际 pscsi 对象名为 <code id="iback-preview">…</code></p>
<label>机械手 <code>/dev/sg</code></label><input id="ichsg" placeholder="/dev/sg3"/>
<div id="idrvwrap"><p class="muted" style="margin:.35rem 0">加载默认后将出现各磁带机 <code>sg</code> 输入框。</p></div>
<table class="data-table" style="margin-top:.75rem"><thead><tr><th>名称</th><th>类型</th><th>LUN</th></tr></thead><tbody id="lunmap-body"></tbody></table>
<label>门户 IP</label><input id="iip" value="0.0.0.0"/>
<label>门户端口</label><input id="ipt" type="number" value="3260"/>
<label><input id="idry" type="checkbox" checked/> 仅 dry-run（推荐先勾选查看 targetcli 脚本）</label><br/>
<label><input id="isudo" type="checkbox"/> 使用 <code>--sudo</code></label><br/>
<button type="button" id="bdoexp">执行 library-export</button>
<pre id="io1" style="max-height:16rem;overflow:auto;"></pre>
</section>
<section class="panel"><h2>解除映射（library-unexport）</h2>
<p class="hint">使用当前 IQN、后端前缀与上表 LUN 号删除 LIO 对象（请先 dry-run 核对）。</p>
<label><input id="udry" type="checkbox" checked/> 仅 dry-run</label>
<label><input id="usudo" type="checkbox"/> 使用 <code>--sudo</code></label><br/>
<button type="button" id="bunexp">按库一键 library-unexport</button>
<button type="button" id="bunexp-adv">高级 unexport（手工 IQN/前缀）</button>
<pre id="io2" style="max-height:12rem;overflow:auto;"></pre>
</section>
<p class="hint"><a href="/admin/library">返回磁带库</a></p>
</main></div>
<script>
"#,
    include_str!("web_boot.js"),
    r#"let VTL_LIMITS={max_online_libraries:8,max_drives_per_library:8,max_data_slots_per_library:256};
function applyProductLimitsFromApi(j){
  if(!j||!j.product_limits)return;
  const L=j.product_limits;
  VTL_LIMITS={max_online_libraries:Number(L.max_online_libraries)||8,max_drives_per_library:Number(L.max_drives_per_library)||8,max_data_slots_per_library:Number(L.max_data_slots_per_library)||256};
  const h=document.getElementById('iscsi-limits-hint');
  if(h){h.innerHTML='产品上限：在线库最多 <strong>'+VTL_LIMITS.max_online_libraries+'</strong> 个；每库最多 <strong>'+VTL_LIMITS.max_drives_per_library+'</strong> 台驱动器、<strong>'+VTL_LIMITS.max_data_slots_per_library+'</strong> 个数据槽。导出与扫描<strong>仅使用当前库配置的驱动器台数</strong>（内核可能可见更多磁带 LUN，多余的不显示、不导出）。';}
}
function bindClick(id,fn){
  const el=document.getElementById(id);
  if(el)el.onclick=fn;
  else console.error('missing #'+id);
}
function showExportOut(obj){
  const t=typeof obj==='string'?obj:JSON.stringify(obj,null,2);
  const io1=document.getElementById('io1');
  if(io1){io1.textContent=t;io1.scrollIntoView({behavior:'smooth',block:'nearest'});}
}
function parsePortalPort(){
  const raw=(document.getElementById('ipt').value||'').trim();
  const port=parseInt(raw,10);
  if(!Number.isFinite(port)||port<1||port>65535)return {ok:false,error:'门户端口须在 1–65535'};
  return {ok:true,port:port};
}
function setExportButtonEnabled(enabled,reason){
  const expBtn=document.getElementById('bdoexp');
  const warnEl=document.getElementById('iscsi-warn');
  if(!expBtn)return;
  expBtn.disabled=!enabled;
  const msg=(typeof reason==='string'&&reason)?reason:(reason!=null?String(reason):'当前库无法执行 library-export');
  if(!enabled){
    expBtn.title=msg;
    if(warnEl){warnEl.textContent=msg;warnEl.style.display='';}
  }else{
    expBtn.title='';
    if(warnEl){
      const w=warnEl.textContent||'';
      if(/library-export|驱动器数为 0|无法执行/.test(w)){
        warnEl.textContent='';
        warnEl.style.display='none';
      }
    }
  }
}
async function jpost(url,body){
  const r=await fetch(url,{method:'POST',headers:{'Content-Type':'application/json'},credentials:'include',body:JSON.stringify(body)});
  const t=await r.text();let j;try{j=JSON.parse(t);}catch{j={raw:t};}return{r,j};
}
function onlineLibs(j){
  return (j.libraries||[]).filter(l=>{
    const n=l.name||'';
    if(n==='__offline__')return false;
    if(l.is_offline_storage===true)return false;
    return !!n;
  });
}
function libFromPage(){
  const v=document.getElementById('iselib').value;
  if(v)return v;
  return new URLSearchParams(location.search).get('library')||'';
}
function ensureLibOption(name){
  const sel=document.getElementById('iselib');
  if(!name||[...sel.options].some(o=>o.value===name))return;
  const o=document.createElement('option');
  o.value=name;o.textContent=name;sel.appendChild(o);
}
function escapeHtml(s){return String(s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
function lunSelHtml(val){
  let h='<select class="lsel">';
  for(let i=0;i<=255;i++)h+='<option value="'+i+'"'+(i===val?' selected':'')+'>'+i+'</option>';
  h+='</select>';return h;
}
let lastDef=null;
function setBackPreview(){
  const p=document.getElementById('iexpid').value.trim();
  const dc=lastDef&&lastDef.drive_count!=null?parseInt(lastDef.drive_count,10):0;
  const drs=[];
  for(let i=0;i<dc;i++)drs.push(p+'_dr'+i);
  document.getElementById('iback-preview').textContent=p?((p+'_ch')+(drs.length?', '+drs.join(', '):'')):'（前缀）_ch、（前缀）_dr0 …';
}
function buildLunRows(lib, lunMap){
  const tb=document.getElementById('lunmap-body');
  tb.innerHTML='';
  const rows=[];
  rows.push({name:lib||'—',type:'介质变换器',lun:lunMap[0]||0});
  for(let i=1;i<lunMap.length;i++)rows.push({name:'磁带机 drive '+(i-1),type:'磁带驱动器',lun:lunMap[i]});
  rows.forEach((r,i)=>{
    const tr=document.createElement('tr');
    tr.innerHTML='<td>'+escapeHtml(r.name)+'</td><td>'+escapeHtml(r.type)+'</td><td>'+lunSelHtml(r.lun)+'</td>';
    tr.querySelector('.lsel').dataset.rowIdx=String(i);
    tb.appendChild(tr);
  });
}
function readLunMap(){
  return [...document.querySelectorAll('#lunmap-body .lsel')].map(s=>parseInt(s.value,10));
}
function buildDriveInputs(n){
  const w=document.getElementById('idrvwrap');
  w.innerHTML='';
  for(let i=0;i<n;i++){
    const lab=document.createElement('label');
    lab.textContent='磁带机 '+i+' /dev/sg';
    const inp=document.createElement('input');
    inp.id='idrv'+i;
    inp.placeholder='/dev/sg'+(4+i);
    inp.style.display='block';
    inp.style.maxWidth='100%';
    w.appendChild(lab);
    w.appendChild(inp);
  }
}
let iselibChangeBound=false;
async function loadLibs(){
  const errEl=document.getElementById('iscsi-err');
  errEl.textContent='';
  const sel=document.getElementById('iselib');
  let respOk=false,status=0,j={};
  try{
    const r=await fetch('/api/libraries',{credentials:'include'});
    status=r.status;
    respOk=r.ok;
    const t=await r.text();
    try{j=JSON.parse(t);}catch{
      errEl.textContent='库列表接口返回非 JSON'+(status===401?'（请先登录）':'');
      return false;
    }
  }catch(e){
    errEl.textContent='加载库列表失败：'+e;
    return false;
  }
  sel.innerHTML='';
  if(!respOk){
    errEl.textContent=j.error||(j.code==='setup_required'?'请先完成初始化配置（/admin/setup-init）':String(status));
    return false;
  }
  applyProductLimitsFromApi(j);
  const all=j.libraries||[];
  const libs=onlineLibs(j);
  libs.forEach(l=>{const o=document.createElement('option');o.value=l.name;o.textContent=l.name;sel.appendChild(o);});
  const q=new URLSearchParams(location.search).get('library');
  if(q){
    const names=new Set((j.libraries||[]).map(l=>l.name));
    if(names.has(q))ensureLibOption(q);
    if([...sel.options].some(o=>o.value===q))sel.value=q;
  }
  if(!sel.value&&sel.options.length)sel.value=sel.options[0].value;
  if(!libs.length){
    const db=j.db_path?(' 数据库：'+j.db_path):'';
    if(j.hint){
      errEl.innerHTML=escapeHtml(String(j.hint))+db+'. <a href="/admin/library">打开磁带库</a>';
    }else if(all.length){
      errEl.innerHTML='仅有离线保留库，不能用于 iSCSI。'+escapeHtml(db)+' <a href="/admin/library">打开磁带库</a> 创建在线库。';
    }else{
      const scsi=j.vtl_scsi_lines>0?'（lsscsi 已见 '+j.vtl_scsi_lines+' 行 VTL，但库未写入 DB）':'（未检测到 VTL SCSI）';
      errEl.innerHTML='暂无在线磁带库'+scsi+'。'+escapeHtml(db)+' 请先在 <a href="/admin/library">磁带库</a> 创建（如 marstor），再点「刷新库列表」。';
    }
    return false;
  }
  if(!iselibChangeBound){
    sel.addEventListener('change',()=>loadDefaults(false));
    iselibChangeBound=true;
  }
  return true;
}
async function loadDefaults(forceNew, noAutoRetry){
  document.getElementById('iscsi-err').textContent='';
  const warnEl=document.getElementById('iscsi-warn');
  warnEl.textContent='';
  warnEl.style.display='none';
  const lib=libFromPage();
  if(!lib){document.getElementById('iscsi-err').textContent='请选择在线库，或从磁带库页点「LUN映射」跳转';return;}
  ensureLibOption(lib);
  document.getElementById('iselib').value=lib;
  let url='/api/manage/iscsi/library-export-defaults?library='+encodeURIComponent(lib);
  if(forceNew)url+='&regenerate=1';
  const r=await fetch(url,{credentials:'include'});
  const j=await r.json();
  if(!r.ok){document.getElementById('iscsi-err').textContent=j.error||r.status;return;}
  lastDef=j;
  if(forceNew){
    warnEl.style.display='';
    warnEl.textContent='已生成新 IQN/前缀（未写入数据库）。若库内仍有导出记录，一键 unexport 仍使用旧参数；更换 IQN 前请先按库 unexport。';
  }
  document.getElementById('iiqn').value=j.iqn||'';
  document.getElementById('iexpid').value=j.export_id||'';
  document.getElementById('iip').value=j.portal_ip||'0.0.0.0';
  document.getElementById('ipt').value=j.portal_port!=null?j.portal_port:3260;
  const dc=parseInt(j.drive_count,10)||0;
  applyProductLimitsFromApi(j);
  document.getElementById('ilib-hint').textContent='库：'+escapeHtml(lib)+'，驱动器数：'+dc+'（仅显示/导出前 '+dc+' 台；上限 '+VTL_LIMITS.max_drives_per_library+'）';
  buildDriveInputs(dc);
  const lunMap=(j.default_lun_map||[]).slice(0,dc+1);
  buildLunRows(lib,lunMap.length?lunMap:(()=>{const a=[0];for(let i=1;i<=dc;i++)a.push(i);return a;})());
  if(j.changer_sg)document.getElementById('ichsg').value=j.changer_sg;
  const savedDrives=(j.drive_sg||[]).slice(0,dc);
  for(let i=0;i<Math.min(dc,savedDrives.length);i++){
    const el=document.getElementById('idrv'+i);
    if(el)el.value=savedDrives[i];
  }
  const st=document.getElementById('iexport-status');
  if(st){
    if(j.has_saved_export){
      st.style.display='';
      st.textContent='已保存导出记录'+(j.exported_at?('（'+j.exported_at+'）'):'')+(j.saved_drive_mismatch?'；驱动器台数已变，建议重新扫描 sg 后再导出':'');
    }else{
      st.style.display='none';
      st.textContent='';
    }
  }
  if(!noAutoRetry&&(!(document.getElementById('iiqn').value||'').trim()||!(document.getElementById('iexpid').value||'').trim())){
    await loadDefaults(true,true);
  }
  setBackPreview();
  if(j.can_export===false){
    setExportButtonEnabled(false,j.export_blocked_reason);
  }else{
    setExportButtonEnabled(true,null);
  }
}
async function scanSg(){
  document.getElementById('iscsi-err').textContent='';
  const lib=libFromPage();
  if(!lib){document.getElementById('iscsi-err').textContent='请先选择在线库';return;}
  ensureLibOption(lib);
  document.getElementById('iselib').value=lib;
  const r=await fetch('/api/manage/transport/scan-sg?library='+encodeURIComponent(lib)+'&transport=iscsi',{credentials:'include'});
  const j=await r.json();
  if(!r.ok){document.getElementById('iscsi-err').textContent=j.error||r.status;return;}
  applyProductLimitsFromApi(j);
  const dc=parseInt(j.drive_count,10)||0;
  if(!dc){document.getElementById('iscsi-err').textContent='当前库驱动器数为 0，请先在磁带库页建库';return;}
  await loadDefaults(false);
  if(j.changer_sg)document.getElementById('ichsg').value=j.changer_sg;
  const drives=(j.drive_sg||[]).slice(0,dc);
  buildDriveInputs(dc);
  for(let i=0;i<drives.length;i++){
    const el=document.getElementById('idrv'+i);
    if(el)el.value=drives[i];
  }
  const lunMap=[];for(let k=0;k<=dc;k++)lunMap.push(k);
  buildLunRows(lib,lunMap);
  setBackPreview();
  document.getElementById('ilib-hint').textContent='库：'+escapeHtml(lib)+'，驱动器数：'+dc+'（扫描已取前 '+drives.length+' 台 /dev/sg；已加载 IQN/LIO 前缀）';
  document.getElementById('io0').textContent=JSON.stringify(j,null,2);
}
async function loadIscsiCfg(){
  const r=await fetch('/api/manage/iscsi/config',{credentials:'include'});
  const t=await r.text();let j;try{j=JSON.parse(t);}catch{j={raw:t};}
  const bar=document.getElementById('icfg-bar');
  if(!r.ok){bar.textContent=j.error||('HTTP '+r.status);return;}
  bar.textContent='tape_dir: '+j.tape_dir+'\ntransport: '+j.transport+'\nvtladm-iscsi: '+j.vtladm_iscsi_path+'\nallow_iscsi_exec: '+(j.allow_iscsi_exec===true);
  document.getElementById('iallow').checked=(j.allow_iscsi_exec===true);
  document.getElementById('iallow-msg').textContent='';
  if(j.iscsi_portals)bar.textContent+='\niscsi_portals: '+j.iscsi_portals;
  if(j.non_unix_build)bar.textContent+='\n（当前为非 Unix 构建：请在 Linux target 上运行。）';
  const hasSaved=lastDef&&lastDef.has_saved_export===true;
  if(j.portal_ip_suggested&&!hasSaved)document.getElementById('iip').value=j.portal_ip_suggested;
  if(j.portal_port_suggested!=null&&!hasSaved)document.getElementById('ipt').value=j.portal_port_suggested;
}
async function doUnexport(byLibrary){
  document.getElementById('io2').textContent='…';
  const lib=libFromPage();
  const body={
    dry_run:document.getElementById('udry').checked,
    sudo:document.getElementById('usudo').checked
  };
  if(byLibrary){
    if(!lib){document.getElementById('io2').textContent=JSON.stringify({error:'请选择在线库'},null,2);return;}
    ensureLibOption(lib);
    document.getElementById('iselib').value=lib;
    body.library=lib;
  }else{
    body.iqn=document.getElementById('iiqn').value.trim();
    body.export_id=document.getElementById('iexpid').value.trim();
    const lunMap=readLunMap();
    if(lunMap.length)body.lun_map=lunMap;
    else if(lastDef&&lastDef.drive_count!=null)body.drives=parseInt(lastDef.drive_count,10);
  }
  const {r,j}=await jpost('/api/manage/iscsi/library-unexport',body);
  document.getElementById('io2').textContent=JSON.stringify(j,null,2);
  if(r.ok&&j.ok&&!body.dry_run)await loadDefaults(false);
}
bindClick('btn-logout',async()=>{await fetch('/api/logout',{method:'POST',credentials:'include'});location.href='/login';});
const iexpidEl=document.getElementById('iexpid');
if(iexpidEl)iexpidEl.oninput=setBackPreview;
bindClick('btn-icfg',loadIscsiCfg);
bindClick('btn-iallow-save',async()=>{
  document.getElementById('iallow-msg').textContent='…';
  const allow=document.getElementById('iallow').checked;
  const {r,j}=await jpost('/api/manage/iscsi/allow-exec',{allow:allow});
  if(!r.ok){document.getElementById('iallow-msg').textContent=j.error||('HTTP '+r.status);return;}
  document.getElementById('iallow-msg').textContent='已保存';
  await loadIscsiCfg();
});
bindClick('btn-ichk',async()=>{
  document.getElementById('io0').textContent='…';
  const {r,j}=await jpost('/api/manage/iscsi/check',{sudo:document.getElementById('ichksudo').checked});
  document.getElementById('io0').textContent=JSON.stringify(j,null,2);
});
bindClick('bloaddef',()=>loadDefaults(true));
bindClick('bscansg',scanSg);
bindClick('bunexp',()=>doUnexport(true));
bindClick('bunexp-adv',()=>doUnexport(false));
bindClick('brefreshlibs',async()=>{
  if(await loadLibs()){
    const lib=libFromPage();
    if(lib)ensureLibOption(lib);
    if(lib)await loadDefaults(false);
  }
});
bindClick('bdoexp',async()=>{
  const errEl=document.getElementById('iscsi-err');
  errEl.textContent='';
  const expBtn=document.getElementById('bdoexp');
  if(expBtn&&expBtn.disabled){
    const msg=expBtn.title||'当前库无法执行 library-export';
    showExportOut({error:msg});
    errEl.textContent=msg;
    return;
  }
  try{
    showExportOut('…');
    const lib=libFromPage();
    if(!lib){showExportOut({error:'请选择在线库'});return;}
    ensureLibOption(lib);
    document.getElementById('iselib').value=lib;
    const iqn=(document.getElementById('iiqn').value||'').trim();
    const expid=(document.getElementById('iexpid').value||'').trim();
    if(!iqn||!expid){
      showExportOut({error:'请先「加载默认」或「扫描 lsscsi」以生成 IQN 与 LIO 前缀'});
      return;
    }
    const inputs=[...document.querySelectorAll('#idrvwrap input')];
    const drives=inputs.map(i=>i.value.trim()).filter(Boolean);
    if(inputs.length&&drives.length!==inputs.length){
      showExportOut({error:'请为每个磁带机填写 /dev/sg 路径'});
      return;
    }
    const lunMap=readLunMap();
    if(lunMap.length!==1+drives.length){
      showExportOut({error:'LUN 行数须等于 1+磁带机数；请先「加载默认」'});
      return;
    }
    const pp=parsePortalPort();
    if(!pp.ok){showExportOut({error:pp.error});errEl.textContent=pp.error;return;}
    const {r,j}=await jpost('/api/manage/iscsi/library-export',{
      library:lib,
      iqn:iqn,
      export_id:expid,
      changer_sg:document.getElementById('ichsg').value.trim(),
      drive_sg:drives,
      lun_map:lunMap,
      portal_ip:document.getElementById('iip').value.trim(),
      portal_port:pp.port,
      dry_run:document.getElementById('idry').checked,
      sudo:document.getElementById('isudo').checked
    });
    showExportOut(j);
    if(!r.ok)errEl.textContent=(j&&j.error)?String(j.error):('HTTP '+r.status);
    if(r.ok&&j.ok&&!document.getElementById('idry').checked)await loadDefaults(false);
  }catch(e){
    showExportOut({error:String(e)});
    errEl.textContent=String(e);
  }
});
(async()=>{
  await loadLibs();
  await loadIscsiCfg();
  const lib=libFromPage();
  if(lib){
    ensureLibOption(lib);
    document.getElementById('iselib').value=lib;
    await loadDefaults(false);
  }
})();
</script></body></html>
"#
);

#[cfg(test)]
mod iscsi_validate_tests {
    use super::{
        iscsi_library_exec_hint, validate_iqn, validate_iscsi_portal_host,
        validate_lun_map_consecutive_from_zero, validate_lun_map_values,
    };

    #[test]
    fn portal_host_rejects_ipv6_and_shellish() {
        assert!(validate_iscsi_portal_host("192.168.1.1").is_ok());
        assert!(validate_iscsi_portal_host("host-1.example").is_ok());
        assert!(validate_iscsi_portal_host("::1").is_err());
        assert!(validate_iscsi_portal_host("evil;rm").is_err());
    }

    #[test]
    fn iqn_rejects_shell_meta() {
        assert!(validate_iqn("iqn.2026-05.org.example:vtl1").is_ok());
        assert!(validate_iqn("iqn.2026-05.org.example:`x`").is_err());
    }

    #[test]
    fn iqn_rejects_underscore_for_lio_rtslib() {
        assert!(validate_iqn("iqn.2026-05.com.marstor:marstor_20260515073437").is_err());
        assert!(validate_iqn("iqn.2026-05.com.marstor:marstor-20260515073437").is_ok());
    }

    #[test]
    fn iscsi_default_iqn_suffix_uses_hyphen_not_underscore() {
        let (iqn, _eid) = super::iscsi_default_iqn_and_export_id("default");
        assert!(
            !iqn.contains('_'),
            "IQN must not contain underscore for LIO: {}",
            iqn
        );
        assert!(iqn.contains(":default-"), "{}", iqn);
    }

    #[test]
    fn lun_map_cap() {
        assert!(validate_lun_map_values(&[0, 255]).is_ok());
        assert!(validate_lun_map_values(&[256]).is_err());
    }

    #[test]
    fn lun_map_export_requires_consecutive_from_zero() {
        assert!(validate_lun_map_consecutive_from_zero(&[0, 1, 2]).is_ok());
        assert!(validate_lun_map_consecutive_from_zero(&[0]).is_ok());
        assert!(validate_lun_map_consecutive_from_zero(&[3, 4, 5]).is_err());
    }

    #[test]
    fn iscsi_library_exec_hint_unexport_ok_ignores_typical_targetcli_stderr() {
        let noise =
            "No such path /iscsi/iqn.2026-05.com.example:x\nNo storage object named id_ch.\n";
        assert!(iscsi_library_exec_hint(false, true, noise, false).is_none());
    }

    #[test]
    fn iscsi_library_exec_hint_export_ok_still_warns_on_stderr_keywords() {
        let noise = "No such path /iscsi/iqn.2026-05.com.example:x\n";
        assert!(iscsi_library_exec_hint(false, true, noise, true).is_some());
    }

    #[test]
    fn iscsi_library_exec_hint_unexport_not_ok_still_shows_failure_hint() {
        assert!(iscsi_library_exec_hint(false, false, "any", false).is_some());
    }

    #[test]
    fn iscsi_library_exec_hint_not_ok_sg_in_use_gives_specific_hint() {
        let stderr = "Cannot configure StorageObject because device /dev/sg4 (SCSI 34:0:0:0) is already in use\n";
        let h = iscsi_library_exec_hint(false, false, stderr, true).expect("hint");
        let s = h.as_str().expect("string hint");
        assert!(s.contains("already in use"), "{}", s);
        assert!(s.contains("library-unexport"), "{}", s);
    }

    #[test]
    fn iscsi_library_exec_hint_wwn_not_valid_gives_iqn_hint() {
        let stderr =
            "WWN not valid as: iqn, naa, eui\nNo such path /iscsi/iqn.2026-05.com.marstor:x\n";
        let h = iscsi_library_exec_hint(false, false, stderr, true).expect("hint");
        let s = h.as_str().expect("string hint");
        assert!(s.contains("WWN") || s.contains("下划线"), "{}", s);
    }
}

#[cfg(test)]
mod web_html_tests {
    use super::build_web_router;
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};
    use axum::Router;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn tmp_web_auth_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "vtladm_web_http_{}_{}_{}",
            label,
            std::process::id(),
            rand::random::<u32>()
        ))
    }

    #[tokio::test]
    async fn test_web_http_get_login_returns_html() {
        let p = tmp_web_auth_path("login");
        let _ = std::fs::remove_file(&p);
        let auth = Arc::new(crate::web_auth::WebState::new(p.clone()));
        auth.init_auth_file().unwrap();
        let app = build_web_router(auth);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("text/html"),
            "unexpected content-type: {:?}",
            ct
        );
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn test_web_http_get_root_redirects_to_login_without_session() {
        let p = tmp_web_auth_path("root302");
        let _ = std::fs::remove_file(&p);
        let auth = Arc::new(crate::web_auth::WebState::new(p.clone()));
        auth.init_auth_file().unwrap();
        let app = build_web_router(auth);
        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            res.status().is_redirection(),
            "expected redirect got {}",
            res.status()
        );
        let loc = res
            .headers()
            .get(header::LOCATION)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        assert!(
            loc.contains("/login"),
            "expected Location .../login..., got {:?}",
            loc
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 与 `web_auth` 测试中 `new_captcha` 的 `a op b = ?` 格式一致。
    fn captcha_answer_from_question(question: &str) -> i32 {
        let parts: Vec<&str> = question.split_whitespace().collect();
        assert_eq!(parts.len(), 5, "expected `a op b = ?`, got {:?}", question);
        let a: i32 = parts[0].parse().expect("a");
        let op = parts[1];
        let b: i32 = parts[2].parse().expect("b");
        match op {
            "+" => a + b,
            "-" => a - b,
            "*" => a * b,
            _ => panic!("unexpected op {:?}", op),
        }
    }

    fn extract_vtl_session_cookie(res: &axum::response::Response) -> String {
        use axum::http::header::SET_COOKIE;
        const PREFIX: &str = "vtl_session=";
        for v in res.headers().get_all(SET_COOKIE) {
            if let Ok(s) = v.to_str() {
                if let Some(i) = s.find(PREFIX) {
                    let rest = &s[i + PREFIX.len()..];
                    let tok = rest.split(';').next().unwrap_or("").trim();
                    if !tok.is_empty() {
                        return format!("vtl_session={}", tok);
                    }
                }
            }
        }
        panic!(
            "no non-empty vtl_session cookie in {:?}",
            res.headers().get_all(SET_COOKIE)
        );
    }

    #[test]
    fn test_extract_vtl_session_cookie_accepts_prefix_before_name() {
        let res = axum::response::Response::builder()
            .header(
                header::SET_COOKIE,
                "Path=/; vtl_session=testtoken123; HttpOnly",
            )
            .body(Body::empty())
            .expect("build synthetic login response");
        assert_eq!(extract_vtl_session_cookie(&res), "vtl_session=testtoken123");
    }

    /// 验证码登录；`Router` 须按值传入。内部用 `clone().oneshot`（`oneshot` 消费 `Service`，不能对 `&mut Router` 调用）。
    ///
    /// **类型**：`build_web_router` 在 `with_state(auth)` 之后为 axum 0.7 的 **`Router`（即 `Router<()>`）**：
    /// `Router<S>` 中的 `S` 表示「仍缺的状态」；状态已注入后不再缺省，故实现 `Service` 与 `oneshot`。
    /// 勿写成 `Router<Arc<WebState>>`（该类型不实现 `Service`，无法用 `oneshot`）。
    async fn web_login_cookie(app: Router<()>) -> (String, Router<()>) {
        let cap_req = Request::builder()
            .uri("/api/captcha")
            .body(Body::empty())
            .expect("build GET /api/captcha request");
        let cap = app
            .clone()
            .oneshot(cap_req)
            .await
            .expect("GET /api/captcha oneshot");
        assert_eq!(cap.status(), StatusCode::OK);
        let cap_body = to_bytes(cap.into_body(), 64 * 1024)
            .await
            .expect("read GET /api/captcha body");
        let cap_j: serde_json::Value =
            serde_json::from_slice(&cap_body).expect("GET /api/captcha JSON body");
        let captcha_id = cap_j["captcha_id"]
            .as_str()
            .expect("captcha_id string")
            .to_string();
        let question = cap_j["question"].as_str().expect("captcha question string");
        let answer = captcha_answer_from_question(question);
        let login_body = serde_json::json!({
            "username": crate::web_auth::DEFAULT_WEB_USER,
            "password": crate::web_auth::DEFAULT_WEB_PASSWORD,
            "captcha_id": captcha_id,
            "captcha_answer": answer.to_string(),
        });
        let login_req = Request::builder()
            .method("POST")
            .uri("/api/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(login_body.to_string()))
            .expect("build POST /api/login request");
        let login = app
            .clone()
            .oneshot(login_req)
            .await
            .expect("POST /api/login oneshot");
        assert_eq!(login.status(), StatusCode::OK, "login failed");
        let cookie = extract_vtl_session_cookie(&login);
        (cookie, app)
    }

    #[tokio::test]
    async fn test_web_http_iscsi_config_401_without_session() {
        let p = tmp_web_auth_path("iscsi_cfg401");
        let _ = std::fs::remove_file(&p);
        let auth = Arc::new(crate::web_auth::WebState::new(p.clone()));
        auth.init_auth_file().unwrap();
        let app = build_web_router(auth);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/manage/iscsi/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn test_web_http_iscsi_config_json_with_session() {
        let p = tmp_web_auth_path("iscsi_cfg200");
        let _ = std::fs::remove_file(&p);
        let auth = Arc::new(crate::web_auth::WebState::new(p.clone()));
        auth.init_auth_file().unwrap();
        let app = build_web_router(auth);
        let (cookie, app) = web_login_cookie(app).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/manage/iscsi/config")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 256 * 1024).await.unwrap();
        let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(j.get("tape_dir").is_some());
        assert!(j.get("non_unix_build").is_some());
        assert!(
            j.get("vtladm_iscsi_linux_only").is_none(),
            "obsolete field vtladm_iscsi_linux_only"
        );
        assert_eq!(j["allow_iscsi_exec"], serde_json::json!(false));
        assert_eq!(j["kernel_geom_prefer_ioctl"], serde_json::json!(true));
        assert_eq!(j["kernel_reload_on_db_change"], serde_json::json!(false));
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn test_web_http_changer_load_401_without_session() {
        let p = tmp_web_auth_path("changer_load401");
        let _ = std::fs::remove_file(&p);
        let auth = Arc::new(crate::web_auth::WebState::new(p.clone()));
        auth.init_auth_file().unwrap();
        let app = build_web_router(auth);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/manage/tape/load")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"library":"lib1","slot":0,"drive":0}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn test_web_http_iscsi_allow_exec_401_without_session() {
        let p = tmp_web_auth_path("iscsi_allow401");
        let _ = std::fs::remove_file(&p);
        let auth = Arc::new(crate::web_auth::WebState::new(p.clone()));
        auth.init_auth_file().unwrap();
        let app = build_web_router(auth);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/manage/iscsi/allow-exec")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"allow":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn test_web_http_iscsi_allow_exec_toggle_with_session() {
        let p = tmp_web_auth_path("iscsi_allow_sess");
        let _ = std::fs::remove_file(&p);
        let auth = Arc::new(crate::web_auth::WebState::new(p.clone()));
        auth.init_auth_file().unwrap();
        let app = build_web_router(auth);
        let (cookie, app) = web_login_cookie(app).await;
        let post_allow = |body: &str| {
            Request::builder()
                .method("POST")
                .uri("/api/manage/iscsi/allow-exec")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .header("X-VTL-CSRF", "1")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let res = app
            .clone()
            .oneshot(post_allow(r#"{"allow":true}"#))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res2 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/manage/iscsi/config")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(res2.into_body(), 64 * 1024).await.unwrap();
        let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(j["allow_iscsi_exec"], serde_json::json!(true));
        let res3 = app.oneshot(post_allow(r#"{"allow":false}"#)).await.unwrap();
        assert_eq!(res3.status(), StatusCode::OK);
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn test_web_http_post_with_session_requires_csrf_header() {
        let p = tmp_web_auth_path("csrf403");
        let _ = std::fs::remove_file(&p);
        let auth = Arc::new(crate::web_auth::WebState::new(p.clone()));
        auth.init_auth_file().unwrap();
        let app = build_web_router(auth);
        let (cookie, app) = web_login_cookie(app).await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/manage/iscsi/allow-exec")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(r#"{"allow":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn test_web_http_iscsi_check_401_without_session() {
        let p = tmp_web_auth_path("iscsi_chk401");
        let _ = std::fs::remove_file(&p);
        let auth = Arc::new(crate::web_auth::WebState::new(p.clone()));
        auth.init_auth_file().unwrap();
        let app = build_web_router(auth);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/manage/iscsi/check")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn test_web_http_iscsi_check_json_with_session() {
        let p = tmp_web_auth_path("iscsi_chk_sess");
        let _ = std::fs::remove_file(&p);
        let auth = Arc::new(crate::web_auth::WebState::new(p.clone()));
        auth.init_auth_file().unwrap();
        let app = build_web_router(auth);
        let (cookie, app) = web_login_cookie(app).await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/manage/iscsi/check")
                    .header(header::COOKIE, &cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("X-VTL-CSRF", "1")
                    .body(Body::from(r#"{"sudo":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        assert!(
            status == StatusCode::OK
                || status == StatusCode::BAD_GATEWAY
                || status == StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected status {}",
            status
        );
        let body = to_bytes(res.into_body(), 512 * 1024).await.unwrap();
        let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if status == StatusCode::OK || status == StatusCode::BAD_GATEWAY {
            assert!(j.get("ok").is_some());
        } else {
            assert!(j.get("error").is_some());
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_web_html_shell_css_has_toast() {
        let css = include_str!("web_shell.css");
        assert!(
            css.contains(".vtl-toast") && css.contains(".vtl-toast-visible"),
            "toast styles for showToast()"
        );
        assert!(css.contains(".panel-tabs"), "tape page tab strip");
    }

    #[test]
    fn test_web_html_boot_has_nav_and_toast() {
        let js = include_str!("web_boot.js");
        assert!(js.contains("normPath") && js.contains("showToast"));
        assert!(js.contains(".adm-side a[data-nav]"));
        assert!(js.contains("window.showToast"));
        assert!(js.contains("X-VTL-CSRF"));
    }

    /// 断言须与 `web_admin_side_inner.html` 一致。
    #[test]
    fn test_web_html_admin_side_fragment() {
        let s = include_str!("web_admin_side_inner.html");
        assert!(s.contains("存储功能"));
        assert!(s.contains("虚拟磁带库"));
        assert!(s.contains("data-nav=\"/admin/library\""));
        assert!(s.contains("data-nav=\"/admin/tapes\""));
        assert!(s.contains("data-nav=\"/admin/assign-slot\""));
        assert!(s.contains("data-nav=\"/admin/changer\""));
        assert!(s.contains("data-nav=\"/admin/shelf\""));
        assert!(s.contains("data-nav=\"/admin/shelf-place\""));
        assert!(s.contains("data-nav=\"/admin/transport\""));
        assert!(s.contains("③ 传输链路"));
    }

    #[test]
    fn test_web_html_changer_page_api_paths() {
        let h = super::ADMIN_CHANGER_HTML;
        assert!(!h.contains("/api/manage/tape/load"));
        assert!(h.contains("/api/manage/robot/sync"));
        assert!(h.contains("/api/manage/robot/reconcile"));
        assert!(h.contains("/api/manage/robot/auto-align"));
        assert!(h.contains("vtl.ko"));
        assert!(h.contains("inventory 对账"));
        assert!(!h.contains("<h1") || !h.contains(">机械手</h1>"));
    }

    #[test]
    fn test_web_html_vp_side_fragment() {
        let s = include_str!("web_vp_side_inner.html");
        assert!(s.contains("VTL 控制台"));
        assert!(s.contains("data-nav=\"/browse/fabric\""));
        assert!(s.contains("data-nav=\"/admin/transport\""));
        assert!(s.contains("③ 传输链路"));
        assert!(s.contains("data-nav=\"/admin/assign-slot\""));
        assert!(s.contains("data-nav=\"/admin/changer\""));
        assert!(s.contains("data-nav=\"/admin/shelf-place\""));
    }

    #[test]
    fn test_web_html_admin_transport_page() {
        let h = super::ADMIN_TRANSPORT_HTML;
        assert!(h.contains("/admin/transport"));
        assert!(h.contains("WEB-WORKFLOW"));
        assert!(h.contains("/api/fabric"));
        assert!(h.contains("/api/patrol"));
        assert!(h.contains("btn-patrol"));
        assert!(h.contains("library-export"));
    }

    #[test]
    fn test_web_html_home_includes_shell_vp_boot() {
        let h = super::HOME_HTML;
        assert!(h.contains(".breadcrumb"));
        assert!(h.contains("VTL 控制台"));
        assert!(h.contains("/admin/transport"));
        assert!(h.contains("① 磁带库"));
        assert!(h.contains("showToast"));
        assert!(h.contains("normPath"));
    }

    #[test]
    fn test_web_html_admin_setup_init() {
        let h = super::ADMIN_SETUP_INIT_HTML;
        assert!(h.contains("/api/setup/status"));
        assert!(h.contains("/api/setup/complete"));
        assert!(h.contains("/opt/vtladm/var/vtl.conf"));
        assert!(h.contains("kernel_vtl_reload_script"));
        assert!(h.contains("vtl_ko"));
        assert!(h.contains("vtl_reload_scan_delay_ms"));
        assert!(h.contains("run_kernel_reload_now"));
    }

    #[test]
    fn test_web_html_login_shell_card_no_boot() {
        let h = super::LOGIN_HTML;
        assert!(h.contains("login-card") && h.contains("login-page"));
        assert!(
            !h.contains("window.showToast"),
            "login page must not embed web_boot (window.showToast)"
        );
        assert!(
            !h.contains("normPath"),
            "login must not embed web_boot (normPath)"
        );
        assert!(
            !h.contains("function runNav"),
            "login must not embed web_boot (runNav)"
        );
        assert!(h.contains("var(--accent)"));
        assert!(!h.contains(crate::web_auth::DEFAULT_WEB_PASSWORD));
    }

    #[test]
    fn test_web_html_admin_library_detail_page() {
        let h = super::ADMIN_LIBRARY_HTML;
        assert!(h.contains("/api/library/detail"));
        assert!(h.contains("id=\"fold-basic\""));
        assert!(h.contains("lib-toolbar"));
        assert!(h.contains("tb-import") && h.contains("磁带入槽") && h.contains("磁带出库"));
        assert!(h.contains("tb-reconcile") && h.contains("'/admin/changer?library='"));
        assert!(h.contains("'/admin/assign-slot?library='"));
        assert!(h.contains("'/admin/shelf-place?library='"));
        assert!(!h.contains("tb-unload"));
        assert!(h.contains("tb-create-tape") && h.contains("tab=create"));
    }

    #[test]
    fn test_web_html_assign_slot_page() {
        let h = super::ADMIN_ASSIGN_SLOT_HTML;
        assert!(h.contains("磁带入槽"));
        assert!(h.contains("/api/manage/tape/assign-slot-batch"));
        assert!(!h.contains("tab=create"));
    }

    /// 后台磁带页嵌入的侧栏已改为「存储功能」标题（与 `web_admin_side_inner.html` 一致）。
    #[test]
    fn test_web_html_admin_tapes_includes_sidebar_boot() {
        let h = super::ADMIN_TAPES_HTML;
        assert!(h.contains("存储功能"));
        assert!(h.contains("磁带与货架"));
        assert!(h.contains("panel-tabs"));
        assert!(h.contains("tab-create"));
        assert!(h.contains("showToast"));
    }

    #[test]
    fn test_web_html_admin_iscsi_target_config_api() {
        let h = super::ADMIN_ISCSI_HTML;
        assert!(h.contains("/api/manage/iscsi/config"));
        assert!(h.contains("/api/manage/iscsi/allow-exec"));
        assert!(h.contains("btn-iallow-save"));
        assert!(h.contains("/api/manage/iscsi/check"));
        assert!(h.contains("环境与权限"));
        assert!(h.contains("loadIscsiCfg"));
        assert!(h.contains("non_unix_build"));
        assert!(h.contains("iscsi_portals"));
        assert!(h.contains("不支持 IPv6"));
        assert!(h.contains("/api/manage/iscsi/library-export-defaults"));
        assert!(h.contains("id=\"iexport-status\""));
        assert!(h.contains("id=\"iiqn\""));
        assert!(h.contains("id=\"bdoexp\""));
        assert!(h.contains("function bindClick("));
        assert!(h.contains("function parsePortalPort("));
        assert!(h.contains("function setExportButtonEnabled("));
        assert!(h.contains("/api/manage/transport/scan-sg"));
        assert!(h.contains("iscsi-limits-hint"));
        assert!(h.contains("/api/manage/iscsi/library-export"));
        assert!(h.contains("library-unexport"));
        assert!(h.contains("bunexp-adv"));
        assert!(h.contains("has_saved_export") || h.contains("已保存导出记录"));
        assert!(h.contains("iqn"));
        assert!(h.contains("内核与主机风险"));
        assert!(h.contains("整机重启") || h.contains("ioctl"));
    }

    #[test]
    fn test_parse_first_iscsi_portal() {
        assert_eq!(
            super::parse_first_iscsi_portal("192.168.1.5:3260"),
            Some(("192.168.1.5".to_string(), 3260))
        );
        assert_eq!(
            super::parse_first_iscsi_portal("0.0.0.0:3260,10.0.0.1:3261"),
            Some(("0.0.0.0".to_string(), 3260))
        );
        assert!(super::parse_first_iscsi_portal("").is_none());
    }

    #[test]
    fn test_web_html_browse_tapes_includes_vp_boot() {
        let h = super::BROWSE_TAPES_HTML;
        assert!(h.contains("磁带目录"));
        assert!(h.contains("VTL 控制台"));
        assert!(h.contains("showToast"));
    }
}
