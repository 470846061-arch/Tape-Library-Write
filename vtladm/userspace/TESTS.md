# VTL Admin 测试

完整测试用例表、环境说明与 CI 建议见仓库根目录：

**[../TEST.md](../TEST.md)**

快速运行：

```bash
cd userspace
cargo test                    # vtladm + vtladm-iscsi 全部单元/集成测试（含 web_auth）
cargo test --bin vtladm       # 仅主程序（含 main.rs 与 web_auth 等模块测试）
cargo test --bin vtladm-iscsi # 仅 iSCSI 辅助工具内置单元测试
cargo test test_web_auth_     # 仅 Web 认证相关用例
cargo test test_web_html_     # Web 内嵌页 / shell / boot / 侧栏片段一致性
cargo test test_web_http_     # Web axum 路由 smoke（登录页、未登录重定向、iSCSI config/check API）
cargo test test_web_http_iscsi_  # 仅 iSCSI Target：`GET …/iscsi/config` / `POST …/iscsi/check` 会话与 JSON
cargo test test_build_vtl_instances_kernel_spec_  # `vtl_instances` 规格串与内核上限夹紧（见 TEST.md K1–K4）
cargo test test_build_plan_b_insmod_spec test_parse_vtl_instances_segments  # 方案 B 满配与段解析（K8–K9）
cargo test test_build_vtl_instances_kernel_spec_padded  # padded 规格：空闲 host 8x256（K10）
cargo test test_kernel_geometry_mode_parse test_format_initial_vtl_conf_plan_b  # Plan B 模板与枚举（K11–K12）
cargo test test_parse_vtl_conf_kernel_geometry_mode_fixed  # vtl.conf fixed 模式（K13，仅 Linux）
cargo test iscsi_validate_tests  # `web.rs`：门户 / IQN / LUN 校验
cargo test test_init_tape_ test_delete_shelf_ test_delete_named_library_  # 磁带初始化 / 删货架 / 删库相关
cargo test test_create_auto_named test_validate_library_name test_create_tape_rejects_globally test_link_kernel_tapes  # 库名/全局磁带名/symlink
cargo test robot_sync_config_linux_tests::  # 机械手单一路径配置（仅 Linux）
# 若已提交 Cargo.lock：cargo test --locked
```

**说明**：`userspace/build.rs` 要求 **Linux 64 位**；在 Windows 上请 `cargo test --target x86_64-unknown-linux-gnu`（需已安装对应 target）。
