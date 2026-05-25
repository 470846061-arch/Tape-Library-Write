use chrono::Utc;
use clap::{Parser, Subcommand};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process;
#[cfg(not(test))]
use std::sync::Mutex;
use thiserror::Error;

mod fab_transport;
mod iscsi_export;
mod kernel_geom_ioctl;
pub(crate) mod lio_hold;
mod patrol;
mod reconcile;
mod robot_sync;
mod scsi_rescan_vtl;
mod scsi_tape_holders;
mod web;
mod web_auth;

#[derive(Error, Debug)]
enum VtlError {
    #[error("数据库错误: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("IO 错误: {0}")]
    IoError(#[from] std::io::Error),
    #[error("容量/大小格式无效: {0}")]
    InvalidSize(String),
    #[error("未找到磁带: {0}")]
    TapeNotFound(String),
    #[error("槽位已被占用")]
    #[allow(dead_code)]
    SlotOccupied,
    #[error("槽位为空")]
    SlotEmpty,
    #[error("驱动器为空")]
    DriveEmpty,
    #[error("驱动器忙")]
    DriveBusy,
    #[error("参数无效: {0}")]
    InvalidParameter(String),
    #[error("权限被拒绝: {0}")]
    PermissionDenied(String),
    #[error("无可用槽位")]
    NoAvailableSlots,
    #[error("磁带名称无效: {0}")]
    InvalidTapeName(String),
    #[error("超出配额: {0}")]
    QuotaExceeded(String),
    #[error("未找到标签: {0}")]
    TagNotFound(String),
    #[error("未找到磁带库: {0}")]
    LibraryNotFound(String),
    #[error("未找到磁带架: {0}")]
    ShelfNotFound(String),
    #[error("磁带库已存在: {0}")]
    LibraryExists(String),
    #[error("磁带当前在驱动器中，请先卸载后再操作")]
    TapeInDrive,
    #[error("磁带须先位于货架上（不得仅在机械手槽内或驱动中）；请先回架后再试")]
    TapeNotOnShelf,
}

/// `cargo test` 用库名：可写入 DB，但**不出现在 Web 列表**且**不导出到 SCSI**（`lsscsi -g`）。
#[cfg(test)]
pub(crate) const LEGACY_DEFAULT_LIBRARY_NAME: &str = "default";
/// Production builds must keep legacy `default` libraries visible for upgraded installs.
#[cfg(not(test))]
pub(crate) const LEGACY_DEFAULT_LIBRARY_NAME: &str = "__vtladm_cargo_test_default__";
const DEFAULT_UNUSED_SHELF_NAME: &str = "unused";

/// 系统保留库：模拟离库后的磁带保管区（仅有货架，无机械手槽位）。
pub(crate) const OFFLINE_LIBRARY_NAME: &str = "__offline__";

/// 仅 `cargo test` 写入 DB 的库名：Web/API 列表与 `lsscsi` 展示均隐藏。
pub(crate) fn is_test_only_library_name(name: &str) -> bool {
    name == LEGACY_DEFAULT_LIBRARY_NAME
}

/// 参与内核 SCSI 导出的在线库（与 [`build_vtl_instances_kernel_spec`] 一致，不含 `__offline__` / `default`）。
pub(crate) fn is_kernel_exported_library_name(name: &str) -> bool {
    !is_test_only_library_name(name) && name != OFFLINE_LIBRARY_NAME
}

/// 在线带库个数（不含 `__offline__` / 测试库 `default`），用于删库判断与建库上限。
pub(crate) fn count_exported_online_libraries(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) FROM vtl_libraries WHERE name NOT IN (?1, ?2)",
        params![OFFLINE_LIBRARY_NAME, LEGACY_DEFAULT_LIBRARY_NAME],
        |r| r.get(0),
    )
}

/// 与内核 `vtl.h` 中 `VTL_MAX_SCSI_INSTANCES` 一致：在线带库（`vtl_instances` 段数，不含 `__offline__`）的**个数上限**（可少于 8）。
pub(crate) const VTL_KERNEL_MAX_ONLINE_LIBRARIES: usize = 8;
/// 与内核 `VTL_MAX_DRIVES` 一致（每库磁带机个数的**上限**，方案 B：8）。
pub(crate) const VTL_KERNEL_MAX_DRIVES_PER_LIB: i32 = 8;
/// 与内核 `VTL_MAX_SLOTS` 一致（每库**数据**槽位个数的**上限**，不含固定 I/E mail 槽）。
pub(crate) const VTL_KERNEL_MAX_DATA_SLOTS_PER_LIB: i32 = 256;

thread_local! {
    static CURRENT_LIBRARY: RefCell<String> = RefCell::new(String::new());
}

fn current_library_name() -> String {
    CURRENT_LIBRARY.with(|c| c.borrow().clone())
}

fn set_current_library(name: &str) {
    CURRENT_LIBRARY.with(|c| *c.borrow_mut() = name.to_string());
}

/// 第一个在线库（按 `id`），不含 `__offline__` 与遗留名 `default`。
pub(crate) fn first_online_library_name(conn: &Connection) -> Result<String, VtlError> {
    let mut stmt = conn.prepare(
        "SELECT name FROM vtl_libraries WHERE name NOT IN (?1, ?2) ORDER BY id ASC LIMIT 1",
    )?;
    let mut rows = stmt.query(params![OFFLINE_LIBRARY_NAME, LEGACY_DEFAULT_LIBRARY_NAME])?;
    if let Some(row) = rows.next()? {
        return row.get::<_, String>(0).map_err(VtlError::from);
    }
    Err(VtlError::InvalidParameter(
        "无在线磁带库；请先：vtladm library create marstor --drives 2 --slots 10".to_string(),
    ))
}

/// CLI `-L` / Web 未指定库名时解析当前库。
pub(crate) fn resolve_active_library_name(opt: Option<&str>) -> Result<String, VtlError> {
    let conn = init_db()?;
    if let Some(raw) = opt {
        let name = raw.trim();
        if !name.is_empty() {
            #[cfg(not(test))]
            if is_test_only_library_name(name) {
                return Err(VtlError::InvalidParameter(format!(
                    "库名 '{}' 仅用于 cargo test，不可作为 -L 操作对象",
                    LEGACY_DEFAULT_LIBRARY_NAME
                )));
            }
            resolve_library_id(&conn, name)?;
            return Ok(name.to_string());
        }
    }
    let cur = current_library_name();
    if !cur.is_empty() {
        if resolve_library_id(&conn, &cur).is_ok() {
            return Ok(cur);
        }
    }
    first_online_library_name(&conn)
}

fn sanitize_lib_dir_component(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 库名仅允许与 `sanitize_lib_dir_component` 一致的字符（canonical 名即输入本身）。
pub(crate) fn validate_library_name(name: &str) -> Result<(), VtlError> {
    if name.is_empty() {
        return Err(VtlError::InvalidParameter(
            "Library name cannot be empty".to_string(),
        ));
    }
    if name.len() > 64 {
        return Err(VtlError::InvalidParameter(
            "Library name too long (max 64)".to_string(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(VtlError::InvalidParameter(
            "库名仅允许 ASCII 字母、数字、连字符(-) 与下划线(_)".to_string(),
        ));
    }
    if sanitize_lib_dir_component(name) != name {
        return Err(VtlError::InvalidParameter(
            "库名规范化后与原名不一致（请仅使用字母、数字、-、_）".to_string(),
        ));
    }
    Ok(())
}

/// 是否同一磁盘文件（硬链接共享 inode；路径 canonicalize 可能不同）。
#[cfg(unix)]
fn same_inode_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => fs::canonicalize(a).ok().as_ref() == fs::canonicalize(b).ok().as_ref(),
    }
}

/// 在 `tape_dir` 根为内核创建 `{name}.vtltape` 别名。优先 **硬链接**（同盘 inode）：
/// 部分环境下内核 `filp_open` 打不开 symlink，用户态 `test -r` 却仍成功。
#[cfg(unix)]
fn link_tape_root_alias(dest: &Path, src: &Path, root: &Path) -> std::io::Result<Option<bool>> {
    use std::os::unix::fs::symlink;

    let src = fs::canonicalize(src)?;
    let dest_points_to_src = || -> bool {
        if !dest.exists() {
            return false;
        }
        if dest.is_symlink() {
            let cur = fs::read_link(dest).ok();
            let tpath = cur.map(|t| if t.is_absolute() { t } else { root.join(t) });
            return tpath.map(|p| same_inode_file(&p, &src)).unwrap_or(false);
        }
        same_inode_file(dest, &src)
    };

    if dest.exists() {
        if !dest_points_to_src() {
            // Duplicate flat copy at tape_dir root: replace with hardlink to library mirror.
            if dest.is_file() && !dest.is_symlink() {
                fs::remove_file(dest)?;
            } else {
                return Ok(None);
            }
        } else {
            if dest.is_symlink() {
                fs::remove_file(dest)?;
                if fs::hard_link(&src, dest).is_ok() {
                    return Ok(Some(true));
                }
                symlink(&src, dest)?;
                return Ok(Some(true));
            }
            return Ok(Some(false));
        }
    }

    match fs::hard_link(&src, dest) {
        Ok(()) => Ok(Some(true)),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            symlink(&src, dest)?;
            Ok(Some(true))
        }
        Err(e) => Err(e),
    }
}

/// 内核 `tape_dir` 使用扁平 `{name}.vtltape`；将各库子目录镜像链接到根并清理失效 symlink。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LinkKernelTapesReport {
    pub linked: usize,
    pub removed_stale: usize,
    pub relocated_flat: usize,
}

pub(crate) fn link_kernel_tapes() -> Result<LinkKernelTapesReport, VtlError> {
    let root = get_tape_dir();
    if !root.is_dir() {
        return Err(VtlError::InvalidParameter(format!(
            "tape_dir 不存在: {}",
            root.display()
        )));
    }
    let mut want: HashMap<String, PathBuf> = HashMap::new();
    let mut conflicts = 0usize;

    for entry in fs::read_dir(&root).map_err(VtlError::IoError)? {
        let entry = entry.map_err(VtlError::IoError)?;
        let ft = entry.file_type().map_err(VtlError::IoError)?;
        if !ft.is_dir() {
            continue;
        }
        let lib = entry.file_name().to_string_lossy().into_owned();
        if lib == OFFLINE_LIBRARY_NAME {
            continue;
        }
        let lib_path = entry.path();
        for sub in [lib_path.clone(), lib_path.join("tape")] {
            let Ok(rd) = fs::read_dir(&sub) else {
                continue;
            };
            for f in rd.flatten() {
                let p = f.path();
                if !p.is_file() {
                    continue;
                }
                let base = match p.file_name().and_then(|n| n.to_str()) {
                    Some(b) if b.ends_with(".vtltape") => b.to_string(),
                    _ => continue,
                };
                let canon = fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
                if let Some(prev) = want.get(&base) {
                    #[cfg(unix)]
                    let dup = !same_inode_file(prev, &p);
                    #[cfg(not(unix))]
                    let dup = prev != &canon;
                    if dup {
                        conflicts += 1;
                    }
                } else {
                    want.insert(base, canon);
                }
            }
        }
    }

    let mut linked = 0usize;
    #[cfg(unix)]
    {
        for (base, src) in &want {
            let dest = root.join(base);
            match link_tape_root_alias(&dest, src, &root).map_err(VtlError::IoError)? {
                Some(true) => linked += 1,
                Some(false) => {}
                None => conflicts += 1,
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (&want, &mut linked);
    }

    let mut removed_stale = 0usize;
    let mut relocated_flat = 0usize;
    const ORPHAN_ROOT: &str = "_orphaned_root";
    for entry in fs::read_dir(&root).map_err(VtlError::IoError)? {
        let entry = entry.map_err(VtlError::IoError)?;
        let ft = entry.file_type().map_err(VtlError::IoError)?;
        let path = entry.path();
        let base = entry.file_name().to_string_lossy().into_owned();
        if !base.ends_with(".vtltape") {
            continue;
        }
        if ft.is_file() && !ft.is_symlink() {
            if let Some(want_path) = want.get(&base) {
                #[cfg(unix)]
                let dup = !same_inode_file(&path, want_path);
                #[cfg(not(unix))]
                let dup = fs::canonicalize(&path).ok().as_ref() != Some(want_path);
                if dup {
                    conflicts += 1;
                }
            } else {
                let orphan_dir = root.join(ORPHAN_ROOT);
                fs::create_dir_all(&orphan_dir).map_err(VtlError::IoError)?;
                let dest = orphan_dir.join(&base);
                if dest.exists() {
                    conflicts += 1;
                } else {
                    fs::rename(&path, &dest).map_err(VtlError::IoError)?;
                    relocated_flat += 1;
                    log_message(&format!(
                        "link_kernel_tapes: moved flat root file {} -> {}",
                        path.display(),
                        dest.display()
                    ));
                }
            }
            continue;
        }
        if !ft.is_symlink() {
            continue;
        }
        let stale = if !want.contains_key(&base) {
            true
        } else {
            let target = fs::read_link(&path).unwrap_or_default();
            let tpath = if target.is_absolute() {
                target
            } else {
                root.join(target)
            };
            !tpath.exists()
        };
        if stale {
            fs::remove_file(&path).map_err(VtlError::IoError)?;
            removed_stale += 1;
        }
    }

    sync_module_tape_dir_sysfs(&root);

    if conflicts > 0 {
        return Err(VtlError::InvalidParameter(format!(
            "link_kernel_tapes: {} 个磁带名在 tape_dir 根目录冲突（须全局唯一；勿在根目录保留实体 .vtltape）",
            conflicts
        )));
    }
    Ok(LinkKernelTapesReport {
        linked,
        removed_stale,
        relocated_flat,
    })
}

fn sync_module_tape_dir_sysfs(dir: &Path) {
    let trimmed = dir.to_string_lossy().trim_end_matches('/').to_string();
    if trimmed.is_empty() {
        return;
    }
    let sysfs = Path::new("/sys/module/vtl/parameters/tape_dir");
    if sysfs.exists() {
        if let Err(e) = fs::write(sysfs, trimmed.as_bytes()) {
            log_error(
                "link_kernel_tapes",
                &format!("write {}: {}", sysfs.display(), e),
            );
        }
    }
}

fn assert_tape_name_globally_unique(
    conn: &Connection,
    name: &str,
    library_id: i64,
) -> Result<(), VtlError> {
    let other: Option<String> = conn
        .query_row(
            "SELECT l.name FROM tapes t
             JOIN vtl_libraries l ON t.library_id = l.id
             WHERE t.name = ?1 AND t.library_id != ?2
             LIMIT 1",
            params![name, library_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(lib) = other {
        return Err(VtlError::InvalidParameter(format!(
            "磁带名 '{}' 已在库 '{}' 中使用（内核 tape_dir 须全局唯一）",
            name, lib
        )));
    }
    Ok(())
}

fn tape_image_path(library_name: &str, tape_name: &str) -> PathBuf {
    get_tape_dir()
        .join(sanitize_lib_dir_component(library_name))
        .join(format!("{}.vtltape", tape_name))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, rusqlite::Error> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        params![table],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn quote_sql_ident(ident: &str) -> String {
    let escaped = ident.replace('"', "\"\"");
    format!("\"{}\"", escaped)
}

fn column_exists(conn: &Connection, table: &str, col: &str) -> Result<bool, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_sql_ident(table)))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == col {
            return Ok(true);
        }
    }
    Ok(false)
}

fn resolve_library_id(conn: &Connection, name: &str) -> Result<i64, VtlError> {
    conn.query_row(
        "SELECT id FROM vtl_libraries WHERE name = ?1",
        params![name],
        |r| r.get(0),
    )
    .map_err(|_| VtlError::LibraryNotFound(name.to_string()))
}

fn default_shelf_id(conn: &Connection, library_id: i64) -> Result<i64, VtlError> {
    conn.query_row(
        "SELECT id FROM shelves WHERE library_id = ?1 AND is_default_unused = 1 LIMIT 1",
        params![library_id],
        |r| r.get(0),
    )
    .map_err(|_| {
        VtlError::ShelfNotFound(format!(
            "default unused shelf for library_id {}",
            library_id
        ))
    })
}

fn resolve_shelf_id(conn: &Connection, library_id: i64, shelf_name: &str) -> Result<i64, VtlError> {
    conn.query_row(
        "SELECT id FROM shelves WHERE library_id = ?1 AND name = ?2",
        params![library_id, shelf_name],
        |r| r.get(0),
    )
    .map_err(|_| VtlError::ShelfNotFound(shelf_name.to_string()))
}

fn lib_config_get(conn: &Connection, library_id: i64, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM library_config WHERE library_id = ?1 AND key = ?2",
        params![library_id, key],
        |r| r.get(0),
    )
    .ok()
}

fn tape_in_drive(conn: &Connection, library_id: i64, tape_id: i64) -> Result<bool, VtlError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM drives WHERE library_id = ?1 AND tape_id = ?2",
        params![library_id, tape_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 确保存在离线保管库及其默认 `unused` 货架，并创建磁带目录。
pub(crate) fn ensure_offline_library(conn: &Connection) -> Result<i64, VtlError> {
    ensure_vtl_core_tables(conn).map_err(VtlError::from)?;
    let lib_id: i64 = if let Ok(id) = conn.query_row(
        "SELECT id FROM vtl_libraries WHERE name = ?1",
        params![OFFLINE_LIBRARY_NAME],
        |r| r.get(0),
    ) {
        id
    } else {
        conn.execute(
            "INSERT INTO vtl_libraries (name, created_at) VALUES (?1, ?2)",
            params![OFFLINE_LIBRARY_NAME, Utc::now().to_rfc3339()],
        )?;
        conn.last_insert_rowid()
    };
    let n_unused: i64 = conn.query_row(
        "SELECT COUNT(*) FROM shelves WHERE library_id = ?1 AND is_default_unused = 1",
        params![lib_id],
        |r| r.get(0),
    )?;
    if n_unused == 0 {
        conn.execute(
            "INSERT INTO shelves (library_id, name, is_default_unused) VALUES (?1, ?2, 1)",
            params![lib_id, DEFAULT_UNUSED_SHELF_NAME],
        )?;
    }
    let lib_dir = get_tape_dir().join(sanitize_lib_dir_component(OFFLINE_LIBRARY_NAME));
    fs::create_dir_all(&lib_dir).map_err(VtlError::from)?;
    Ok(lib_id)
}

/// 在离线保管区新建用户货架（与在线库无关）。
pub(crate) fn create_offline_shelf(shelf_name: &str) -> Result<(), VtlError> {
    if shelf_name.is_empty() {
        return Err(VtlError::InvalidParameter(
            "Shelf name cannot be empty".to_string(),
        ));
    }
    if shelf_name == DEFAULT_UNUSED_SHELF_NAME {
        return Err(VtlError::InvalidParameter(format!(
            "Shelf name '{}' is reserved for the default unused shelf",
            DEFAULT_UNUSED_SHELF_NAME
        )));
    }
    let conn = init_db()?;
    let offline_lib_id = ensure_offline_library(&conn)?;
    conn.execute(
        "INSERT INTO shelves (library_id, name, is_default_unused) VALUES (?1, ?2, 0)",
        params![offline_lib_id, shelf_name],
    )
    .map_err(|_| {
        VtlError::InvalidParameter(format!(
            "Shelf '{}' may already exist in offline storage",
            shelf_name
        ))
    })?;
    println!(
        "Created offline shelf '{}' (under reserved library '{}')",
        shelf_name, OFFLINE_LIBRARY_NAME
    );
    Ok(())
}

/// 将多条磁带从指定在线库移至离线保管区的目标货架（会移动镜像文件目录）。
pub(crate) fn move_tapes_to_offline_shelf(
    from_library: &str,
    tape_names: &[String],
    offline_shelf_name: &str,
) -> Result<(), VtlError> {
    if tape_names.is_empty() {
        return Err(VtlError::InvalidParameter(
            "tape list cannot be empty".to_string(),
        ));
    }
    let mut seen_names = std::collections::HashSet::new();
    for n in tape_names {
        if !seen_names.insert(n.as_str()) {
            return Err(VtlError::InvalidParameter(format!(
                "duplicate tape name in batch: {}",
                n
            )));
        }
    }
    let mut conn = init_db()?;
    let from_lib_id = resolve_library_id(&conn, from_library)?;
    let offline_lib_id = ensure_offline_library(&conn)?;
    let target_shelf_id = resolve_shelf_id(&conn, offline_lib_id, offline_shelf_name)?;

    for tape_name in tape_names {
        let tape_id: i64 = conn
            .query_row(
                "SELECT id FROM tapes WHERE library_id = ?1 AND name = ?2",
                params![from_lib_id, tape_name.as_str()],
                |r| r.get(0),
            )
            .map_err(|_| VtlError::TapeNotFound(tape_name.clone()))?;

        if tape_in_drive(&conn, from_lib_id, tape_id)? {
            return Err(VtlError::TapeInDrive);
        }

        let dup: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tapes WHERE library_id = ?1 AND name = ?2",
            params![offline_lib_id, tape_name.as_str()],
            |r| r.get(0),
        )?;
        if dup > 0 {
            return Err(VtlError::InvalidParameter(format!(
                "offline storage already has tape '{}'; rename the source tape or remove the offline copy first",
                tape_name
            )));
        }

        let current_slot: Option<i32> = conn.query_row(
            "SELECT slot FROM tapes WHERE id = ?1",
            params![tape_id],
            |r| r.get::<_, Option<i32>>(0),
        )?;
        let image_path: String = conn.query_row(
            "SELECT image_path FROM tapes WHERE id = ?1",
            params![tape_id],
            |r| r.get(0),
        )?;
        let old_path = PathBuf::from(&image_path);
        let new_path = tape_image_path(OFFLINE_LIBRARY_NAME, tape_name);
        if new_path.exists() {
            return Err(VtlError::InvalidParameter(format!(
                "target path already exists: {}",
                new_path.display()
            )));
        }
        if let Some(parent) = new_path.parent() {
            fs::create_dir_all(parent).map_err(VtlError::from)?;
        }
        fs::rename(&old_path, &new_path).map_err(|e| {
            VtlError::IoError(std::io::Error::new(
                e.kind(),
                format!(
                    "rename {} -> {}: {}",
                    old_path.display(),
                    new_path.display(),
                    e
                ),
            ))
        })?;

        let tx = conn.transaction()?;
        if let Some(s) = current_slot {
            tx.execute(
                "UPDATE slots SET tape_id = NULL WHERE library_id = ?1 AND slot_id = ?2",
                params![from_lib_id, s],
            )?;
        }
        let r = tx.execute(
            "UPDATE tapes SET library_id = ?1, shelf_id = ?2, slot = NULL, image_path = ?3 WHERE id = ?4",
            params![
                offline_lib_id,
                target_shelf_id,
                new_path.to_string_lossy(),
                tape_id
            ],
        );
        if let Err(e) = r {
            let _ = fs::rename(&new_path, &old_path);
            return Err(VtlError::from(e));
        }
        tx.commit()?;
    }

    println!(
        "Moved {} tape(s) from library '{}' to offline shelf '{}'",
        tape_names.len(),
        from_library,
        offline_shelf_name
    );
    Ok(())
}

