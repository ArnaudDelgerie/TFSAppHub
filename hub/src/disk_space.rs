//! Free space available for a new file or directory, with a reserved margin.

use std::{
    ffi::CString,
    fmt, io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

pub const MARGIN: u64 = 64 * 1024 * 1024;

/// Bytes available to an unprivileged process on the nearest existing ancestor.
pub fn available_bytes(path: &Path) -> io::Result<u64> {
    #[cfg(test)]
    if let Some(bytes) = OVERRIDE.with(|value| value.get()) {
        return Ok(bytes);
    }

    let mut ancestor = path;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no existing ancestor"))?;
    }
    let name = CString::new(ancestor.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `name` is a terminated C string and `stats` points to writable memory.
    if unsafe { libc::statvfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statvfs returned success, so it initialized the output struct.
    let stats = unsafe { stats.assume_init() };
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

pub fn room(path: &Path) -> io::Result<u64> {
    Ok(available_bytes(path)?.saturating_sub(MARGIN))
}

#[derive(Debug)]
pub struct InsufficientSpace {
    pub path: PathBuf,
    pub needed: u64,
    pub available: u64,
}

impl fmt::Display for InsufficientSpace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} needs {} MiB but has {} MiB of room (64 MiB reserved margin)",
            self.path.display(),
            self.needed.div_ceil(1024 * 1024),
            self.available / (1024 * 1024)
        )
    }
}

#[cfg(test)]
thread_local! {
    static OVERRIDE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub fn with_available_bytes<T>(bytes: u64, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OVERRIDE.with(|value| value.set(self.0));
        }
    }
    let old = OVERRIDE.with(|value| value.replace(Some(bytes)));
    let _restore = Restore(old);
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_saturates_below_the_margin() {
        with_available_bytes(MARGIN - 1, || {
            assert_eq!(room(Path::new("/missing")).unwrap(), 0)
        });
    }

    #[test]
    fn missing_paths_use_the_nearest_existing_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("not-yet-created/file");
        assert_eq!(
            available_bytes(&nested).unwrap(),
            available_bytes(temp.path()).unwrap()
        );
    }
}
