//! Atomic no-replace file moves on Windows.

/// Moves one file without overwriting the destination or emulating a move with
/// copy/delete operations. Both paths remain live for the complete system call.
pub fn rename_no_replace(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    let wide = |path: &std::path::Path| -> std::io::Result<Vec<u16>> {
        let mut value: Vec<_> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path contains NUL",
            ));
        }
        value.push(0);
        Ok(value)
    };
    let source = wide(source)?;
    let destination = wide(destination)?;
    // SAFETY: Both pointers refer to live, NUL-terminated UTF-16 buffers.
    // Zero flags forbid replacement and cross-volume copy/delete emulation.
    #[allow(unsafe_code)]
    let moved = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            0,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