fn assign_one_tape_to_slot(
    tx: &rusqlite::Transaction<'_>,
    library_id: i64,
    tape_name: &str,
    slot: i32,
) -> Result<(), VtlError> {
    let tape_id: i64 = tx
        .query_row(
            "SELECT id FROM tapes WHERE library_id = ?1 AND name = ?2",
            params![library_id, tape_name],
            |r| r.get(0),
        )
        .map_err(|_| VtlError::TapeNotFound(tape_name.to_string()))?;

    if tape_in_drive(&*tx, library_id, tape_id)? {
        return Err(VtlError::TapeInDrive);
    }

    let (shelf_id, in_slot): (Option<i64>, Option<i32>) = tx.query_row(
        "SELECT shelf_id, slot FROM tapes WHERE id = ?1",
        params![tape_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    if in_slot.is_some() {
        return Err(VtlError::InvalidParameter(
            "Tape is already in a robot slot; use shelf place first if you need to move it"
                .to_string(),
        ));
    }
    if shelf_id.is_none() {
        return Err(VtlError::TapeNotOnShelf);
    }

    let occupied: i64 = tx.query_row(
        "SELECT COUNT(*) FROM slots WHERE library_id = ?1 AND slot_id = ?2 AND tape_id IS NOT NULL",
        params![library_id, slot],
        |r| r.get(0),
    )?;
    if occupied > 0 {
        return Err(VtlError::InvalidParameter(format!(
            "Slot {} is not empty",
            slot
        )));
    }

    tx.execute(
        "UPDATE tapes SET slot = ?1, shelf_id = NULL WHERE id = ?2",
        params![slot, tape_id],
    )?;
    tx.execute(
        "UPDATE slots SET tape_id = ?1 WHERE library_id = ?2 AND slot_id = ?3",
        params![tape_id, library_id, slot],
    )?;
    Ok(())
}

fn assert_data_slot_empty(
    conn: &Connection,
    online_library_id: i64,
    slot: i32,
) -> Result<(), VtlError> {
    let occupied: i64 = conn.query_row(
        "SELECT COUNT(*) FROM slots WHERE library_id = ?1 AND slot_id = ?2 AND tape_id IS NOT NULL",
        params![online_library_id, slot],
        |r| r.get(0),
    )?;
    if occupied > 0 {
        return Err(VtlError::InvalidParameter(format!(
            "Slot {} is not empty",
            slot
        )));
    }
    Ok(())
}

/// 校验单条入槽请求（不修改数据）。`from_offline` 为 true 时表示磁带当前在 `__offline__` 货架上。
fn validate_tape_ready_for_online_slot(
    conn: &Connection,
    online_lib_id: i64,
    offline_lib_id: i64,
    tape_name: &str,
    slot: i32,
    from_offline: bool,
) -> Result<(), VtlError> {
    assert_data_slot_empty(conn, online_lib_id, slot)?;
    if from_offline {
        let (tape_id, shelf_id, in_slot): (i64, Option<i64>, Option<i32>) = conn
            .query_row(
                "SELECT id, shelf_id, slot FROM tapes WHERE library_id = ?1 AND name = ?2",
                params![offline_lib_id, tape_name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|_| VtlError::TapeNotFound(tape_name.to_string()))?;
        if in_slot.is_some() {
            return Err(VtlError::InvalidParameter(
                "offline tape is already bound to a slot (data inconsistency)".to_string(),
            ));
        }
        if shelf_id.is_none() {
            return Err(VtlError::TapeNotOnShelf);
        }
        if tape_in_drive(conn, offline_lib_id, tape_id)? {
            return Err(VtlError::TapeInDrive);
        }
        let dup: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tapes WHERE library_id = ?1 AND name = ?2",
            params![online_lib_id, tape_name],
            |r| r.get(0),
        )?;
        if dup > 0 {
            return Err(VtlError::InvalidParameter(format!(
                "target library already has a tape named '{}'; remove or rename it before importing from offline",
                tape_name
            )));
        }
    } else {
        let tape_id: i64 = conn
            .query_row(
                "SELECT id FROM tapes WHERE library_id = ?1 AND name = ?2",
                params![online_lib_id, tape_name],
                |r| r.get(0),
            )
            .map_err(|_| VtlError::TapeNotFound(tape_name.to_string()))?;
        if tape_in_drive(conn, online_lib_id, tape_id)? {
            return Err(VtlError::TapeInDrive);
        }
        let (shelf_id, in_slot): (Option<i64>, Option<i32>) = conn.query_row(
            "SELECT shelf_id, slot FROM tapes WHERE id = ?1",
            params![tape_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if in_slot.is_some() {
            return Err(VtlError::InvalidParameter(
                "Tape is already in a robot slot; use shelf place first if you need to move it"
                    .to_string(),
            ));
        }
        if shelf_id.is_none() {
            return Err(VtlError::TapeNotOnShelf);
        }
    }
    Ok(())
}

/// 将离线保管区货架上的磁带迁入指定在线库槽位（移动镜像文件并更新 `library_id` / `slot` / `image_path`）。
fn import_offline_tape_to_online_slot(
    conn: &mut Connection,
    online_lib_name: &str,
    online_lib_id: i64,
    offline_lib_id: i64,
    tape_name: &str,
    slot: i32,
) -> Result<(), VtlError> {
    let (tape_id, shelf_id, current_slot, image_path): (i64, Option<i64>, Option<i32>, String) =
        conn.query_row(
            "SELECT id, shelf_id, slot, image_path FROM tapes WHERE library_id = ?1 AND name = ?2",
            params![offline_lib_id, tape_name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|_| VtlError::TapeNotFound(tape_name.to_string()))?;

    if tape_in_drive(conn, offline_lib_id, tape_id)? {
        return Err(VtlError::TapeInDrive);
    }
    if current_slot.is_some() {
        return Err(VtlError::InvalidParameter(
            "offline tape is already in a slot (data inconsistency)".to_string(),
        ));
    }
    if shelf_id.is_none() {
        return Err(VtlError::TapeNotOnShelf);
    }

    let dup: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tapes WHERE library_id = ?1 AND name = ?2",
        params![online_lib_id, tape_name],
        |r| r.get(0),
    )?;
    if dup > 0 {
        return Err(VtlError::InvalidParameter(format!(
            "target library already has a tape named '{}'",
            tape_name
        )));
    }

    assert_data_slot_empty(conn, online_lib_id, slot)?;

    let old_path = PathBuf::from(&image_path);
    let new_path = tape_image_path(online_lib_name, tape_name);
    if new_path.exists() {
        return Err(VtlError::InvalidParameter(format!(
            "target path already exists: {}",
            new_path.display()
        )));
    }
    if let Some(parent) = new_path.parent() {
        fs::create_dir_all(parent).map_err(VtlError::from)?;
    }
    fs::rename(&old_path, &new_path).map_err(|e| {
        VtlError::IoError(std::io::Error::new(
            e.kind(),
            format!(
                "rename {} -> {}: {}",
                old_path.display(),
                new_path.display(),
                e
            ),
        ))
    })?;

    let tx = conn.transaction()?;
    let r = tx.execute(
        "UPDATE tapes SET library_id = ?1, shelf_id = NULL, slot = ?2, image_path = ?3 WHERE id = ?4",
        params![
            online_lib_id,
            slot,
            new_path.to_string_lossy(),
            tape_id
        ],
    );
    if let Err(e) = r {
        rollback_import_rename(&new_path, &old_path);
        return Err(VtlError::from(e));
    }
    let r2 = tx.execute(
        "UPDATE slots SET tape_id = ?1 WHERE library_id = ?2 AND slot_id = ?3",
        params![tape_id, online_lib_id, slot],
    );
    if let Err(e) = r2 {
        rollback_import_rename(&new_path, &old_path);
        return Err(VtlError::from(e));
    }
    if let Err(e) = tx.commit() {
        rollback_import_rename(&new_path, &old_path);
        return Err(VtlError::from(e));
    }
    Ok(())
}

fn rollback_import_rename(new_path: &Path, old_path: &Path) {
    if let Err(e) = fs::rename(new_path, old_path) {
        log_error(
            "import_offline_tape_to_online_slot",
            &format!(
                "rollback rename {} -> {} failed: {}",
                new_path.display(),
                old_path.display(),
                e
            ),
        );
    }
}

/// 批量入槽：磁带可来自**目标在线库货架**，或来自**离线保管区货架**（`from_offline`）。
/// 先校验空槽数量与每条状态，再**逐条**提交（离线路径含文件 `rename`）。
///
/// **一致性说明**：本条与条之间**非单一大事务**；若批量中途某条失败，其前条目可能已入槽或已迁文件，请根据界面或数据库核对后重试，勿盲目重复整批提交。
pub(crate) fn assign_tapes_to_slots_batch(
    library: &str,
    pairs: &[(String, i32, bool)],
) -> Result<(), VtlError> {
    if pairs.is_empty() {
        return Err(VtlError::InvalidParameter(
            "batch pairs cannot be empty".to_string(),
        ));
    }
    if library == OFFLINE_LIBRARY_NAME {
        return Err(VtlError::InvalidParameter(
            "cannot assign robot slots in offline storage library".to_string(),
        ));
    }
    let mut conn = init_db()?;
    let online_lib_id = resolve_library_id(&conn, library)?;
    let offline_lib_id = ensure_offline_library(&conn)?;
    let empty: i64 = conn.query_row(
        "SELECT COUNT(*) FROM slots WHERE library_id = ?1 AND tape_id IS NULL AND is_import_export = 0",
        params![online_lib_id],
        |r| r.get(0),
    )?;
    if pairs.len() as i64 > empty {
        return Err(VtlError::InvalidParameter(format!(
            "batch size {} exceeds available empty data slots ({})",
            pairs.len(),
            empty
        )));
    }
    let mut seen_slots = std::collections::HashSet::new();
    for (_, slot, _) in pairs {
        if !seen_slots.insert(*slot) {
            return Err(VtlError::InvalidParameter(
                "duplicate slot in batch".to_string(),
            ));
        }
    }
    let mut seen_tapes = std::collections::HashSet::new();
    for (tape_name, _, _) in pairs {
        if !seen_tapes.insert(tape_name.as_str()) {
            return Err(VtlError::InvalidParameter(
                "duplicate tape in batch".to_string(),
            ));
        }
    }

    for (tape_name, slot, from_offline) in pairs {
        validate_tape_ready_for_online_slot(
            &conn,
            online_lib_id,
            offline_lib_id,
            tape_name.as_str(),
            *slot,
            *from_offline,
        )?;
    }

    for (tape_name, slot, from_offline) in pairs {
        if *from_offline {
            import_offline_tape_to_online_slot(
                &mut conn,
                library,
                online_lib_id,
                offline_lib_id,
                tape_name.as_str(),
                *slot,
            )?;
        } else {
            let tx = conn.transaction()?;
            assign_one_tape_to_slot(&tx, online_lib_id, tape_name.as_str(), *slot)?;
            tx.commit()?;
            if robot_sync::robot_ioctl_enabled() {
                let bc =
                    robot_sync::tape_barcode_for_name(&conn, online_lib_id, tape_name.as_str());
                if let Err(e) = robot_sync::kernel_slot_place(
                    &conn,
                    online_lib_id,
                    *slot,
                    tape_name.as_str(),
                    bc.as_deref(),
                ) {
                    robot_sync::warn_kernel_sync_failed("assign-slot-batch", &e);
                }
            }
        }
    }
    if robot_sync::robot_sync_enabled() {
        reconcile::try_post_op_auto_align(online_lib_id);
    }
    println!(
        "Assigned {} tape(s) to slots in library '{}'",
        pairs.len(),
        library
    );
    Ok(())
}

/// 在同一库中按行批量创建磁带（`size` 为每条的人类可读大小字符串）。
pub(crate) fn create_tapes_batch(
    library: &str,
    shelf: Option<&str>,
    items: &[(String, String)],
) -> Result<(), VtlError> {
    if items.is_empty() {
        return Err(VtlError::InvalidParameter(
            "batch items cannot be empty".to_string(),
        ));
    }
    let prev = current_library_name();
    set_current_library(library);
    let r = (|| -> Result<(), VtlError> {
        for (name, size_s) in items {
            let size = parse_size(size_s)?;
            create_tape(name, size, shelf)?;
        }
        Ok(())
    })();
    set_current_library(&prev);
    r
}

/// Auto tape name prefix: `{library}_tape` so names are unique under flat kernel `tape_dir`.
fn auto_tape_name_prefix(library: &str) -> String {
    format!("{}_tape", sanitize_lib_dir_component(library))
}

/// 当前库中 `{lib}_tape` + 十进制数字 的名称里，最大的数字后缀。
fn max_auto_tape_suffix_for_library(
    conn: &Connection,
    library_id: i64,
    library: &str,
) -> Result<u64, VtlError> {
    let prefixed = auto_tape_name_prefix(library);
    let mut stmt = conn.prepare("SELECT name FROM tapes WHERE library_id = ?1")?;
    let rows = stmt.query_map(params![library_id], |r| r.get::<_, String>(0))?;
    let mut max_n: u64 = 0;
    for row in rows {
        let name = row?;
        let rest = name.strip_prefix(&prefixed);
        if let Some(rest) = rest {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(n) = rest.parse::<u64>() {
                    max_n = max_n.max(n);
                }
            }
        }
    }
    Ok(max_n)
}

fn allocate_auto_tape_names(library: &str, start: u64, count: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    let prefix = auto_tape_name_prefix(library);
    let end = start + count as u64 - 1;
    let width = std::cmp::max(2, format!("{}", end).len());
    (start..=end)
        .map(|n| format!("{}{:0width$}", prefix, n, width = width))
        .collect()
}

/// 按库内已有 `{lib}_tape`+数字 规则顺序生成名称（如 `marstor_tape01`），批量创建。
pub(crate) fn create_auto_named_tapes_batch(
    library: &str,
    shelf: Option<&str>,
    count: usize,
    size: u64,
) -> Result<Vec<String>, VtlError> {
    if count == 0 {
        return Err(VtlError::InvalidParameter(
            "count must be at least 1".to_string(),
        ));
    }
    if count > 10_000 {
        return Err(VtlError::InvalidParameter(
            "count cannot exceed 10000".to_string(),
        ));
    }
    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, library)?;
    let max_n = max_auto_tape_suffix_for_library(&conn, library_id, library)?;
    let start = max_n + 1;
    let names = allocate_auto_tape_names(library, start, count);

    let prev = current_library_name();
    set_current_library(library);
    let r = (|| -> Result<(), VtlError> {
        for name in &names {
            create_tape(name, size, shelf)?;
        }
        Ok(())
    })();
    set_current_library(&prev);
    r?;
    Ok(names)
}

/// 同一库内将磁带从货架 A 批量迁到货架 B（须在货架上、不在槽内、不在驱动中）；不移动镜像文件。
pub(crate) fn migrate_tapes_between_shelves(
    library: &str,
    from_shelf: &str,
    to_shelf: &str,
    tape_names: &[String],
) -> Result<(), VtlError> {
    if tape_names.is_empty() {
        return Err(VtlError::InvalidParameter(
            "tape list cannot be empty".to_string(),
        ));
    }
    if from_shelf == to_shelf {
        return Err(VtlError::InvalidParameter(
            "source and target shelf must differ".to_string(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for n in tape_names {
        if !seen.insert(n.as_str()) {
            return Err(VtlError::InvalidParameter(format!(
                "duplicate tape name in batch: {}",
                n
            )));
        }
    }
    let mut conn = init_db()?;
    let lib_id = resolve_library_id(&conn, library)?;
    let from_id = resolve_shelf_id(&conn, lib_id, from_shelf)?;
    let to_id = resolve_shelf_id(&conn, lib_id, to_shelf)?;
    let tx = conn.transaction()?;
    for name in tape_names {
        let (tape_id, shelf_id, slot): (i64, Option<i64>, Option<i32>) = tx
            .query_row(
                "SELECT id, shelf_id, slot FROM tapes WHERE library_id = ?1 AND name = ?2",
                params![lib_id, name.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|_| VtlError::TapeNotFound(name.clone()))?;
        if shelf_id != Some(from_id) {
            return Err(VtlError::InvalidParameter(format!(
                "tape '{}' is not on source shelf '{}'",
                name, from_shelf
            )));
        }
        if slot.is_some() {
            return Err(VtlError::InvalidParameter(format!(
                "tape '{}' is in a robot slot; move to shelf first",
                name
            )));
        }
        if tape_in_drive(&*tx, lib_id, tape_id)? {
            return Err(VtlError::TapeInDrive);
        }
        tx.execute(
            "UPDATE tapes SET shelf_id = ?1 WHERE id = ?2",
            params![to_id, tape_id],
        )?;
    }
    tx.commit()?;
    println!(
        "Migrated {} tape(s) from shelf '{}' to '{}' in library '{}'",
        tape_names.len(),
        from_shelf,
        to_shelf,
        library
    );
    Ok(())
}

fn ensure_vtl_core_tables(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS vtl_libraries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS shelves (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            library_id INTEGER NOT NULL REFERENCES vtl_libraries(id),
            name TEXT NOT NULL,
            is_default_unused INTEGER NOT NULL DEFAULT 0,
            UNIQUE(library_id, name)
        );
        CREATE TABLE IF NOT EXISTS library_config (
            library_id INTEGER NOT NULL REFERENCES vtl_libraries(id),
            key TEXT NOT NULL,
            value TEXT,
            PRIMARY KEY (library_id, key)
        );
        "#,
    )?;
    Ok(())
}

fn migrate_legacy_to_v2(conn: &mut Connection) -> Result<(), VtlError> {
    conn.execute("PRAGMA foreign_keys = OFF", [])?;
    let tx = conn.transaction()?;

    ensure_vtl_core_tables(&tx)?;
    // v1→v2：优先挂到已有在线库；仅当库表为空且确有旧数据时，才建遗留名 `default` 行（不参与 vtl_instances）。
    let lib_id: i64 = if let Ok(id) = tx.query_row(
        "SELECT id FROM vtl_libraries WHERE name NOT IN (?1, ?2) ORDER BY id ASC LIMIT 1",
        params![OFFLINE_LIBRARY_NAME, LEGACY_DEFAULT_LIBRARY_NAME],
        |r| r.get(0),
    ) {
        id
    } else if let Ok(id) = tx.query_row(
        "SELECT id FROM vtl_libraries WHERE name = ?1",
        params![LEGACY_DEFAULT_LIBRARY_NAME],
        |r| r.get(0),
    ) {
        id
    } else {
        // 仅 v1 旧库无库行时的一次性锚点；不参与 vtl_instances，可在建正式库后 `library delete default`。
        tx.execute(
            "INSERT INTO vtl_libraries (name, created_at) VALUES (?1, ?2)",
            params![LEGACY_DEFAULT_LIBRARY_NAME, Utc::now().to_rfc3339()],
        )?;
        tx.last_insert_rowid()
    };

    let has_unused: i64 = tx.query_row(
        "SELECT COUNT(*) FROM shelves WHERE library_id = ?1 AND is_default_unused = 1",
        params![lib_id],
        |r| r.get(0),
    )?;
    if has_unused == 0 {
        tx.execute(
            "INSERT INTO shelves (library_id, name, is_default_unused) VALUES (?1, ?2, 1)",
            params![lib_id, DEFAULT_UNUSED_SHELF_NAME],
        )?;
    }
    let shelf_id: i64 = tx.query_row(
        "SELECT id FROM shelves WHERE library_id = ?1 AND is_default_unused = 1 LIMIT 1",
        params![lib_id],
        |r| r.get(0),
    )?;

    if !column_exists(&tx, "tapes", "library_id")? {
        tx.execute(
            "ALTER TABLE tapes ADD COLUMN library_id INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }
    if !column_exists(&tx, "tapes", "shelf_id")? {
        tx.execute("ALTER TABLE tapes ADD COLUMN shelf_id INTEGER", [])?;
    }

    tx.execute(
        "UPDATE tapes SET library_id = ?1 WHERE library_id IS NULL",
        params![lib_id],
    )?;

    tx.execute(
        "UPDATE tapes SET shelf_id = ?1
         WHERE shelf_id IS NULL AND slot IS NULL
         AND id NOT IN (SELECT tape_id FROM drives WHERE tape_id IS NOT NULL)",
        params![shelf_id],
    )?;

    tx.execute(
        "CREATE TABLE IF NOT EXISTS library_config (
            library_id INTEGER NOT NULL REFERENCES vtl_libraries(id),
            key TEXT NOT NULL,
            value TEXT,
            PRIMARY KEY (library_id, key)
        )",
        [],
    )?;

    tx.execute(
        "INSERT OR REPLACE INTO library_config (library_id, key, value)
         SELECT ?1, key, value FROM config",
        params![lib_id],
    )?;

    if !column_exists(&tx, "slots", "library_id")? {
        tx.execute(
            "CREATE TABLE slots_new (
                library_id INTEGER NOT NULL,
                slot_id INTEGER NOT NULL,
                tape_id INTEGER,
                is_import_export INTEGER DEFAULT 0,
                PRIMARY KEY (library_id, slot_id)
            )",
            [],
        )?;
        tx.execute(
            "INSERT INTO slots_new (library_id, slot_id, tape_id, is_import_export)
             SELECT ?1, slot_id, tape_id, CASE WHEN is_import_export THEN 1 ELSE 0 END FROM slots",
            params![lib_id],
        )?;
        tx.execute("DROP TABLE slots", [])?;
        tx.execute("ALTER TABLE slots_new RENAME TO slots", [])?;
    }

    if !column_exists(&tx, "drives", "library_id")? {
        tx.execute(
            "CREATE TABLE drives_new (
                library_id INTEGER NOT NULL,
                drive_id INTEGER NOT NULL,
                tape_id INTEGER,
                PRIMARY KEY (library_id, drive_id)
            )",
            [],
        )?;
        tx.execute(
            "INSERT INTO drives_new (library_id, drive_id, tape_id)
             SELECT ?1, drive_id, tape_id FROM drives",
            params![lib_id],
        )?;
        tx.execute("DROP TABLE drives", [])?;
        tx.execute("ALTER TABLE drives_new RENAME TO drives", [])?;
    }

    tx.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_tapes_lib_name ON tapes(library_id, name)",
        [],
    )?;
    tx.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_tapes_lib_barcode ON tapes(library_id, barcode)",
        [],
    )?;

    tx.commit()?;
    conn.execute("PRAGMA foreign_keys = ON", [])?;
    Ok(())
}

fn create_fresh_v2_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS vtl_libraries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS shelves (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            library_id INTEGER NOT NULL REFERENCES vtl_libraries(id),
            name TEXT NOT NULL,
            is_default_unused INTEGER NOT NULL DEFAULT 0,
            UNIQUE(library_id, name)
        );
        CREATE TABLE IF NOT EXISTS library_config (
            library_id INTEGER NOT NULL REFERENCES vtl_libraries(id),
            key TEXT NOT NULL,
            value TEXT,
            PRIMARY KEY (library_id, key)
        );
        CREATE TABLE IF NOT EXISTS tapes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            library_id INTEGER NOT NULL REFERENCES vtl_libraries(id),
            shelf_id INTEGER REFERENCES shelves(id),
            barcode TEXT NOT NULL,
            name TEXT NOT NULL,
            slot INTEGER,
            capacity_bytes INTEGER NOT NULL,
            used_bytes INTEGER DEFAULT 0,
            created_at TIMESTAMP NOT NULL,
            image_path TEXT NOT NULL,
            UNIQUE(library_id, name),
            UNIQUE(library_id, barcode)
        );
        CREATE TABLE IF NOT EXISTS slots (
            library_id INTEGER NOT NULL REFERENCES vtl_libraries(id),
            slot_id INTEGER NOT NULL,
            tape_id INTEGER REFERENCES tapes(id),
            is_import_export INTEGER DEFAULT 0,
            PRIMARY KEY (library_id, slot_id)
        );
        CREATE TABLE IF NOT EXISTS drives (
            library_id INTEGER NOT NULL REFERENCES vtl_libraries(id),
            drive_id INTEGER NOT NULL,
            tape_id INTEGER REFERENCES tapes(id),
            PRIMARY KEY (library_id, drive_id)
        );
        CREATE TABLE IF NOT EXISTS config (
            key TEXT PRIMARY KEY,
            value TEXT
        );
        CREATE TABLE IF NOT EXISTS tags (
            id INTEGER PRIMARY KEY,
            name TEXT UNIQUE NOT NULL
        );
        CREATE TABLE IF NOT EXISTS tape_tags (
            tape_id INTEGER REFERENCES tapes(id),
            tag_id INTEGER REFERENCES tags(id),
            PRIMARY KEY (tape_id, tag_id)
        );
        "#,
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
struct VtlConfig {
    db_path: PathBuf,
    tape_dir: PathBuf,
    log_dir: PathBuf,
    /// 单一日志文件上限；超过后轮转为 `*.1` … `*.5`
    log_max_bytes: u64,
    transport: fab_transport::FabTransport,
    iscsi_iqn: Option<String>,
    iscsi_portals: Option<String>,
    fc_wwpn: Option<String>,
    /// 若设置且 **`kernel_reload_on_db_change=true`**：在 ioctl 不可用或失败时，于 **创建新库**、**删除库**、或 **`library-config` 改 max_drives/slots** 后执行该脚本（**`rmmod`/`insmod`**），参数为 `vtl_instances` 规格串；需 root。**不当重载可导致内核 panic/OOPS 或整机重启**；见 `docs/SCSI.md` §1c 与 `scripts/vtl-kernel-reload.sh`。单次跳过可设 **`VTL_SKIP_KERNEL_RELOAD=1`**。Web/CLI **不直接** `insmod`，仅 **`/bin/sh <script> <spec>`**。
    kernel_vtl_reload_script: Option<PathBuf>,
    /// **`true`**：ioctl 失败（或关闭 ioctl）时允许执行 **`kernel_vtl_reload_script`**。**`false`（默认）**：不执行整模块重载脚本；若 **`kernel_geom_prefer_ioctl=true`**（默认），改库后仍会**自动尝试** **`/dev/vtl` ioctl** 对齐几何（无 `rmmod`/`insmod`）。环境 **`VTL_KERNEL_RELOAD_ON_DB_CHANGE=1`** 可设为 **`true`**。
    kernel_reload_on_db_change: bool,
    /// Linux 上优先通过 **`/dev/vtl`** ioctl 应用 `vtl_instances` 规格（**不**整模块 `rmmod`/`insmod`）；失败或旧内核无该 ioctl 时再回退到 `kernel_vtl_reload_script`（若已配置）。**`VTL_KERNEL_GEOM_IOCTL=0`** 可关闭优先 ioctl。
    kernel_geom_prefer_ioctl: bool,
    /// **`fixed`**（方案 B）：`insmod` 一次加载最大拓扑（默认 **8×8×256**），改库用 **`RESIZE_GEOMETRY` ioctl**；增删 SCSI host 数须维护窗口 **`vtl-kernelctl reload`**。**`legacy`**：与旧版相同（优先热 **`SET_INSTANCES`**）。
    kernel_geometry_mode: KernelGeometryMode,
    /// 传给内核重载钩子的 `VTL_KO`（`vtl-kernel-reload.sh` 中的模块路径）；未设置时由脚本使用默认路径。
    vtl_ko: Option<PathBuf>,
    /// 传给重载钩子的 `VTL_SCAN_DELAY_MS`（与内核模块 `scan_delay_ms` 一致；Kylin 等多驱建议 ≥500）。
    vtl_reload_scan_delay_ms: Option<u32>,
    /// After `load`/`unload`/`assign-slot`, sync kernel changer via `/dev/vtl` ioctl (default true on Linux).
    robot_sync: bool,
    /// After patrol / post-op: auto pull kernel→DB when drift is safe (default true).
    auto_reconcile_pull: bool,
    /// Enable `vtl-robot-sync.timer` periodic kernel→DB catalog hints (default true).
    auto_sync_db_from_kernel: bool,
    /// Kernel module INQUIRY personality (insmod `personality=`): vtl, ibm, stk, hp.
    kernel_personality: String,
}

const DEFAULT_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// 内核几何策略（见 `docs/SCSI.md` §1g 方案 B）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KernelGeometryMode {
    /// `insmod` 固定 8 段 `8x256`；运行期仅 resize ioctl。
    Fixed,
    /// 按 DB 推导规格；优先热 `SET_INSTANCES`（默认 `allow_hot_geom=0` 时返回 EBUSY）。
    Legacy,
}

impl KernelGeometryMode {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "fixed" | "plan_b" | "semi_thin" => Some(Self::Fixed),
            "legacy" | "dynamic" | "hot" => Some(Self::Legacy),
            _ => None,
        }
    }
}

/// 方案 B：`insmod`/`vtl-kernelctl start` 用满配拓扑（8 库 × 8 驱 × 256 槽），配合 **`noscan=1`**。
pub(crate) fn build_plan_b_insmod_spec() -> String {
    std::iter::repeat("8x256")
        .take(VTL_KERNEL_MAX_ONLINE_LIBRARIES)
        .collect::<Vec<_>>()
        .join(",")
}

fn count_vtl_instances_segments(spec: &str) -> usize {
    spec.split(',').filter(|t| !t.trim().is_empty()).count()
}

fn read_loaded_kernel_vtl_host_count() -> Option<usize> {
    let path = "/sys/module/vtl/parameters/vtl_instances";
    let s = std::fs::read_to_string(path).ok()?;
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let n = count_vtl_instances_segments(t);
    if n == 0 {
        None
    } else {
        Some(n)
    }
}

/// 解析 `vtl_instances` 逗号段为 `(drives, slots)`（与内核 NxM 一致）。
pub(crate) fn parse_vtl_instances_segments(spec: &str) -> Vec<(i32, i32)> {
    spec.split(',')
        .filter_map(|tok| {
            let t = tok.trim();
            if t.is_empty() {
                return None;
            }
            let (a, b) = t.split_once('x')?;
            let mut d: i32 = a.trim().parse().ok()?;
            let mut s: i32 = b.trim().parse().ok()?;
            d = d.clamp(1, VTL_KERNEL_MAX_DRIVES_PER_LIB);
            s = s.clamp(1, VTL_KERNEL_MAX_DATA_SLOTS_PER_LIB);
            Some((d, s))
        })
        .collect()
}

fn read_live_kernel_vtl_instances_segments() -> Option<Vec<(i32, i32)>> {
    let path = "/sys/module/vtl/parameters/vtl_instances";
    let s = std::fs::read_to_string(path).ok()?;
    let segs = parse_vtl_instances_segments(s.trim());
    if segs.is_empty() {
        None
    } else {
        Some(segs)
    }
}

fn plan_b_idle_host_segment() -> (i32, i32) {
    (
        VTL_KERNEL_MAX_DRIVES_PER_LIB,
        VTL_KERNEL_MAX_DATA_SLOTS_PER_LIB,
    )
}

/// 方案 B：前若干段为 DB 在线库几何，其余段保留 sysfs 当前值（不把未用 host 缩成 `1x1`）。
pub(crate) fn build_vtl_instances_kernel_spec_padded(n_hosts: usize) -> Result<String, VtlError> {
    let compact = build_vtl_instances_kernel_spec()?;
    let db_parts: Vec<String> = compact
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut live = read_live_kernel_vtl_instances_segments().unwrap_or_default();
    if live.len() < n_hosts {
        let fill = plan_b_idle_host_segment();
        live.resize(n_hosts, fill);
    }
    let mut parts: Vec<String> = Vec::with_capacity(n_hosts);
    for i in 0..n_hosts {
        if i < db_parts.len() {
            parts.push(db_parts[i].clone());
        } else if i < live.len() {
            parts.push(format!("{}x{}", live[i].0, live[i].1));
        } else {
            let (d, s) = plan_b_idle_host_segment();
            parts.push(format!("{}x{}", d, s));
        }
    }
    Ok(parts.join(","))
}

fn merge_env_transport_and_log(config: &mut VtlConfig) {
    if let Ok(s) = std::env::var("VTL_LOG_MAX_BYTES") {
        if let Ok(v) = s.parse::<u64>() {
            config.log_max_bytes = v.max(4096);
        }
    }
    if let Ok(s) = std::env::var("VTL_TRANSPORT") {
        if let Some(t) = fab_transport::parse_fab_transport(&s) {
            config.transport = t;
        }
    }
    if let Ok(s) = std::env::var("VTL_ISCSI_IQN") {
        config.iscsi_iqn = Some(s);
    }
    if let Ok(s) = std::env::var("VTL_ISCSI_PORTALS") {
        config.iscsi_portals = Some(s);
    }
    if let Ok(s) = std::env::var("VTL_FC_WWPN") {
        config.fc_wwpn = Some(s);
    }
    if let Ok(s) = std::env::var("VTL_KERNEL_VTL_RELOAD_SCRIPT") {
        let p = PathBuf::from(s.trim());
        if !p.as_os_str().is_empty() {
            config.kernel_vtl_reload_script = Some(p);
        }
    }
    if let Ok(s) = std::env::var("VTL_KO") {
        let p = PathBuf::from(s.trim());
        if !p.as_os_str().is_empty() {
            config.vtl_ko = Some(p);
        }
    }
    if let Ok(s) = std::env::var("VTL_SCAN_DELAY_MS") {
        if let Ok(v) = s.parse::<u32>() {
            config.vtl_reload_scan_delay_ms = Some(v.min(600_000));
        }
    }
    if let Ok(s) = std::env::var("VTL_KERNEL_RELOAD_ON_DB_CHANGE") {
        let t = s.trim().to_ascii_lowercase();
        if matches!(t.as_str(), "0" | "false" | "no" | "off") {
            config.kernel_reload_on_db_change = false;
        } else if matches!(t.as_str(), "1" | "true" | "yes" | "on") {
            config.kernel_reload_on_db_change = true;
        }
    }
    if let Ok(s) = std::env::var("VTL_KERNEL_GEOM_IOCTL") {
        let t = s.trim().to_ascii_lowercase();
        if matches!(t.as_str(), "0" | "false" | "no" | "off") {
            config.kernel_geom_prefer_ioctl = false;
        } else if matches!(t.as_str(), "1" | "true" | "yes" | "on") {
            config.kernel_geom_prefer_ioctl = true;
        }
    }
    if let Ok(s) = std::env::var("VTL_KERNEL_GEOMETRY_MODE") {
        if let Some(m) = KernelGeometryMode::parse(&s) {
            config.kernel_geometry_mode = m;
        }
    }
}

fn apply_vtl_conf_kv(key: &str, value: &str, config: &mut VtlConfig) {
    match key {
        "db_path" => {
            if std::env::var("VTL_DB_PATH").is_err() {
                config.db_path = PathBuf::from(value);
            }
        }
        "tape_dir" => {
            if std::env::var("VTL_TAPE_DIR").is_err() {
                config.tape_dir = PathBuf::from(value);
            }
        }
        "log_dir" => {
            if std::env::var("VTL_LOG_DIR").is_err() {
                config.log_dir = PathBuf::from(value);
            }
        }
        "log_max_bytes" => {
            if std::env::var("VTL_LOG_MAX_BYTES").is_err() {
                if let Ok(v) = value.parse::<u64>() {
                    config.log_max_bytes = v.max(4096);
                }
            }
        }
        "transport" => {
            if std::env::var("VTL_TRANSPORT").is_err() {
                if let Some(t) = fab_transport::parse_fab_transport(value) {
                    config.transport = t;
                }
            }
        }
        "iscsi_iqn" => {
            if std::env::var("VTL_ISCSI_IQN").is_err() {
                config.iscsi_iqn = Some(value.to_string());
            }
        }
        "iscsi_portals" => {
            if std::env::var("VTL_ISCSI_PORTALS").is_err() {
                config.iscsi_portals = Some(value.to_string());
            }
        }
        "fc_wwpn" => {
            if std::env::var("VTL_FC_WWPN").is_err() {
                config.fc_wwpn = Some(value.to_string());
            }
        }
        "kernel_vtl_reload_script" => {
            if std::env::var("VTL_KERNEL_VTL_RELOAD_SCRIPT").is_err() {
                let p = PathBuf::from(value.trim());
                config.kernel_vtl_reload_script = if p.as_os_str().is_empty() {
                    None
                } else {
                    Some(p)
                };
            }
        }
        "vtl_ko" => {
            if std::env::var("VTL_KO").is_err() {
                let p = PathBuf::from(value.trim());
                config.vtl_ko = if p.as_os_str().is_empty() {
                    None
                } else {
                    Some(p)
                };
            }
        }
        "vtl_reload_scan_delay_ms" => {
            if std::env::var("VTL_SCAN_DELAY_MS").is_err() {
                if let Ok(v) = value.trim().parse::<u32>() {
                    config.vtl_reload_scan_delay_ms = Some(v.min(600_000));
                }
            }
        }
        "kernel_reload_on_db_change" => {
            if std::env::var("VTL_KERNEL_RELOAD_ON_DB_CHANGE").is_err() {
                let t = value.trim().to_ascii_lowercase();
                if matches!(t.as_str(), "0" | "false" | "no" | "off") {
                    config.kernel_reload_on_db_change = false;
                } else if matches!(t.as_str(), "1" | "true" | "yes" | "on") {
                    config.kernel_reload_on_db_change = true;
                }
            }
        }
        "kernel_geom_prefer_ioctl" => {
            if std::env::var("VTL_KERNEL_GEOM_IOCTL").is_err() {
                let t = value.trim().to_ascii_lowercase();
                if matches!(t.as_str(), "0" | "false" | "no" | "off") {
                    config.kernel_geom_prefer_ioctl = false;
                } else if matches!(t.as_str(), "1" | "true" | "yes" | "on") {
                    config.kernel_geom_prefer_ioctl = true;
                }
            }
        }
        "kernel_geometry_mode" => {
            if std::env::var("VTL_KERNEL_GEOMETRY_MODE").is_err() {
                if let Some(m) = KernelGeometryMode::parse(value) {
                    config.kernel_geometry_mode = m;
                }
            }
        }
        "robot_sync" => {
            if std::env::var("VTL_ROBOT_SYNC").is_err() {
                let t = value.trim().to_ascii_lowercase();
                if matches!(t.as_str(), "0" | "false" | "no" | "off") {
                    config.robot_sync = false;
                } else if matches!(t.as_str(), "1" | "true" | "yes" | "on") {
                    config.robot_sync = true;
                }
            }
        }
        "robot_authority" | "auto_reconcile_apply" => {
            eprintln!(
                "Warning: vtl.conf key '{}' is removed (runtime robot is always vtl.ko); line ignored",
                key
            );
        }
        "auto_reconcile_pull" => {
            if std::env::var("VTL_AUTO_RECONCILE_PULL").is_err() {
                let t = value.trim().to_ascii_lowercase();
                if matches!(t.as_str(), "0" | "false" | "no" | "off") {
                    config.auto_reconcile_pull = false;
                } else if matches!(t.as_str(), "1" | "true" | "yes" | "on") {
                    config.auto_reconcile_pull = true;
                }
            }
        }
        "auto_sync_db_from_kernel" => {
            if std::env::var("VTL_AUTO_SYNC_DB_FROM_KERNEL").is_err() {
                let t = value.trim().to_ascii_lowercase();
                if matches!(t.as_str(), "0" | "false" | "no" | "off") {
                    config.auto_sync_db_from_kernel = false;
                } else if matches!(t.as_str(), "1" | "true" | "yes" | "on") {
                    config.auto_sync_db_from_kernel = true;
                }
            }
        }
        "personality" | "kernel_personality" => {
            if std::env::var("VTL_PERSONALITY").is_err() {
                let t = value.trim();
                if !t.is_empty() {
                    config.kernel_personality = t.to_string();
                }
            }
        }
        _ => {}
    }
}

