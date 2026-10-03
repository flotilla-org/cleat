//! Caller-owned POSIX shared memory backing.
#[cfg(unix)]
use std::{fs::File, io};

/// A successful return transfers unlink responsibility to the caller.
#[cfg(unix)]
pub(crate) fn create_shm(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // Short enough for platforms with a 31-character POSIX shm name limit.
    let name = std::ffi::CString::new(format!("/cl-{}", &uuid::Uuid::new_v4().simple().to_string()[..24])).unwrap();
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600) };
    if fd < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let result = (|| {
        let len = libc::off_t::try_from(bytes.len()).map_err(|_| io::Error::other("image length exceeds POSIX shm capacity"))?;
        if unsafe { libc::ftruncate(file.as_raw_fd(), len) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if !bytes.is_empty() {
            // POSIX shm supports mapping; read/write syscalls are not portable.
            let mapping = unsafe {
                libc::mmap(std::ptr::null_mut(), bytes.len(), libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, file.as_raw_fd(), 0)
            };
            if mapping == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: mapping is writable for the exact payload length and
            // independent of the source slice; this is the sole payload copy.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapping.cast::<u8>(), bytes.len());
                libc::munmap(mapping, bytes.len());
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        unsafe {
            libc::shm_unlink(name.as_ptr());
        }
        return Err(error.to_string());
    }
    Ok(name.as_bytes().to_vec())
}

#[cfg(not(unix))]
pub(crate) fn create_shm(_: &[u8]) -> Result<Vec<u8>, String> {
    Err("unsupported image backing: POSIX shm requires Unix".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    // POSIX shm payloads, including empty objects, round-trip through a mapping.
    // Explicit lengths cover empty/minimum and the 64 KiB transfer boundary.
    #[cfg(unix)]
    #[test]
    fn shm_payloads_round_trip_and_names_are_caller_owned() {
        use std::os::fd::{AsRawFd, FromRawFd};
        struct Cleanup(std::ffi::CString);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe {
                    libc::shm_unlink(self.0.as_ptr());
                }
            }
        }
        for len in [0, 1, 65535, 65536, 65537] {
            let bytes: Vec<u8> = (0..len).map(|n| (n % 251) as u8).collect();
            let name = Cleanup(std::ffi::CString::new(create_shm(&bytes).unwrap()).unwrap());
            let fd = unsafe { libc::shm_open(name.0.as_ptr(), libc::O_RDONLY, 0) };
            assert!(fd >= 0);
            let file = unsafe { File::from_raw_fd(fd) };
            // macOS reports page-rounded shm allocation size; the payload is
            // the exact prefix identified by data_len, not the physical size.
            assert!(file.metadata().unwrap().len() >= len as u64);
            if len > 0 {
                let mapping = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0) };
                assert_ne!(mapping, libc::MAP_FAILED);
                assert!(unsafe { std::slice::from_raw_parts(mapping.cast::<u8>(), len) } == bytes, "shm payload differs at length {len}");
                assert_eq!(unsafe { libc::munmap(mapping, len) }, 0);
            }
            assert_eq!(unsafe { libc::shm_unlink(name.0.as_ptr()) }, 0);
            assert_eq!(unsafe { libc::shm_open(name.0.as_ptr(), libc::O_RDONLY, 0) }, -1);
        }
    }
}
