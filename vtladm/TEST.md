# VTL Admin 完整测试说明

本文档与 `userspace/src/main.rs`、`userspace/src/web.rs`（含 **`#[tokio::test]`** HTTP 探针）、`userspace/src/web_auth.rs`、`userspace/src/bin/vtladm_iscsi.rs` 中的 **`#[test]`** 保持同步，描述全部自动化用例及运行方式。

---

## 1. 运行命令

**构建目标**：`userspace/build.rs` 要求 **`target_os=linux`** 且 **64 位**。在 Windows 上开发请交叉编译，例如：

```bash
rustup target add x86_64-unknown-linux-gnu
cd userspace
cargo test --target x86_64-unknown-linux-gnu
```

### 1.1 全部测试（`vtladm` + `vtladm-iscsi`）

```bash
cd userspace
cargo test
```

### 1.2 仅 `vtladm` 主程序测试

```bash
cd userspace
cargo test --bin vtladm
```

### 1.3 仅 `vtladm-iscsi` 单元测试（不调用 targetcli）

```bash
cd userspace
cargo test --bin vtladm-iscsi
```

### 1.4 单线程（环境变量类用例）

部分用例会临时读写 `VTL_TRANSPORT`、`VTL_LOG_MAX_BYTES` 等；并行时理论上可能干扰，可：

```bash
cd userspace
cargo test -- --test-threads=1
```

### 1.5 按名称过滤示例

```bash
cargo test test_parse_
cargo test test_assign_slot
cargo test test_shelf_
cargo test test_transport
cargo test test_log_rotation
cargo test test_quota
cargo test test_tag_
cargo test test_web_auth_
cargo test test_web_html_
cargo test test_web_http_
cargo test test_web_http_iscsi
cargo test test_init_tape_
cargo test test_delete_shelf_
cargo test test_delete_named_library_
cargo test test_build_vtl_instances_kernel_spec_
cargo test test_build_plan_b_insmod_spec test_parse_vtl_instances_segments
cargo test test_kernel_geometry_mode_parse test_format_initial_vtl_conf_plan_b
cargo test test_create_named_library_rejects_ninth_online_library
cargo test test_config_set_rejects_
cargo test iscsi_validate_tests
cargo test robot_sync_config_linux_tests::   # 机械手 / sync-db 配置（仅 Linux 编译运行）
cargo test robot_preset_linux_tests::      # reconcile 预设（仅 Linux）
cargo test test_format_initial_vtl_conf_robot_backup_defaults
```

---

## 2. 测试环境与约定

| 项 | 说明 |
|----|------|
| 临时数据 | `test_utils::prepare_temp_vtl(prefix)` 在系统临时目录创建 `vtladm_<prefix>/`，设置 `VTL_DB_PATH`、`VTL_TAPE_DIR`、`VTL_LOG_DIR` |
| 配置隔离 | 同时设置 **`VTL_USE_ENV_ONLY=1`**，不读取宿主机 **`/var/lib/vtl/vtl.conf`**（生产环境仅此路径为有效主配置） |
| 当前库 | 默认将 `CURRENT_LIBRARY` 设为 `default`（`set_current_library`） |
| 清理 | `cleanup_temp_vtl` 删除临时目录；部分用例会恢复曾改动的环境变量 |

---

## 3. 完整用例清单（`vtladm`，按源码顺序）