/// 0-based index of an online library in the kernel `vtl_instances` segment order (`ORDER BY id ASC`, same as [`build_vtl_instances_kernel_spec`]).
pub(crate) fn online_library_export_index(
    conn: &rusqlite::Connection,
    library_id: i64,
) -> Result<usize, VtlError> {
    let mut stmt =
        conn.prepare("SELECT id FROM vtl_libraries WHERE name NOT IN (?1, ?2) ORDER BY id ASC")?;
    let ids: Vec<i64> = stmt
        .query_map(
            params![OFFLINE_LIBRARY_NAME, LEGACY_DEFAULT_LIBRARY_NAME],
            |r| r.get(0),
        )?
        .collect::<Result<_, _>>()?;
    ids.iter()
        .position(|&id| id == library_id)
        .ok_or_else(|| VtlError::LibraryNotFound(format!("library_id={}", library_id)))
}

/// Comma-separated `drivesxslots` per online library for kernel `vtl_instances=...` (order by DB `id`).
pub(crate) fn build_vtl_instances_kernel_spec() -> Result<String, VtlError> {
    let conn = init_db()?;
    let mut stmt =
        conn.prepare("SELECT id FROM vtl_libraries WHERE name NOT IN (?1, ?2) ORDER BY id ASC")?;
    let mut ids: Vec<i64> = stmt
        .query_map(
            params![OFFLINE_LIBRARY_NAME, LEGACY_DEFAULT_LIBRARY_NAME],
            |r| r.get(0),
        )?
        .collect::<Result<_, _>>()?;
    if ids.len() > VTL_KERNEL_MAX_ONLINE_LIBRARIES {
        log_message(&format!(
            "build_vtl_instances_kernel_spec: {} online libraries, using first {}",
            ids.len(),
            VTL_KERNEL_MAX_ONLINE_LIBRARIES
        ));
        ids.truncate(VTL_KERNEL_MAX_ONLINE_LIBRARIES);
    }
    if ids.is_empty() {
        return Err(VtlError::InvalidParameter(
            "无可用在线磁带库（已排除 __offline__ 与遗留 default）；请 vtladm library create NAME --drives N --slots M"
                .to_string(),
        ));
    }
    let mut parts = Vec::new();
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
            let name: String = conn.query_row(
                "SELECT name FROM vtl_libraries WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )?;
            log_message(&format!(
                "build_vtl_instances_kernel_spec: skip library '{}' (no drives/slots rows); delete or configure it",
                name
            ));
            continue;
        }
        let d = (drives as i32).clamp(1, VTL_KERNEL_MAX_DRIVES_PER_LIB);
        let s = (slots as i32).clamp(1, VTL_KERNEL_MAX_DATA_SLOTS_PER_LIB);
        parts.push(format!("{}x{}", d, s));
    }
    if parts.is_empty() {
        return Err(VtlError::InvalidParameter(
            "无在线库具有有效驱动器/槽位几何（或均为空壳库）；请 library create 或 config set max_drives/slots"
                .to_string(),
        ));
    }
    Ok(parts.join(","))
}

fn vtl_inst_spec_cache_path() -> PathBuf {
    primary_vtl_statedir().join(".last_vtl_instances_spec")
}

fn write_vtl_inst_spec_cache(spec: &str) {
    let p = vtl_inst_spec_cache_path();
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&p, spec.trim());
}

/// Result of [`maybe_reload_kernel_vtl_after_db_change`] for Web/API surfaces.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct KernelGeomSync {
    /// e.g. `ioctl_ok`, `rescan_only`, `ioctl_failed`, `script_refused_holders`, `skipped`
    pub kernel_geom: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub kernel_geom_detail: String,
    /// `ok` / `failed` / `skipped` after ioctl/reload when hosts need LUN enumeration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scsi_rescan: Option<String>,
}

impl KernelGeomSync {
    fn status(status: &str, detail: impl Into<String>) -> Self {
        Self {
            kernel_geom: status.to_string(),
            kernel_geom_detail: detail.into(),
            scsi_rescan: None,
        }
    }

    /// ioctl/reload applied: refresh SCSI LUNs so `lsscsi -g` matches DB drives (independent of iSCSI export).
    fn with_post_geom_rescan(mut self, context: &str) -> Self {
        if !matches!(
            self.kernel_geom.as_str(),
            "ioctl_ok" | "reload_ok" | "script_ok"
        ) {
            return self;
        }
        let r = scsi_rescan_vtl::try_scsi_rescan_after_geom_change(context);
        self.scsi_rescan = Some(r.to_string());
        if r == "failed" {
            let base = self.kernel_geom_detail.trim();
            self.kernel_geom_detail = if base.is_empty() {
                "SCSI rescan failed; run vtl-scsi-scan-all-hosts.sh then lsscsi -g".into()
            } else {
                format!(
                    "{base}; SCSI rescan failed — run: sh /opt/vtladm/scripts/vtl-scsi-scan-all-hosts.sh 5"
                )
            };
        }
        self
    }
}

/// Refuse hot geometry ioctl when LIO still exports VTL sg nodes (unless override env set).
fn refuse_ioctl_before_hot_geom() -> Option<String> {
    if std::env::var("VTL_ALLOW_GEOM_IOCTL_WITH_LIO")
        .ok()
        .as_deref()
        == Some("1")
    {
        return None;
    }
    if lio_hold::lio_pscsi_references_vtl_sg() {
        return Some(
            "LIO pscsi still references VTL /dev/sg — library-unexport or targetcli clearconfig before geometry ioctl (override: VTL_ALLOW_GEOM_IOCTL_WITH_LIO=1)".into(),
        );
    }
    None
}

/// Refuse hot geometry ioctl when tape/sg nodes have open handles (unless override env set).
fn refuse_ioctl_if_vtl_holders_busy() -> Option<String> {
    if std::env::var("VTL_FORCE_GEOM_IOCTL").ok().as_deref() == Some("1") {
        return None;
    }
    use scsi_tape_holders::VtlHoldersProbe;
    match scsi_tape_holders::probe_vtl_device_holders() {
        VtlHoldersProbe::Busy => Some(
            "VTL /dev/st*|sg*|sch* in use — stop backup before geometry ioctl (override: VTL_FORCE_GEOM_IOCTL=1)".into(),
        ),
        VtlHoldersProbe::FuserUnavailable => Some(
            "fuser not installed — cannot verify VTL holders before ioctl (install psmisc, or VTL_FORCE_GEOM_IOCTL=1)".into(),
        ),
        VtlHoldersProbe::Clear => None,
    }
}

fn find_vtl_kernelctl_bin() -> Option<PathBuf> {
    const CANDIDATES: &[&str] = &[
        "/opt/vtladm/sbin/vtl-kernelctl",
        "/usr/local/sbin/vtl-kernelctl",
    ];
    for p in CANDIDATES {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Full module reload via `vtl-kernelctl reload` (insmod-time geometry; no hot SET_INSTANCES).
fn run_vtl_kernelctl_reload() -> Result<(), String> {
    let ctl = find_vtl_kernelctl_bin().ok_or_else(|| {
        "vtl-kernelctl not found under /opt/vtladm/sbin or /usr/local/sbin".to_string()
    })?;
    let out = process::Command::new(&ctl)
        .arg("reload")
        .output()
        .map_err(|e| format!("{}: {}", ctl.display(), e))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        Err(format!(
            "{} reload failed (exit {:?}): {}{}",
            ctl.display(),
            out.status.code(),
            stdout.trim(),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(" stderr: {}", stderr.trim())
            }
        ))
    }
}

fn ioctl_error_benign(e: &std::io::Error) -> bool {
    let errno = e.raw_os_error();
    matches!(
        errno,
        Some(libc::ENOTTY) | Some(libc::ENODEV) | Some(libc::ENOENT)
    ) || e.kind() == std::io::ErrorKind::Unsupported
}

/// After library DB changes that affect geometry:
/// 0. If **`/var/lib/vtl/.last_vtl_instances_spec`** matches the DB spec, **`scsi_rescan_vtl_hosts`**
///    runs only (mhVTL-style **`echo "- - -" > scan`** on each `proc_name==vtl` host; no ioctl / hook).
///    Skip with **`VTL_SKIP_SPEC_CACHE=1`** or **`VTL_NO_SCSI_RESCAN_ON_UNCHANGED_SPEC=1`**.
///    **`VTL_SCSI_RESCAN_STAGGER_MS`**: delay **between** host writes only (not after the last).
/// 1. Builds **`vtl_instances`** spec from the DB.
/// 2. If **`kernel_geom_prefer_ioctl`** is true (default on Linux), tries **`/dev/vtl` `VTL_IOCTL_SET_INSTANCES`**
///    first — **even when `kernel_reload_on_db_change` is false`** (automatic light sync, no `rmmod`/`insmod`).
///    On success, writes the spec cache and returns.
/// 3. If ioctl fails with **`EBUSY`** (`allow_hot_geom=0` or scsi devices present) and **`kernel_vtl_reload_script`** is set, runs the reload script (safe rmmod/insmod path).
/// 4. Other ioctl failures: runs **`kernel_vtl_reload_script`** only when **`kernel_reload_on_db_change`** is true.
///
/// **`VTL_SKIP_KERNEL_RELOAD=1`**: skip ioctl, script, and unchanged-spec rescan for this process.
/// See `docs/SCSI.md` §1c / §1e / §1f.
pub(crate) fn maybe_reload_kernel_vtl_after_db_change() -> KernelGeomSync {
    if std::env::var("VTL_SKIP_KERNEL_RELOAD").ok().as_deref() == Some("1") {
        log_message("kernel_vtl_reload_script: skipped (VTL_SKIP_KERNEL_RELOAD=1)");
        return KernelGeomSync::status("skipped", "VTL_SKIP_KERNEL_RELOAD=1");
    }
    let cfg_now = get_config_uncached();

    let plan_b_fixed = cfg_now.kernel_geometry_mode == KernelGeometryMode::Fixed;
    let spec = match if plan_b_fixed {
        read_loaded_kernel_vtl_host_count()
            .and_then(|n| build_vtl_instances_kernel_spec_padded(n).ok())
            .or_else(|| build_vtl_instances_kernel_spec().ok())
    } else {
        build_vtl_instances_kernel_spec().ok()
    } {
        Some(s) => s,
        None => match build_vtl_instances_kernel_spec() {
            Ok(s) => s,
            Err(e) => {
                log_error("kernel_vtl_reload", &format!("build spec: {}", e));
                return KernelGeomSync::status("spec_error", e.to_string());
            }
        },
    };

    if std::env::var("VTL_SKIP_SPEC_CACHE").ok().as_deref() != Some("1")
        && std::env::var("VTL_NO_SCSI_RESCAN_ON_UNCHANGED_SPEC")
            .ok()
            .as_deref()
            != Some("1")
    {
        let cache_path = vtl_inst_spec_cache_path();
        if let Ok(cached) = fs::read_to_string(&cache_path) {
            if cached.trim() == spec.trim() {
                match scsi_rescan_vtl::scsi_rescan_vtl_hosts() {
                    Ok(()) => {
                        log_message(&format!(
                            "kernel: vtl_instances spec unchanged ({}) — SCSI host rescan only (mhVTL-style; see docs/SCSI.md §1f)",
                            spec
                        ));
                        if robot_sync::robot_sync_enabled() {
                            robot_sync::sync_all_online_libraries_after_geom();
                        }
                        let mut sync = KernelGeomSync::status("rescan_only", spec);
                        sync.scsi_rescan = Some("ok".into());
                        return sync;
                    }
                    Err(e) => {
                        log_error(
                            "scsi_rescan_vtl",
                            &format!("{} — falling back to ioctl/hook path", e),
                        );
                    }
                }
            }
        }
    }

    let mut ioctl_hot_geom_disabled = false;

    if cfg_now.kernel_geom_prefer_ioctl {
        if let Some(msg) = refuse_ioctl_before_hot_geom() {
            log_error("kernel_vtl_ioctl", &msg);
            return KernelGeomSync::status("lio_exported", msg);
        }
        if let Some(msg) = refuse_ioctl_if_vtl_holders_busy() {
            log_error("kernel_vtl_ioctl", &msg);
            return KernelGeomSync::status("holders_busy", msg);
        }
        let ioctl_result = if plan_b_fixed {
            kernel_geom_ioctl::try_apply_kernel_geom_resize_via_ioctl(&spec)
        } else {
            kernel_geom_ioctl::try_apply_kernel_vtl_instances_via_ioctl(&spec)
        };
        match ioctl_result {
            Ok(()) => {
                let via = if plan_b_fixed {
                    "RESIZE_GEOMETRY"
                } else {
                    "SET_INSTANCES"
                };
                log_message(&format!(
                    "kernel geometry: applied via /dev/vtl {} ioctl (spec {}) — no full module reload",
                    via, spec
                ));
                write_vtl_inst_spec_cache(&spec);
                if robot_sync::robot_sync_enabled() {
                    robot_sync::sync_all_online_libraries_after_geom();
                }
                return KernelGeomSync::status("ioctl_ok", spec).with_post_geom_rescan("ioctl_ok");
            }
            Err(e) => {
                if e.raw_os_error() == Some(libc::EINVAL) && plan_b_fixed {
                    log_message(
                        "kernel geometry: RESIZE_GEOMETRY EINVAL (SCSI host count != DB libraries) — vtl-kernelctl reload required",
                    );
                    return KernelGeomSync::status(
                        "need_reload",
                        "library count changed; run vtl-kernelctl reload in a maintenance window (Plan B fixed topology)",
                    );
                }
                if !plan_b_fixed && e.raw_os_error() == Some(libc::EBUSY) {
                    if let Ok(()) = kernel_geom_ioctl::try_apply_kernel_geom_resize_via_ioctl(&spec)
                    {
                        log_message(&format!(
                            "kernel geometry: SET_INSTANCES refused; RESIZE_GEOMETRY ok (spec {})",
                            spec
                        ));
                        write_vtl_inst_spec_cache(&spec);
                        if robot_sync::robot_sync_enabled() {
                            robot_sync::sync_all_online_libraries_after_geom();
                        }
                        return KernelGeomSync::status("ioctl_ok", spec)
                            .with_post_geom_rescan("ioctl_resize");
                    }
                }
                if e.raw_os_error() == Some(libc::EBUSY) {
                    ioctl_hot_geom_disabled = true;
                    log_message(
                        "kernel geometry: geometry ioctl refused (EBUSY) — trying kernel_vtl_reload_script or vtl-kernelctl reload",
                    );
                } else if !cfg_now.kernel_reload_on_db_change {
                    if ioctl_error_benign(&e) {
                        log_message(&format!(
                            "kernel geometry: ioctl unavailable ({}) with kernel_reload_on_db_change=false — not running hook script; upgrade vtl.ko for SET_INSTANCES or set kernel_reload_on_db_change=true with kernel_vtl_reload_script (see docs/SCSI.md §1c)",
                            e
                        ));
                        return KernelGeomSync::status("ioctl_unavailable", e.to_string());
                    }
                    log_error(
                        "kernel_vtl_ioctl",
                        &format!(
                            "{} — kernel_reload_on_db_change=false; not running hook script (see docs/SCSI.md §1c)",
                            e
                        ),
                    );
                    return KernelGeomSync::status("ioctl_failed", e.to_string());
                } else if ioctl_error_benign(&e) {
                    log_message(&format!(
                        "kernel geometry: ioctl unavailable ({}); falling back to kernel_vtl_reload_script if configured",
                        e
                    ));
                } else {
                    log_error(
                        "kernel_vtl_ioctl",
                        &format!(
                            "{} (will still try kernel_vtl_reload_script if configured)",
                            e
                        ),
                    );
                }
            }
        }
    } else if !cfg_now.kernel_reload_on_db_change {
        log_message(
            "kernel: skipped align (kernel_geom_prefer_ioctl=false and kernel_reload_on_db_change=false)；维护窗口手动重载或开启其一；见 docs/SCSI.md §1c",
        );
        return KernelGeomSync::status("skipped", "ioctl and script disabled in vtl.conf");
    }

    let script = match cfg_now.kernel_vtl_reload_script {
        Some(p) => p,
        None => {
            if ioctl_hot_geom_disabled {
                if let Some(msg) = refuse_ioctl_before_hot_geom() {
                    return KernelGeomSync::status("lio_exported", msg);
                }
                if let Some(msg) = refuse_ioctl_if_vtl_holders_busy() {
                    return KernelGeomSync::status("holders_busy", msg);
                }
                log_message("kernel geometry: trying vtl-kernelctl reload (allow_hot_geom=0)");
                match run_vtl_kernelctl_reload() {
                    Ok(()) => {
                        write_vtl_inst_spec_cache(&spec);
                        if robot_sync::robot_sync_enabled() {
                            robot_sync::sync_all_online_libraries_after_geom();
                        }
                        return KernelGeomSync::status("reload_ok", spec)
                            .with_post_geom_rescan("reload_ok");
                    }
                    Err(e) => {
                        log_error("kernel_vtl_reload", &e);
                        return KernelGeomSync::status(
                            "hot_geom_disabled",
                            format!(
                                "{} — or configure kernel_vtl_reload_script={}",
                                e, DEFAULT_INIT_KERNEL_RELOAD_SCRIPT
                            ),
                        );
                    }
                }
            }
            log_message(
                "kernel_vtl_reload_script: not configured; ioctl failed or disabled and no rmmod/insmod fallback — set kernel_vtl_reload_script or upgrade vtl.ko with /dev/vtl SET_INSTANCES (see docs/SCSI.md)",
            );
            return KernelGeomSync::status(
                "no_fallback",
                "ioctl failed or disabled; no kernel_vtl_reload_script",
            );
        }
    };

    if scsi_tape_holders::refuse_rmmod_for_safety() {
        use scsi_tape_holders::VtlHoldersProbe;
        let msg = match scsi_tape_holders::probe_vtl_device_holders() {
            VtlHoldersProbe::FuserUnavailable => {
                "fuser not installed (install psmisc); use ioctl or maintenance window"
            }
            VtlHoldersProbe::Busy => {
                "VTL /dev/st*|sg*|sch* in use (Kylin: rmmod while open can kdump)"
            }
            VtlHoldersProbe::Clear => "holder check",
        };
        log_error(
            "kernel_vtl_reload",
            &format!(
                "refusing kernel_vtl_reload_script: {}. Override only with VTL_FORCE_RMMOD=1.",
                msg
            ),
        );
        let status = match scsi_tape_holders::probe_vtl_device_holders() {
            VtlHoldersProbe::FuserUnavailable => "script_refused_no_fuser",
            _ => "script_refused_holders",
        };
        return KernelGeomSync::status(status, msg);
    }

    log_message(&format!(
        "kernel_vtl_reload_script: running {} with spec {} (rmmod/insmod; see docs/SCSI.md §1c)",
        script.display(),
        spec
    ));
    eprintln!(
        "vtladm: 即将执行 kernel_vtl_reload_script={}，vtl_instances 规格={}（内含 rmmod/insmod，可能导致整机重启）。可设 VTL_SKIP_KERNEL_RELOAD=1 跳过。详见 userspace/docs/SCSI.md §1c。",
        script.display(),
        spec
    );
    let mut cmd = process::Command::new("/bin/sh");
    cmd.arg(&script).arg(&spec);
    if let Some(ref ko) = cfg_now.vtl_ko {
        cmd.env("VTL_KO", ko);
    }
    if let Some(ms) = cfg_now.vtl_reload_scan_delay_ms {
        cmd.env("VTL_SCAN_DELAY_MS", ms.to_string());
    }
    let run = cmd.status();
    match run {
        Ok(s) if s.success() => {
            log_message(&format!("kernel_vtl_reload_script OK ({})", spec));
            write_vtl_inst_spec_cache(&spec);
            if robot_sync::robot_sync_enabled() {
                robot_sync::sync_all_online_libraries_after_geom();
            }
            KernelGeomSync::status("script_ok", spec).with_post_geom_rescan("script_ok")
        }
        Ok(s) => {
            let detail = format!("{} exited {:?}", script.display(), s.code());
            log_error("kernel_vtl_reload", &detail);
            KernelGeomSync::status("script_failed", detail)
        }
        Err(e) => {
            let detail = format!("{}: {}", script.display(), e);
            log_error("kernel_vtl_reload", &detail);
            KernelGeomSync::status("script_failed", detail)
        }
    }
}

const MAILSLOT_OFFSET: i32 = 100;
const MAX_SLOTS: i32 = 1000;
const MAX_DRIVES: i32 = 100;

fn get_config_uncached() -> VtlConfig {
    let mut config = VtlConfig {
        db_path: PathBuf::from(DEFAULT_INIT_DB_PATH),
        tape_dir: PathBuf::from(DEFAULT_INIT_TAPE_DIR),
        log_dir: PathBuf::from(DEFAULT_INIT_LOG_DIR),
        log_max_bytes: DEFAULT_LOG_MAX_BYTES,
        transport: fab_transport::FabTransport::default(),
        iscsi_iqn: None,
        iscsi_portals: None,
        fc_wwpn: None,
        kernel_vtl_reload_script: None,
        kernel_reload_on_db_change: false,
        kernel_geom_prefer_ioctl: true,
        kernel_geometry_mode: KernelGeometryMode::Legacy,
        vtl_ko: None,
        vtl_reload_scan_delay_ms: None,
        robot_sync: true,
        #[cfg(test)]
        auto_reconcile_pull: false,
        #[cfg(not(test))]
        auto_reconcile_pull: true,
        #[cfg(test)]
        auto_sync_db_from_kernel: false,
        #[cfg(not(test))]
        auto_sync_db_from_kernel: true,
        kernel_personality: "vtl".to_string(),
    };

    if let Ok(env_db) = std::env::var("VTL_DB_PATH") {
        config.db_path = PathBuf::from(env_db);
    }
    if let Ok(env_tape) = std::env::var("VTL_TAPE_DIR") {
        config.tape_dir = PathBuf::from(env_tape);
    }
    if let Ok(env_log) = std::env::var("VTL_LOG_DIR") {
        config.log_dir = PathBuf::from(env_log);
    }

    merge_env_transport_and_log(&mut config);

    if config.db_path.starts_with("/tmp/") {
        return config;
    }
    if std::env::var("VTL_USE_ENV_ONLY").ok().as_deref() == Some("1") {
        return config;
    }

    // 唯一主配置路径（与 Web 向导、文档一致；不再读取 /etc 或相对路径）
    let conf_path = primary_vtl_conf_path();
    if conf_path.exists() {
        if let Ok(content) = fs::read_to_string(&conf_path) {
            for line in content.lines() {
                let line = line.trim();
                if line.starts_with('#') || line.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = line.splitn(2, '=').collect();
                if parts.len() != 2 {
                    continue;
                }
                let key = parts[0].trim();
                let value = parts[1].trim();
                apply_vtl_conf_kv(key, value, &mut config);
            }
        }
    }

    merge_env_transport_and_log(&mut config);
    config
}

#[cfg(not(test))]
static CONFIG_CACHE: Mutex<Option<VtlConfig>> = Mutex::new(None);

/// Web 完成首次写入 `vtl.conf` 等之后调用，使后续 `get_config()` 重新读盘。
pub(crate) fn invalidate_vtl_config_cache() {
    #[cfg(not(test))]
    if let Ok(mut g) = CONFIG_CACHE.lock() {
        *g = None;
    }
}

pub(crate) fn get_config() -> VtlConfig {
    #[cfg(test)]
    {
        get_config_uncached()
    }
    #[cfg(not(test))]
    {
        let mut guard = CONFIG_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref c) = *guard {
            return c.clone();
        }
        let fresh = get_config_uncached();
        *guard = Some(fresh.clone());
        fresh
    }
}

/// 生产安装前缀（与 `install.sh` / `/opt/vtladm` 布局一致；单元测试校验路径落在此树下）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const VTL_INSTALL_PREFIX: &str = "/opt/vtladm";
/// 生产环境主配置文件路径（Linux 首次向导依赖其是否存在）。
pub(crate) const PRIMARY_VTL_CONF: &str = "/opt/vtladm/var/vtl.conf";
pub(crate) const PRIMARY_VTL_STATEDIR: &str = "/opt/vtladm/var";

pub(crate) const DEFAULT_INIT_DB_PATH: &str = "/opt/vtladm/var/vtl.db";
pub(crate) const DEFAULT_INIT_TAPE_DIR: &str = "/opt/vtladm/var/tapes";
pub(crate) const DEFAULT_INIT_LOG_DIR: &str = "/opt/vtladm/var/log/vtl";
pub(crate) const DEFAULT_INIT_KERNEL_RELOAD_SCRIPT: &str =
    "/opt/vtladm/scripts/vtl-kernel-reload.sh";
pub(crate) const DEFAULT_INIT_VTL_KO: &str = "/opt/vtladm/ko/vtl.ko";

/// Update or append one `key=value` line in the primary `vtl.conf`.
pub(crate) fn update_primary_vtl_conf_kv(key: &str, value: &str) -> Result<(), VtlError> {
    let path = primary_vtl_conf_path();
    let mut lines: Vec<String> = if path.exists() {
        fs::read_to_string(&path)?
            .lines()
            .map(|l| l.to_string())
            .collect()
    } else {
        Vec::new()
    };
    let prefix = format!("{}=", key);
    let mut found = false;
    for line in lines.iter_mut() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with(&prefix) {
            *line = format!("{}={}", key, value);
            found = true;
            break;
        }
    }
    if !found {
        lines.push(format!("{}={}", key, value));
    }
    fs::write(&path, format!("{}\n", lines.join("\n")))?;
    invalidate_vtl_config_cache();
    Ok(())
}

/// 主配置路径：`VTL_CONF_PATH` 优先，否则 [`PRIMARY_VTL_CONF`]。
pub(crate) fn primary_vtl_conf_path() -> PathBuf {
    if let Ok(p) = std::env::var("VTL_CONF_PATH") {
        if !p.trim().is_empty() {
            return PathBuf::from(p.trim());
        }
    }
    PathBuf::from(PRIMARY_VTL_CONF)
}

/// 状态目录（`.last_vtl_instances_spec` 等）：主配置所在目录。
pub(crate) fn primary_vtl_statedir() -> PathBuf {
    primary_vtl_conf_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(PRIMARY_VTL_STATEDIR))
}

