//! Free space on the disk holding a path, for long recordings (decision 37): refuse to start one
//! with almost no room, and stop -- keeping everything so far -- before the disk fills.

use std::path::Path;

/// Bytes available to this user on the volume holding `path`; None if it cannot be told.
pub fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 { return None; }
        Some(s.f_bavail as u64 * s.f_frsize as u64)
    }
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut avail = 0u64;
        unsafe { GetDiskFreeSpaceExW(&HSTRING::from(path.as_os_str()), Some(&mut avail), None, None) }.ok()?;
        Some(avail)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn free_space_is_known_for_the_temp_folder() {
        let b = super::free_bytes(&std::env::temp_dir()).expect("free space");
        assert!(b > 0);
    }
}