| # | 用例名 | 类型 | 简述 |
|---|--------|------|------|
| 1 | `test_parse_size` | 单元 | 合法大小字符串 → 字节数 |
| 2 | `test_parse_size_invalid` | 单元 | 非法字符串 → `Err` |
| 3 | `test_parse_slot` | 单元 | `slotN` / `mailN` / 纯数字 |
| 4 | `test_parse_drive` | 单元 | `driveN` / 纯数字 |
| 5 | `test_format_size` | 单元 | 字节 → 人类可读 |
| 6 | `test_generate_barcode` | 单元 | `VTL` 前缀、不重复 |
| 7 | `test_validate_tape_name` | 单元 | 空、非法字符、过长 |
| 8 | `test_transport_parse_variants` | 单元 | `fab_transport::parse_fab_transport` |
| 9 | `test_integration_workflow` | 集成 | 建库、建带、导入、`load`/`unload` |
| 10 | `test_error_handling_tape_not_found` | 集成 | 删除不存在磁带 |
| 11 | `test_error_handling_load_from_empty_slot` | 集成 | 空槽 `load` |
| 12 | `test_snapshot_functionality` | 集成 | 快照文件生成 |
| 13 | `test_export_functionality` | 集成 | 槽位导出镜像 |
| 14 | `test_export_checksum_writes_sidecar` | 集成 | `export --checksum` 生成 `<path>.sha256` |
| 15 | `test_eject_tape_to_mailslot` | 集成 | 弹入邮箱槽 |
| 16 | `test_error_handling_drive_busy` | 集成 | 驱动器占用时再次 `load` |
| 17 | `test_error_handling_unload_empty_drive` | 集成 | 空驱动器 `unload` |
| 18 | `test_error_handling_eject_empty_slot` | 集成 | 空槽 `eject` |
| 19 | `test_config_set_and_show` | 集成 | `config set` / `config show` |
| 20 | `test_load_unload_cycle` | 集成 | 多次 `load`/`unload` |
| 21 | `test_multiple_tapes_inventory` | 集成 | 多盘入槽后 `inventory` |
| 22 | `test_import_nonexistent_file` | 集成 | 导入路径不存在 |
| 23 | `test_logging_functions` | 集成 | `vtladm.log` / `vtladm_errors.log` 写入 |
| 24 | `test_concurrent_tape_operations` | 集成 | 多线程 `create_tape` |
| 25 | `test_parse_size_does_not_write_vtl_log` | 集成 | `parse_size` 不写日志文件 |
| 26 | `test_delete_tape_removes_tape_tag_links` | 集成 | 删带后 `tape_tags` 无残留 |
| 27 | `test_batch_create_tapes` | 集成 | 批量创建 + `list_tapes` |
| 28 | `test_batch_import_tapes` | 集成 | 批量导入目录 |
| 29 | `test_quota_management` | 集成 | `quota set` / `show` / `check` |
| 30 | `test_tag_system` | 集成 | 标签增删列与 `tag delete` |
| 31 | `test_search_functionality` | 集成 | `search_tapes` 多条件 |
| 32 | `test_quota_exceeded` | 集成 | 配额满后第二盘失败 |
| 33 | `test_named_library_and_custom_shelf` | 集成 | 多库 + 自定义架 + 数据隔离 |
| 34 | `test_assign_tape_from_shelf_to_slot` | 集成 | `assign-slot` → `load`/`unload` |
| 35 | `test_assign_slot_rejected_when_tape_in_drive` | 集成 | 在驱动器时禁止 `assign-slot` |
| 36 | `test_assign_slot_rejected_when_already_in_slot` | 集成 | 已在槽位禁止再 `assign-slot` |
| 37 | `test_assign_slot_rejected_when_target_slot_occupied` | 集成 | 目标槽已有带 |
| 38 | `test_assign_slot_rejected_after_import_to_slot` | 集成 | `import` 入槽后禁止 `assign-slot` 换槽 |
| 39 | `test_shelf_place_rejected_when_tape_in_drive` | 集成 | 在驱动器时禁止 `shelf place` |
| 40 | `test_create_shelf_rejects_reserved_unused_name` | 集成 | 保留名 `unused` |
| 41 | `test_create_shelf_empty_name` | 集成 | 空架名 |
| 42 | `test_create_duplicate_shelf` | 集成 | 同名架第二次创建 |
| 43 | `test_create_tape_unknown_shelf` | 集成 | 不存在的 `--shelf` |
| 44 | `test_shelf_place_unknown_shelf` | 集成 | `shelf place` 目标架不存在 |
| 45 | `test_list_shelf_tapes_unknown_shelf` | 集成 | 按架列出时架不存在 |
| 46 | `test_assign_slot_tape_not_found` | 集成 | 磁带名不存在 |
| 47 | `test_create_named_library_empty_name` | 集成 | 空库名 |
| 48 | `test_create_named_library_name_too_long` | 集成 | 库名长度 > 64 |
| 48a | `test_validate_library_name_rejects_invalid_chars` | 单元 | 库名非法字符（空格、`.` 等） |
| 48b | `test_create_named_library_rejects_invalid_name` | 集成 | `library create` 拒绝非法库名 |
| 48c | `test_create_auto_named_tapes_batch` | 集成 | `{库名}_tapeNN` 批量命名 |
| 48d | `test_create_tape_rejects_globally_duplicate_name` | 集成 | 跨库重复磁带名被拒 |
| 48d2 | `test_import_tape_rejects_globally_duplicate_name` | 集成 | 导入磁带时跨库重名被拒 |
| 48d3 | `test_link_kernel_tapes_conflict_on_flat_root_file` | 集成 | 根目录实体文件与库内同名冲突 |
| 48e | `test_link_kernel_tapes_links_and_removes_stale_symlink` | 集成 | 根目录 symlink 链接与陈旧清理（仅 Unix） |
| 49 | `test_create_named_library_duplicate_when_has_slots` | 集成 | 已有槽位的库名重复创建 |
| 50 | `test_list_tapes_unknown_library` | 集成 | 当前库名不存在 |
| 51 | `test_list_libraries_succeeds` | 集成 | `library list` 路径成功 |
| 52 | `test_snapshot_unknown_tape` | 集成 | 快照目标不存在 |
| 53 | `test_tag_add_unknown_tape` | 集成 | 给不存在磁带加标签 |
| 54 | `test_tag_remove_unknown_tape` | 集成 | 从不存在磁带删标签 |
| 55 | `test_tag_delete_unknown_tag` | 集成 | 删除不存在全局标签 |
| 56 | `test_tag_list_unknown_tape` | 集成 | 列出不存在磁带的标签 |
| 57 | `test_tag_list_all_ok` | 集成 | `tag list` 无参数 |
| 58 | `test_inventory_succeeds` | 集成 | `inventory` 成功 |
| 59 | `test_status_succeeds` | 集成 | `status` 成功 |
| 60 | `test_transport_config_from_env` | 集成 | `VTL_TRANSPORT` / `VTL_ISCSI_IQN` |
| 61 | `test_transport_cli_helpers_ok` | 集成 | `transport show/check/guide` |
| 62 | `test_log_rotation_creates_archive` | 集成 | 设 `VTL_LOG_MAX_BYTES=4096`（配置下限），多次 `log_message` 后生成 `vtladm.log.1` |
| 63 | `test_error_log_rotation_creates_archive` | 集成 | 同上阈值，`log_error` 多次后生成 `vtladm_errors.log.1` |
| 64 | `test_log_max_bytes_env_respected` | 集成 | `VTL_LOG_MAX_BYTES` 生效 |

### 3.1 磁带初始化、删除货架、删除在线库（`main.rs`）

| # | 用例名 | 类型 | 简述 |
|---|--------|------|------|
| A1 | `test_init_tape_clears_used_bytes_and_truncates` | 集成 | `init_tape_in_library` 将 `used_bytes` 置 0 并截断镜像 |
| A1b | `test_init_tape_rejects_when_in_slot` | 集成 | 槽内磁带：`TapeNotOnShelf`，文案含「货架」或「机械手」 |
| A1c | `test_init_tape_rejects_when_in_drive` | 集成 | 驱动内磁带：`TapeInDrive`，文案含「驱动」 |
| A1d | `test_init_tape_rejects_when_not_on_shelf` | 集成 | 无 `shelf_id`：`TapeNotOnShelf`，文案含「货架」 |
| A2 | `test_delete_shelf_empty_ok` | 集成 | 空自建架可 `delete_shelf_in_library` |
| A3 | `test_delete_shelf_rejects_when_tapes_present` | 集成 | 架上有带时删除被拒 |
| A3b | `test_delete_shelf_rejects_default_unused` | 集成 | 不可删除默认 `unused` 架 |
| A4 | `test_delete_named_library_ok` | 集成 | 两在线库时删除其一成功 |
| A5 | `test_delete_named_library_rejects_last_online` | 集成 | 仅剩一个在线库时删除被拒 |
| A6 | `test_delete_named_library_rejects_offline_reserved` | 集成 | 不可删除 `__offline__` |