/// Web 首次配置向导：在 Linux 且未设置 `VTL_USE_ENV_ONLY=1` 时，若主配置不存在则须先完成向导。
pub(crate) fn setup_gate_active() -> bool {
    #[cfg(test)]
    {
        return false;
    }
    #[cfg(not(test))]
    {
        if std::env::var("VTL_USE_ENV_ONLY").ok().as_deref() == Some("1") {
            return false;
        }
        !primary_vtl_conf_path().exists()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InitialVtlConfParams {
    pub db_path: PathBuf,
    pub tape_dir: PathBuf,
    pub log_dir: PathBuf,
    pub kernel_vtl_reload_script: Option<PathBuf>,
    pub vtl_ko: Option<PathBuf>,
    pub vtl_reload_scan_delay_ms: Option<u32>,
}

fn vtl_conf_template_suffix() -> &'static str {
    r#"

# Single log file size before rotation (bytes, min 4096)
# log_max_bytes=10485760

# Fabric: local | iscsi | fc (see docs/TRANSPORT.md; vtladm does not start iSCSI/FC targets)
# transport=local

# Optional when planning iSCSI / FC (informational)
# iscsi_iqn=iqn.2003-01.org.linux-iscsi.example:vtl
# iscsi_portals=0.0.0.0:3260
# fc_wwpn=10:00:00:00:00:00:00:01

# Optional: passed to kernel_vtl_reload_script as VTL_SCAN_DELAY_MS (insmod scan_delay_ms=…; default 500 in script)
# vtl_reload_scan_delay_ms=500

# Default: kernel_reload_on_db_change=false skips rmmod/insmod hook script on DB changes; ioctl may still apply if kernel_geom_prefer_ioctl=true (see docs/SCSI.md §1c). Env: VTL_KERNEL_RELOAD_ON_DB_CHANGE=0|1
# kernel_reload_on_db_change=false
#
# If you enable kernel_reload_on_db_change=true: keep kernel_geom_prefer_ioctl=true (default) and use a vtl.ko with /dev/vtl SET_INSTANCES (see docs/SCSI.md §1e) so ioctl runs first and avoids full rmmod/insmod when possible; still set kernel_vtl_reload_script as fallback.

# Optional: if false, do NOT try /dev/vtl ioctl first to apply vtl_instances (no full module reload); go straight to kernel_vtl_reload_script when set. Env: VTL_KERNEL_GEOM_IOCTL=0
# kernel_geom_prefer_ioctl=false
"#
}

pub(crate) fn format_initial_vtl_conf(params: &InitialVtlConfParams) -> String {
    let mut body = String::from("# VTL Configuration File\n");
    body.push_str("# Database path\n");
    body.push_str(&format!("db_path={}\n\n", params.db_path.display()));
    body.push_str("# Tape image directory\n");
    body.push_str(&format!("tape_dir={}\n\n", params.tape_dir.display()));
    body.push_str("# Log directory\n");
    body.push_str(&format!("log_dir={}\n", params.log_dir.display()));
    if let Some(ref p) = params.kernel_vtl_reload_script {
        body.push_str(&format!("\nkernel_vtl_reload_script={}\n", p.display()));
    }
    if let Some(ref p) = params.vtl_ko {
        body.push_str(&format!("\nvtl_ko={}\n", p.display()));
    }
    if let Some(ms) = params.vtl_reload_scan_delay_ms {
        body.push_str(&format!("\nvtl_reload_scan_delay_ms={}\n", ms));
    }
    body.push_str(
        "\n# Default: no rmmod/insmod hook on library DB changes (kernel_reload_on_db_change=false).\n# With kernel_geom_prefer_ioctl=true (default), vtladm still auto-tries /dev/vtl ioctl after DB changes when supported.\n# Refused while LIO exports VTL /dev/sg or tape nodes are open — unexport first or vtl-kernelctl reload in maintenance.\nkernel_reload_on_db_change=false\n# Plan B: insmod max topology once; idle SCSI hosts keep sysfs geometry (not 1x1). See docs/SCSI.md §1g.\n# kernel_geometry_mode=fixed\n# SCSI INQUIRY for backup software: vtl | ibm | stk | hp (requires vtl-kernelctl reload).\npersonality=ibm\n# Runtime mechanical hand is always vtl.ko; SQLite is tape catalog only.\nrobot_sync=true\nauto_reconcile_pull=true\n# Periodic kernel→DB catalog hints (vtl-robot-sync.timer, every 5 min).\nauto_sync_db_from_kernel=true\n",
    );
    body.push_str(vtl_conf_template_suffix());
    body
}

/// 创建状态目录（默认 `/opt/vtladm/var`）、数据/磁带/日志目录，并写入主配置。
pub(crate) fn write_initial_vtl_conf(params: &InitialVtlConfParams) -> Result<(), VtlError> {
    fs::create_dir_all(primary_vtl_statedir())?;
    if let Some(parent) = params.db_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir_all(&params.tape_dir)?;
    fs::create_dir_all(&params.log_dir)?;
    fs::write(primary_vtl_conf_path(), format_initial_vtl_conf(params))?;
    Ok(())
}

/// Web 向导提交：校验路径、写主配置、刷新配置缓存。
///
/// `run_kernel_reload_now`：为 **`true`** 时在本函数末尾调用 [`maybe_reload_kernel_vtl_after_db_change`]（ioctl / 脚本，含 **`rmmod`/`insmod`** 风险）。
/// 默认 **`false`**，避免「仅填写脚本路径保存配置」即触发整机高风险操作。
pub(crate) fn try_complete_primary_vtl_setup_from_web(
    db_path: &str,
    tape_dir: &str,
    log_dir: &str,
    kernel_vtl_reload_script: &str,
    vtl_ko: &str,
    vtl_reload_scan_delay_ms: &str,
    run_kernel_reload_now: bool,
) -> Result<KernelGeomSync, String> {
    if std::env::var("VTL_USE_ENV_ONLY").ok().as_deref() == Some("1") {
        return Err("已启用 VTL_USE_ENV_ONLY，不能使用 Web 主配置向导".into());
    }
    if primary_vtl_conf_path().exists() {
        return Err("主配置文件已存在，无需重复初始化".into());
    }
    let db = normalize_setup_path(db_path, DEFAULT_INIT_DB_PATH)?;
    let tape = normalize_setup_path(tape_dir, DEFAULT_INIT_TAPE_DIR)?;
    let logd = normalize_setup_path(log_dir, DEFAULT_INIT_LOG_DIR)?;
    let script = optional_abs_path(kernel_vtl_reload_script)?;
    let ko = optional_abs_path(vtl_ko)?;
    let scan_ms = optional_scan_delay_ms(vtl_reload_scan_delay_ms)?;
    let params = InitialVtlConfParams {
        db_path: db,
        tape_dir: tape,
        log_dir: logd,
        kernel_vtl_reload_script: script,
        vtl_ko: ko,
        vtl_reload_scan_delay_ms: scan_ms,
    };
    write_initial_vtl_conf(&params).map_err(|e| e.to_string())?;
    invalidate_vtl_config_cache();
    if run_kernel_reload_now {
        Ok(maybe_reload_kernel_vtl_after_db_change())
    } else {
        Ok(KernelGeomSync::status(
            "skipped",
            "run_kernel_reload_now=false",
        ))
    }
}

fn normalize_setup_path(input: &str, default: &str) -> Result<PathBuf, String> {
    let t = input.trim();
    if t.is_empty() {
        return Ok(PathBuf::from(default));
    }
    let p = PathBuf::from(t);
    if !p.is_absolute() {
        return Err(format!("路径须为绝对路径: {}", t));
    }
    Ok(p)
}

fn optional_scan_delay_ms(input: &str) -> Result<Option<u32>, String> {
    let t = input.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let v = t
        .parse::<u32>()
        .map_err(|_| format!("vtl_reload_scan_delay_ms 须为非负整数: {}", t))?;
    if v > 600_000 {
        return Err("vtl_reload_scan_delay_ms 超过 600000（10 分钟）".into());
    }
    Ok(Some(v))
}

fn optional_abs_path(input: &str) -> Result<Option<PathBuf>, String> {
    let t = input.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let p = PathBuf::from(t);
    if !p.is_absolute() {
        return Err(format!("路径须为绝对路径: {}", t));
    }
    Ok(Some(p))
}

/// 日志轮转深度：`name.1` … `name.5`
const LOG_ROTATE_DEPTH: u32 = 5;

fn maybe_rotate_log_file(
    log_dir: &Path,
    file_name: &str,
    line_len: u64,
    max_bytes: u64,
) -> std::io::Result<()> {
    let max_bytes = max_bytes.max(4096);
    let path = log_dir.join(file_name);
    if !path.exists() {
        return Ok(());
    }
    let len = fs::metadata(&path)?.len();
    if len.saturating_add(line_len) <= max_bytes {
        return Ok(());
    }

    let oldest = log_dir.join(format!("{}.{}", file_name, LOG_ROTATE_DEPTH));
    let _ = fs::remove_file(&oldest);
    for i in (1..LOG_ROTATE_DEPTH).rev() {
        let from = log_dir.join(format!("{}.{}", file_name, i));
        let to = log_dir.join(format!("{}.{}", file_name, i + 1));
        if from.exists() {
            if to.exists() {
                fs::remove_file(&to)?;
            }
            fs::rename(&from, &to)?;
        }
    }
    let first_rot = log_dir.join(format!("{}.1", file_name));
    if first_rot.exists() {
        fs::remove_file(&first_rot)?;
    }
    fs::rename(&path, &first_rot)?;
    Ok(())
}

fn try_append_log_line_in(
    log_dir: &Path,
    file_name: &str,
    line: &str,
    max_bytes: u64,
) -> std::io::Result<()> {
    fs::create_dir_all(log_dir)?;
    let line_len = line.len() as u64 + 1;
    maybe_rotate_log_file(log_dir, file_name, line_len, max_bytes)?;
    let path = log_dir.join(file_name);
    let mut file = File::options().append(true).create(true).open(&path)?;
    writeln!(file, "{}", line)
}

fn log_message(msg: &str) {
    let config = get_config();
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let line = format!("[{}] {}", timestamp, msg);
    if let Err(e) =
        try_append_log_line_in(&config.log_dir, "vtladm.log", &line, config.log_max_bytes)
    {
        eprintln!("Warning: Failed to write to log: {}", e);
    }
}

fn log_error(msg: &str, error: &str) {
    let config = get_config();
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let log_entry = format!("[{}] ERROR: {} - {}", timestamp, msg, error);
    match try_append_log_line_in(
        &config.log_dir,
        "vtladm_errors.log",
        &log_entry,
        config.log_max_bytes,
    ) {
        Ok(()) => eprintln!("{}", log_entry),
        Err(e) => eprintln!("Warning: Failed to write to error log: {}", e),
    }
}

#[derive(Parser)]
#[command(name = "vtladm")]
#[command(about = "VTL (Virtual Tape Library) Administration Tool")]
struct Cli {
    /// 当前操作的虚拟磁带库名称（可建多个命名库）
    /// 当前操作的在线库；省略时使用第一个在线库（不含 __offline__ / default）
    #[arg(short = 'L', long = "library", global = true)]
    library: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    CreateTape {
        name: String,
        #[arg(short, long, default_value = "100G")]
        size: String,
        /// 放入指定磁带架；默认放入「未使用」架
        #[arg(short = 'S', long = "shelf")]
        shelf: Option<String>,
        #[arg(short, long)]
        tags: Vec<String>,
    },
    DeleteTape {
        name: String,
    },
    /// 将磁带清空为「出厂」状态：`used_bytes` 置 0，镜像截断为标称容量。磁带须在**货架上**（未入机械手槽）、且**不在驱动中**。与入槽/回架一致：在驱动内为 `TapeInDrive`，未在货架（含仅在槽内）为 `TapeNotOnShelf`。
    InitTape {
        name: String,
    },
    ListTapes,
    Load {
        source: String,
        target: String,
    },
    Unload {
        drive: String,
    },
    Eject {
        slot: String,
    },
    /// 将 DB 槽位/装带状态同步到内核机械手（`/dev/vtl` ioctl）。
    Robot {
        #[command(subcommand)]
        subcommand: RobotSubcommand,
    },
    Config {
        #[command(subcommand)]
        subcommand: ConfigSubcommand,
    },
    Inventory,
    Status,
    Snapshot {
        tape: String,
        snapshot: String,
    },
    Import {
        path: String,
        slot: String,
    },
    Export {
        slot: String,
        output: String,
        /// 导出完成后写入 `<output>.sha256`（GNU `sha256sum` 兼容的一行摘要）
        #[arg(long)]
        checksum: bool,
    },
    CreateLibrary {
        name: String,
        #[arg(short, long, default_value = "2")]
        drives: i32,
        #[arg(short, long, default_value = "10")]
        slots: i32,
    },
    Library {
        #[command(subcommand)]
        subcommand: LibrarySubcommand,
    },
    Shelf {
        #[command(subcommand)]
        subcommand: ShelfSubcommand,
    },
    /// 将磁带从磁带架移入机械手槽位
    AssignSlot {
        tape: String,
        slot: String,
    },
    InitConfig,
    BatchCreate {
        #[arg(short, long)]
        count: i32,
        #[arg(short, long, default_value = "100G")]
        size: String,
        #[arg(short, long)]
        prefix: String,
        #[arg(short, long)]
        tags: Vec<String>,
    },
    BatchImport {
        #[arg(short, long)]
        directory: String,
        #[arg(short, long)]
        start_slot: i32,
    },
    Quota {
        #[command(subcommand)]
        subcommand: QuotaSubcommand,
    },
    Tag {
        #[command(subcommand)]
        subcommand: TagSubcommand,
    },
    Search {
        #[arg(short, long)]
        name: Option<String>,
        #[arg(short, long)]
        tag: Option<String>,
        #[arg(short, long)]
        min_size: Option<String>,
        #[arg(short, long)]
        max_size: Option<String>,
        #[arg(short, long)]
        free_space: Option<bool>,
    },
    /// SCSI / FC / iSCSI 部署意图与检查（见 docs/TRANSPORT.md）
    Transport {
        #[command(subcommand)]
        subcommand: TransportSubcommand,
    },
    /// 启动 Web 界面（须登录；默认仅监听本机）
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value = "8765")]
        port: u16,
    },
    /// 全栈巡检（内核 / SCSI / DB / 自动 sync-db）；退出码 0/1/2 供 systemd 与监控使用。
    Patrol,
    /// 输出 `vtl_instances` 规格（供 `vtl-kernelctl` / `VTL_INSTANCES`）。**`--insmod-max`**：方案 B 满配 **`8x256×8`**。
    KernelSpec {
        #[arg(
            long,
            help = "Plan B: 8 libraries × 8 drives × 256 slots insmod topology"
        )]
        insmod_max: bool,
    },
    /// 按 DB 对齐内核几何（ioctl / 脚本，遵守 LIO/持有者安全门控）。
    KernelAlign {
        #[arg(long)]
        quiet: bool,
    },
    /// 重置 Web 登录（重写 log_dir/web_admin.json，默认用户 admin）
    ResetWebAuth {
        /// 新密码（至少 8 字符）；省略则恢复默认初始密码
        #[arg(long)]
        password: Option<String>,
    },
}

#[derive(Subcommand)]
enum TransportSubcommand {
    Show,
    Check,
    Guide,
}

#[derive(Subcommand)]
enum LibrarySubcommand {
    List,
    Create {
        name: String,
        #[arg(short, long, default_value = "2")]
        drives: i32,
        #[arg(short, long, default_value = "10")]
        slots: i32,
    },
    /// 删除命名在线库（不可删 `__offline__`；至少保留一个在线库）
    Delete {
        name: String,
    },
}

#[derive(Subcommand)]
enum ShelfSubcommand {
    List,
    Create {
        name: String,
    },
    /// 列出磁带架上的磁带（默认列出所有架）
    Tapes {
        #[arg(short, long)]
        shelf: Option<String>,
    },
    /// 将磁带移到磁带架（须先不在驱动器中）
    Place {
        tape: String,
        #[arg(short, long)]
        shelf: Option<String>,
    },
    /// 删除自建货架（「未使用」默认架不可删；架上须无磁带）
    Delete {
        name: String,
    },
}

#[derive(Subcommand)]
enum RobotSubcommand {
    /// 将当前库 DB 中所有已入槽磁带同步到内核机械手槽位。
    Sync,
    /// 比较 DB 与内核介质位置；`--apply` 以 DB 为准修复内核，`--pull` 以内核为准写回 DB。
    Reconcile {
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        pull: bool,
    },
    /// 自动对齐：离架介质从内核撤出，并按配置 apply/pull 修复漂移。
    AutoAlign,
    /// 对所有在线库执行 `auto-align`。
    AutoAlignAll,
    /// 备份软件模式：将各在线库内核机械手状态同步到 DB（供定时任务 `vtl-robot-sync` 调用）。
    SyncDb,
}

#[derive(Subcommand)]
enum ConfigSubcommand {
    Set {
        #[arg(required = true)]
        params: Vec<String>,
    },
    Show,
}

#[derive(Subcommand)]
enum QuotaSubcommand {
    Set {
        #[arg(short, long)]
        max_total_size: Option<String>,
        #[arg(short, long)]
        max_tapes: Option<i32>,
    },
    Show,
    Check,
}

#[derive(Subcommand)]
enum TagSubcommand {
    Add {
        #[arg(short, long)]
        tape: String,
        #[arg(short, long)]
        tags: Vec<String>,
    },
    Remove {
        #[arg(short, long)]
        tape: String,
        #[arg(short, long)]
        tags: Vec<String>,
    },
    List {
        #[arg(short, long)]
        tape: Option<String>,
    },
    Delete {
        #[arg(short, long)]
        tag: String,
    },
}

fn get_db_path() -> PathBuf {
    get_config().db_path.clone()
}

fn get_tape_dir() -> PathBuf {
    get_config().tape_dir.clone()
}

fn init_db() -> Result<Connection, VtlError> {
    let db_path = get_db_path();
    if let Some(parent) = db_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    log_message(&format!("Opening database: {}", db_path.display()));
    let mut conn = Connection::open(&db_path)?;
    // Web 等多连接并发写库时避免瞬时 SQLITE_BUSY 直接失败（仍可能超时后报错）。
    conn.busy_timeout(std::time::Duration::from_secs(8))?;
    conn.execute("PRAGMA foreign_keys = ON", [])?;

    let tapes_exists = table_exists(&conn, "tapes")?;
    if !tapes_exists {
        create_fresh_v2_schema(&conn)?;
        ensure_offline_library(&conn)?;
    } else if !column_exists(&conn, "tapes", "library_id")? {
        migrate_legacy_to_v2(&mut conn)?;
    } else {
        ensure_vtl_core_tables(&conn)?;
        let _ = ensure_offline_library(&conn);
    }

    conn.execute(
        "CREATE TABLE IF NOT EXISTS config (
            key TEXT PRIMARY KEY,
            value TEXT
        )",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS tags (
            id INTEGER PRIMARY KEY,
            name TEXT UNIQUE NOT NULL
        )",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS tape_tags (
            tape_id INTEGER REFERENCES tapes(id),
            tag_id INTEGER REFERENCES tags(id),
            PRIMARY KEY (tape_id, tag_id)
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tapes_slot ON tapes(slot)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tapes_library ON tapes(library_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_slots_tape_id ON slots(tape_id)",
        [],
    )?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_tags_name ON tags(name)", [])?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tape_tags_tape_id ON tape_tags(tape_id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_tape_tags_tag_id ON tape_tags(tag_id)",
        [],
    )?;

    iscsi_export::ensure_iscsi_exports_table(&conn)?;

    log_message("Database initialized successfully");
    ensure_offline_library(&conn)?;
    Ok(conn)
}

fn parse_size(size_str: &str) -> Result<u64, VtlError> {
    let upper_str = size_str.to_uppercase();
    let size_str = upper_str.trim();
    let mut num_str = String::new();
    let mut unit = String::new();

    for c in size_str.chars() {
        if c.is_ascii_digit() || c == '.' {
            num_str.push(c);
        } else {
            unit.push(c);
        }
    }

    let num: f64 = num_str
        .parse()
        .map_err(|_| VtlError::InvalidSize(format!("Invalid number: {}", num_str)))?;

    let multiplier = match unit.as_str() {
        "" | "B" => 1u64,
        "K" | "KB" => 1024u64,
        "M" | "MB" => 1024u64 * 1024,
        "G" | "GB" => 1024u64 * 1024 * 1024,
        "T" | "TB" => 1024u64 * 1024 * 1024 * 1024,
        _ => return Err(VtlError::InvalidSize(format!("Unknown unit: {}", unit))),
    };

    if !num.is_finite() || num < 0.0 {
        return Err(VtlError::InvalidSize(format!(
            "Invalid number: {}",
            num_str
        )));
    }
    let bytes = num * multiplier as f64;
    if !bytes.is_finite() || bytes > u64::MAX as f64 {
        return Err(VtlError::InvalidSize(format!(
            "Size too large: {}",
            size_str
        )));
    }
    Ok(bytes as u64)
}

fn parse_slot(slot_str: &str) -> Option<i32> {
    let slot_str = slot_str.trim().to_lowercase();
    let value = if slot_str.starts_with("slot") {
        slot_str.trim_start_matches("slot").parse().ok()?
    } else if slot_str.starts_with("mail") {
        let num = slot_str.trim_start_matches("mail").parse::<i32>().ok()?;
        return Some(MAILSLOT_OFFSET + num);
    } else {
        slot_str.parse().ok()?
    };

    if value < 0 || value > MAX_SLOTS {
        return None;
    }
    Some(value)
}

fn parse_drive(drive_str: &str) -> Option<i32> {
    let drive_str = drive_str.trim().to_lowercase();
    let value = if drive_str.starts_with("drive") {
        drive_str.trim_start_matches("drive").parse().ok()?
    } else {
        drive_str.parse().ok()?
    };

    if value < 0 || value > MAX_DRIVES {
        return None;
    }
    Some(value)
}

fn generate_barcode() -> String {
    format!("VTL{:06X}", rand::random::<u32>())
}

fn validate_tape_name(name: &str) -> Result<(), VtlError> {
    if name.is_empty() {
        return Err(VtlError::InvalidTapeName(
            "Tape name cannot be empty".to_string(),
        ));
    }
    if name == "." || name == ".." {
        return Err(VtlError::InvalidTapeName(
            "Tape name cannot be '.' or '..'".to_string(),
        ));
    }
    if name.len() > 255 {
        return Err(VtlError::InvalidTapeName(
            "Tape name too long (max 255 characters)".to_string(),
        ));
    }
    let invalid_chars = ['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'];
    for c in name.chars() {
        if invalid_chars.contains(&c) || c.is_control() {
            return Err(VtlError::InvalidTapeName(format!(
                "Invalid character in tape name: '{}'",
                c
            )));
        }
    }
    Ok(())
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{}K", bytes / 1024)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{}M", bytes / (1024 * 1024))
    } else if bytes < 1024 * 1024 * 1024 * 1024 {
        format!("{}G", bytes / (1024 * 1024 * 1024))
    } else {
        format!("{}T", bytes / (1024 * 1024 * 1024 * 1024))
    }
}

fn create_tape(name: &str, size: u64, shelf_name: Option<&str>) -> Result<(), VtlError> {
    validate_tape_name(name)?;
    check_quota(size)?;
    let lib_name = current_library_name();
    log_message(&format!(
        "Creating tape '{}' in library '{}' with size {}",
        name,
        lib_name,
        format_size(size)
    ));

    let mut conn = init_db()?;
    let library_id = resolve_library_id(&conn, &lib_name)?;
    assert_tape_name_globally_unique(&conn, name, library_id)?;
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tapes WHERE library_id = ?1 AND name = ?2",
        params![library_id, name],
        |r| r.get(0),
    )?;
    if exists > 0 {
        return Err(VtlError::InvalidParameter(format!(
            "磁带名 '{}' 在本库中已存在",
            name
        )));
    }
    let shelf_id = if let Some(sn) = shelf_name {
        resolve_shelf_id(&conn, library_id, sn)?
    } else {
        default_shelf_id(&conn, library_id)?
    };

    let lib_dir = get_tape_dir().join(sanitize_lib_dir_component(&lib_name));
    match fs::create_dir_all(&lib_dir) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            log_error(
                "create_tape",
                &format!("Permission denied: {}", lib_dir.display()),
            );
            return Err(VtlError::PermissionDenied(format!(
                "Cannot create tape directory: {}",
                lib_dir.display()
            )));
        }
        Err(e) => {
            log_error("create_tape", &e.to_string());
            return Err(VtlError::IoError(e));
        }
    }

    let image_path = tape_image_path(&lib_name, name);

    log_message(&format!("Creating tape image at: {}", image_path.display()));
    let file = File::create(&image_path)?;
    file.set_len(size)?;
    file.sync_all()?;

    let barcode = generate_barcode();
    let tx = conn.transaction()?;

    if let Err(e) = tx.execute(
        "INSERT INTO tapes (library_id, shelf_id, barcode, name, slot, capacity_bytes, used_bytes, created_at, image_path)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, 0, ?6, ?7)",
        params![
            library_id,
            shelf_id,
            barcode,
            name,
            size,
            Utc::now(),
            image_path.to_string_lossy()
        ],
    ) {
        let _ = fs::remove_file(&image_path);
        return Err(VtlError::from(e));
    }

    if let Err(e) = tx.commit() {
        let _ = fs::remove_file(&image_path);
        return Err(VtlError::from(e));
    }

    log_message(&format!(
        "Successfully created tape '{}' (barcode: {}, size: {})",
        name,
        barcode,
        format_size(size)
    ));
    println!(
        "Created tape '{}' (barcode: {}, size: {}) in library '{}'",
        name,
        barcode,
        format_size(size),
        lib_name
    );
    println!("Image path: {}", image_path.display());

    Ok(())
}

