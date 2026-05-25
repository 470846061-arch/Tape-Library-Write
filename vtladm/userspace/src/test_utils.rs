//! 测试共享环境准备（由 `main.rs` 的 `#[cfg(test)] mod test_utils` 引入）。
use std::fs;
use std::path::PathBuf;

/// 在系统临时目录下创建 `vtladm_<prefix>` 并设置 `VTL_*` 环境变量；返回该目录供测试结束时清理。
pub(super) fn prepare_temp_vtl(prefix: &str) -> PathBuf {
    let test_dir = std::env::temp_dir().join(format!("vtladm_{}", prefix));
    let _ = fs::remove_dir_all(&test_dir);
    fs::create_dir_all(&test_dir).expect("create test dir");
    let db_path = test_dir.join("test.db");
    let tape_dir = test_dir.join("tapes");
    let log_dir = test_dir.join("logs");
    std::env::set_var("VTL_DB_PATH", db_path.to_str().unwrap());
    std::env::set_var("VTL_TAPE_DIR", tape_dir.to_str().unwrap());
    std::env::set_var("VTL_LOG_DIR", log_dir.to_str().unwrap());
    // 不读取宿主机上的 vtl.conf：测试使用 VTL_USE_ENV_ONLY=1 时仅环境变量；与生产一致时仅读取 /var/lib/vtl/vtl.conf
    std::env::set_var("VTL_USE_ENV_ONLY", "1");
    super::set_current_library("");
    test_dir
}

pub(super) fn cleanup_temp_vtl(dir: &PathBuf) {
    let _ = fs::remove_dir_all(dir);
}