| K1 | `test_build_vtl_instances_kernel_spec_single_online_library` | 单元 | `build_vtl_instances_kernel_spec`：单在线库 `2x10` 与 DB 驱动器/数据槽计数一致 |
| K2 | `test_build_vtl_instances_kernel_spec_clamped_to_kernel_caps` 等 | 单元 | DB 驱动器行数超过 **8** 时规格串夹紧为 **`8×N`**；另有 **`test_build_vtl_instances_kernel_spec_at_max_slots`**（**`2×256`**）；**`test_create_named_library_rejects_too_many_drives` / `test_create_named_library_rejects_too_many_slots`**（>8 / >256） |
| K3 | `test_build_vtl_instances_kernel_spec_truncates_ninth_online_library` | 单元 | DB 中出现第 **9** 个在线库（SQL 模拟）时，规格串仅 **8** 段、`id` 升序截断，与 **`VTL_MAX_SCSI_INSTANCES`** 一致 |
| K4 | `test_build_vtl_instances_kernel_spec_clamps_data_slots_from_db_count` | 单元 | 单库数据槽行数 **>256** 时，规格段夹紧为 **`1×256`** |
| K5 | `test_create_named_library_rejects_ninth_online_library` | 单元 | 已有 **8** 个在线库时再建第 **9** 个 → `Err`（与 API 上限一致） |
| K6 | `test_config_set_rejects_max_drives_above_kernel_cap` | 单元 | **`library-config`**：`max_drives=9` → `Err`（产品上限 **8** 驱/库） |
| K7 | `test_config_set_rejects_slots_above_kernel_cap` | 单元 | **`library-config`**：`slots=257` → `Err` |
| K8 | `test_build_plan_b_insmod_spec` | 单元 | 方案 B 满配 **`8x256` × 8**（`build_plan_b_insmod_spec`） |
| K9 | `test_parse_vtl_instances_segments` | 单元 | 解析 `2x10,8x256` 等段 |
| K10 | `test_build_vtl_instances_kernel_spec_padded` | 单元 | `build_vtl_instances_kernel_spec_padded(8)`：DB 库 + 空闲 host **`8x256`**（非 `1x1`） |
| K11 | `test_kernel_geometry_mode_parse` | 单元 | `fixed` / `plan_b` / `legacy` 枚举解析 |
| K12 | `test_format_initial_vtl_conf_plan_b_hint` | 单元 | `init-config` 模板含 Plan B 注释与 `# kernel_geometry_mode=fixed` |
| K13 | `test_parse_vtl_conf_kernel_geometry_mode_fixed` | 单元（**仅 Linux**） | `parse_vtl_conf_lines_for_test` 解析 **`kernel_geometry_mode=fixed`** |

### 3.1a 机械手 / 巡检配置（**仅 Linux**，`#[cfg(target_os = "linux")]`）

| # | 用例名 | 模块 | 简述 |
|---|--------|------|------|
| R1 | `apply_robot_authority_preset_kernel_mode` | `reconcile.rs` | `kernel` → `apply=false`、`pull=true`、`auto_sync_db_from_kernel=true` |
| R2 | `apply_robot_authority_preset_db_mode` | `reconcile.rs` | `db` → `apply=true`、`pull=false`、`auto_sync_db_from_kernel=false` |
| R3 | `apply_robot_authority_preset_backup_alias` | `reconcile.rs` | `backup` 与 `kernel` 预设相同 |
| R4 | `finalize_robot_authority_overrides_stray_apply_pull_lines` | `reconcile.rs` | `robot_authority=kernel` 时 `finalize` 覆盖文件中矛盾的 apply/pull |
| R5 | `test_format_initial_vtl_conf_robot_backup_defaults` | `main.rs` | `init-config` 模板含 `robot_sync`、`robot_authority=kernel`、`auto_sync_db_from_kernel` |
| R6 | `test_parse_vtl_conf_robot_authority_kernel_preset` | `main.rs` | 解析 `vtl.conf` 行后内核权威预设生效 |
| R7 | `test_parse_vtl_conf_robot_authority_db_preset` | `main.rs` | `robot_authority=db` 时 vtladm 权威且关闭 `auto_sync_db_from_kernel` |
| R8 | `test_kernel_robot_authority_from_env_overrides_conf` | `main.rs` | `VTL_ROBOT_AUTHORITY=backup` 时 `kernel_robot_authority()` 为真 |
| R9 | `test_auto_sync_db_from_kernel_respects_conf_false` | `main.rs` | `auto_sync_db_from_kernel=false` 可关闭定时 sync-db |
| R10 | `test_sync_db_from_kernel_rejects_db_authority_env` | `main.rs` | `VTL_ROBOT_AUTHORITY=db` 时 `sync_db_from_kernel_all_libraries` → `InvalidParameter` |
| R11 | `test_vtladm_push_disabled_when_kernel_authority` | `main.rs` | 内核权威时 `vtladm_push_to_kernel_enabled()` 为假 |
| R12 | `test_get_config_reads_robot_authority_from_conf_file` | `main.rs` | `VTL_CONF_PATH` 下 `robot_authority=kernel`（`db_path` 在 `/var/tmp/…`，非 `/tmp` 短路） |

| P1 | `patrol_exit_code_crit_over_warn` | `patrol.rs` | CRIT 优先于 WARN |
| P2 | `patrol_exit_code_warn_only_default_lenient` | `patrol.rs` | 默认：仅 WARN → exit 0 |
| P3 | `patrol_exit_code_warn_only_when_strict` | `patrol.rs` | `VTL_PATROL_STRICT=1` 时 WARN → exit 1 |