fn check_quota(size: u64) -> Result<(), VtlError> {
    let conn = init_db()?;
    let lib_name = current_library_name();
    let library_id = resolve_library_id(&conn, &lib_name)?;

    let max_total_size = lib_config_get(&conn, library_id, "quota_max_total_size").or_else(|| {
        conn.query_row(
            "SELECT value FROM config WHERE key = 'quota_max_total_size'",
            params![],
            |row| row.get::<usize, String>(0),
        )
        .ok()
    });

    let max_tapes_str = lib_config_get(&conn, library_id, "quota_max_tapes").or_else(|| {
        conn.query_row(
            "SELECT value FROM config WHERE key = 'quota_max_tapes'",
            params![],
            |row| row.get::<usize, String>(0),
        )
        .ok()
    });

    let max_tapes = max_tapes_str.as_ref().and_then(|s| s.parse::<i32>().ok());

    if let Some(max_total_size_str) = max_total_size {
        let max_total_size_bytes = parse_size(&max_total_size_str)?;

        let current_total: u64 = conn
            .query_row(
                "SELECT COALESCE(SUM(capacity_bytes), 0) FROM tapes WHERE library_id = ?1",
                params![library_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let requested_total = current_total.checked_add(size).ok_or_else(|| {
            VtlError::QuotaExceeded("Total size calculation overflow".to_string())
        })?;
        if requested_total > max_total_size_bytes {
            return Err(VtlError::QuotaExceeded(format!(
                "Total size limit exceeded. Current: {}, New: {}, Max: {}",
                format_size(current_total),
                format_size(requested_total),
                format_size(max_total_size_bytes)
            )));
        }
    }

    if let Some(max_tapes) = max_tapes {
        let current_count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM tapes WHERE library_id = ?1",
                params![library_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if current_count >= max_tapes {
            return Err(VtlError::QuotaExceeded(format!(
                "Maximum tape count exceeded. Current: {}, Max: {}",
                current_count, max_tapes
            )));
        }
    }

    Ok(())
}

fn add_tags_to_tape(conn: &mut Connection, tape_id: i64, tags: &[String]) -> Result<(), VtlError> {
    let tx = conn.transaction()?;

    for tag_name in tags {
        let tag_id: Option<i64> = tx
            .query_row(
                "SELECT id FROM tags WHERE name = ?1",
                params![tag_name],
                |row| row.get(0),
            )
            .optional()?;

        let tag_id = if let Some(id) = tag_id {
            id
        } else {
            tx.execute("INSERT INTO tags (name) VALUES (?1)", params![tag_name])?;
            tx.last_insert_rowid()
        };

        tx.execute(
            "INSERT OR IGNORE INTO tape_tags (tape_id, tag_id) VALUES (?1, ?2)",
            params![tape_id, tag_id],
        )?;
    }

    tx.commit()?;
    Ok(())
}

fn batch_create_tapes(
    count: i32,
    size: u64,
    prefix: &str,
    tags: &[String],
) -> Result<(), VtlError> {
    log_message(&format!(
        "Batch creating {} tapes with prefix '{}'",
        count, prefix
    ));

    for i in 0..count {
        let name = format!("{}_{:04}", prefix, i);
        create_tape(&name, size, None)?;

        let mut conn = init_db()?;
        let library_id = resolve_library_id(&conn, &current_library_name())?;
        let tape_id: i64 = conn
            .query_row(
                "SELECT id FROM tapes WHERE library_id = ?1 AND name = ?2",
                params![library_id, name],
                |row| row.get(0),
            )
            .map_err(|_| VtlError::TapeNotFound(name.clone()))?;

        if !tags.is_empty() {
            add_tags_to_tape(&mut conn, tape_id, tags)?;
        }
    }

    log_message(&format!("Successfully created {} tapes", count));
    println!("Created {} tapes with prefix '{}'", count, prefix);

    Ok(())
}

fn batch_import_tapes(directory: &str, start_slot: i32) -> Result<(), VtlError> {
    log_message(&format!("Batch importing tapes from '{}'", directory));

    let dir_path = Path::new(directory);
    if !dir_path.exists() || !dir_path.is_dir() {
        return Err(VtlError::InvalidParameter(format!(
            "Directory not found: {}",
            directory
        )));
    }

    let mut slot = start_slot;
    let mut count = 0;

    for entry in fs::read_dir(dir_path)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() && path.extension().map_or(false, |ext| ext == "vtltape") {
            let file_name = path.file_name().unwrap().to_string_lossy();
            let _tape_name = file_name.strip_suffix(".vtltape").unwrap_or(&file_name);

            import_tape(&path.to_string_lossy(), slot)?;
            count += 1;
            slot += 1;
        }
    }

    log_message(&format!("Successfully imported {} tapes", count));
    println!("Imported {} tapes starting from slot {}", count, start_slot);

    Ok(())
}

fn quota_set(max_total_size: Option<&str>, max_tapes: Option<i32>) -> Result<(), VtlError> {
    log_message("Setting quota limits");

    let mut conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;
    let tx = conn.transaction()?;

    if let Some(size) = max_total_size {
        parse_size(size)?;
        tx.execute(
            "INSERT OR REPLACE INTO library_config (library_id, key, value) VALUES (?1, 'quota_max_total_size', ?2)",
            params![library_id, size],
        )?;
    }

    if let Some(count) = max_tapes {
        tx.execute(
            "INSERT OR REPLACE INTO library_config (library_id, key, value) VALUES (?1, 'quota_max_tapes', ?2)",
            params![library_id, count.to_string()],
        )?;
    }

    tx.commit()?;

    log_message("Quota settings updated successfully");
    println!("Quota settings updated successfully");

    Ok(())
}

fn quota_show() -> Result<(), VtlError> {
    log_message("Showing quota settings");

    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let max_total_size: String = lib_config_get(&conn, library_id, "quota_max_total_size")
        .or_else(|| {
            conn.query_row(
                "SELECT value FROM config WHERE key = 'quota_max_total_size'",
                params![],
                |row| row.get(0),
            )
            .ok()
        })
        .unwrap_or_else(|| "Unlimited".to_string());

    let max_tapes: String = lib_config_get(&conn, library_id, "quota_max_tapes")
        .or_else(|| {
            conn.query_row(
                "SELECT value FROM config WHERE key = 'quota_max_tapes'",
                params![],
                |row| row.get(0),
            )
            .ok()
        })
        .unwrap_or_else(|| "Unlimited".to_string());

    let current_total: u64 = conn
        .query_row(
            "SELECT COALESCE(SUM(capacity_bytes), 0) FROM tapes WHERE library_id = ?1",
            params![library_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let current_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM tapes WHERE library_id = ?1",
            params![library_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    println!("Quota Settings (library: {}):", current_library_name());
    println!("  Maximum total size: {}", max_total_size);
    println!("  Maximum tape count: {}", max_tapes);
    println!();
    println!("Current Usage:");
    println!("  Total tape size: {}", format_size(current_total));
    println!("  Total tape count: {}", current_count);

    Ok(())
}

fn quota_check() -> Result<(), VtlError> {
    log_message("Checking quota");

    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let max_total_size: Option<String> = lib_config_get(&conn, library_id, "quota_max_total_size")
        .or(conn
            .query_row(
                "SELECT value FROM config WHERE key = 'quota_max_total_size'",
                params![],
                |row| row.get(0),
            )
            .optional()?);

    let max_tapes: Option<i32> = lib_config_get(&conn, library_id, "quota_max_tapes")
        .or(conn
            .query_row(
                "SELECT value FROM config WHERE key = 'quota_max_tapes'",
                params![],
                |row| row.get(0),
            )
            .optional()?)
        .map(|s: String| s.parse().unwrap_or(i32::MAX));

    let current_total: u64 = conn
        .query_row(
            "SELECT COALESCE(SUM(capacity_bytes), 0) FROM tapes WHERE library_id = ?1",
            params![library_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let current_count: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM tapes WHERE library_id = ?1",
            params![library_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let mut exceeded = false;

    if let Some(max_total_size_str) = max_total_size {
        let max_total_size_bytes = parse_size(&max_total_size_str)?;
        if current_total > max_total_size_bytes {
            println!("ERROR: Total size quota exceeded!");
            println!(
                "  Current: {}, Max: {}",
                format_size(current_total),
                format_size(max_total_size_bytes)
            );
            exceeded = true;
        }
    }

    if let Some(max_tapes) = max_tapes {
        if current_count > max_tapes {
            println!("ERROR: Tape count quota exceeded!");
            println!("  Current: {}, Max: {}", current_count, max_tapes);
            exceeded = true;
        }
    }

    if !exceeded {
        println!("Quota check passed - all limits are within bounds");
    }

    Ok(())
}

fn tag_add(tape_name: &str, tags: &[String]) -> Result<(), VtlError> {
    log_message(&format!("Adding tags {:?} to tape '{}'", tags, tape_name));

    let mut conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let tape_id: i64 = conn
        .query_row(
            "SELECT id FROM tapes WHERE library_id = ?1 AND name = ?2",
            params![library_id, tape_name],
            |row| row.get(0),
        )
        .map_err(|_| VtlError::TapeNotFound(tape_name.to_string()))?;

    add_tags_to_tape(&mut conn, tape_id, tags)?;

    log_message(&format!("Successfully added tags to tape '{}'", tape_name));
    println!("Added tags {:?} to tape '{}'", tags, tape_name);

    Ok(())
}

fn tag_remove(tape_name: &str, tags: &[String]) -> Result<(), VtlError> {
    log_message(&format!(
        "Removing tags {:?} from tape '{}'",
        tags, tape_name
    ));

    let mut conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let tape_id: i64 = conn
        .query_row(
            "SELECT id FROM tapes WHERE library_id = ?1 AND name = ?2",
            params![library_id, tape_name],
            |row| row.get(0),
        )
        .map_err(|_| VtlError::TapeNotFound(tape_name.to_string()))?;

    let tx = conn.transaction()?;

    for tag_name in tags {
        if let Ok(tag_id) = tx.query_row(
            "SELECT id FROM tags WHERE name = ?1",
            params![tag_name],
            |row| row.get::<usize, i64>(0),
        ) {
            tx.execute(
                "DELETE FROM tape_tags WHERE tape_id = ?1 AND tag_id = ?2",
                params![tape_id, tag_id],
            )?;
        }
    }

    tx.commit()?;

    log_message(&format!(
        "Successfully removed tags from tape '{}'",
        tape_name
    ));
    println!("Removed tags {:?} from tape '{}'", tags, tape_name);

    Ok(())
}

fn tag_list(tape_name: Option<String>) -> Result<(), VtlError> {
    log_message("Listing tags");

    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    if let Some(name) = tape_name {
        let tape_id: i64 = conn
            .query_row(
                "SELECT id FROM tapes WHERE library_id = ?1 AND name = ?2",
                params![library_id, name],
                |row| row.get(0),
            )
            .map_err(|_| VtlError::TapeNotFound(name.clone()))?;

        let mut stmt = conn.prepare(
            "SELECT t.name FROM tags t
             JOIN tape_tags tt ON t.id = tt.tag_id
             WHERE tt.tape_id = ?1",
        )?;

        let tags = stmt.query_map(params![tape_id], |row| row.get::<usize, String>(0))?;

        println!("Tags for tape '{}':", name);
        for tag in tags {
            println!("  - {}", tag?);
        }
    } else {
        let mut stmt = conn.prepare("SELECT name FROM tags ORDER BY name")?;
        let tags = stmt.query_map(params![], |row| row.get::<usize, String>(0))?;

        println!("All tags:");
        for tag in tags {
            println!("  - {}", tag?);
        }
    }

    Ok(())
}

fn tag_delete(tag_name: &str) -> Result<(), VtlError> {
    log_message(&format!("Deleting tag '{}'", tag_name));

    let mut conn = init_db()?;

    let tag_id: i64 = conn
        .query_row(
            "SELECT id FROM tags WHERE name = ?1",
            params![tag_name],
            |row| row.get::<usize, i64>(0),
        )
        .map_err(|_| VtlError::TagNotFound(tag_name.to_string()))?;

    let tx = conn.transaction()?;

    tx.execute("DELETE FROM tape_tags WHERE tag_id = ?1", params![tag_id])?;
    tx.execute("DELETE FROM tags WHERE id = ?1", params![tag_id])?;

    tx.commit()?;

    log_message(&format!("Successfully deleted tag '{}'", tag_name));
    println!("Deleted tag '{}'", tag_name);

    Ok(())
}

fn search_tapes(
    name: Option<String>,
    tag: Option<String>,
    min_size: Option<String>,
    max_size: Option<String>,
    free_space: Option<bool>,
) -> Result<(), VtlError> {
    log_message("Searching tapes");

    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let mut query = String::from(
        "SELECT t.name, t.barcode, t.slot, t.capacity_bytes, t.used_bytes, s.is_import_export \
         FROM tapes t \
         LEFT JOIN slots s ON t.library_id = s.library_id AND t.slot = s.slot_id \
         WHERE t.library_id = ?1",
    );
    let mut bind: Vec<String> = Vec::new();
    bind.push(library_id.to_string());

    if let Some(n) = name {
        query.push_str(" AND t.name LIKE ?");
        bind.push(format!("%{}%", n));
    }

    if let Some(t) = tag {
        query.push_str(" AND EXISTS (SELECT 1 FROM tape_tags tt JOIN tags tag ON tt.tag_id = tag.id WHERE tt.tape_id = t.id AND tag.name = ?)");
        bind.push(t);
    }

    if let Some(min) = min_size {
        let min_bytes = parse_size(&min)?;
        query.push_str(" AND t.capacity_bytes >= ?");
        bind.push(min_bytes.to_string());
    }

    if let Some(max) = max_size {
        let max_bytes = parse_size(&max)?;
        query.push_str(" AND t.capacity_bytes <= ?");
        bind.push(max_bytes.to_string());
    }

    if free_space == Some(true) {
        query.push_str(" AND t.used_bytes = 0");
    }

    query.push_str(" ORDER BY t.name");

    let mut stmt = conn.prepare(&query)?;

    let tape_params: Vec<&str> = bind.iter().map(|s| s.as_str()).collect();
    let tapes = stmt.query_map(rusqlite::params_from_iter(tape_params), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i32>>(2)?,
            row.get::<_, u64>(3)?,
            row.get::<_, u64>(4)?,
            row.get::<_, Option<i64>>(5)?,
        ))
    })?;

    println!(
        "{:<20} {:<12} {:<8} {:<10} {:<10} {}",
        "Name", "Barcode", "Slot", "Size", "Used", "Type"
    );
    println!("{}", "-".repeat(70));

    let mut found = false;
    for tape in tapes {
        found = true;
        let (name, barcode, slot, capacity, used, is_mailslot) = tape?;

        let slot_str = match slot {
            Some(s) => {
                if is_mailslot.map(|v| v != 0).unwrap_or(false) {
                    format!("mail{}", s - MAILSLOT_OFFSET)
                } else {
                    format!("slot{}", s)
                }
            }
            None => "-".to_string(),
        };

        println!(
            "{:<20} {:<12} {:<8} {:<10} {:<10} {}",
            name,
            barcode,
            slot_str,
            format_size(capacity),
            format_size(used),
            if slot.is_none() {
                "Shelf/offline"
            } else {
                "In robot"
            }
        );
    }

    if !found {
        println!("No tapes found matching the search criteria");
    }

    Ok(())
}

fn delete_tape(name: &str) -> Result<(), VtlError> {
    delete_tape_in_library(&current_library_name(), name)
}

pub(crate) fn delete_tape_in_library(library: &str, name: &str) -> Result<(), VtlError> {
    log_message(&format!(
        "Deleting tape '{}' from library '{}'",
        name, library
    ));

    let mut conn = init_db()?;
    let library_id = resolve_library_id(&conn, library)?;

    let (tape_id, image_path): (i64, String) = conn
        .query_row(
            "SELECT id, image_path FROM tapes WHERE library_id = ?1 AND name = ?2",
            params![library_id, name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| {
            log_error("delete_tape", &format!("未找到磁带: {}", name));
            VtlError::TapeNotFound(name.to_string())
        })?;

    let image_path_buf = PathBuf::from(&image_path);
    let deleted_path =
        image_path_buf.with_extension(format!("vtltape.deleted.{}", std::process::id()));
    if deleted_path.exists() {
        return Err(VtlError::InvalidParameter(format!(
            "staged delete path already exists: {}",
            deleted_path.display()
        )));
    }
    if let Err(e) = fs::rename(&image_path_buf, &deleted_path) {
        log_error(
            "delete_tape_in_library",
            &format!("failed to stage tape image removal {}: {}", image_path, e),
        );
        return Err(VtlError::from(e));
    }

    let tx = conn.transaction()?;

    tx.execute(
        "UPDATE slots SET tape_id = NULL WHERE library_id = ?1 AND tape_id = ?2",
        params![library_id, tape_id],
    )?;

    tx.execute(
        "UPDATE drives SET tape_id = NULL WHERE library_id = ?1 AND tape_id = ?2",
        params![library_id, tape_id],
    )?;

    tx.execute("DELETE FROM tape_tags WHERE tape_id = ?1", params![tape_id])?;

    tx.execute("DELETE FROM tapes WHERE id = ?1", params![tape_id])?;

    if let Err(e) = tx.commit() {
        let _ = fs::rename(&deleted_path, &image_path_buf);
        return Err(VtlError::from(e));
    }

    if let Err(e) = fs::remove_file(&deleted_path) {
        log_error(
            "delete_tape_in_library",
            &format!(
                "DB committed but failed to remove staged tape image {}: {}",
                deleted_path.display(),
                e
            ),
        );
        return Err(VtlError::from(e));
    }

    log_message(&format!("Successfully deleted tape '{}'", name));
    println!("Deleted tape '{}' from library '{}'", name, library);

    Ok(())
}

/// 初始化磁带：将 `used_bytes` 置 0，并把镜像文件截断为标称容量（空白带）。
/// 磁带须**在货架上**（`slot` 为空、`shelf_id` 已绑定）、**不在驱动中**；若在机械手槽内请先移回货架。
/// 与 `assign-slot` / `shelf place` 一致：在驱动内返回 `TapeInDrive`；不在货架（含仅在槽内）返回 `TapeNotOnShelf`。
pub(crate) fn init_tape_in_library(library: &str, name: &str) -> Result<(), VtlError> {
    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, library)?;
    let (tape_id, image_path, capacity, slot, shelf_id): (i64, String, u64, Option<i32>, Option<i64>) =
        conn
            .query_row(
                "SELECT id, image_path, capacity_bytes, slot, shelf_id FROM tapes WHERE library_id = ?1 AND name = ?2",
                params![library_id, name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .map_err(|_| VtlError::TapeNotFound(name.to_string()))?;

    if tape_in_drive(&conn, library_id, tape_id)? {
        return Err(VtlError::TapeInDrive);
    }
    if slot.is_some() {
        return Err(VtlError::TapeNotOnShelf);
    }
    if shelf_id.is_none() {
        return Err(VtlError::TapeNotOnShelf);
    }

    log_message(&format!(
        "正在初始化（擦除）磁带 '{}'（库 '{}'）",
        name, library
    ));

    let f = OpenOptions::new()
        .write(true)
        .open(Path::new(&image_path))
        .map_err(|e| {
            VtlError::IoError(std::io::Error::new(
                e.kind(),
                format!("打开磁带镜像 {} 失败: {}", image_path, e),
            ))
        })?;
    let prev_len = f.metadata().map_err(VtlError::from)?.len();
    f.set_len(capacity).map_err(VtlError::from)?;
    if let Err(e) = f.sync_all() {
        log_error(
            "init_tape_in_library",
            &format!(
                "镜像已截断为标称容量，但 sync 失败（{}），请检查磁盘与权限",
                e
            ),
        );
    }

    let rows = match conn.execute(
        "UPDATE tapes SET used_bytes = 0 WHERE id = ?1",
        params![tape_id],
    ) {
        Ok(n) => n,
        Err(e) => {
            if let Err(re) = f.set_len(prev_len) {
                log_error(
                    "init_tape_in_library",
                    &format!(
                        "数据库更新失败，且无法将镜像长度恢复为截断前的 {} 字节: {}",
                        prev_len, re
                    ),
                );
            } else {
                log_error(
                    "init_tape_in_library",
                    &format!(
                        "数据库更新失败，已将镜像长度恢复为 {} 字节；请重试或检查数据库",
                        prev_len
                    ),
                );
            }
            return Err(VtlError::from(e));
        }
    };
    if rows != 1 {
        let _ = f.set_len(prev_len);
        return Err(VtlError::TapeNotFound(name.to_string()));
    }

    log_message(&format!(
        "磁带 '{}' 已初始化（容量 {}）",
        name,
        format_size(capacity)
    ));
    println!(
        "已初始化磁带 '{}'（库 '{}'）：used_bytes 已置 0，镜像已截断为 {}",
        name,
        library,
        format_size(capacity)
    );
    Ok(())
}

/// 删除自建货架（不可删除默认「未使用」架；架上须无任何磁带）。
pub(crate) fn delete_shelf_in_library(library: &str, shelf_name: &str) -> Result<(), VtlError> {
    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, library)?;
    let (shelf_id, is_default): (i64, i64) = conn
        .query_row(
            "SELECT id, is_default_unused FROM shelves WHERE library_id = ?1 AND name = ?2",
            params![library_id, shelf_name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| VtlError::ShelfNotFound(shelf_name.to_string()))?;

    if is_default != 0 {
        return Err(VtlError::InvalidParameter(
            "不可删除默认「未使用」磁带架".to_string(),
        ));
    }

    let cnt: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tapes WHERE shelf_id = ?1",
        params![shelf_id],
        |r| r.get(0),
    )?;
    if cnt > 0 {
        return Err(VtlError::InvalidParameter(format!(
            "磁带架 '{}' 上仍有 {} 盘磁带，请先迁移或删除磁带",
            shelf_name, cnt
        )));
    }

    conn.execute(
        "DELETE FROM shelves WHERE id = ?1 AND library_id = ?2",
        params![shelf_id, library_id],
    )?;
    println!("Deleted shelf '{}' from library '{}'", shelf_name, library);
    Ok(())
}

/// 删除整个命名在线库（不可删 `__offline__`；至少须保留一个在线库）。删除库目录下残余镜像目录。
/// 返回：磁盘清理阶段出现的**非致命**警告（每条一行说明）；DB 已提交后即使非空也表示删除库成功。
pub(crate) fn delete_named_library(name: &str) -> Result<(Vec<String>, KernelGeomSync), VtlError> {
    if name == OFFLINE_LIBRARY_NAME {
        return Err(VtlError::InvalidParameter(format!(
            "不可删除系统保留库 '{}'",
            OFFLINE_LIBRARY_NAME
        )));
    }
    let mut conn = init_db()?;
    let online_count = count_exported_online_libraries(&conn)?;
    let library_id = resolve_library_id(&conn, name)?;
    if online_count <= 1 {
        let tape_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tapes WHERE library_id = ?1",
            params![library_id],
            |r| r.get(0),
        )?;
        let sole_legacy_default = name == LEGACY_DEFAULT_LIBRARY_NAME && tape_count == 0;
        if !sole_legacy_default {
            return Err(VtlError::InvalidParameter(
                "不可删除最后一个在线磁带库".to_string(),
            ));
        }
    }

    let paths: Vec<String> = {
        let mut stmt = conn.prepare("SELECT image_path FROM tapes WHERE library_id = ?1")?;
        let out = stmt
            .query_map(params![library_id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        out
    };

    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM tape_tags WHERE tape_id IN (SELECT id FROM tapes WHERE library_id = ?1)",
        params![library_id],
    )?;
    tx.execute(
        "UPDATE drives SET tape_id = NULL WHERE library_id = ?1",
        params![library_id],
    )?;
    tx.execute(
        "UPDATE slots SET tape_id = NULL WHERE library_id = ?1",
        params![library_id],
    )?;
    tx.execute(
        "DELETE FROM tapes WHERE library_id = ?1",
        params![library_id],
    )?;
    tx.execute(
        "DELETE FROM library_config WHERE library_id = ?1",
        params![library_id],
    )?;
    tx.execute(
        "DELETE FROM drives WHERE library_id = ?1",
        params![library_id],
    )?;
    tx.execute(
        "DELETE FROM slots WHERE library_id = ?1",
        params![library_id],
    )?;
    tx.execute(
        "DELETE FROM shelves WHERE library_id = ?1",
        params![library_id],
    )?;
    tx.execute(
        "DELETE FROM vtl_libraries WHERE id = ?1",
        params![library_id],
    )?;
    tx.commit()?;

    let mut file_warnings: Vec<String> = Vec::new();
    for p in paths {
        if let Err(e) = fs::remove_file(&p) {
            let msg = format!("remove_file {}: {}", p, e);
            log_error("delete_named_library", &msg);
            file_warnings.push(msg);
        }
    }
    let lib_dir = get_tape_dir().join(sanitize_lib_dir_component(name));
    if lib_dir.exists() {
        if let Err(e) = fs::remove_dir_all(&lib_dir) {
            let msg = format!("remove_dir_all {}: {}", lib_dir.display(), e);
            log_error("delete_named_library", &msg);
            file_warnings.push(msg);
        }
    }

    if current_library_name() == name {
        if let Ok(conn) = init_db() {
            if let Ok(next) = first_online_library_name(&conn) {
                set_current_library(&next);
            } else {
                set_current_library("");
            }
        } else {
            set_current_library("");
        }
    }

    log_message(&format!("Deleted VTL library '{}'", name));
    println!("Deleted library '{}'", name);
    if !file_warnings.is_empty() {
        log_message(&format!(
            "delete_named_library: {} file/dir cleanup warning(s) for library '{}'",
            file_warnings.len(),
            name
        ));
    }
    let geom = maybe_reload_kernel_vtl_after_db_change();
    Ok((file_warnings, geom))
}

fn list_tapes() -> Result<(), VtlError> {
    log_message("Listing all tapes");
    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let mut stmt = conn.prepare(
        "SELECT t.name, t.barcode, t.slot, t.capacity_bytes, t.used_bytes, s.is_import_export,
                sh.name as shelf_name,
                (SELECT COUNT(*) FROM drives d WHERE d.library_id = t.library_id AND d.tape_id = t.id) as in_drive
         FROM tapes t
         LEFT JOIN slots s ON t.library_id = s.library_id AND t.slot = s.slot_id
         LEFT JOIN shelves sh ON t.shelf_id = sh.id
         WHERE t.library_id = ?1
         ORDER BY t.id",
    )?;

    let tapes = stmt.query_map(params![library_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i32>>(2)?,
            row.get::<_, u64>(3)?,
            row.get::<_, u64>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, i64>(7)?,
        ))
    })?;

    println!(
        "{:<18} {:<12} {:<10} {:<14} {:<8} {:<8} {}",
        "Name", "Barcode", "Location", "Shelf", "Size", "Used", "Note"
    );
    println!("{}", "-".repeat(88));

    let kernel_locs = if robot_sync::robot_sync_enabled() {
        robot_sync::kernel_inventory(&conn, library_id).ok()
    } else {
        None
    };

    for tape in tapes {
        let (name, barcode, slot, capacity, used, is_mailslot, shelf_name, in_drive) = tape?;

        let (location, note) = if let Some(ref kmap) = kernel_locs {
            if let Some(loc) = kmap.get(&name) {
                use robot_sync::MediumLocation;
                let locs = match loc {
                    MediumLocation::Drive(d) => {
                        (format!("drive{}", d), "In drive (kernel)".to_string())
                    }
                    MediumLocation::DataSlot(s) => {
                        (format!("slot{}", s), "In robot (kernel)".to_string())
                    }
                    MediumLocation::MailSlot(m) => (
                        format!("mail{}", m - MAILSLOT_OFFSET),
                        "In IE (kernel)".to_string(),
                    ),
                };
                locs
            } else if in_drive > 0 {
                ("drive".to_string(), "In drive".to_string())
            } else if let Some(s) = slot {
                let loc = if is_mailslot.map(|v| v != 0).unwrap_or(false) {
                    format!("mail{}", s - MAILSLOT_OFFSET)
                } else {
                    format!("slot{}", s)
                };
                (loc, "In robot (DB hint)".to_string())
            } else {
                ("-".to_string(), "Catalog only".to_string())
            }
        } else if in_drive > 0 {
            ("drive".to_string(), "In drive".to_string())
        } else if let Some(s) = slot {
            let loc = if is_mailslot.map(|v| v != 0).unwrap_or(false) {
                format!("mail{}", s - MAILSLOT_OFFSET)
            } else {
                format!("slot{}", s)
            };
            (loc, "In robot".to_string())
        } else {
            ("-".to_string(), "Catalog only".to_string())
        };

        let shelf_disp = shelf_name.unwrap_or_else(|| "-".to_string());

        println!(
            "{:<18} {:<12} {:<10} {:<14} {:<8} {:<8} {}",
            name,
            barcode,
            location,
            shelf_disp,
            format_size(capacity),
            format_size(used),
            note
        );
    }

    Ok(())
}

pub(crate) fn load_tape_in_library(library: &str, slot: i32, drive: i32) -> Result<(), VtlError> {
    let prev = current_library_name();
    set_current_library(library);
    let r = load_tape(slot, drive);
    set_current_library(&prev);
    r
}

pub(crate) fn unload_tape_in_library(library: &str, drive: i32) -> Result<i32, VtlError> {
    let prev = current_library_name();
    set_current_library(library);
    let r = unload_tape(drive);
    set_current_library(&prev);
    r
}

pub(crate) fn eject_tape_in_library(library: &str, slot: i32) -> Result<i32, VtlError> {
    let prev = current_library_name();
    set_current_library(library);
    let r = eject_tape(slot);
    set_current_library(&prev);
    r
}

pub(crate) fn sync_db_from_kernel_library(library: &str) -> Result<usize, VtlError> {
    if !robot_sync::robot_sync_enabled() {
        return Ok(0);
    }
    let mut conn = init_db()?;
    let library_id = resolve_library_id(&conn, library)?;
    reconcile::mirror_kernel_catalog_hints_only(&mut conn, library_id)
}

fn load_tape(slot: i32, drive: i32) -> Result<(), VtlError> {
    log_message(&format!(
        "Loading tape from slot {} to drive {}",
        slot, drive
    ));

    let mut conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let tx = conn.transaction()?;

    let tape_id: Option<i64> = tx
        .query_row(
            "SELECT tape_id FROM slots WHERE library_id = ?1 AND slot_id = ?2",
            params![library_id, slot],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();

    let tape_id = tape_id.ok_or_else(|| {
        log_error("load_tape", &format!("Slot {} is empty", slot));
        VtlError::SlotEmpty
    })?;

    let existing_tape: Option<i64> = tx
        .query_row(
            "SELECT tape_id FROM drives WHERE library_id = ?1 AND drive_id = ?2",
            params![library_id, drive],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();

    if existing_tape.is_some() {
        log_error("load_tape", &format!("Drive {} is busy", drive));
        return Err(VtlError::DriveBusy);
    }

    tx.execute(
        "UPDATE drives SET tape_id = ?1 WHERE library_id = ?2 AND drive_id = ?3",
        params![tape_id, library_id, drive],
    )?;

    tx.execute(
        "UPDATE tapes SET slot = NULL, shelf_id = NULL WHERE id = ?1",
        params![tape_id],
    )?;

    tx.execute(
        "UPDATE slots SET tape_id = NULL WHERE library_id = ?1 AND slot_id = ?2",
        params![library_id, slot],
    )?;

    tx.commit()?;

    let tape_name: String = conn.query_row(
        "SELECT name FROM tapes WHERE id = ?1",
        params![tape_id],
        |row| row.get(0),
    )?;
    let barcode: Option<String> = conn.query_row(
        "SELECT barcode FROM tapes WHERE id = ?1",
        params![tape_id],
        |row| row.get(0),
    )?;

    if robot_sync::robot_ioctl_enabled() {
        if let Err(e) = robot_sync::kernel_load(
            &conn,
            library_id,
            slot,
            drive,
            &tape_name,
            barcode.as_deref(),
        ) {
            robot_sync::warn_kernel_sync_failed("load", &e);
        }
    }
    if robot_sync::robot_sync_enabled() {
        reconcile::try_post_op_auto_align(library_id);
    }

    log_message(&format!(
        "Successfully loaded tape '{}' from slot {} to drive {}",
        tape_name, slot, drive
    ));
    println!(
        "Loaded tape '{}' from slot {} to drive {}",
        tape_name, slot, drive
    );

    Ok(())
}

/// Returns the slot id the tape was moved to.
fn unload_tape(drive: i32) -> Result<i32, VtlError> {
    log_message(&format!("Unloading tape from drive {}", drive));

    let mut conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let tx = conn.transaction()?;

    let tape_id: Option<i64> = tx
        .query_row(
            "SELECT tape_id FROM drives WHERE library_id = ?1 AND drive_id = ?2",
            params![library_id, drive],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();

    let tape_id = tape_id.ok_or_else(|| {
        log_error("unload_tape", &format!("Drive {} is empty", drive));
        VtlError::DriveEmpty
    })?;

    let original_slot: Option<i32> = tx
        .query_row(
            "SELECT slot FROM tapes WHERE id = ?1",
            params![tape_id],
            |row| row.get::<_, Option<i32>>(0),
        )
        .optional()?
        .flatten();

    let target_slot = if let Some(slot) = original_slot {
        slot
    } else {
        let mut stmt = tx.prepare(
            "SELECT slot_id FROM slots WHERE library_id = ?1 AND tape_id IS NULL AND is_import_export = 0 ORDER BY slot_id",
        )?;

        let slots: Vec<i32> = stmt
            .query_map(params![library_id], |row| row.get(0))?
            .collect::<Result<_, _>>()?;

        *slots.first().ok_or_else(|| {
            log_error("unload_tape", "No available slots");
            VtlError::NoAvailableSlots
        })?
    };

    tx.execute(
        "UPDATE tapes SET slot = ?1, shelf_id = NULL WHERE id = ?2",
        params![target_slot, tape_id],
    )?;

    tx.execute(
        "UPDATE slots SET tape_id = ?1 WHERE library_id = ?2 AND slot_id = ?3",
        params![tape_id, library_id, target_slot],
    )?;

    tx.execute(
        "UPDATE drives SET tape_id = NULL WHERE library_id = ?1 AND drive_id = ?2",
        params![library_id, drive],
    )?;

    tx.commit()?;

    let tape_name: String = conn.query_row(
        "SELECT name FROM tapes WHERE id = ?1",
        params![tape_id],
        |row| row.get(0),
    )?;

    if robot_sync::robot_ioctl_enabled() {
        if let Err(e) = robot_sync::kernel_unload(&conn, library_id, drive, target_slot) {
            robot_sync::warn_kernel_sync_failed("unload", &e);
        }
    }
    if robot_sync::robot_sync_enabled() {
        reconcile::try_post_op_auto_align(library_id);
    }

    log_message(&format!(
        "Successfully unloaded tape '{}' from drive {} to slot {}",
        tape_name, drive, target_slot
    ));
    println!(
        "Unloaded tape '{}' from drive {} to slot {}",
        tape_name, drive, target_slot
    );

    Ok(target_slot)
}

/// Returns the DB mailslot `slot_id` the tape was moved to.
fn eject_tape(slot: i32) -> Result<i32, VtlError> {
    log_message(&format!("Ejecting tape from slot {} to mailslot", slot));

    let mut conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let tx = conn.transaction()?;

    let tape_id: Option<i64> = tx
        .query_row(
            "SELECT tape_id FROM slots WHERE library_id = ?1 AND slot_id = ?2",
            params![library_id, slot],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();

    let tape_id = tape_id.ok_or_else(|| {
        log_error("eject_tape", &format!("Slot {} is empty", slot));
        VtlError::SlotEmpty
    })?;

    let mailslot: i32 = tx
        .query_row(
            "SELECT slot_id FROM slots WHERE library_id = ?1 AND tape_id IS NULL AND is_import_export != 0 ORDER BY slot_id",
            params![library_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| {
            log_error("eject_tape", "No available mailslots");
            VtlError::NoAvailableSlots
        })?;

    tx.execute(
        "UPDATE tapes SET slot = ?1, shelf_id = NULL WHERE id = ?2",
        params![mailslot, tape_id],
    )?;

    tx.execute(
        "UPDATE slots SET tape_id = NULL WHERE library_id = ?1 AND slot_id = ?2",
        params![library_id, slot],
    )?;

    tx.execute(
        "UPDATE slots SET tape_id = ?1 WHERE library_id = ?2 AND slot_id = ?3",
        params![tape_id, library_id, mailslot],
    )?;

    tx.commit()?;

    let tape_name: String = conn.query_row(
        "SELECT name FROM tapes WHERE id = ?1",
        params![tape_id],
        |row| row.get(0),
    )?;

    if robot_sync::robot_ioctl_enabled() {
        let dst = robot_sync::db_mailslot_to_element(mailslot);
        if let Err(e) = robot_sync::kernel_move_medium(&conn, library_id, slot, dst) {
            robot_sync::warn_kernel_sync_failed("eject", &e);
        }
    }
    if robot_sync::robot_sync_enabled() {
        reconcile::try_post_op_auto_align(library_id);
    }

    log_message(&format!(
        "Successfully ejected tape '{}' from slot {} to mailslot {}",
        tape_name, slot, mailslot
    ));
    println!(
        "Ejected tape '{}' from slot {} to mailslot {}",
        tape_name, slot, mailslot
    );

    Ok(mailslot)
}

fn apply_vtl_conf_set_kv(key: &str, value: &str) -> Result<(), VtlError> {
    match key {
        "robot_authority" | "auto_reconcile_apply" => {
            return Err(VtlError::InvalidParameter(format!(
                "vtl.conf key '{}' removed (runtime robot is always vtl.ko)",
                key
            )));
        }
        "robot_sync" | "auto_sync_db_from_kernel" | "auto_reconcile_pull" => {
            let t = value.trim().to_ascii_lowercase();
            if !matches!(
                t.as_str(),
                "true" | "false" | "1" | "0" | "yes" | "no" | "on" | "off"
            ) {
                return Err(VtlError::InvalidParameter(format!(
                    "{}: expected true/false",
                    key
                )));
            }
            let norm = if matches!(t.as_str(), "true" | "1" | "yes" | "on") {
                "true"
            } else {
                "false"
            };
            update_primary_vtl_conf_kv(key, norm)?;
            println!("vtl.conf: {}={}", key, norm);
        }
        _ => {
            return Err(VtlError::InvalidParameter(format!(
                "unknown vtl.conf key: {} (supported: robot_sync, auto_sync_db_from_kernel, auto_reconcile_pull)",
                key
            )));
        }
    }
    Ok(())
}

fn config_set(params: &[String]) -> Result<(), VtlError> {
    log_message(&format!("Setting config: {:?}", params));

    let mut max_drives: Option<i32> = None;
    let mut slots: Option<i32> = None;
    let mut vtl_conf: Vec<(String, String)> = Vec::new();

    for param in params {
        let parts: Vec<&str> = param.splitn(2, '=').collect();
        if parts.len() != 2 {
            return Err(VtlError::InvalidParameter(format!(
                "Invalid format: {}",
                param
            )));
        }
        let key = parts[0].trim();
        let value = parts[1].trim();
        if matches!(
            key,
            "robot_sync" | "auto_sync_db_from_kernel" | "auto_reconcile_pull"
        ) {
            vtl_conf.push((key.to_string(), value.to_string()));
            continue;
        }
        match key {
            "max_drives" => {
                max_drives = Some(value.parse().map_err(|_| {
                    VtlError::InvalidParameter(format!("Invalid max_drives: {}", value))
                })?);
            }
            "slots" => {
                slots = Some(value.parse().map_err(|_| {
                    VtlError::InvalidParameter(format!("Invalid slots: {}", value))
                })?);
            }
            _ => {
                return Err(VtlError::InvalidParameter(format!(
                    "Unknown parameter: {} (library: max_drives, slots; global vtl.conf: robot_sync, auto_sync_db_from_kernel, …)",
                    key
                )));
            }
        }
    }

    for (k, v) in &vtl_conf {
        apply_vtl_conf_set_kv(k, v)?;
    }

    if max_drives.is_none() && slots.is_none() && vtl_conf.is_empty() {
        return Err(VtlError::InvalidParameter("no parameters given".into()));
    }

    if max_drives.is_none() && slots.is_none() {
        return Ok(());
    }

    let mut conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let tx = conn.transaction()?;

    if let Some(d) = max_drives {
        if d < 1 || d > VTL_KERNEL_MAX_DRIVES_PER_LIB {
            return Err(VtlError::InvalidParameter(format!(
                "max_drives 须在 1..={} 之间（与内核 vtl 一致）",
                VTL_KERNEL_MAX_DRIVES_PER_LIB
            )));
        }
        tx.execute(
            "INSERT OR REPLACE INTO library_config (library_id, key, value) VALUES (?1, 'max_drives', ?2)",
            params![library_id, d.to_string()],
        )?;

        let current_drives: i32 = tx.query_row(
            "SELECT COUNT(*) FROM drives WHERE library_id = ?1",
            params![library_id],
            |row| row.get(0),
        )?;

        if d > current_drives {
            for i in current_drives..d {
                tx.execute(
                    "INSERT OR IGNORE INTO drives (library_id, drive_id, tape_id) VALUES (?1, ?2, NULL)",
                    params![library_id, i],
                )?;
            }
        }
    }

    if let Some(s) = slots {
        if s < 1 || s > VTL_KERNEL_MAX_DATA_SLOTS_PER_LIB {
            return Err(VtlError::InvalidParameter(format!(
                "slots 须在 1..={} 之间（数据槽；与内核 vtl 一致）",
                VTL_KERNEL_MAX_DATA_SLOTS_PER_LIB
            )));
        }
        tx.execute(
            "INSERT OR REPLACE INTO library_config (library_id, key, value) VALUES (?1, 'slots', ?2)",
            params![library_id, s.to_string()],
        )?;

        let current_slots: i32 = tx.query_row(
            "SELECT COUNT(*) FROM slots WHERE library_id = ?1 AND is_import_export = 0",
            params![library_id],
            |row| row.get(0),
        )?;

        if s > current_slots {
            for i in current_slots..s {
                tx.execute(
                    "INSERT OR IGNORE INTO slots (library_id, slot_id, tape_id, is_import_export) VALUES (?1, ?2, NULL, 0)",
                    params![library_id, i],
                )?;
            }
        }
    }

    tx.commit()?;

    log_message("Configuration updated successfully");
    println!("Configuration updated successfully");

    if max_drives.is_some() || slots.is_some() {
        maybe_reload_kernel_vtl_after_db_change();
    }

    Ok(())
}

fn config_show() -> Result<(), VtlError> {
    log_message("Showing configuration");
    let conn = init_db()?;
    let config = get_config();
    let lib_name = current_library_name();
    let library_id = resolve_library_id(&conn, &lib_name)?;

    let max_drives: String = lib_config_get(&conn, library_id, "max_drives")
        .or_else(|| {
            conn.query_row(
                "SELECT value FROM config WHERE key = 'max_drives'",
                params![],
                |row| row.get(0),
            )
            .ok()
        })
        .unwrap_or_else(|| "2".to_string());

    let slots: String = lib_config_get(&conn, library_id, "slots")
        .or_else(|| {
            conn.query_row(
                "SELECT value FROM config WHERE key = 'slots'",
                params![],
                |row| row.get(0),
            )
            .ok()
        })
        .unwrap_or_else(|| "10".to_string());

    let mailslots: i32 = conn
        .query_row(
            "SELECT COUNT(*) FROM slots WHERE library_id = ?1 AND is_import_export != 0",
            params![library_id],
            |row| row.get(0),
        )
        .unwrap_or(4);

    println!("VTL Configuration (library: {}):", lib_name);
    println!("  Database path: {}", config.db_path.display());
    println!("  Tape directory: {}", config.tape_dir.display());
    println!("  Log directory: {}", config.log_dir.display());
    println!("  max_drives: {}", max_drives);
    println!("  slots: {}", slots);
    println!("  mailslots: {}", mailslots);

    Ok(())
}

fn inventory() -> Result<(), VtlError> {
    log_message("Showing inventory");
    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;
    let view = robot_sync::changer_inventory_display(&conn, library_id)?;

    println!(
        "VTL Inventory (library: {}, source: {}):",
        current_library_name(),
        view.source
    );
    println!();

    println!("{:<8} {:<20} {:<12}", "Slot", "Tape Name", "Barcode");
    println!("{}", "-".repeat(40));
    for row in &view.data_slots {
        println!(
            "{:<8} {:<20} {:<12}",
            row.label,
            row.tape_name.as_deref().unwrap_or("-"),
            row.barcode.as_deref().unwrap_or("-")
        );
    }

    println!();
    println!("Drives:");
    println!("{:<8} {:<20} {:<12}", "Drive", "Loaded Tape", "Barcode");
    println!("{}", "-".repeat(40));
    for row in &view.drives {
        println!(
            "{:<8} {:<20} {:<12}",
            row.label,
            row.tape_name.as_deref().unwrap_or("-"),
            row.barcode.as_deref().unwrap_or("-")
        );
    }

    println!();
    println!("Mailslots (Import/Export):");
    println!("{:<8} {:<20} {:<12}", "Slot", "Tape Name", "Barcode");
    println!("{}", "-".repeat(40));
    for row in &view.mailslots {
        println!(
            "{:<8} {:<20} {:<12}",
            row.label,
            row.tape_name.as_deref().unwrap_or("-"),
            row.barcode.as_deref().unwrap_or("-")
        );
    }

    Ok(())
}

fn status() -> Result<(), VtlError> {
    log_message("Showing status");
    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let tape_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tapes WHERE library_id = ?1",
        params![library_id],
        |row| row.get(0),
    )?;

    let loaded_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM drives WHERE library_id = ?1 AND tape_id IS NOT NULL",
        params![library_id],
        |row| row.get(0),
    )?;

    let drive_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM drives WHERE library_id = ?1",
        params![library_id],
        |row| row.get(0),
    )?;

    let slot_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM slots WHERE library_id = ?1 AND is_import_export = 0",
        params![library_id],
        |row| row.get(0),
    )?;

    let mailslot_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM slots WHERE library_id = ?1 AND is_import_export != 0",
        params![library_id],
        |row| row.get(0),
    )?;

    println!("VTL Library Status ({}):", current_library_name());
    println!("  Total Tapes: {}", tape_count);
    println!("  Loaded Tapes: {}", loaded_count);
    println!("  Total Drives: {}", drive_count);
    println!("  Total Slots: {}", slot_count);
    println!("  Mailslots: {}", mailslot_count);

    Ok(())
}

fn snapshot_tape(tape: &str, snapshot: &str) -> Result<(), VtlError> {
    log_message(&format!(
        "Creating snapshot '{}' for tape '{}'",
        snapshot, tape
    ));
    validate_tape_name(snapshot)?;

    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let (image_path, capacity): (String, u64) = conn
        .query_row(
            "SELECT image_path, capacity_bytes FROM tapes WHERE library_id = ?1 AND name = ?2",
            params![library_id, tape],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| {
            log_error("snapshot_tape", &format!("未找到磁带: {}", tape));
            VtlError::TapeNotFound(tape.to_string())
        })?;

    let lib_dir = get_tape_dir().join(sanitize_lib_dir_component(&current_library_name()));
    let snapshot_path = lib_dir.join(format!("{}_{}.vtltape", tape, snapshot));

    let mut src = File::open(&image_path)?;
    let mut dst = File::create(&snapshot_path)?;
    std::io::copy(&mut src, &mut dst)?;
    dst.sync_all()?;

    log_message(&format!(
        "Successfully created snapshot '{}' for tape '{}' at {}",
        snapshot,
        tape,
        snapshot_path.display()
    ));
    println!("Created snapshot '{}' for tape '{}'", snapshot, tape);
    println!("Snapshot location: {}", snapshot_path.display());
    println!("Snapshot size: {}", format_size(capacity));

    Ok(())
}

fn import_tape(path: &str, slot: i32) -> Result<(), VtlError> {
    log_message(&format!("Importing tape from '{}' to slot {}", path, slot));

    let mut conn = init_db()?;
    let lib_name = current_library_name();
    let library_id = resolve_library_id(&conn, &lib_name)?;
    let source_path = Path::new(path);

    if !source_path.exists() {
        log_error("import_tape", &format!("File not found: {}", path));
        return Err(VtlError::IoError(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("File not found: {}", path),
        )));
    }

    let file_name = source_path.file_name().unwrap().to_string_lossy();
    let tape_name = file_name
        .strip_suffix(".vtltape")
        .unwrap_or(&file_name)
        .to_string();
    validate_tape_name(&tape_name)?;

    let existing_tape: Option<i64> = conn
        .query_row(
            "SELECT id FROM tapes WHERE library_id = ?1 AND name = ?2",
            params![library_id, tape_name],
            |row| row.get(0),
        )
        .optional()?;
    if existing_tape.is_none() {
        assert_tape_name_globally_unique(&conn, &tape_name, library_id)?;
    } else if tape_in_drive(&conn, library_id, existing_tape.unwrap())? {
        return Err(VtlError::TapeInDrive);
    }

    let lib_dir = get_tape_dir().join(sanitize_lib_dir_component(&lib_name));
    fs::create_dir_all(&lib_dir)?;

    let dest_path = tape_image_path(&lib_name, &tape_name);
    fs::copy(source_path, &dest_path)?;
    File::open(&dest_path)?.sync_all()?;

    let metadata = source_path.metadata()?;
    let barcode = generate_barcode();

    if existing_tape.is_none() {
        check_quota(metadata.len())?;
    }

    let tx_result: Result<(), VtlError> = (|| {
        let tx = conn.transaction()?;
        let tape_id = if let Some(id) = existing_tape {
            tx.execute(
                "UPDATE tapes SET slot = ?1, shelf_id = NULL, image_path = ?2 WHERE id = ?3",
                params![slot, dest_path.to_string_lossy(), id],
            )?;
            id
        } else {
            tx.execute(
                "INSERT INTO tapes (library_id, shelf_id, barcode, name, slot, capacity_bytes, used_bytes, created_at, image_path)
                 VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    library_id,
                    barcode.as_str(),
                    tape_name.as_str(),
                    slot,
                    metadata.len(),
                    metadata.len(),
                    Utc::now(),
                    dest_path.to_string_lossy()
                ],
            )?;
            tx.last_insert_rowid()
        };

        tx.execute(
            "UPDATE slots SET tape_id = ?1 WHERE library_id = ?2 AND slot_id = ?3",
            params![tape_id, library_id, slot],
        )?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(e) = tx_result {
        let _ = fs::remove_file(&dest_path);
        return Err(e);
    }

    log_message(&format!(
        "Successfully imported '{}' to slot {} as '{}' (barcode: {})",
        path, slot, tape_name, barcode
    ));
    println!(
        "Imported '{}' to slot {} as '{}' (barcode: {})",
        path, slot, tape_name, barcode
    );

    Ok(())
}

fn export_tape(slot: i32, output: &str, checksum: bool) -> Result<(), VtlError> {
    log_message(&format!(
        "Exporting tape from slot {} to '{}'",
        slot, output
    ));

    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;

    let (image_path, tape_name): (String, String) = conn
        .query_row(
            "SELECT t.image_path, t.name FROM tapes t
             JOIN slots s ON t.library_id = s.library_id AND t.slot = s.slot_id
             WHERE s.library_id = ?1 AND s.slot_id = ?2",
            params![library_id, slot],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| {
            log_error("export_tape", &format!("Slot {} is empty", slot));
            VtlError::SlotEmpty
        })?;

    fs::copy(&image_path, output)?;

    if checksum {
        let mut f = fs::File::open(output)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let digest = hasher.finalize();
        let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
        let sidecar = format!("{}.sha256", output);
        let fname = Path::new(output)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("export.vtltape");
        fs::write(&sidecar, format!("{}  {}\n", hex, fname))?;
        println!("SHA256 {}  (sidecar: {})", hex, sidecar);
    }

    log_message(&format!(
        "Successfully exported tape '{}' from slot {} to {}",
        tape_name, slot, output
    ));
    println!(
        "Exported tape '{}' from slot {} to {}",
        tape_name, slot, output
    );

    Ok(())
}

fn list_libraries() -> Result<(), VtlError> {
    let conn = init_db()?;
    let mut stmt = conn.prepare("SELECT id, name, created_at FROM vtl_libraries ORDER BY name")?;
    println!("Named virtual tape libraries:");
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (id, name, created) = row?;
        if is_test_only_library_name(&name) {
            continue;
        }
        println!("  {:>4}  {}  ({})", id, name, created);
    }
    Ok(())
}

pub(crate) fn create_named_library(
    name: &str,
    drives: i32,
    slots: i32,
) -> Result<KernelGeomSync, VtlError> {
    if name.is_empty() {
        return Err(VtlError::InvalidParameter(
            "Library name cannot be empty".to_string(),
        ));
    }
    validate_library_name(name)?;
    if name == OFFLINE_LIBRARY_NAME {
        return Err(VtlError::InvalidParameter(format!(
            "Library name '{}' is reserved for offline tape storage",
            OFFLINE_LIBRARY_NAME
        )));
    }
    #[cfg(not(test))]
    if name == LEGACY_DEFAULT_LIBRARY_NAME {
        return Err(VtlError::InvalidParameter(format!(
            "库名 '{}' 已废弃；请使用显式名称，例如：vtladm library create marstor --drives 2 --slots 10",
            LEGACY_DEFAULT_LIBRARY_NAME
        )));
    }
    if drives < 1 || drives > VTL_KERNEL_MAX_DRIVES_PER_LIB {
        return Err(VtlError::InvalidParameter(format!(
            "驱动器数须在 1..={} 之间（与内核 vtl 一致）",
            VTL_KERNEL_MAX_DRIVES_PER_LIB
        )));
    }
    if slots < 1 || slots > VTL_KERNEL_MAX_DATA_SLOTS_PER_LIB {
        return Err(VtlError::InvalidParameter(format!(
            "数据槽位数须在 1..={} 之间（不含 I/E mail 槽；与内核 vtl 一致）",
            VTL_KERNEL_MAX_DATA_SLOTS_PER_LIB
        )));
    }
    log_message(&format!(
        "Creating VTL library '{}' with {} drives and {} slots",
        name, drives, slots
    ));

    let mut conn = init_db()?;

    let existing_slots: Option<i64> = if let Ok(lib_id) = resolve_library_id(&conn, name) {
        Some(conn.query_row(
            "SELECT COUNT(*) FROM slots WHERE library_id = ?1",
            params![lib_id],
            |r| r.get(0),
        )?)
    } else {
        None
    };

    if let Some(n) = existing_slots {
        if n > 0 {
            return Err(VtlError::LibraryExists(name.to_string()));
        }
    }

    let tx = conn.transaction()?;
    let lib_id = if let Ok(id) = resolve_library_id(&tx, name) {
        id
    } else {
        let n_online = count_exported_online_libraries(&tx)?;
        if n_online >= VTL_KERNEL_MAX_ONLINE_LIBRARIES as i64 {
            return Err(VtlError::InvalidParameter(format!(
                "在线带库已达上限 {}（内核 vtl_instances 最多 {} 段；不含 {} / {}）",
                VTL_KERNEL_MAX_ONLINE_LIBRARIES,
                VTL_KERNEL_MAX_ONLINE_LIBRARIES,
                OFFLINE_LIBRARY_NAME,
                LEGACY_DEFAULT_LIBRARY_NAME
            )));
        }
        tx.execute(
            "INSERT INTO vtl_libraries (name, created_at) VALUES (?1, ?2)",
            params![name, Utc::now().to_rfc3339()],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO shelves (library_id, name, is_default_unused) VALUES (?1, ?2, 1)",
            params![id, DEFAULT_UNUSED_SHELF_NAME],
        )?;
        id
    };

    tx.execute(
        "INSERT OR REPLACE INTO library_config (library_id, key, value) VALUES (?1, 'max_drives', ?2)",
        params![lib_id, drives.to_string()],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO library_config (library_id, key, value) VALUES (?1, 'slots', ?2)",
        params![lib_id, slots.to_string()],
    )?;

    for i in 0..drives {
        tx.execute(
            "INSERT OR IGNORE INTO drives (library_id, drive_id, tape_id) VALUES (?1, ?2, NULL)",
            params![lib_id, i],
        )?;
    }

    for i in 0..slots {
        tx.execute(
            "INSERT OR IGNORE INTO slots (library_id, slot_id, tape_id, is_import_export) VALUES (?1, ?2, NULL, 0)",
            params![lib_id, i],
        )?;
    }

    for i in 0..4 {
        tx.execute(
            "INSERT OR IGNORE INTO slots (library_id, slot_id, tape_id, is_import_export) VALUES (?1, ?2, NULL, 1)",
            params![lib_id, MAILSLOT_OFFSET + i],
        )?;
    }

    tx.commit()?;

    let lib_dir = get_tape_dir().join(sanitize_lib_dir_component(name));
    fs::create_dir_all(&lib_dir)?;

    log_message(&format!(
        "Successfully created VTL library '{}' with {} drives and {} slots",
        name, drives, slots
    ));
    println!(
        "Created VTL library '{}' with {} drives and {} slots (default shelf: '{}')",
        name, drives, slots, DEFAULT_UNUSED_SHELF_NAME
    );
    println!("Use: vtladm --library {} ...", name);
    println!("Configuration saved to {}", get_db_path().display());

    let geom = maybe_reload_kernel_vtl_after_db_change();
    if geom.kernel_geom != "ioctl_ok"
        && geom.kernel_geom != "rescan_only"
        && geom.kernel_geom != "hot_geom_disabled"
        && geom.kernel_geom != "reload_ok"
        && geom.kernel_geom != "script_ok"
    {
        println!(
            "kernel geometry sync: {} {}",
            geom.kernel_geom, geom.kernel_geom_detail
        );
    } else if geom.kernel_geom == "hot_geom_disabled" {
        println!(
            "kernel geometry: {} — run: vtl-kernelctl reload",
            geom.kernel_geom_detail
        );
    }
    if geom.scsi_rescan.as_deref() == Some("failed") {
        println!(
            "SCSI: lsscsi may not match library geometry yet — run: sh /opt/vtladm/scripts/vtl-scsi-scan-all-hosts.sh 5"
        );
    } else if matches!(
        geom.kernel_geom.as_str(),
        "ioctl_ok" | "reload_ok" | "script_ok"
    ) {
        println!(
            "SCSI: kernel geometry applied; local lsscsi -g should show 1 changer + {} drive(s) per library host (iSCSI library-export is separate)",
            drives
        );
    }
    Ok(geom)
}

fn list_shelves() -> Result<(), VtlError> {
    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;
    let mut stmt = conn.prepare(
        "SELECT id, name, is_default_unused FROM shelves WHERE library_id = ?1 ORDER BY is_default_unused DESC, name",
    )?;
    println!("Tape shelves (library: {}):", current_library_name());
    for row in stmt.query_map(params![library_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (id, name, is_def) = row?;
        let tag = if is_def != 0 {
            " [default / 未使用]"
        } else {
            ""
        };
        println!("  {:>4}  {}{}", id, name, tag);
    }
    Ok(())
}

fn create_shelf(shelf_name: &str) -> Result<(), VtlError> {
    if shelf_name.is_empty() {
        return Err(VtlError::InvalidParameter(
            "Shelf name cannot be empty".to_string(),
        ));
    }
    if shelf_name == DEFAULT_UNUSED_SHELF_NAME {
        return Err(VtlError::InvalidParameter(format!(
            "Shelf name '{}' is reserved for the default unused shelf",
            DEFAULT_UNUSED_SHELF_NAME
        )));
    }
    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;
    conn.execute(
        "INSERT INTO shelves (library_id, name, is_default_unused) VALUES (?1, ?2, 0)",
        params![library_id, shelf_name],
    )
    .map_err(|_| VtlError::InvalidParameter(format!("Shelf '{}' may already exist", shelf_name)))?;
    println!(
        "Created shelf '{}' in library '{}'",
        shelf_name,
        current_library_name()
    );
    Ok(())
}

fn list_shelf_tapes(shelf_filter: Option<&str>) -> Result<(), VtlError> {
    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &current_library_name())?;
    let lib = current_library_name();

    if let Some(sname) = shelf_filter {
        let sid = resolve_shelf_id(&conn, library_id, sname)?;
        let mut stmt = conn.prepare(
            "SELECT name, barcode, capacity_bytes, used_bytes FROM tapes WHERE library_id = ?1 AND shelf_id = ?2 ORDER BY name",
        )?;
        println!("Tapes on shelf '{}' (library: {}):", sname, lib);
        for row in stmt.query_map(params![library_id, sid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u64>(2)?,
                r.get::<_, u64>(3)?,
            ))
        })? {
            let (n, bc, cap, used) = row?;
            println!(
                "  {:<20} {:<12} size {} used {}",
                n,
                bc,
                format_size(cap),
                format_size(used)
            );
        }
    } else {
        let mut sh =
            conn.prepare("SELECT id, name FROM shelves WHERE library_id = ?1 ORDER BY name")?;
        println!("Tapes by shelf (library: {}):", lib);
        for shelf_row in sh.query_map(params![library_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })? {
            let (sid, sname) = shelf_row?;
            let mut stmt = conn.prepare(
                "SELECT name, barcode, capacity_bytes FROM tapes WHERE library_id = ?1 AND shelf_id = ?2 ORDER BY name",
            )?;
            println!("--- shelf: {} ---", sname);
            for t in stmt.query_map(params![library_id, sid], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u64>(2)?,
                ))
            })? {
                let (n, bc, cap) = t?;
                println!("  {:<20} {:<12} {}", n, bc, format_size(cap));
            }
        }
    }
    Ok(())
}

