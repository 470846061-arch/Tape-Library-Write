//! `vtladm` / `vtladm-iscsi`: **Linux x86_64/aarch64 64-bit only** (kernel `vtl.ko`, ioctl, sysfs).

fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "linux" {
        panic!(
            "vtladm: unsupported target OS {:?} (Linux only). \
             Cross-compile: cargo build --release --target x86_64-unknown-linux-gnu \
             or aarch64-unknown-linux-gnu",
            os
        );
    }

    let pw = std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_default();
    if pw != "64" {
        panic!(
            "vtladm: unsupported target pointer width {:?} (64-bit only).",
            pw
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
}