全栈 `run_patrol` / ioctl **未**单测，见 `INTEGRATION-TEST.md` 手工 `vtladm patrol`。

`init-tape` 与 `assign-slot` / `shelf place` 在「在驱动内 / 未在货架」场景下返回相同的 `TapeInDrive`、`TapeNotOnShelf` 变体；`VtlError` 的 `Display` 以中文为主。初始化成功路径会先截断镜像并 `sync`，再更新数据库；若 `UPDATE` 失败会尽力将镜像长度恢复为截断前并记错误日志。

| W1 | `test_web_auth_init_creates_json_with_defaults` | 单元 | `init_auth_file` 生成 `web_admin.json` 默认用户与 `allow_iscsi_exec=false` |
| W2 | `test_web_auth_login_default_password_and_session` | 单元 | 默认口令 `login` 成功，`session_username` 返回 `admin` |
| W3 | `test_web_auth_login_wrong_password` | 单元 | 错误密码 `login` 失败 |
| W4 | `test_web_auth_captcha_correct_answer_accepted` | 单元 | 多轮算术验证码：按题干计算答案后 `verify_captcha` 为真 |
| W5 | `test_web_auth_captcha_wrong_answer_rejected` | 单元 | 错误答案 `verify_captcha` 为假 |
| W6 | `test_web_auth_captcha_double_verify_consumes_entry` | 单元 | 验证码一次性：第二次同 id 校验失败 |
| W7 | `test_web_auth_change_password_roundtrip` | 单元 | `change_password` 后旧口令失败、新口令可登录；过短新密码被拒 |
| W8 | `test_web_auth_wrong_username_rejected` | 单元 | 非配置用户名 `login` 失败 |

### 3.2 Web 静态页与片段（`web.rs` + `web_shell.css` / `web_boot.js` / `web_*_side_inner.html`）

| # | 用例名 | 类型 | 简述 |
|---|--------|------|------|
| 72 | `test_web_html_shell_css_has_toast` | 单元 | `web_shell.css` 含 `.vtl-toast` 与 `.panel-tabs` 等样式 |
| 73 | `test_web_html_boot_has_nav_and_toast` | 单元 | `web_boot.js` 含 `normPath`、`showToast`、侧栏 `data-nav` 高亮逻辑 |
| 74 | `test_web_html_admin_side_fragment` | 单元 | `web_admin_side_inner.html` 含「存储功能」「虚拟磁带库」「磁带库」、`data-nav="/admin/library"`、`货架与离线`、`data-nav="/admin/shelf"` |
| 75 | `test_web_html_vp_side_fragment` | 单元 | `web_vp_side_inner.html` 含「VTL 控制台」、**管理入口**与浏览链接 |
| 76 | `test_web_html_home_includes_shell_vp_boot` | 单元 | `HOME_HTML` 串联 shell、控制台侧栏、`web_boot` |
| 77 | `test_web_html_login_shell_card_no_boot` | 单元 | `LOGIN_HTML` 使用 shell 变量与 `login-card`，**不**嵌入 `showToast` / boot |
| 78 | `test_web_html_admin_tapes_includes_sidebar_boot` | 单元 | `ADMIN_TAPES_HTML` 含后台侧栏「存储功能」、`web_boot`、标签分屏（`panel-tabs` / `tab-create`） |
| 78a | `test_web_html_admin_iscsi_target_config_api` | 单元 | `ADMIN_ISCSI_HTML` 含 `GET /api/manage/iscsi/config`、`POST /api/manage/iscsi/check`、`library-export-defaults` / `scan-sg` / `library-export` API、环境与权限说明、**内核 OOPS / 重启风险**提示、`IPv6` 提示、`loadIscsiCfg` / `non_unix_build` |
| 78a-2 | `test_parse_first_iscsi_portal` | 单元 | `iscsi_portals` 逗号首段 `host:port` → `(host, port)`；空串 `None`（**不支持 IPv6 字面量**，见 `VTLADM-ISCSI.md`） |
| 78a-3 | `iscsi_validate_tests` | 单元 | `web.rs`：`validate_iscsi_portal_host`、`validate_iqn`、`validate_lun_map_values`（门户 / IQN / LUN 上限 **255**） |
| 78b | `test_web_http_iscsi_config_401_without_session` | 集成 | 无会话 `GET /api/manage/iscsi/config` → **401** |
| 78c | `test_web_http_iscsi_config_json_with_session` | 集成 | 验证码登录后同路径 → **200**；JSON 含 `tape_dir`、`non_unix_build`；**不得**含已废弃字段 `vtladm_iscsi_linux_only`；默认 `allow_iscsi_exec=false` |
| 78c-2 | `test_extract_vtl_session_cookie_accepts_prefix_before_name` | 单元 | `Set-Cookie` 中 `vtl_session=` 前有其它属性时仍能抽出 `Cookie` 头值（与 `web_login_cookie` 解析一致） |
| 78d | `test_web_http_iscsi_check_401_without_session` | 集成 | 无会话 `POST /api/manage/iscsi/check`（body `{}`）→ **401** |
| 78e | `test_web_http_iscsi_check_json_with_session` | 集成 | 登录后 `POST`（`{"sudo":false}`）→ **200 / 502 / 500** 之一；200 或 502 时 JSON 含 **`ok`**；500 时含 **`error`**（取决于本机能否启动 `vtladm-iscsi check`） |
| 79 | `test_web_html_browse_tapes_includes_vp_boot` | 单元 | `BROWSE_TAPES_HTML` 含控制台侧栏与 boot |
| 80 | `test_web_http_get_login_returns_html` | 集成 | `build_web_router` + `GET /login` → 200、`Content-Type` 含 `text/html` |
| 81 | `test_web_http_get_root_redirects_to_login_without_session` | 集成 | 无 cookie 时 `GET /` → 重定向且 `Location` 含 `/login` |

---

## 4. `vtladm-iscsi` 单元测试（清单）