fn shelf_place_tape(tape_name: &str, shelf_name: Option<&str>) -> Result<(), VtlError> {
    let mut conn = init_db()?;
    let lib_name = current_library_name();
    let library_id = resolve_library_id(&conn, &lib_name)?;
    let tape_id: i64 = conn
        .query_row(
            "SELECT id FROM tapes WHERE library_id = ?1 AND name = ?2",
            params![library_id, tape_name],
            |r| r.get(0),
        )
        .map_err(|_| VtlError::TapeNotFound(tape_name.to_string()))?;

    if tape_in_drive(&conn, library_id, tape_id)? {
        return Err(VtlError::TapeInDrive);
    }

    let target_shelf = if let Some(s) = shelf_name {
        resolve_shelf_id(&conn, library_id, s)?
    } else {
        default_shelf_id(&conn, library_id)?
    };

    let tx = conn.transaction()?;
    let current_slot: Option<i32> = tx.query_row(
        "SELECT slot FROM tapes WHERE id = ?1",
        params![tape_id],
        |r| r.get::<_, Option<i32>>(0),
    )?;

    if let Some(s) = current_slot {
        tx.execute(
            "UPDATE slots SET tape_id = NULL WHERE library_id = ?1 AND slot_id = ?2",
            params![library_id, s],
        )?;
    }
    tx.execute(
        "UPDATE slots SET tape_id = NULL WHERE library_id = ?1 AND tape_id = ?2",
        params![library_id, tape_id],
    )?;

    tx.execute(
        "UPDATE tapes SET slot = NULL, shelf_id = ?1 WHERE id = ?2",
        params![target_shelf, tape_id],
    )?;
    tx.commit()?;

    if robot_sync::robot_sync_enabled() {
        if let Err(e) = robot_sync::evacuate_tape_from_changer(&conn, library_id, tape_name) {
            eprintln!(
                "Warning: shelf-place: database updated but kernel evacuate failed: {} (run: vtladm -L <lib> robot auto-align)",
                e
            );
        }
        reconcile::try_post_op_auto_align(library_id);
    }

    println!(
        "Tape '{}' moved to shelf in library '{}'",
        tape_name, lib_name
    );
    Ok(())
}

fn assign_tape_to_slot(tape_name: &str, slot: i32) -> Result<(), VtlError> {
    let mut conn = init_db()?;
    let lib_name = current_library_name();
    let library_id = resolve_library_id(&conn, &lib_name)?;
    let tx = conn.transaction()?;
    assign_one_tape_to_slot(&tx, library_id, tape_name, slot)?;
    tx.commit()?;

    if robot_sync::robot_ioctl_enabled() {
        let bc = robot_sync::tape_barcode_for_name(&conn, library_id, tape_name);
        if let Err(e) =
            robot_sync::kernel_slot_place(&conn, library_id, slot, tape_name, bc.as_deref())
        {
            robot_sync::warn_kernel_sync_failed("assign-slot", &e);
        }
    }
    if robot_sync::robot_sync_enabled() {
        reconcile::try_post_op_auto_align(library_id);
    }

    println!(
        "Tape '{}' assigned to slot{} in library '{}'",
        tape_name, slot, lib_name
    );
    Ok(())
}

fn robot_sync_db_from_kernel() -> Result<(), VtlError> {
    let report = reconcile::sync_db_from_kernel_all_libraries()?;
    println!(
        "sync-db: {} online librar{}; {} kernel data-slot hint(s) mirrored to tapes.slot",
        report.libraries,
        if report.libraries == 1 { "y" } else { "ies" },
        report.tapes_updated
    );
    Ok(())
}

fn robot_auto_align_library() -> Result<(), VtlError> {
    let lib_name = current_library_name();
    let conn = init_db()?;
    let library_id = resolve_library_id(&conn, &lib_name)?;
    let r = reconcile::auto_align_library(library_id)?;
    println!(
        "Auto-align library '{}': evacuated={} applied={} pulled={} drifts_remaining={}",
        lib_name, r.evacuated, r.fixes_applied, r.pull_updates, r.drifts_remaining
    );
    if r.drifts_remaining > 0 {
        return Err(VtlError::InvalidParameter(format!(
            "AUTO_ALIGN_DRIFT: {} drift(s) remain; run: vtladm -L {} robot reconcile [--apply|--pull]",
            r.drifts_remaining, lib_name
        )));
    }
    Ok(())
}

fn robot_reconcile_library(apply: bool, pull: bool) -> Result<(), VtlError> {
    let conn = init_db()?;
    let lib_name = current_library_name();
    let library_id = resolve_library_id(&conn, &lib_name)?;
    let report = match reconcile::reconcile_library(library_id, apply, pull) {
        Ok(r) => r,
        Err(VtlError::InvalidParameter(msg)) if msg.starts_with("RECONCILE_IOCTL") => {
            eprintln!("{}", msg);
            return Err(VtlError::InvalidParameter(msg));
        }
        Err(e) => return Err(e),
    };
    reconcile::print_reconcile_report(&lib_name, &report);
    if apply && report.drifts.is_empty() {
        println!("Reconcile apply complete: DB and kernel now match.");
    }
    if !apply && !pull && !report.drifts.is_empty() {
        return Err(VtlError::InvalidParameter(reconcile::drift_error_message(
            &report,
        )));
    }
    Ok(())
}

fn robot_sync_library() -> Result<(), VtlError> {
    let conn = init_db()?;
    let lib_name = current_library_name();
    let library_id = resolve_library_id(&conn, &lib_name)?;
    let fixes = reconcile::push_db_to_kernel(&conn, library_id)?;
    println!(
        "Kernel full sync for library '{}' ({} changer operation(s); slots+drives+mailslots)",
        lib_name, fixes
    );
    Ok(())
}

fn reset_web_auth(password: Option<&str>) -> Result<(), VtlError> {
    let pw = password.unwrap_or(web_auth::DEFAULT_WEB_PASSWORD);
    let auth_path = get_config().log_dir.join("web_admin.json");
    let st = web_auth::WebState::new(auth_path.clone());
    st.force_reset_auth(pw)
        .map_err(|e| VtlError::InvalidParameter(e))?;
    println!(
        "Web auth reset: user={} file={}",
        web_auth::DEFAULT_WEB_USER,
        auth_path.display()
    );
    if password.is_none() {
        println!(
            "Default password: {} (change it after login)",
            web_auth::DEFAULT_WEB_PASSWORD
        );
    }
    Ok(())
}

/// 安装/建库等子命令在尚无在线库时也可运行（`init-config`、`library create` 等）。
fn command_requires_active_library(cmd: &Commands) -> bool {
    match cmd {
        Commands::InitConfig
        | Commands::CreateLibrary { .. }
        | Commands::Library { .. }
        | Commands::Transport { .. }
        | Commands::Serve { .. }
        | Commands::Patrol
        | Commands::ResetWebAuth { .. } => false,
        Commands::Robot {
            subcommand: RobotSubcommand::AutoAlignAll,
        } => false,
        _ => true,
    }
}

fn init_config() -> Result<(), VtlError> {
    log_message("Initializing configuration file");
    let params = InitialVtlConfParams {
        db_path: PathBuf::from(DEFAULT_INIT_DB_PATH),
        tape_dir: PathBuf::from(DEFAULT_INIT_TAPE_DIR),
        log_dir: PathBuf::from(DEFAULT_INIT_LOG_DIR),
        kernel_vtl_reload_script: Some(PathBuf::from(DEFAULT_INIT_KERNEL_RELOAD_SCRIPT)),
        vtl_ko: Some(PathBuf::from(DEFAULT_INIT_VTL_KO)),
        vtl_reload_scan_delay_ms: None,
    };
    write_initial_vtl_conf(&params)?;
    let conf = primary_vtl_conf_path();
    log_message(&format!("Configuration file created at {}", conf.display()));
    println!("Created default configuration file at {}", conf.display());
    println!("You can modify this file to customize paths.");

    Ok(())
}

