//! Physical core count of the machine the sidecar runs on, for sizing ORT
//! thread pools.

/// Physical cores across all performance levels, without SMT siblings.
/// `None` where the platform doesn't report it.
pub fn physical_cores() -> Option<usize> {
    #[cfg(target_os = "macos")]
    {
        macos::sysctl_usize(c"hw.physicalcpu")
    }
    #[cfg(windows)]
    {
        windows::physical_cores()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CStr;

    pub fn sysctl_usize(name: &CStr) -> Option<usize> {
        let mut value: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        // SAFETY: `value` and `len` describe a writable c_int-sized buffer,
        // and `name` is NUL-terminated.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut value as *mut libc::c_int).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && value > 0).then_some(value as usize)
    }
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
    };

    /// One `RelationProcessorCore` record per physical core. Records are
    /// variable-size; each starts with `Relationship: i32, Size: u32`.
    pub fn physical_cores() -> Option<usize> {
        let mut len: u32 = 0;
        // SAFETY: a null buffer with length 0 only asks for the needed size.
        unsafe {
            GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len)
        };
        if len == 0 {
            return None;
        }
        // u64 storage keeps the records 8-byte aligned, as the API expects.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buf` holds at least `len` writable bytes.
        let ok = unsafe {
            GetLogicalProcessorInformationEx(
                RelationProcessorCore,
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if ok == 0 {
            return None;
        }
        // SAFETY: the u64 buffer is at least `len` bytes, now initialized by the call.
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), len as usize) };
        let mut cores = 0;
        let mut offset = 0;
        while offset + 8 <= bytes.len() {
            let size = u32::from_ne_bytes(bytes[offset + 4..offset + 8].try_into().ok()?) as usize;
            if size == 0 {
                break;
            }
            cores += 1;
            offset += size;
        }
        (cores > 0).then_some(cores)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn detects_physical_cores_on_supported_platforms() {
        let physical = super::physical_cores().expect("physical core count");
        assert!(physical >= 1);
    }
}