| # | 用例名 | 简述 |
|---|--------|------|
| 1 | `test_validate_iqn_accepts_and_rejects` | 合法 / 非法 IQN |
| 2 | `test_validate_fileio_name` | FILEIO 名称字符集 |
| 3 | `test_ensure_trailing_exit_appends_exit` | 批处理脚本自动补 `exit` |
| 4 | `test_ensure_trailing_exit_keeps_single_exit` | 已有 `exit` 不重复追加 |
| 5 | `test_library_export_script_order` | `library-export` 脚本（默认 **tpg1**）：`cd /iscsi` → `create` IQN → 相对 **`cd <iqn>`**、**`cd tpg1`**，再 `set attribute` / `luns/` / `portals/` |
| 5b | `test_library_export_script_merged_path_no_tpg1_segment` | **`Merged`**：相对 **`cd <iqn>`** 后**无** **`cd tpg1`**，且不含 **`cd /iscsi/<iqn>/`** 绝对段 |
| 5c | `test_library_unexport_script_merged_luns_path` | **`library-unexport`** merged：`cd /iscsi` → **`cd <iqn>`** → **`cd luns`** |
| 6 | `test_library_unexport_script_deletes_luns_high_to_low` | `library-unexport` 按 lun 号从高到低删除，并删除各 pscsi 后端名 |
| 7 | `test_validate_export_id` | `library-export` 所用 `--id` 字符集与 `fileio_name` 规则一致 |
| 8 | `test_parse_comma_lun_map` | 逗号 LUN 列表解析、重复检测、**LUN>255 拒绝**、合法 `255,0` |

**说明**：`vtladm-iscsi` **不**在 CI 中调用真实 `targetcli`（需 root 与 LIO）；集成验证请在 Linux 目标机上用手动或 **`quick-export` / `library-export` 加 `--dry-run`** 完成。详见 [userspace/docs/VTLADM-ISCSI.md](userspace/docs/VTLADM-ISCSI.md)。**内核崩溃 / 重启** 多与 **`vtl.ko` 重载**、**错误 pscsi 节点** 或 **vermagic 不匹配** 有关，见 [userspace/docs/SCSI.md](userspace/docs/SCSI.md) **§1c**。

---

## 5. 按功能域速查

| 域 | 相关用例编号（§3） |
|----|-------------------|
| 解析与校验 | 1–8 |
| 内核 `vtl_instances` / 方案 B（`build_vtl_instances_kernel_spec`、padded、满配） | K1–K4、K8–K13 |
| 机械手与驱动器 | 9–11, 15–20, 33–38 |
| 机械手配置 / sync-db（Linux 单测） | R1–R12（§3.1a） |
| 巡检退出码 | P1–P3（§3.1a） |
| 快照 / 导入 / 导出 / 弹出 | 12–14, 21, 51 |
| 配置与配额 | 18, 28–31, 63 |
| 标签与搜索 | 25, 29–30, 52–56 |
| 命名库与磁带架 | 32, 39–50 |
| `assign-slot` 与导入 | 33–37, 45 |
| 日志与轮转 | 22–24, 61–63 |
| 传输 CLI | 8, 59–60 |
| Web 认证（`web_auth`，§3.1 表 W1–W8） | W1–W8 |
| Web 静态页、片段与 HTTP 探针（`web.rs`，§3.2） | 72–78a、78a-2、**78a-3**、78b–78e、78c-2、79–81 |
| iSCSI Target Web（config / check API，`web.rs`） | 78a、78a-2、**78a-3**、78b–78e、78c-2（与 `VTLADM-ISCSI.md`「Web 管理页」对应） |

---

## 6. 环境变量（测试相关）