fn main() -> Result<(), VtlError> {
    let cli = Cli::parse();
    if command_requires_active_library(&cli.command) {
        let active_lib = resolve_active_library_name(cli.library.as_deref())?;
        set_current_library(&active_lib);
    } else if let Some(ref lib) = cli.library {
        let name = lib.trim();
        if !name.is_empty() {
            let conn = init_db()?;
            resolve_library_id(&conn, name)?;
            set_current_library(name);
        }
    }

    match &cli.command {
        Commands::CreateTape {
            name,
            size,
            shelf,
            tags,
        } => {
            let size_bytes = parse_size(size).unwrap_or_else(|e| {
                log_error("main", &e.to_string());
                eprintln!("Error: {}", e);
                process::exit(1);
            });
            create_tape(name, size_bytes, shelf.as_deref())?;

            if !tags.is_empty() {
                let mut conn = init_db()?;
                let library_id = resolve_library_id(&conn, &current_library_name())?;
                let tape_id: i64 = conn
                    .query_row(
                        "SELECT id FROM tapes WHERE library_id = ?1 AND name = ?2",
                        params![library_id, name],
                        |row| row.get(0),
                    )
                    .map_err(|_| VtlError::TapeNotFound(name.clone()))?;
                add_tags_to_tape(&mut conn, tape_id, tags)?;
            }
        }
        Commands::DeleteTape { name } => delete_tape(name)?,
        Commands::InitTape { name } => init_tape_in_library(&current_library_name(), name)?,
        Commands::ListTapes => list_tapes()?,
        Commands::Load { source, target } => {
            let slot = parse_slot(source)
                .ok_or_else(|| VtlError::InvalidParameter(format!("Invalid slot: {}", source)))?;
            let drive = parse_drive(target)
                .ok_or_else(|| VtlError::InvalidParameter(format!("Invalid drive: {}", target)))?;
            load_tape(slot, drive)?;
        }
        Commands::Unload { drive } => {
            let drive_num = parse_drive(drive)
                .ok_or_else(|| VtlError::InvalidParameter(format!("Invalid drive: {}", drive)))?;
            unload_tape(drive_num)?;
        }
        Commands::Eject { slot } => {
            let slot_num = parse_slot(slot)
                .ok_or_else(|| VtlError::InvalidParameter(format!("Invalid slot: {}", slot)))?;
            eject_tape(slot_num)?;
        }
        Commands::Robot { subcommand } => match subcommand {
            RobotSubcommand::Sync => robot_sync_library()?,
            RobotSubcommand::Reconcile { apply, pull } => {
                robot_reconcile_library(*apply, *pull)?;
            }
            RobotSubcommand::AutoAlign => robot_auto_align_library()?,
            RobotSubcommand::AutoAlignAll => reconcile::auto_align_all_online_libraries()?,
            RobotSubcommand::SyncDb => robot_sync_db_from_kernel()?,
        },
        Commands::Config { subcommand } => match subcommand {
            ConfigSubcommand::Set { params } => config_set(params)?,
            ConfigSubcommand::Show => config_show()?,
        },
        Commands::Inventory => inventory()?,
        Commands::Status => status()?,
        Commands::Snapshot { tape, snapshot } => snapshot_tape(tape, snapshot)?,
        Commands::Import { path, slot } => {
            let slot_num = parse_slot(slot)
                .ok_or_else(|| VtlError::InvalidParameter(format!("Invalid slot: {}", slot)))?;
            import_tape(path, slot_num)?;
        }
        Commands::Export {
            slot,
            output,
            checksum,
        } => {
            let slot_num = parse_slot(slot)
                .ok_or_else(|| VtlError::InvalidParameter(format!("Invalid slot: {}", slot)))?;
            export_tape(slot_num, output, *checksum)?;
        }
        Commands::CreateLibrary {
            name,
            drives,
            slots,
        } => {
            create_named_library(name, *drives, *slots)?;
        }
        Commands::Library { subcommand } => match subcommand {
            LibrarySubcommand::List => list_libraries()?,
            LibrarySubcommand::Create {
                name,
                drives,
                slots,
            } => {
                create_named_library(name, *drives, *slots)?;
            }
            LibrarySubcommand::Delete { name } => {
                let (warnings, geom) = delete_named_library(name)?;
                if geom.kernel_geom != "ioctl_ok"
                    && geom.kernel_geom != "rescan_only"
                    && geom.kernel_geom != "script_ok"
                    && geom.kernel_geom != "reload_ok"
                    && geom.kernel_geom != "hot_geom_disabled"
                {
                    eprintln!(
                        "kernel geometry sync: {} {}",
                        geom.kernel_geom, geom.kernel_geom_detail
                    );
                }
                for w in &warnings {
                    eprintln!("Warning: {}", w);
                }
                if !warnings.is_empty() {
                    log_message(&format!(
                        "Library '{}' deleted with {} file/directory cleanup warning(s); check tape_dir",
                        name,
                        warnings.len()
                    ));
                }
            }
        },
        Commands::Shelf { subcommand } => match subcommand {
            ShelfSubcommand::List => list_shelves()?,
            ShelfSubcommand::Create { name } => create_shelf(name)?,
            ShelfSubcommand::Tapes { shelf } => list_shelf_tapes(shelf.as_deref())?,
            ShelfSubcommand::Place { tape, shelf } => shelf_place_tape(tape, shelf.as_deref())?,
            ShelfSubcommand::Delete { name } => {
                delete_shelf_in_library(&current_library_name(), name)?;
            }
        },
        Commands::AssignSlot { tape, slot } => {
            let slot_num = parse_slot(slot)
                .ok_or_else(|| VtlError::InvalidParameter(format!("Invalid slot: {}", slot)))?;
            assign_tape_to_slot(tape, slot_num)?;
        }
        Commands::InitConfig => init_config()?,
        Commands::BatchCreate {
            count,
            size,
            prefix,
            tags,
        } => {
            let size_bytes = parse_size(size).unwrap_or_else(|e| {
                log_error("main", &e.to_string());
                eprintln!("Error: {}", e);
                process::exit(1);
            });
            batch_create_tapes(*count, size_bytes, prefix, tags)?;
        }
        Commands::BatchImport {
            directory,
            start_slot,
        } => {
            batch_import_tapes(directory, *start_slot)?;
        }
        Commands::Quota { subcommand } => match subcommand {
            QuotaSubcommand::Set {
                max_total_size,
                max_tapes,
            } => {
                quota_set(max_total_size.as_deref(), *max_tapes)?;
            }
            QuotaSubcommand::Show => quota_show()?,
            QuotaSubcommand::Check => quota_check()?,
        },
        Commands::Tag { subcommand } => match subcommand {
            TagSubcommand::Add { tape, tags } => tag_add(tape, tags)?,
            TagSubcommand::Remove { tape, tags } => tag_remove(tape, tags)?,
            TagSubcommand::List { tape } => tag_list(tape.clone())?,
            TagSubcommand::Delete { tag } => tag_delete(tag)?,
        },
        Commands::Search {
            name,
            tag,
            min_size,
            max_size,
            free_space,
        } => {
            search_tapes(
                name.clone(),
                tag.clone(),
                min_size.clone(),
                max_size.clone(),
                *free_space,
            )?;
        }
        Commands::Transport { subcommand } => match subcommand {
            TransportSubcommand::Show => fab_transport::transport_show()?,
            TransportSubcommand::Check => fab_transport::transport_check()?,
            TransportSubcommand::Guide => fab_transport::transport_guide()?,
        },
        Commands::Serve { host, port } => web::run_web_ui(host, *port)?,
        Commands::Patrol => {
            let code = patrol::run_patrol();
            if code != patrol::PATROL_EXIT_OK {
                std::process::exit(code);
            }
        }
        Commands::KernelSpec { insmod_max } => {
            let spec = if *insmod_max {
                build_plan_b_insmod_spec()
            } else {
                build_vtl_instances_kernel_spec()?
            };
            print!("{}", spec);
        }
        Commands::KernelAlign { quiet } => {
            let geom = maybe_reload_kernel_vtl_after_db_change();
            if !*quiet {
                println!(
                    "kernel_geom={} detail={}",
                    geom.kernel_geom, geom.kernel_geom_detail
                );
            }
            match geom.kernel_geom.as_str() {
                "ioctl_ok" | "rescan_only" | "skipped" | "ioctl_unavailable"
                | "hot_geom_disabled" | "script_ok" | "reload_ok" => {}
                "lio_exported"
                | "holders_busy"
                | "script_refused_holders"
                | "script_refused_no_fuser" => {
                    eprintln!("kernel-align refused: {}", geom.kernel_geom_detail);
                    process::exit(2);
                }
                _ => {
                    eprintln!(
                        "kernel-align failed: {} — {}",
                        geom.kernel_geom, geom.kernel_geom_detail
                    );
                    process::exit(1);
                }
            }
        }
        Commands::ResetWebAuth { password } => reset_web_auth(password.as_deref())?,
    }

    Ok(())
}

#[cfg(test)]
mod test_utils;

/// Baseline `VtlConfig` for robot-authority / `vtl.conf` parsing unit tests (Linux).
#[cfg(test)]
pub(crate) fn test_baseline_vtl_config() -> VtlConfig {
    VtlConfig {
        db_path: PathBuf::from("/var/tmp/vtladm_utest/unused.db"),
        tape_dir: PathBuf::from("/var/tmp/vtladm_utest/tapes"),
        log_dir: PathBuf::from("/var/tmp/vtladm_utest/log"),
        log_max_bytes: DEFAULT_LOG_MAX_BYTES,
        transport: fab_transport::FabTransport::default(),
        iscsi_iqn: None,
        iscsi_portals: None,
        fc_wwpn: None,
        kernel_vtl_reload_script: None,
        kernel_reload_on_db_change: false,
        kernel_geom_prefer_ioctl: true,
        kernel_geometry_mode: KernelGeometryMode::Legacy,
        vtl_ko: None,
        vtl_reload_scan_delay_ms: None,
        robot_sync: true,
        auto_reconcile_pull: false,
        auto_sync_db_from_kernel: false,
        kernel_personality: "vtl".to_string(),
    }
}

#[cfg(all(test, target_os = "linux"))]
fn parse_vtl_conf_lines_for_test(content: &str) -> VtlConfig {
    let mut config = test_baseline_vtl_config();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(2, '=').collect();
        if parts.len() != 2 {
            continue;
        }
        apply_vtl_conf_kv(parts[0].trim(), parts[1].trim(), &mut config);
    }
    config
}

#[cfg(test)]
mod tests {
    use super::test_utils::{cleanup_temp_vtl, prepare_temp_vtl};
    use super::*;

    #[test]
    fn test_vtladm_supported_on_64_bit_pointer_only() {
        assert_eq!(
            std::mem::size_of::<usize>(),
            8,
            "vtladm is built for 64-bit targets only (see userspace/build.rs)"
        );
    }

