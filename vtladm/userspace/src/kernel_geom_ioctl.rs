//! Apply DB-derived `vtl_instances` geometry via `/dev/vtl` ioctl (no `rmmod`/`insmod`).
//! Requires root (CAP_SYS_ADMIN) and a `vtl.ko` built with `VTL_IOCTL_SET_INSTANCES`.
//!
//! **64-bit targets only** (enforced by workspace `build.rs`). The request code matches Linux
//! `ioctl(2)` encoding: `_IOC(_IOC_WRITE, 'V', 5, sizeof(struct vtl_set_instances_ioctl))` where
//! the struct is exactly **`VTL_INST_SPEC_MAX`** bytes of `char spec[]` (see `kernel/src/vtl_misc.c`).

pub(crate) const VTL_INST_SPEC_MAX: usize = 384;

#[repr(C)]
struct VtlSetInstancesIoctl {
    spec: [u8; VTL_INST_SPEC_MAX],
}

/// `_IOW('V', 5, struct vtl_set_instances_ioctl)` — must match `kernel/src/vtl_misc.c`.
const VTL_IOCTL_SET_INSTANCES: libc::c_ulong = 0x4180_5605;
/// `_IOW('V', 10, struct vtl_set_instances_ioctl)` — Plan B resize without host teardown.
const VTL_IOCTL_RESIZE_GEOMETRY: libc::c_ulong = 0x4180_560a;

fn apply_spec_ioctl(spec: &str, cmd: libc::c_ulong) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;

    if spec.len() >= VTL_INST_SPEC_MAX {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "vtl_instances spec exceeds ioctl buffer",
        ));
    }
    let mut req = VtlSetInstancesIoctl {
        spec: [0u8; VTL_INST_SPEC_MAX],
    };
    req.spec[..spec.len()].copy_from_slice(spec.as_bytes());

    let f = OpenOptions::new().read(true).write(true).open("/dev/vtl")?;

    let r = unsafe {
        libc::ioctl(
            f.as_raw_fd(),
            cmd,
            &mut req as *mut VtlSetInstancesIoctl as *mut libc::c_void,
        )
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn try_apply_kernel_vtl_instances_via_ioctl(spec: &str) -> std::io::Result<()> {
    apply_spec_ioctl(spec, VTL_IOCTL_SET_INSTANCES)
}

/// Plan B: same `vtl_instances` spec string; host count must match insmod-time count.
pub(crate) fn try_apply_kernel_geom_resize_via_ioctl(spec: &str) -> std::io::Result<()> {
    apply_spec_ioctl(spec, VTL_IOCTL_RESIZE_GEOMETRY)
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    #[test]
    fn vtl_ioctl_set_instances_request_matches_kernel_ioc_macro() {
        const DIR_SHIFT: u32 = 30;
        const SIZE_SHIFT: u32 = 16;
        const TYPE_SHIFT: u32 = 8;
        let cmd =
            (1u32 << DIR_SHIFT) | (384u32 << SIZE_SHIFT) | ((b'V' as u32) << TYPE_SHIFT) | 5u32;
        assert_eq!(cmd as libc::c_ulong, super::VTL_IOCTL_SET_INSTANCES);
    }

    #[test]
    fn vtl_ioctl_resize_geometry_request_matches_kernel_ioc_macro() {
        const DIR_SHIFT: u32 = 30;
        const SIZE_SHIFT: u32 = 16;
        const TYPE_SHIFT: u32 = 8;
        let cmd =
            (1u32 << DIR_SHIFT) | (384u32 << SIZE_SHIFT) | ((b'V' as u32) << TYPE_SHIFT) | 10u32;
        assert_eq!(cmd as libc::c_ulong, super::VTL_IOCTL_RESIZE_GEOMETRY);
    }

    #[test]
    fn spec_len_ge_buffer_returns_invalid_input() {
        let s = "a".repeat(super::VTL_INST_SPEC_MAX);
        let e = super::try_apply_kernel_vtl_instances_via_ioctl(&s)
            .expect_err("oversized spec must be rejected");
        assert_eq!(e.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn spec_len_just_below_buffer_is_not_invalid_input() {
        let s = "1".repeat(super::VTL_INST_SPEC_MAX - 1);
        let e = super::try_apply_kernel_vtl_instances_via_ioctl(&s).expect_err("no /dev/vtl in CI");
        assert_ne!(e.kind(), ErrorKind::InvalidInput);
    }
}