| 变量名 | 描述 |
|--------|------|
| `VTL_DB_PATH` | SQLite 路径 |
| `VTL_TAPE_DIR` | 磁带镜像根目录 |
| `VTL_LOG_DIR` | 日志目录 |
| `VTL_USE_ENV_ONLY` | `1` 时不读宿主机 `vtl.conf` |
| `VTL_LOG_MAX_BYTES` | 单日志文件上限（字节，≥4096） |
| `VTL_TRANSPORT` | `local` / `iscsi` / `fc` |
| `VTL_ISCSI_IQN` / `VTL_ISCSI_PORTALS` / `VTL_FC_WWPN` | 与 `vtl.conf` 同名 |
| `VTL_INSMOD_FORCE` | 设为 **`1`** 时，`vtl-kernel-reload.sh` 使用 **`insmod -f`**（默认 **不**设置：普通 `insmod`，降低 vermagic 不匹配导致 panic 的风险） |
| `VTL_RELOAD_SLEEP_SEC` | `rmmod vtl` 与 `insmod` 之间的秒数（脚本默认 **2**；仅钩子脚本读取） |
| `VTL_SCAN_HOST_STAGGER_MS` | 传给 `insmod` 的 **`scan_host_stagger_ms`**（默认 **3000**；多库时拉长各 host 的 **`scsi_scan_host`** 间隔，减轻并行 **`st` probe**） |
| `VTL_ISCSI_SHELL_PATH` | 设为 **`merged`**（或 **`datera`**）时，在 **`cd /iscsi` + `create <iqn>`** 之后**不**再 **`cd tpg1`**（仅 **`cd <iqn>`**），供 **`ls` 下该 IQN 无 `tpg1` 子项** 的旧 targetcli；默认 **`tpg1`**（含相对 **`cd <iqn>`** + **`cd tpg1`**，兼容 IQN 内冒号）。`vtladm serve` 下可在 **systemd 或启动脚本** 里为进程设置该变量。 |
| `VTL_KERNEL_RELOAD_ON_DB_CHANGE` | 设为 **`1`/`true`/`on`** 时：ioctl 失败（或关闭 ioctl）后允许自动执行 **`kernel_vtl_reload_script`**。**`0`/`false`/`off` 或未写 `kernel_reload_on_db_change`** 时**不**跑整模块脚本；**若 `kernel_geom_prefer_ioctl` 仍为默认开启**，改库后**仍会**先试 **`/dev/vtl` ioctl**。与 **`kernel_reload_on_db_change`**（`vtl.conf`）同义；见 `docs/SCSI.md` §1c。 |
| `VTL_KERNEL_GEOM_IOCTL` | 设为 **`0`/`false`/`off`** 时，**不**优先通过 **`/dev/vtl`** ioctl 应用几何（无整模块 **`rmmod`/`insmod`**）；与 **`kernel_geom_prefer_ioctl`**（`vtl.conf`）同义；见 `docs/SCSI.md` §1e。 |
| `VTL_KERNEL_GEOMETRY_MODE` | **`fixed`** / **`plan_b`** / **`semi_thin`** 或 **`legacy`**；与 **`kernel_geometry_mode`**（`vtl.conf`）同义；**`fixed`** 时 **`vtl-kernelctl start`** 用 **`kernel-spec --insmod-max`** + **`noscan=1`**，见 `docs/SCSI.md` §1g。 |
| `VTL_SKIP_SPEC_CACHE` | 设为 **`1`** 时，**不**读取 **`/var/lib/vtl/.last_vtl_instances_spec`**，改库后**不**走「规格未变则只 SCSI `scan`」分支；见 `docs/SCSI.md` §1f。 |
| `VTL_NO_SCSI_RESCAN_ON_UNCHANGED_SPEC` | 设为 **`1`** 时，即使规格与缓存一致也**不**对 **`vtl`** SCSI host 写 **`scan`**；见 `docs/SCSI.md` §1f。 |
| `VTL_SCSI_RESCAN_STAGGER_MS` | 各 **`vtl`** host **`scan`** 写入之间的间隔毫秒数（Rust 路径默认 **50**；**不在**最后一次写入之后再延迟）；见 `docs/SCSI.md` §1f。 |
| `VTL_ROBOT_SYNC` | `0`/`false` 关闭机械手 ioctl 路径（与 `robot_sync` 同义） |
| `VTL_ROBOT_AUTHORITY` | `kernel`/`backup`/`scsi`/`external` 或 `db`/`vtladm`；覆盖 `vtl.conf` 中 `robot_authority` |
| `VTL_AUTO_RECONCILE_APPLY` / `VTL_AUTO_RECONCILE_PULL` | 覆盖 `auto_reconcile_*`（`robot_authority` 预设解析后仍可能被 env 覆盖） |
| `VTL_AUTO_SYNC_DB_FROM_KERNEL` | 覆盖 `auto_sync_db_from_kernel`（定时 `vtl-robot-sync` / patrol sync-db） |
| `VTL_CONF_PATH` | 主配置路径（R12 等 Linux 单测使用；生产默认 `/opt/vtladm/var/vtl.conf`） |

## 7. CI 建议命令

**`userspace` 仅支持 64 位目标**（`build.rs` 检查 `CARGO_CFG_TARGET_POINTER_WIDTH`）；CI 请使用 **`x86_64-*`** / **`aarch64-*`** 等默认 host triple，勿用 **`i686-*`** 等 32 位 triple。

```yaml
# 示例（GitHub Actions / 其他 CI；须在 Linux x86_64/aarch64 上运行，见 userspace/build.rs）
- run: cd userspace && cargo test --verbose
# 仅机械手配置单测（Linux）：
# - run: cd userspace && cargo test --verbose robot_authority_linux_tests robot_preset_linux_tests test_format_initial_vtl_conf_robot
```

若仓库已提交 **`Cargo.lock`**（推荐），固定依赖解析、兼容旧版 **Cargo 1.82**（避免 `edition2024` 类清单）：

```bash
cd userspace && cargo test --locked --verbose
```

如需最大化隔离环境变量用例：

```bash
cd userspace && cargo test --verbose -- --test-threads=1
```

---

## 8. 手动与补充说明

- CLI 行为与生产路径见根目录 [README.md](README.md)、[INSTALL.md](INSTALL.md)。
- `userspace/TESTS.md` 为指向本文件的短链接说明。
- 日志轮转文件：`vtladm.log`、`vtladm_errors.log` 及 `.1`…`.5` 后缀。
- **内核 / 主机稳定性**（panic、重启）：多与 **`kernel_vtl_reload_script` 在 I/O 存在时 `rmmod`**、**`insmod -f` 与内核不匹配**、或 **`library-export` 误绑 `/dev/sg`** 有关；无法单靠 `cargo test` 覆盖。上线前请在 **Linux 目标机**上按 [SCSI.md](userspace/docs/SCSI.md) **§1c** 做 **dry-run**、维护窗口与 **`lsscsi -g`** 核对。

### 8.1 `vtl.ko` 分阶段稳定性（目标机脚本）

脚本：**`userspace/scripts/vtl-kernel-stability.sh`**（需 **root**）。在已确认「**不加载 vtl 长期不重启**」后，用下表**一次只跑一个 phase**（或 **`integration-smoke`** 串联），崩溃后 **`journalctl -b -1 -k`**、**`tail /var/crash/…/vmcore-dmesg.txt`**，并 **`vtladm-collect-diagnostics.sh`** 打包。

**麒麟 4.19 实测结论（2026-05-16）**

| 场景 | 结果 |
|------|------|
| `phase-a-idle`（insmod + 等待 + 1h 不扫 SCSI） | 通过 |
| `phase-a` + **`VTL_SKIP_RMMOD=1`**（不 rmmod，每 60s `lsscsi`） | 通过 |
| `phase-a` 在 **大量 fuser 占用** 下 **rmmod → insmod** | **kdump**（`/var/crash/…-21:13:39`） |
| 手工 **无占用时 rmmod → insmod** | 可通过 |

**推荐**：生产与稳定性测试优先 **不 rmmod**（`ioctl` / SCSI rescan）；必须卸模块前先 **`check-holders`**。