    #[test]
    fn test_default_paths_under_install_prefix() {
        for p in [
            PRIMARY_VTL_CONF,
            DEFAULT_INIT_DB_PATH,
            DEFAULT_INIT_TAPE_DIR,
            DEFAULT_INIT_LOG_DIR,
            DEFAULT_INIT_KERNEL_RELOAD_SCRIPT,
            DEFAULT_INIT_VTL_KO,
        ] {
            assert!(
                p.starts_with(VTL_INSTALL_PREFIX),
                "{} not under {}",
                p,
                VTL_INSTALL_PREFIX
            );
        }
        assert_eq!(PRIMARY_VTL_STATEDIR, "/opt/vtladm/var");
    }

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("100B").unwrap(), 100);
        assert_eq!(parse_size("1024B").unwrap(), 1024);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("1KB").unwrap(), 1024);
        assert_eq!(parse_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1MB").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("1GB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(
            parse_size("2.5G").unwrap(),
            (2.5 * 1024.0 * 1024.0 * 1024.0) as u64
        );
    }

    #[test]
    fn test_format_initial_vtl_conf_includes_keys() {
        let p = InitialVtlConfParams {
            db_path: PathBuf::from("/a/vtl.db"),
            tape_dir: PathBuf::from("/b/tapes"),
            log_dir: PathBuf::from("/c/log/vtl"),
            kernel_vtl_reload_script: Some(PathBuf::from("/usr/local/bin/vtl-kernel-reload.sh")),
            vtl_ko: Some(PathBuf::from("/lib/modules/6.1.0/extra/vtl.ko")),
            vtl_reload_scan_delay_ms: Some(450),
        };
        let s = format_initial_vtl_conf(&p);
        assert!(s.contains("db_path=/a/vtl.db"));
        assert!(s.contains("tape_dir=/b/tapes"));
        assert!(s.contains("log_dir=/c/log/vtl"));
        assert!(s.contains("kernel_vtl_reload_script=/usr/local/bin/vtl-kernel-reload.sh"));
        assert!(s.contains("vtl_ko=/lib/modules/6.1.0/extra/vtl.ko"));
        assert!(s.contains("vtl_reload_scan_delay_ms=450"));
        assert!(s.contains("kernel_reload_on_db_change=false"));
        assert!(s.contains("kernel_geom_prefer_ioctl=false"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_format_initial_vtl_conf_robot_backup_defaults() {
        let p = InitialVtlConfParams {
            db_path: PathBuf::from("/opt/vtladm/var/vtl.db"),
            tape_dir: PathBuf::from("/opt/vtladm/var/tapes"),
            log_dir: PathBuf::from("/opt/vtladm/var/log/vtl"),
            kernel_vtl_reload_script: None,
            vtl_ko: None,
            vtl_reload_scan_delay_ms: None,
        };
        let s = format_initial_vtl_conf(&p);
        assert!(s.contains("robot_sync=true"));
        assert!(s.contains("auto_reconcile_pull=true"));
        assert!(s.contains("auto_sync_db_from_kernel=true"));
        assert!(!s.contains("robot_authority"));
    }

    #[test]
    fn test_format_initial_vtl_conf_plan_b_hint() {
        let p = InitialVtlConfParams {
            db_path: PathBuf::from("/opt/vtladm/var/vtl.db"),
            tape_dir: PathBuf::from("/opt/vtladm/var/tapes"),
            log_dir: PathBuf::from("/opt/vtladm/var/log/vtl"),
            kernel_vtl_reload_script: None,
            vtl_ko: None,
            vtl_reload_scan_delay_ms: None,
        };
        let s = format_initial_vtl_conf(&p);
        assert!(s.contains("Plan B"));
        assert!(s.contains("# kernel_geometry_mode=fixed"));
        assert!(s.contains("kernel_reload_on_db_change=false"));
    }

    #[test]
    fn test_kernel_geometry_mode_parse() {
        assert_eq!(
            KernelGeometryMode::parse("fixed"),
            Some(KernelGeometryMode::Fixed)
        );
        assert_eq!(
            KernelGeometryMode::parse("plan_b"),
            Some(KernelGeometryMode::Fixed)
        );
        assert_eq!(
            KernelGeometryMode::parse("semi_thin"),
            Some(KernelGeometryMode::Fixed)
        );
        assert_eq!(
            KernelGeometryMode::parse("legacy"),
            Some(KernelGeometryMode::Legacy)
        );
        assert_eq!(KernelGeometryMode::parse("bogus"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_vtl_conf_kernel_geometry_mode_fixed() {
        let cfg = parse_vtl_conf_lines_for_test(
            "kernel_geometry_mode=fixed\nkernel_geometry_mode=legacy\n",
        );
        assert_eq!(cfg.kernel_geometry_mode, KernelGeometryMode::Fixed);
        let cfg2 = parse_vtl_conf_lines_for_test("kernel_geometry_mode=plan_b\n");
        assert_eq!(cfg2.kernel_geometry_mode, KernelGeometryMode::Fixed);
    }

    #[cfg(target_os = "linux")]
    mod robot_sync_config_linux_tests {
        use super::*;

        #[test]
        fn test_parse_vtl_conf_robot_defaults() {
            let cfg = parse_vtl_conf_lines_for_test(
                "auto_reconcile_pull=false\nauto_sync_db_from_kernel=true\n",
            );
            assert!(!cfg.auto_reconcile_pull);
            assert!(cfg.auto_sync_db_from_kernel);
        }

        #[test]
        fn test_parse_vtl_conf_personality() {
            let cfg = parse_vtl_conf_lines_for_test("personality=IBM\n");
            assert_eq!(cfg.kernel_personality, "IBM");
        }

        #[test]
        fn test_robot_ioctl_follows_robot_sync() {
            let snap: Vec<(String, Option<String>)> = ["VTL_ROBOT_SYNC", "VTL_USE_ENV_ONLY"]
                .iter()
                .map(|k| (k.to_string(), std::env::var(k).ok()))
                .collect();
            std::env::set_var("VTL_USE_ENV_ONLY", "1");
            std::env::set_var("VTL_ROBOT_SYNC", "1");
            invalidate_vtl_config_cache();
            assert!(robot_sync::robot_sync_enabled());
            assert!(robot_sync::robot_ioctl_enabled());
            std::env::set_var("VTL_ROBOT_SYNC", "0");
            invalidate_vtl_config_cache();
            assert!(!robot_sync::robot_ioctl_enabled());
            for (k, v) in snap {
                match v {
                    Some(s) => std::env::set_var(&k, s),
                    None => std::env::remove_var(&k),
                }
            }
            invalidate_vtl_config_cache();
        }

        #[test]
        fn test_changer_inventory_uses_kernel_when_robot_sync_on() {
            let snap: Vec<(String, Option<String>)> = ["VTL_ROBOT_SYNC", "VTL_USE_ENV_ONLY"]
                .iter()
                .map(|k| (k.to_string(), std::env::var(k).ok()))
                .collect();
            std::env::set_var("VTL_USE_ENV_ONLY", "1");
            std::env::set_var("VTL_ROBOT_SYNC", "1");
            invalidate_vtl_config_cache();
            assert!(robot_sync::changer_inventory_uses_kernel());
            std::env::set_var("VTL_ROBOT_SYNC", "0");
            invalidate_vtl_config_cache();
            assert!(!robot_sync::changer_inventory_uses_kernel());
            for (k, v) in snap {
                match v {
                    Some(s) => std::env::set_var(&k, s),
                    None => std::env::remove_var(&k),
                }
            }
            invalidate_vtl_config_cache();
        }
    }

    #[test]
    fn test_build_vtl_instances_kernel_spec_single_online_library() {
        let dir = prepare_temp_vtl("vtl_inst_spec_one");
        let _ = create_named_library("marstor", 2, 10);
        let spec = build_vtl_instances_kernel_spec().expect("spec");
        assert_eq!(spec, "2x10");
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_count_exported_online_libraries_excludes_default() {
        let dir = prepare_temp_vtl("count_exported_libs");
        let _ = create_named_library(LEGACY_DEFAULT_LIBRARY_NAME, 1, 1);
        let _ = create_named_library("marstor", 2, 10);
        let conn = init_db().expect("db");
        assert_eq!(count_exported_online_libraries(&conn).expect("count"), 1);
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_build_vtl_instances_kernel_spec_excludes_legacy_default_name() {
        let dir = prepare_temp_vtl("vtl_inst_spec_no_default");
        let _ = create_named_library(LEGACY_DEFAULT_LIBRARY_NAME, 1, 1);
        let _ = create_named_library("marstor", 2, 10);
        let spec = build_vtl_instances_kernel_spec().expect("spec");
        assert_eq!(spec, "2x10");
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_build_vtl_instances_kernel_spec_clamped_to_kernel_caps() {
        let dir = prepare_temp_vtl("vtl_inst_spec_clamp");
        let _ = create_named_library("marstor", 8, 50).unwrap();
        let conn = init_db().expect("db");
        let lib_id = resolve_library_id(&conn, "marstor").expect("lib");
        for i in 8..12 {
            conn.execute(
                "INSERT OR IGNORE INTO drives (library_id, drive_id, tape_id) VALUES (?1, ?2, NULL)",
                params![lib_id, i],
            )
            .expect("extra drive row");
        }
        let spec = build_vtl_instances_kernel_spec().expect("spec");
        assert_eq!(spec, "8x50");
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_build_vtl_instances_kernel_spec_skips_empty_shell_library() {
        let dir = prepare_temp_vtl("vtl_inst_spec_empty");
        let conn = init_db().expect("db");
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO vtl_libraries (name, created_at) VALUES ('empty_shell', ?1)",
            params![now],
        )
        .expect("insert");
        let _ = create_named_library("real", 2, 8);
        let spec = build_vtl_instances_kernel_spec().expect("spec");
        assert_eq!(spec, "2x8");
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_build_vtl_instances_kernel_spec_rejects_all_empty_shells() {
        let dir = prepare_temp_vtl("vtl_inst_spec_all_empty");
        let conn = init_db().expect("db");
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO vtl_libraries (name, created_at) VALUES ('shell_only', ?1)",
            params![now],
        )
        .expect("insert");
        let r = build_vtl_instances_kernel_spec();
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_build_vtl_instances_kernel_spec_at_max_slots() {
        let dir = prepare_temp_vtl("vtl_inst_spec_maxslot");
        let _ = create_named_library("marstor", 2, 256);
        let spec = build_vtl_instances_kernel_spec().expect("spec");
        assert_eq!(spec, "2x256");
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_build_plan_b_insmod_spec() {
        assert_eq!(
            build_plan_b_insmod_spec(),
            "8x256,8x256,8x256,8x256,8x256,8x256,8x256,8x256"
        );
    }

    #[test]
    fn test_parse_vtl_instances_segments() {
        let v = parse_vtl_instances_segments("2x10,8x256,8x256");
        assert_eq!(v, vec![(2, 10), (8, 256), (8, 256)]);
    }

    #[test]
    fn test_build_vtl_instances_kernel_spec_padded() {
        let dir = prepare_temp_vtl("vtl_inst_spec_pad");
        let _ = create_named_library("marstor", 2, 10);
        let spec = build_vtl_instances_kernel_spec_padded(8).expect("spec");
        // 无 vtl.ko 时 sysfs 不可用，空闲 host 垫满配 8x256（非 1x1）
        assert_eq!(spec, "2x10,8x256,8x256,8x256,8x256,8x256,8x256,8x256");
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_named_library_rejects_too_many_drives() {
        let dir = prepare_temp_vtl("vtl_reject_drv");
        let r = create_named_library("default", 17, 10);
        assert!(r.is_err());
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_named_library_rejects_too_many_slots() {
        let dir = prepare_temp_vtl("vtl_reject_slot");
        let r = create_named_library("default", 2, 257);
        assert!(r.is_err());
        cleanup_temp_vtl(&dir);
    }

    /// 正常路径最多 8 个在线库；若 DB 中因迁移等出现第 9 个，`vtl_instances` 规格只取 `id` 升序前 8 个（与内核段数上限一致）。
    #[test]
    fn test_build_vtl_instances_kernel_spec_truncates_ninth_online_library() {
        let dir = prepare_temp_vtl("vtl_inst_spec_nine");
        let _ = init_db().expect("db");
        for n in 1..=7 {
            create_named_library(&format!("lx{}", n), 1, n + 1).expect("lib");
        }
        let conn = init_db().expect("db");
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO vtl_libraries (name, created_at) VALUES ('overflow', ?1)",
            params![now],
        )
        .expect("insert overflow lib");
        let oid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO shelves (library_id, name, is_default_unused) VALUES (?1, 'unused', 1)",
            params![oid],
        )
        .expect("shelf");
        conn.execute(
            "INSERT INTO drives (library_id, drive_id, tape_id) VALUES (?1, 0, NULL)",
            params![oid],
        )
        .expect("drive");
        conn
            .execute(
                "INSERT INTO slots (library_id, slot_id, tape_id, is_import_export) VALUES (?1, 0, NULL, 0)",
                params![oid],
            )
            .expect("slot");
        drop(conn);
        let spec = build_vtl_instances_kernel_spec().expect("spec");
        assert_eq!(spec.split(',').count(), 8);
        assert_eq!(
            spec, "1x1,1x2,1x3,1x4,1x5,1x6,1x7,1x8",
            "id 升序前 8 个在线库；overflow 被截断"
        );
        cleanup_temp_vtl(&dir);
    }

    /// DB 中数据槽行数超过内核上限时，生成 `vtl_instances` 段仍夹紧到 256（与 `VTL_MAX_SLOTS` 一致）。
    #[test]
    fn test_build_vtl_instances_kernel_spec_clamps_data_slots_from_db_count() {
        let dir = prepare_temp_vtl("vtl_inst_spec_slot_clamp");
        let _ = create_named_library("marstor", 1, 1).expect("lib");
        let conn = init_db().expect("db");
        let lib_id = resolve_library_id(&conn, "marstor").expect("id");
        // `create_named_library` 已占用 slot_id 0（数据）与 MAILSLOT_OFFSET..+4（I/E，同主键 library_id+slot_id）。
        // 不可对 100..103 再 INSERT 数据槽，否则 OR IGNORE 静默跳过，总数会少 4。
        for sid in 1..100 {
            conn.execute(
                "INSERT OR IGNORE INTO slots (library_id, slot_id, tape_id, is_import_export) VALUES (?1, ?2, NULL, 0)",
                params![lib_id, sid],
            )
            .expect("extra slot");
        }
        for sid in 104..=260 {
            conn.execute(
                "INSERT OR IGNORE INTO slots (library_id, slot_id, tape_id, is_import_export) VALUES (?1, ?2, NULL, 0)",
                params![lib_id, sid],
            )
            .expect("extra slot");
        }
        drop(conn);
        let spec = build_vtl_instances_kernel_spec().expect("spec");
        assert_eq!(spec, "1x256");
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_named_library_rejects_ninth_online_library() {
        let dir = prepare_temp_vtl("vtl_reject_9lib");
        let _ = init_db().expect("db");
        for n in 1..=7 {
            create_named_library(&format!("ly{}", n), 1, 1).expect("lib");
        }
        let r = create_named_library("ly8", 1, 1);
        assert!(r.is_err());
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_config_set_rejects_max_drives_above_kernel_cap() {
        let dir = prepare_temp_vtl("cfg_max_drv");
        let _ = create_named_library("default", 1, 1).unwrap();
        let r = config_set(&["max_drives=9".to_string()]);
        assert!(r.is_err());
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_config_set_rejects_slots_above_kernel_cap() {
        let dir = prepare_temp_vtl("cfg_max_slot");
        let _ = create_named_library("default", 1, 1).unwrap();
        let r = config_set(&["slots=257".to_string()]);
        assert!(r.is_err());
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_parse_size_invalid() {
        assert!(parse_size("invalid").is_err());
        assert!(parse_size("X").is_err());
        assert!(parse_size("-1G").is_err());
        assert!(parse_size("999999999999999999999T").is_err());
    }

    #[test]
    fn test_parse_slot() {
        assert_eq!(parse_slot("slot0"), Some(0));
        assert_eq!(parse_slot("slot1"), Some(1));
        assert_eq!(parse_slot("slot10"), Some(10));
        assert_eq!(parse_slot("mail0"), Some(100));
        assert_eq!(parse_slot("mail1"), Some(101));
        assert_eq!(parse_slot("0"), Some(0));
        assert_eq!(parse_slot("10"), Some(10));
        assert_eq!(parse_slot("invalid"), None);
    }

    #[test]
    fn test_parse_drive() {
        assert_eq!(parse_drive("drive0"), Some(0));
        assert_eq!(parse_drive("drive1"), Some(1));
        assert_eq!(parse_drive("0"), Some(0));
        assert_eq!(parse_drive("5"), Some(5));
        assert_eq!(parse_drive("invalid"), None);
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(1024), "1K");
        assert_eq!(format_size(2048), "2K");
        assert_eq!(format_size(1024 * 1024), "1M");
        assert_eq!(format_size(2 * 1024 * 1024), "2M");
        assert_eq!(format_size(1024 * 1024 * 1024), "1G");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2G");
    }

    #[test]
    fn test_generate_barcode() {
        let barcode1 = generate_barcode();
        let barcode2 = generate_barcode();

        assert!(barcode1.starts_with("VTL"));
        assert!(barcode2.starts_with("VTL"));
        assert_ne!(barcode1, barcode2);
    }

    #[test]
    fn test_integration_workflow() {
        let dir = prepare_temp_vtl("integration_test");

        let result = create_named_library("default", 2, 4);
        assert!(result.is_ok());

        let result = create_tape("test_tape_1", 10 * 1024 * 1024, None);
        assert!(result.is_ok());

        let result = create_tape("test_tape_2", 20 * 1024 * 1024, None);
        assert!(result.is_ok());

        let result = list_tapes();
        assert!(result.is_ok());

        let p = dir
            .join("tapes")
            .join("default")
            .join("test_tape_1.vtltape");
        let result = import_tape(p.to_str().unwrap(), 0);
        assert!(result.is_ok());

        let result = load_tape(0, 0);
        assert!(result.is_ok());

        let result = unload_tape(0);
        assert!(result.is_ok());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_error_handling_tape_not_found() {
        let dir = prepare_temp_vtl("error_test");

        let _ = create_named_library("default", 1, 1);

        let result = delete_tape("nonexistent_tape");
        assert!(result.is_err());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_error_handling_load_from_empty_slot() {
        let dir = prepare_temp_vtl("load_error_test");

        let _ = create_named_library("default", 1, 1);

        let result = load_tape(0, 0);
        assert!(result.is_err());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_snapshot_functionality() {
        let dir = prepare_temp_vtl("snapshot_test");

        let _ = create_named_library("default", 1, 1);
        let _ = create_tape("snap_test", 5 * 1024 * 1024, None);

        let result = snapshot_tape("snap_test", "snap_001");
        assert!(result.is_ok());

        let snapshot_path = dir
            .join("tapes")
            .join("default")
            .join("snap_test_snap_001.vtltape");
        assert!(snapshot_path.exists());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_export_functionality() {
        let dir = prepare_temp_vtl("export_test");

        let _ = create_named_library("default", 1, 1);
        let _ = create_tape("export_test", 5 * 1024 * 1024, None);
        let p = dir
            .join("tapes")
            .join("default")
            .join("export_test.vtltape");
        let _ = import_tape(p.to_str().unwrap(), 0);

        let export_path = dir.join("exported.vtltape");
        let result = export_tape(0, export_path.to_str().unwrap(), false);
        assert!(result.is_ok());
        assert!(export_path.exists());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_export_checksum_writes_sidecar() {
        let dir = prepare_temp_vtl("export_checksum_test");

        let _ = create_named_library("default", 1, 1);
        let _ = create_tape("ck_tape", 5 * 1024 * 1024, None);
        let p = dir.join("tapes").join("default").join("ck_tape.vtltape");
        let _ = import_tape(p.to_str().unwrap(), 0);

        let export_path = dir.join("out.vtltape");
        let result = export_tape(0, export_path.to_str().unwrap(), true);
        assert!(result.is_ok());
        let sidecar = dir.join("out.vtltape.sha256");
        assert!(sidecar.exists());
        let line = fs::read_to_string(&sidecar).expect("sidecar");
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        assert_eq!(parts[0].len(), 64);
        assert_eq!(parts.len(), 2);

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_eject_tape_to_mailslot() {
        let dir = prepare_temp_vtl("eject_test");

        let _ = create_named_library("default", 1, 1);
        let _ = create_tape("eject_test", 5 * 1024 * 1024, None);
        let p = dir.join("tapes").join("default").join("eject_test.vtltape");
        let _ = import_tape(p.to_str().unwrap(), 0);

        let result = eject_tape(0);
        assert!(result.is_ok());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_error_handling_drive_busy() {
        let dir = prepare_temp_vtl("drive_busy_test");

        let _ = create_named_library("default", 1, 2);
        let _ = create_tape("tape1", 5 * 1024 * 1024, None);
        let p = dir.join("tapes").join("default").join("tape1.vtltape");
        let _ = import_tape(p.to_str().unwrap(), 0);

        let _ = load_tape(0, 0);
        let result = load_tape(0, 0);
        assert!(result.is_err());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_error_handling_unload_empty_drive() {
        let dir = prepare_temp_vtl("unload_empty_test");

        let _ = create_named_library("default", 1, 1);

        let result = unload_tape(0);
        assert!(result.is_err());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_error_handling_eject_empty_slot() {
        let dir = prepare_temp_vtl("eject_empty_test");

        let _ = create_named_library("default", 1, 1);

        let result = eject_tape(0);
        assert!(result.is_err());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_config_set_and_show() {
        let dir = prepare_temp_vtl("config_test");

        let _ = create_named_library("default", 1, 1);

        let result = config_set(&["max_drives=4".to_string(), "slots=20".to_string()]);
        assert!(result.is_ok());

        let result = config_show();
        assert!(result.is_ok());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_load_unload_cycle() {
        let dir = prepare_temp_vtl("cycle_test");

        let _ = create_named_library("default", 2, 4);
        let _ = create_tape("cycle_tape", 10 * 1024 * 1024, None);
        let p = dir.join("tapes").join("default").join("cycle_tape.vtltape");
        let _ = import_tape(p.to_str().unwrap(), 0);

        for i in 0..3 {
            let result = load_tape(0, 0);
            assert!(result.is_ok(), "Load attempt {} failed", i);

            let result = unload_tape(0);
            assert!(result.is_ok(), "Unload attempt {} failed", i);
        }

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_multiple_tapes_inventory() {
        let dir = prepare_temp_vtl("multi_inventory_test");

        let _ = create_named_library("default", 2, 8);

        for i in 0..4 {
            let _ = create_tape(&format!("multi_tape_{}", i), 5 * 1024 * 1024, None);
            let p = dir
                .join("tapes")
                .join("default")
                .join(format!("multi_tape_{}.vtltape", i));
            let _ = import_tape(p.to_str().unwrap(), i as i32);
        }

        let result = inventory();
        assert!(result.is_ok());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_import_nonexistent_file() {
        let dir = prepare_temp_vtl("import_nonexistent_test");

        let _ = create_named_library("default", 1, 1);

        let result = import_tape("/nonexistent/path/file.vtltape", 0);
        assert!(result.is_err());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_logging_functions() {
        let dir = prepare_temp_vtl("logging_test");

        let _ = create_named_library("default", 1, 1);

        log_message("Test message");
        log_error("Test operation", "Test error");

        let log_path = dir.join("logs").join("vtladm.log");
        let error_log_path = dir.join("logs").join("vtladm_errors.log");

        assert!(log_path.exists());
        assert!(error_log_path.exists());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_concurrent_tape_operations() {
        use std::thread;

        let dir = prepare_temp_vtl("concurrent_test");

        let _ = create_named_library("default", 4, 16);

        let handles: Vec<_> = (0..4)
            .map(|i| {
                thread::spawn(move || {
                    let tape_name = format!("concurrent_tape_{}", i);
                    let result = create_tape(&tape_name, 5 * 1024 * 1024, None);
                    assert!(result.is_ok(), "Failed to create tape {}", i);
                })
            })
            .collect();

        for handle in handles {
            let _ = handle.join();
        }

        let result = list_tapes();
        assert!(result.is_ok());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_validate_tape_name() {
        assert!(validate_tape_name("valid_tape_123").is_ok());
        assert!(validate_tape_name("tape-001").is_ok());
        assert!(validate_tape_name("").is_err());
        assert!(validate_tape_name("tape/name").is_err());
        assert!(validate_tape_name("tape\\name").is_err());
        assert!(validate_tape_name("tape:name").is_err());
        assert!(validate_tape_name("tape*name").is_err());
        assert!(validate_tape_name("tape?name").is_err());
        assert!(validate_tape_name(".").is_err());
        assert!(validate_tape_name("..").is_err());
        assert!(validate_tape_name("bad\nname").is_err());
        assert!(validate_tape_name(&"a".repeat(256)).is_err());
        assert!(validate_tape_name("valid_name").is_ok());
    }

    /// `parse_size` 在配额/搜索等路径上会被频繁调用，不得触发写日志（避免热路径磁盘 I/O）。
    #[test]
    fn test_parse_size_does_not_write_vtl_log() {
        let dir = prepare_temp_vtl("parse_no_log");
        let log_path = dir.join("logs").join("vtladm.log");
        for _ in 0..32 {
            assert_eq!(parse_size("512M").unwrap(), 512 * 1024 * 1024);
        }
        assert!(
            !log_path.exists(),
            "parse_size must not create or append vtladm.log"
        );
        cleanup_temp_vtl(&dir);
    }

    /// 删除磁带时应一并删除 `tape_tags` 关联，避免残留行。
    #[test]
    fn test_delete_tape_removes_tape_tag_links() {
        let dir = prepare_temp_vtl("del_tape_tags");
        let _ = create_named_library("default", 1, 2);
        let _ = create_tape("tagged_for_delete", 1024 * 1024, None);
        let _ = tag_add("tagged_for_delete", &["keep_schema".to_string()]);
        let before: i64 = init_db()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM tape_tags", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 1);
        let _ = delete_tape("tagged_for_delete");
        let after: i64 = init_db()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM tape_tags", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(after, 0);
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_batch_create_tapes() {
        let dir = prepare_temp_vtl("batch_create_test");

        let _ = create_named_library("default", 2, 10);

        let result = batch_create_tapes(
            3,
            5 * 1024 * 1024,
            "batch_test",
            &["backup".to_string(), "important".to_string()],
        );
        assert!(result.is_ok());

        let result = list_tapes();
        assert!(result.is_ok());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_batch_import_tapes() {
        use std::fs;

        let dir = prepare_temp_vtl("batch_import_test");

        let _ = create_named_library("default", 2, 10);
        let _ = create_tape("import_tape_1", 5 * 1024 * 1024, None);
        let _ = create_tape("import_tape_2", 5 * 1024 * 1024, None);

        let import_dir = dir.join("import");
        fs::create_dir_all(&import_dir).unwrap();
        fs::copy(
            dir.join("tapes")
                .join("default")
                .join("import_tape_1.vtltape"),
            import_dir.join("import_tape_1.vtltape"),
        )
        .unwrap();
        fs::copy(
            dir.join("tapes")
                .join("default")
                .join("import_tape_2.vtltape"),
            import_dir.join("import_tape_2.vtltape"),
        )
        .unwrap();

        let result = batch_import_tapes(import_dir.to_str().unwrap(), 0);
        assert!(result.is_ok());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_quota_management() {
        let dir = prepare_temp_vtl("quota_test");

        let _ = create_named_library("default", 2, 10);

        let result = quota_set(Some("100M"), Some(5));
        assert!(result.is_ok());

        let result = quota_show();
        assert!(result.is_ok());

        let result = quota_check();
        assert!(result.is_ok());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_tag_system() {
        let dir = prepare_temp_vtl("tag_test");

        let _ = create_named_library("default", 2, 10);
        let _ = create_tape("tag_test_tape", 5 * 1024 * 1024, None);

        let result = tag_add("tag_test_tape", &["backup".to_string(), "2024".to_string()]);
        assert!(result.is_ok());

        let result = tag_list(Some("tag_test_tape".to_string()));
        assert!(result.is_ok());

        let result = tag_remove("tag_test_tape", &["2024".to_string()]);
        assert!(result.is_ok());

        let result = tag_list(None);
        assert!(result.is_ok());

        let result = tag_delete("backup");
        assert!(result.is_ok());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_search_functionality() {
        let dir = prepare_temp_vtl("search_test");

        let _ = create_named_library("default", 2, 10);
        let _ = create_tape("search_test_1", 10 * 1024 * 1024, None);
        let _ = create_tape("search_test_2", 20 * 1024 * 1024, None);

        let mut conn = init_db().unwrap();
        let library_id = resolve_library_id(&conn, &current_library_name()).unwrap();
        let tape_id: i64 = conn
            .query_row(
                "SELECT id FROM tapes WHERE library_id = ?1 AND name = 'search_test_1'",
                params![library_id],
                |row| row.get(0),
            )
            .unwrap();
        add_tags_to_tape(&mut conn, tape_id, &["important".to_string()]).unwrap();

        let result = search_tapes(Some("search".to_string()), None, None, None, None);
        assert!(result.is_ok());

        let result = search_tapes(None, Some("important".to_string()), None, None, None);
        assert!(result.is_ok());

        let result = search_tapes(
            None,
            None,
            Some("5M".to_string()),
            Some("25M".to_string()),
            None,
        );
        assert!(result.is_ok());

        let result = search_tapes(None, None, None, None, Some(true));
        assert!(result.is_ok());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_quota_exceeded() {
        let dir = prepare_temp_vtl("quota_exceed_test");

        let _ = create_named_library("default", 2, 10);
        let _ = quota_set(Some("10M"), Some(1));

        let result = create_tape("quota_test_1", 5 * 1024 * 1024, None);
        assert!(result.is_ok());

        let result = create_tape("quota_test_2", 5 * 1024 * 1024, None);
        assert!(result.is_err());

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_auto_named_tapes_batch() {
        let dir = prepare_temp_vtl("auto_tape_names");
        let _ = create_named_library("default", 1, 2);
        let v1 = create_auto_named_tapes_batch("default", None, 2, 1024 * 1024).unwrap();
        assert_eq!(v1, vec!["default_tape01", "default_tape02"]);
        let v2 = create_auto_named_tapes_batch("default", None, 1, 1024 * 1024).unwrap();
        assert_eq!(v2, vec!["default_tape03"]);
        let conn = init_db().unwrap();
        let lid = resolve_library_id(&conn, "default").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tapes WHERE library_id = ?1 AND name IN ('default_tape01','default_tape02','default_tape03')",
                params![lid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3);
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_validate_library_name_rejects_invalid_chars() {
        let dir = prepare_temp_vtl("lib_bad_name");
        assert!(validate_library_name("bad name").is_err());
        assert!(validate_library_name("a.b").is_err());
        assert!(validate_library_name("ok_lib-1").is_ok());
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_tape_rejects_globally_duplicate_name() {
        let dir = prepare_temp_vtl("tape_global_dup");
        let _ = create_named_library("lib_a", 1, 2).unwrap();
        let _ = create_named_library("lib_b", 1, 2).unwrap();
        set_current_library("lib_a");
        let _ = create_tape("shared_tape", 1024, None).unwrap();
        set_current_library("lib_b");
        let r = create_tape("shared_tape", 1024, None);
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_import_tape_rejects_globally_duplicate_name() {
        let dir = prepare_temp_vtl("import_global_dup");
        let _ = create_named_library("lib_x", 1, 2).unwrap();
        let _ = create_named_library("lib_y", 1, 2).unwrap();
        set_current_library("lib_x");
        let _ = create_tape("imp_dup", 4096, None).unwrap();
        let src = dir.join("tapes").join("lib_y").join("imp_dup.vtltape");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::write(&src, b"x").unwrap();
        set_current_library("lib_y");
        let r = import_tape(src.to_str().unwrap(), 0);
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_link_kernel_tapes_replaces_stale_flat_root_copy() {
        let dir = prepare_temp_vtl("link_conflict");
        let root = dir.join("tapes");
        std::env::set_var("VTL_TAPE_DIR", root.to_str().unwrap());
        invalidate_vtl_config_cache();
        let lib_dir = root.join("marstor");
        fs::create_dir_all(&lib_dir).unwrap();
        let img = lib_dir.join("dup.vtltape");
        File::create(&img).unwrap().set_len(100).unwrap();
        File::create(root.join("dup.vtltape"))
            .unwrap()
            .set_len(200)
            .unwrap();
        let rep = link_kernel_tapes().expect("replace duplicate flat root with hardlink");
        assert!(rep.linked >= 1);
        assert!(same_inode_file(&root.join("dup.vtltape"), &img));
        cleanup_temp_vtl(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_link_kernel_tapes_hardlink_at_root_not_conflict() {
        let dir = prepare_temp_vtl("link_hardlink_ok");
        let root = dir.join("tapes");
        std::env::set_var("VTL_TAPE_DIR", root.to_str().unwrap());
        invalidate_vtl_config_cache();
        let lib_dir = root.join("marstor");
        fs::create_dir_all(&lib_dir).unwrap();
        let img = lib_dir.join("marstor_tape01.vtltape");
        File::create(&img).unwrap().set_len(4096).unwrap();
        std::fs::hard_link(&img, root.join("marstor_tape01.vtltape")).unwrap();
        let rep = link_kernel_tapes().expect("hardlink alias at root is valid");
        assert_eq!(rep.linked, 0);
        cleanup_temp_vtl(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_link_kernel_tapes_links_and_removes_stale_symlink() {
        let dir = prepare_temp_vtl("link_kernel");
        let root = dir.join("tapes");
        std::env::set_var("VTL_TAPE_DIR", root.to_str().unwrap());
        invalidate_vtl_config_cache();
        let lib_dir = root.join("marstor");
        fs::create_dir_all(&lib_dir).unwrap();
        let img = lib_dir.join("marstor_tape01.vtltape");
        File::create(&img).unwrap().set_len(4096).unwrap();
        let stale = root.join("old_tape.vtltape");
        std::os::unix::fs::symlink("/nonexistent/old.vtltape", &stale).unwrap();
        let rep = link_kernel_tapes().unwrap();
        assert!(rep.linked >= 1);
        assert!(rep.removed_stale >= 1);
        assert!(root.join("marstor_tape01.vtltape").exists());
        assert!(!stale.exists());
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_migrate_tapes_between_shelves_ok() {
        let dir = prepare_temp_vtl("mig_shelf_ok");
        let _ = create_named_library("default", 1, 2);
        set_current_library("default");
        let _ = create_shelf("sA").unwrap();
        let _ = create_shelf("sB").unwrap();
        let _ = create_tape("tmove", 1024 * 1024, Some("sA")).unwrap();
        let _ =
            migrate_tapes_between_shelves("default", "sA", "sB", &[String::from("tmove")]).unwrap();
        let conn = init_db().unwrap();
        let lid = resolve_library_id(&conn, "default").unwrap();
        let sid_b = resolve_shelf_id(&conn, lid, "sB").unwrap();
        let sh: i64 = conn
            .query_row(
                "SELECT shelf_id FROM tapes WHERE library_id = ?1 AND name = 'tmove'",
                params![lid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sh, sid_b);
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_init_tape_clears_used_bytes_and_truncates() {
        let dir = prepare_temp_vtl("init_tape_clear");
        let _ = create_named_library("default", 1, 2);
        set_current_library("default");
        let _ = create_tape("ti", 4096, None).unwrap();
        let conn = init_db().unwrap();
        let lid = resolve_library_id(&conn, "default").unwrap();
        conn.execute(
            "UPDATE tapes SET used_bytes = 1000 WHERE library_id = ?1 AND name = 'ti'",
            params![lid],
        )
        .unwrap();
        drop(conn);
        let _ = init_tape_in_library("default", "ti").unwrap();
        let conn = init_db().unwrap();
        let ub: u64 = conn
            .query_row(
                "SELECT used_bytes FROM tapes WHERE library_id = ?1 AND name = 'ti'",
                params![lid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ub, 0);
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_init_tape_rejects_when_in_slot() {
        let dir = prepare_temp_vtl("init_tape_in_slot");
        let _ = create_named_library("default", 1, 2);
        set_current_library("default");
        let _ = create_tape("in_slot", 4096, None).unwrap();
        let _ = assign_tape_to_slot("in_slot", 0).unwrap();
        let r = init_tape_in_library("default", "in_slot");
        match r {
            Err(e @ VtlError::TapeNotOnShelf) => {
                let s = e.to_string();
                assert!(
                    s.contains("货架") || s.contains("机械手"),
                    "unexpected message: {}",
                    s
                );
            }
            other => panic!("expected TapeNotOnShelf, got {:?}", other),
        }
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_init_tape_rejects_when_in_drive() {
        let dir = prepare_temp_vtl("init_tape_in_drive");
        let _ = create_named_library("default", 1, 2);
        set_current_library("default");
        let _ = create_tape("in_drv", 4096, None).unwrap();
        let _ = assign_tape_to_slot("in_drv", 0).unwrap();
        let _ = load_tape(0, 0).unwrap();
        let r = init_tape_in_library("default", "in_drv");
        match r {
            Err(e @ VtlError::TapeInDrive) => {
                let s = e.to_string();
                assert!(s.contains("驱动"), "unexpected message: {}", s);
            }
            other => panic!("expected TapeInDrive, got {:?}", other),
        }
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_init_tape_rejects_when_not_on_shelf() {
        let dir = prepare_temp_vtl("init_tape_no_shelf");
        let _ = create_named_library("default", 1, 2);
        set_current_library("default");
        let _ = create_tape("no_shelf", 4096, None).unwrap();
        let conn = init_db().unwrap();
        let lid = resolve_library_id(&conn, "default").unwrap();
        conn.execute(
            "UPDATE tapes SET shelf_id = NULL WHERE library_id = ?1 AND name = 'no_shelf'",
            params![lid],
        )
        .unwrap();
        drop(conn);
        let r = init_tape_in_library("default", "no_shelf");
        match r {
            Err(e @ VtlError::TapeNotOnShelf) => {
                let s = e.to_string();
                assert!(s.contains("货架"), "unexpected message: {}", s);
            }
            other => panic!("expected TapeNotOnShelf, got {:?}", other),
        }
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_delete_shelf_empty_ok() {
        let dir = prepare_temp_vtl("del_shelf_empty");
        let _ = create_named_library("default", 1, 2);
        set_current_library("default");
        let _ = create_shelf("sx").unwrap();
        let _ = delete_shelf_in_library("default", "sx").unwrap();
        let conn = init_db().unwrap();
        let lid = resolve_library_id(&conn, "default").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM shelves WHERE library_id = ?1 AND name = 'sx'",
                params![lid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_delete_shelf_rejects_when_tapes_present() {
        let dir = prepare_temp_vtl("del_shelf_has_tapes");
        let _ = create_named_library("default", 1, 2);
        set_current_library("default");
        let _ = create_shelf("sy").unwrap();
        let _ = create_tape("t_on_shelf", 1024, Some("sy")).unwrap();
        let r = delete_shelf_in_library("default", "sy");
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_delete_shelf_rejects_default_unused() {
        let dir = prepare_temp_vtl("del_shelf_default");
        let _ = create_named_library("default", 1, 2);
        let r = delete_shelf_in_library("default", DEFAULT_UNUSED_SHELF_NAME);
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_delete_named_library_ok() {
        let dir = prepare_temp_vtl("del_lib_ok");
        let _ = create_named_library("default", 1, 2);
        let _ = create_named_library("tmp_lib", 1, 2).unwrap();
        let _ = delete_named_library("tmp_lib").unwrap().0;
        let conn = init_db().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM vtl_libraries WHERE name = 'tmp_lib'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_delete_named_library_rejects_last_online() {
        let dir = prepare_temp_vtl("del_lib_last_online");
        let _ = create_named_library("marstor", 1, 2);
        let r = delete_named_library("marstor");
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_delete_empty_legacy_default_library_ok() {
        let dir = prepare_temp_vtl("del_legacy_default_empty");
        let _ = create_named_library(LEGACY_DEFAULT_LIBRARY_NAME, 1, 2);
        let r = delete_named_library(LEGACY_DEFAULT_LIBRARY_NAME);
        assert!(r.is_ok(), "{:?}", r);
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_delete_named_library_rejects_offline_reserved() {
        let dir = prepare_temp_vtl("del_lib_offline");
        let _ = create_named_library("default", 1, 2);
        let r = delete_named_library(OFFLINE_LIBRARY_NAME);
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_named_library_and_custom_shelf() {
        let dir = prepare_temp_vtl("named_lib_shelf");
        let _ = create_named_library("default", 1, 2);
        let _ = create_named_library("lib_b", 1, 2);

        set_current_library("lib_b");
        let _ = create_shelf("archive").unwrap();
        let _ = create_tape("only_b", 1024 * 1024, Some("archive")).unwrap();

        set_current_library("default");
        let _ = create_tape("only_default", 1024 * 1024, None).unwrap();

        let conn = init_db().unwrap();
        let id_b = resolve_library_id(&conn, "lib_b").unwrap();
        let cnt_b: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tapes WHERE library_id = ?1",
                params![id_b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt_b, 1);

        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_assign_tape_from_shelf_to_slot() {
        let dir = prepare_temp_vtl("assign_shelf_slot");
        let _ = create_named_library("default", 1, 2);
        let _ = create_tape("shelf_tape", 2 * 1024 * 1024, None).unwrap();
        let _ = assign_tape_to_slot("shelf_tape", 0).unwrap();
        let _ = load_tape(0, 0).unwrap();
        let _ = unload_tape(0).unwrap();
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_assign_offline_tape_into_online_library_slot() {
        let dir = prepare_temp_vtl("assign_offline_into_slot");
        let _ = create_named_library("default", 1, 4);
        set_current_library("default");
        let _ = create_tape("off_in", 2 * 1024 * 1024, None).unwrap();
        let _ = create_offline_shelf("rack1").unwrap();
        let _ = move_tapes_to_offline_shelf("default", &[String::from("off_in")], "rack1").unwrap();
        let _ =
            assign_tapes_to_slots_batch("default", &[(String::from("off_in"), 0, true)]).unwrap();
        let conn = init_db().unwrap();
        let lid = resolve_library_id(&conn, "default").unwrap();
        let in_slot: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM slots WHERE library_id = ?1 AND slot_id = 0 AND tape_id IS NOT NULL",
                params![lid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(in_slot, 1);
        let off_id = resolve_library_id(&conn, OFFLINE_LIBRARY_NAME).unwrap();
        let still_off: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tapes WHERE library_id = ?1 AND name = 'off_in'",
                params![off_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_off, 0);
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_assign_slot_rejected_when_tape_in_drive() {
        let dir = prepare_temp_vtl("assign_in_drive");
        let _ = create_named_library("default", 1, 2);
        let _ = create_tape("td", 2 * 1024 * 1024, None).unwrap();
        let _ = assign_tape_to_slot("td", 0).unwrap();
        let _ = load_tape(0, 0).unwrap();
        let r = assign_tape_to_slot("td", 1);
        assert!(matches!(r, Err(VtlError::TapeInDrive)));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_import_rejects_overwriting_loaded_tape() {
        let dir = prepare_temp_vtl("import_loaded");
        let _ = create_named_library("default", 1, 2);
        set_current_library("default");
        let _ = create_tape("loaded_import", 2 * 1024 * 1024, None).unwrap();
        let _ = assign_tape_to_slot("loaded_import", 0).unwrap();
        let _ = load_tape(0, 0).unwrap();
        let src = dir.join("loaded_import.vtltape");
        File::create(&src).unwrap().set_len(4096).unwrap();
        let r = import_tape(src.to_str().unwrap(), 1);
        assert!(matches!(r, Err(VtlError::TapeInDrive)));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_assign_slot_rejected_when_already_in_slot() {
        let dir = prepare_temp_vtl("assign_already_slot");
        let _ = create_named_library("default", 1, 3);
        let _ = create_tape("t1", 1024 * 1024, None).unwrap();
        let _ = assign_tape_to_slot("t1", 0).unwrap();
        let r = assign_tape_to_slot("t1", 1);
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_assign_slot_rejected_when_target_slot_occupied() {
        let dir = prepare_temp_vtl("assign_slot_busy");
        let _ = create_named_library("default", 1, 3);
        let _ = create_tape("ta", 1024 * 1024, None).unwrap();
        let _ = create_tape("tb", 1024 * 1024, None).unwrap();
        let _ = assign_tape_to_slot("ta", 0).unwrap();
        let _ = assign_tape_to_slot("tb", 1).unwrap();
        let r = assign_tape_to_slot("tb", 0);
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_shelf_place_rejected_when_tape_in_drive() {
        let dir = prepare_temp_vtl("shelf_place_drive");
        let _ = create_named_library("default", 1, 2);
        let _ = create_tape("sp", 2 * 1024 * 1024, None).unwrap();
        let _ = assign_tape_to_slot("sp", 0).unwrap();
        let _ = load_tape(0, 0).unwrap();
        let r = shelf_place_tape("sp", None);
        assert!(matches!(r, Err(VtlError::TapeInDrive)));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_shelf_rejects_reserved_unused_name() {
        let dir = prepare_temp_vtl("shelf_reserved");
        let _ = create_named_library("default", 1, 1);
        let r = create_shelf(DEFAULT_UNUSED_SHELF_NAME);
        assert!(r.is_err());
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_list_tapes_unknown_library() {
        let dir = prepare_temp_vtl("list_unknown_lib");
        let _ = create_named_library("default", 1, 1);
        set_current_library("does_not_exist_zzz");
        let r = list_tapes();
        assert!(matches!(r, Err(VtlError::LibraryNotFound(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_snapshot_unknown_tape() {
        let dir = prepare_temp_vtl("snap_no_tape");
        let _ = create_named_library("default", 1, 1);
        let r = snapshot_tape("no_such_tape", "snap1");
        assert!(matches!(r, Err(VtlError::TapeNotFound(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_snapshot_rejects_pathlike_name() {
        let dir = prepare_temp_vtl("snap_bad_name");
        let _ = create_named_library("default", 1, 1);
        let r = snapshot_tape("no_such_tape", "..");
        assert!(matches!(r, Err(VtlError::InvalidTapeName(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_tag_add_unknown_tape() {
        let dir = prepare_temp_vtl("tag_no_tape");
        let _ = create_named_library("default", 1, 1);
        let r = tag_add("ghost", &["x".to_string()]);
        assert!(matches!(r, Err(VtlError::TapeNotFound(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_transport_parse_variants() {
        use super::fab_transport::{parse_fab_transport, FabTransport};
        assert_eq!(parse_fab_transport("local"), Some(FabTransport::LocalScsi));
        assert_eq!(parse_fab_transport("ISCSI"), Some(FabTransport::Iscsi));
        assert_eq!(parse_fab_transport("fcp"), Some(FabTransport::Fc));
        assert_eq!(parse_fab_transport("unknown_mode"), None);
    }

    #[test]
    fn test_transport_config_from_env() {
        let dir = prepare_temp_vtl("transport_env");
        let old_t = std::env::var("VTL_TRANSPORT").ok();
        let old_i = std::env::var("VTL_ISCSI_IQN").ok();
        std::env::set_var("VTL_TRANSPORT", "iscsi");
        std::env::set_var("VTL_ISCSI_IQN", "iqn.test:vtl");
        let c = get_config();
        assert_eq!(c.transport, super::fab_transport::FabTransport::Iscsi);
        assert_eq!(c.iscsi_iqn.as_deref(), Some("iqn.test:vtl"));
        match &old_t {
            Some(v) => std::env::set_var("VTL_TRANSPORT", v),
            None => std::env::remove_var("VTL_TRANSPORT"),
        }
        match &old_i {
            Some(v) => std::env::set_var("VTL_ISCSI_IQN", v),
            None => std::env::remove_var("VTL_ISCSI_IQN"),
        }
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_log_rotation_creates_archive() {
        let dir = prepare_temp_vtl("log_rotate");
        let old_max = std::env::var("VTL_LOG_MAX_BYTES").ok();
        // 与 get_config 中下限一致：小于 4096 会被抬到 4096，故用 4096 并多写几行保证触发轮转
        std::env::set_var("VTL_LOG_MAX_BYTES", "4096");
        let msg = "x".repeat(120);
        for _ in 0..40 {
            log_message(&msg);
        }
        let rotated = dir.join("logs").join("vtladm.log.1");
        assert!(
            rotated.exists(),
            "expected log rotation to create vtladm.log.1"
        );
        match &old_max {
            Some(v) => std::env::set_var("VTL_LOG_MAX_BYTES", v),
            None => std::env::remove_var("VTL_LOG_MAX_BYTES"),
        }
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_error_log_rotation_creates_archive() {
        let dir = prepare_temp_vtl("log_rotate_err");
        let old_max = std::env::var("VTL_LOG_MAX_BYTES").ok();
        std::env::set_var("VTL_LOG_MAX_BYTES", "4096");
        let msg = "y".repeat(100);
        for i in 0..40 {
            log_error("rotate_probe", &format!("{} {}", i, &msg));
        }
        let rotated = dir.join("logs").join("vtladm_errors.log.1");
        assert!(
            rotated.exists(),
            "expected error log rotation to create vtladm_errors.log.1"
        );
        match &old_max {
            Some(v) => std::env::set_var("VTL_LOG_MAX_BYTES", v),
            None => std::env::remove_var("VTL_LOG_MAX_BYTES"),
        }
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_tape_unknown_shelf() {
        let dir = prepare_temp_vtl("tape_bad_shelf");
        let _ = create_named_library("default", 1, 2);
        let r = create_tape("t1", 1024 * 1024, Some("no_such_shelf_xyz"));
        assert!(matches!(r, Err(VtlError::ShelfNotFound(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_named_library_empty_name() {
        let dir = prepare_temp_vtl("lib_empty_name");
        let r = create_named_library("", 1, 2);
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_named_library_rejects_invalid_name() {
        let dir = prepare_temp_vtl("lib_invalid");
        let r = create_named_library("bad lib", 1, 2);
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_duplicate_shelf() {
        let dir = prepare_temp_vtl("shelf_dup");
        let _ = create_named_library("default", 1, 2);
        let _ = create_shelf("dup_shelf").unwrap();
        let r = create_shelf("dup_shelf");
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_shelf_empty_name() {
        let dir = prepare_temp_vtl("shelf_empty");
        let _ = create_named_library("default", 1, 2);
        let r = create_shelf("");
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_shelf_place_unknown_shelf() {
        let dir = prepare_temp_vtl("place_bad_shelf");
        let _ = create_named_library("default", 1, 2);
        let _ = create_tape("tp", 1024 * 1024, None).unwrap();
        let r = shelf_place_tape("tp", Some("missing_shelf"));
        assert!(matches!(r, Err(VtlError::ShelfNotFound(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_list_shelf_tapes_unknown_shelf() {
        let dir = prepare_temp_vtl("list_shelf_bad");
        let _ = create_named_library("default", 1, 2);
        let r = list_shelf_tapes(Some("no_shelf"));
        assert!(matches!(r, Err(VtlError::ShelfNotFound(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_assign_slot_tape_not_found() {
        let dir = prepare_temp_vtl("assign_no_tape");
        let _ = create_named_library("default", 1, 2);
        let r = assign_tape_to_slot("nonexistent_tape_name", 0);
        assert!(matches!(r, Err(VtlError::TapeNotFound(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_tag_remove_unknown_tape() {
        let dir = prepare_temp_vtl("tag_rm_no_tape");
        let _ = create_named_library("default", 1, 2);
        let r = tag_remove("ghost_tape", &["a".to_string()]);
        assert!(matches!(r, Err(VtlError::TapeNotFound(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_named_library_duplicate_when_has_slots() {
        let dir = prepare_temp_vtl("lib_dup_slots");
        let _ = create_named_library("dup_lib", 1, 2).unwrap();
        let r = create_named_library("dup_lib", 1, 4);
        assert!(matches!(r, Err(VtlError::LibraryExists(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_transport_cli_helpers_ok() {
        let dir = prepare_temp_vtl("transport_cli");
        assert!(fab_transport::transport_show().is_ok());
        assert!(fab_transport::transport_check().is_ok());
        assert!(fab_transport::transport_guide().is_ok());
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_log_max_bytes_env_respected() {
        let dir = prepare_temp_vtl("log_max_env");
        let old = std::env::var("VTL_LOG_MAX_BYTES").ok();
        std::env::set_var("VTL_LOG_MAX_BYTES", "12345");
        let c = get_config();
        assert_eq!(c.log_max_bytes, 12345);
        match &old {
            Some(v) => std::env::set_var("VTL_LOG_MAX_BYTES", v),
            None => std::env::remove_var("VTL_LOG_MAX_BYTES"),
        }
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_assign_slot_rejected_after_import_to_slot() {
        let dir = prepare_temp_vtl("assign_after_import");
        let _ = create_named_library("default", 1, 2);
        let _ = create_tape("imp_slot", 2 * 1024 * 1024, None).unwrap();
        let p = dir.join("tapes").join("default").join("imp_slot.vtltape");
        let _ = import_tape(p.to_str().unwrap(), 0).unwrap();
        let r = assign_tape_to_slot("imp_slot", 1);
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_tag_delete_unknown_tag() {
        let dir = prepare_temp_vtl("tag_del_unknown");
        let _ = create_named_library("default", 1, 2);
        let r = tag_delete("nonexistent_tag_xyz");
        assert!(matches!(r, Err(VtlError::TagNotFound(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_tag_list_unknown_tape() {
        let dir = prepare_temp_vtl("tag_list_bad_tape");
        let _ = create_named_library("default", 1, 2);
        let r = tag_list(Some("no_tape".to_string()));
        assert!(matches!(r, Err(VtlError::TapeNotFound(_))));
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_tag_list_all_ok() {
        let dir = prepare_temp_vtl("tag_list_all");
        let _ = create_named_library("default", 1, 2);
        let r = tag_list(None);
        assert!(r.is_ok());
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_inventory_succeeds() {
        let dir = prepare_temp_vtl("inventory_ok");
        let _ = create_named_library("default", 1, 2);
        let _ = create_tape("inv_t", 1024 * 1024, None).unwrap();
        let r = inventory();
        assert!(r.is_ok());
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_status_succeeds() {
        let dir = prepare_temp_vtl("status_ok");
        let _ = create_named_library("default", 1, 2);
        let r = status();
        assert!(r.is_ok());
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_list_libraries_succeeds() {
        let dir = prepare_temp_vtl("lib_list_ok");
        let _ = create_named_library("default", 1, 1);
        let r = list_libraries();
        assert!(r.is_ok());
        cleanup_temp_vtl(&dir);
    }

    #[test]
    fn test_create_named_library_name_too_long() {
        let dir = prepare_temp_vtl("lib_name_long");
        let long = "a".repeat(65);
        let r = create_named_library(&long, 1, 2);
        assert!(matches!(r, Err(VtlError::InvalidParameter(_))));
        cleanup_temp_vtl(&dir);
    }
}
