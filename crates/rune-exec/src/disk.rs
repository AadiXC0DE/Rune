//! Free space on the file system that holds a path.

use camino::Utf8Path;

/// Returns the bytes available to this user on the file system holding
/// `path`, or `None` when the system cannot say.
#[must_use]
pub fn free_bytes(path: &Utf8Path) -> Option<u64> {
    imp::free_bytes(path)
}

#[cfg(unix)]
mod imp {
    use camino::Utf8Path;

    // The field types differ between platforms; on some they already are u64.
    #[allow(clippy::useless_conversion)]
    pub fn free_bytes(path: &Utf8Path) -> Option<u64> {
        let stats = rustix::fs::statvfs(path.as_std_path()).ok()?;
        u64::try_from(stats.f_bavail)
            .ok()?
            .checked_mul(u64::try_from(stats.f_frsize).ok()?)
    }
}

#[cfg(windows)]
mod imp {
    #![allow(unsafe_code)]

    use std::os::windows::ffi::OsStrExt as _;

    use camino::Utf8Path;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    pub fn free_bytes(path: &Utf8Path) -> Option<u64> {
        let wide: Vec<u16> = path
            .as_std_path()
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut available = 0_u64;
        // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the
        // call; the output pointer names a live u64, and the two totals the
        // call can also report are not requested (null).
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &raw mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(available)
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use camino::Utf8Path;

    pub const fn free_bytes(_path: &Utf8Path) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_temporary_directory_has_some_free_space() {
        let dir = std::env::temp_dir();
        let path = Utf8Path::from_path(&dir).expect("utf8 temp dir");
        assert!(free_bytes(path).is_some_and(|bytes| bytes > 0));
    }
}