| Phase | 命令概要 | 验证什么 |
|-------|----------|----------|
| **preflight** | `preflight` | `vtl.ko`、journal/kdump、若已加载且 fuser 有占用则 WARN |
| **check-holders** | `check-holders` | `/dev/st*`/`sg*`/`sch*`/`ch*` 是否被占用（**通过后才宜 rmmod**） |
| **integration-smoke** | `integration-smoke` | **preflight + A-idle + A-probe**（默认各 **600s**；**全程 `VTL_SKIP_RMMOD=1`**，不 rmmod） |
| **A-idle** | `phase-a-idle` | insmod + 等待 + **浸泡期间不扫 SCSI**（基线） |
| **A-probe** | `phase-a-probe` | 等同 **`VTL_SKIP_RMMOD=1` 的 phase-a**（周期性 `lsscsi`） |
| **A** | `phase-a` | 默认先 **safe rmmod**（有占用则 **拒绝**）再 insmod + 等待 + 周期性快照 |
| **reload** | `phase-reload` | **无占用** 时 rmmod + insmod + 等待 + 单次快照 |
| **B** | `VTL_INST_SPEC='…' phase-b` | 多段几何 + 空载浸泡；**已加载且 fuser 有占用**（或 **`VTL_SKIP_RMMOD=1`**）时用 **`SET_INSTANCES` ioctl**，不 **rmmod** |
| **C** | 先 A 或 B 后 `phase-c` | 周期性 **`mt -f /dev/st* status`** |
| **D** | `phase-d`（**`/dev/vtl`** + **python3**） | **`SET_INSTANCES` ioctl** |
| **E** | `phase-e` | 周期性 **`vtl-scsi-rescan.sh`** |

常用环境变量：

| 变量 | 含义 |
|------|------|
| `VTL_KO` | `vtl.ko` 路径 |
| `VTL_INST_SPEC` | 如 `1x4` 或 `1x4,2x8`（phase B/D） |
| `VTL_SOAK_SEC` | 单 phase 浸泡秒数（默认 **3600**） |
| `VTL_INTEGRATION_SOAK_SEC` | **integration-smoke** 每段浸泡（默认 **600**） |
| `VTL_LOG_DIR` | 日志目录（默认 **`/var/log/vtl-stability`**） |
| `VTL_POST_INSMOD_WAIT_SEC` | insmod 后等待再探测（默认 **120**，麒麟建议保留） |
| `VTL_SKIP_RMMOD=1` | 跳过 `rmmod`（**phase-a-probe** 默认行为） |
| `VTL_FORCE_RMMOD=1` | 有 fuser 占用仍 **rmmod**（**不安全**，仅排障） |

ioctl 成功后 **`/sys/module/vtl/parameters/vtl_instances`** 应与规格串一致（新内核）；仍以 **`lsscsi -g`** 为准。Web 创建/删除库 API 返回 **`kernel_geom`** / **`kernel_geom_detail`**（如 `ioctl_ok`、`ioctl_failed`）。

示例：

```bash
cd /root/vtladm
sudo sed -i 's/\r$//' userspace/scripts/vtl-kernel-stability.sh

# 集成冒烟（约 20 分钟 + 等待；可 nohup）
sudo nohup sh userspace/scripts/vtl-kernel-stability.sh integration-smoke \
  >>/tmp/vtl-integration-smoke.log 2>&1 &
tail -f /tmp/vtl-integration-smoke.log

# 完整 1h 集成（过夜前）
sudo VTL_INTEGRATION_SOAK_SEC=3600 sh userspace/scripts/vtl-kernel-stability.sh integration-smoke

# 分步 1h（与现场已通过组合一致）
sudo VTL_SOAK_SEC=3600 sh userspace/scripts/vtl-kernel-stability.sh phase-a-idle
sudo VTL_SKIP_RMMOD=1 VTL_SOAK_SEC=3600 sh userspace/scripts/vtl-kernel-stability.sh phase-a-probe

# 必须测 rmmod 周期时：先确认无占用
sudo sh userspace/scripts/vtl-kernel-stability.sh check-holders
sudo sh userspace/scripts/vtl-kernel-stability.sh phase-reload

# 多库：先 phase-d 配几何，再 skip-rmmod 只浸泡（不重复 insmod/ioctl）
sudo VTL_INST_SPEC='1x4,2x8' sh userspace/scripts/vtl-kernel-stability.sh phase-d
sudo VTL_SKIP_RMMOD=1 VTL_SOAK_SEC=3600 VTL_INST_SPEC='1x4,2x8' \
  sh userspace/scripts/vtl-kernel-stability.sh phase-b

# 有 fuser 占用且未配好几何：phase-b 自动 ioctl（勿用 VTL_FORCE_RMMOD）
sudo VTL_SOAK_SEC=3600 VTL_INST_SPEC='1x4,2x8' \
  sh userspace/scripts/vtl-kernel-stability.sh phase-b
```

崩溃后：

```bash
ls -lt /var/crash | head -3
tail -200 /var/crash/127.0.0.1-*/vmcore-dmesg.txt | grep -E 'Oops|panic|Call Trace|vtl'
sudo sh userspace/scripts/vtladm-collect-diagnostics.sh /tmp/vtl-diag.tar.gz
```

## 9. 故障排除

| 现象 | 处理 |
|------|------|
| 数据库锁定 | 结束占用进程或换 `VTL_DB_PATH` |
| 权限 | 临时目录测试一般无需 root |
| 残留临时目录 | 删除 `vtladm_<prefix>` |
| `vtladm-iscsi` 与 targetcli 不符 | 使用 `--dry-run` 检查脚本，并对照发行版 LIO 文档微调 |
| **内核 OOPS / 主机重启**（改库后、`library-export`、重载 `vtl.ko` 后） | 查 **`dmesg`**；确认 **`vtl.ko` 与 `uname -r` 一致**；停备份后再 **`rmmod`**；钩子脚本勿默认 **`insmod -f`**（见 **`vtl-kernel-reload.sh`**）；**pscsi 仅绑 VTL 的 sg**；详见 [SCSI.md](userspace/docs/SCSI.md) **§1c**、[VTLADM-ISCSI.md](userspace/docs/VTLADM-ISCSI.md) |
| `cargo` 报 `edition2024` / 无法解析某 crate 的 `Cargo.toml` | 使用仓库中的 **`Cargo.toml` 钉版本**（如 `uuid`、`time`）；提交 **`Cargo.lock`** 后使用 **`cargo build --locked` / `cargo test --locked`**；或升级发行版提供的 **Rust/Cargo** |
| `Could not resolve host: rsproxy.cn` / `Unable to update registry` / `git fetch … crates.io-index` 失败 | 见下文 **§9.1 Cargo 索引与镜像** |
| `axum = "^0.7.5"` 失败，且候选只有 **0.5.x** | **crates.io 索引过旧或镜像未同步**（索引里尚无 `axum` 0.7）。处理：①换官方或已同步的源后删除索引缓存再拉取，例如 **`rm -rf ~/.cargo/registry/index`** 后重试 **`cargo fetch`** 或 **`cargo update`**；②勿把 **`cargo` 报错全文粘贴进 `Cargo.toml`**（会破坏清单）；③离线见 **§9.1** 的 **`cargo vendor`** |
| **`cargo` 1.51.x** / **rustc 1.5x** | **过旧，不能构建当前 `userspace`**（`axum` 0.7 等要求 **rustc ≥ 1.66**，见 **`userspace/Cargo.toml` 顶部注释**）。请在 Linux 构建机换用较新 **Rust/Cargo**（发行版包、容器、或单位允许的安装方式）；仅换 `crates.io` 镜像**不能**绕过语言版本限制 |

### 9.1 Cargo 索引与镜像（如 rsproxy.cn）

**原因**：全局或项目里的 **`$HOME/.cargo/config.toml`** 常通过 **`[source]` + `replace-with`** 把默认 **`crates-io`** 指到国内镜像（例如 **`https://rsproxy.cn/crates.io-index`**）。若构建机 **DNS 不可用、无外网、或镜像域名被墙/宕机**，`cargo build` 在解析 **`axum`** 等依赖时会失败，**与 vtladm 源码无关**。

**可选处理**：

1. **能访问公网时**：修好 **DNS**；或把镜像改成当前环境可解析、可 **`git fetch`** 的地址；或暂时 **取消 `replace-with`**，改回官方源（需能访问 **`github.com/rust-lang/crates.io-index`** 或等价官方索引）。
2. **完全离线 / 内网**：在一台有外网的机器上 **`cargo vendor`** 生成 **`vendor/`**，把 **`vendor` + `Cargo.lock`** 拷到内网，并在 **`config.toml`** 中配置 **`[source.crates-io]`** 为 **`directory`** 指向该目录（参见 [Cargo 文档：source replacement](https://doc.rust-lang.org/cargo/reference/source-replacement.html)）。
3. **仅验证是否镜像问题**：可临时把有问题的 **`[source.*]`** 整段注释掉后重试 **`cargo fetch`**（注意会走默认源，需网络策略允许）。

查看当前是否启用了替换源：

```bash
cat ~/.cargo/config.toml
# 或：rg -n "replace-with|rsproxy|source" ~/.cargo/config.toml .cargo/config.toml 2>/dev/null
```

### 9.2 安装 Rust：无外网 / 无法解析 `static.rust-lang.org`

**现象**：`rustup-init.sh` / `curl` 报 **`Could not resolve host: static.rust-lang.org`**，说明本机 **DNS 或出站策略** 访问不到官方域名（与 vtladm 源码无关）。

**可选路径**（择一，按单位安全策略）：

| 方式 | 说明 |
|------|------|
| **修 DNS / 放行** | 在 `/etc/resolv.conf` 或内网 DNS 上能解析 **`static.rust-lang.org`**（及若使用镜像则对应镜像域名），防火墙允许 **HTTPS 出站**。 |
| **国内镜像装 rustup**（能解析镜像时） | 例如：`export RUSTUP_DIST_SERVER=https://mirrors.tuna.tsinghua.edu.cn/rustup` 与 `RUSTUP_UPDATE_ROOT=https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup`，再从该镜像提供的 **`rustup-init.sh`** 安装（以镜像站说明为准）。 |
| **发行版软件包** | `dnf install rust cargo` / `apt install cargo` 等，版本可能仍偏旧，需 **≥ rustc 1.66**（见 README 构建说明）；若不够再换带新 Rust 的发行版或模块流。 |
| **完全离线** | 在能上网的机器下载 **`rustup` 安装包 + 对应 `rustc`/`cargo` 的 tarball**（或发行版 `.rpm`/`.deb`），经 **U 盘 / 内网制品库** 拷到构建机，按 [rustup 离线安装](https://rust-lang.github.io/rustup/installation/other.html) 或发行版文档本地安装。 |
| **不在本机编译** | 在 **CI / 能联网的构建机** 编出 **`vtladm`/`vtladm-iscsi` 二进制**，只把产物部署到 **localhost** 环境。 |

**若已设置 `RUSTUP_DIST_SERVER` 指向中科大等镜像，仍报 `Name or service not known` / `dns error`**：说明本机 **对公网域名一律无法解析**（未配 DNS、仅内网、或安全策略禁止解析外网）。换镜像 URL **不能**解决，需要：①运维配置 **可解析的 DNS**（或 **HTTP(S) 代理** 且代理能代理解析）；②或改用上表 **完全离线 / 发行版包 / 不在本机编译**。

**`could not parse TOML configuration in ~/.cargo/config.toml`**（例如 **`key with no value`**、行内出现 **`EOF`**）：全局 Cargo 配置 **损坏或误粘贴**。处理：① **`mv ~/.cargo/config.toml ~/.cargo/config.toml.bad`** 后重试（无该文件时 Cargo 用默认行为）；②或编辑该文件，删掉无效行，保证 **合法 TOML**（每节 `[table]`、每键 **`key = "value"`**）。错误发生在解析 **全局配置** 阶段，**早于** 读取项目 `Cargo.toml`。

**注意**：仅解决 **crates.io 镜像** 不能替代 **Rust 编译器版本**；若当前仍是 **Cargo 1.51**，必须换用 **较新的 rustc/cargo** 才能构建当前 `userspace`。
