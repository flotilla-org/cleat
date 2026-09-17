//! A retained representation independent of transport. Local files are immutable
//! once published. Clients acquire a hard link before acknowledging an offer;
//! unlinking either peer's name cannot invalidate the other peer's lifetime.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug)]
pub(crate) enum ImageBacking {
    Bytes(Vec<u8>),
    Local(LocalImage),
}
impl ImageBacking {
    pub fn capture(bytes: &[u8]) -> Self {
        LocalImage::create(bytes).map(Self::Local).unwrap_or_else(|_| Self::Bytes(bytes.to_vec()))
    }
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::Local(file) => file.bytes(),
        }
    }
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Bytes(_) => None,
            Self::Local(file) => Some(&file.path),
        }
    }
}

#[derive(Debug)]
pub(crate) struct RetainedImage {
    pub image_id: u32,
    pub generation: u64,
    pub backing: ImageBacking,
}
impl RetainedImage {
    pub fn from_owned(image: crate::provider::TerminalImageBytes) -> Arc<Self> {
        Arc::new(Self { image_id: image.image_id, generation: image.generation, backing: ImageBacking::capture(&image.bytes) })
    }
    pub fn bytes(&self) -> &[u8] {
        self.backing.bytes()
    }
}

#[derive(Debug)]
pub(crate) struct LocalImage {
    path: PathBuf,
    #[cfg(unix)]
    mapping: Option<std::ptr::NonNull<libc::c_void>>,
    #[cfg(unix)]
    len: usize,
    #[cfg(not(unix))]
    data: Vec<u8>,
}
// Published files are immutable and the read-only mapping outlives all slices.
#[cfg(unix)]
unsafe impl Send for LocalImage {}
#[cfg(unix)]
unsafe impl Sync for LocalImage {}
impl LocalImage {
    fn name() -> PathBuf {
        std::env::temp_dir().join(format!("cleat-image-{}-{}", std::process::id(), uuid::Uuid::new_v4()))
    }
    pub fn create(bytes: &[u8]) -> io::Result<Self> {
        let path = Self::name();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| {
            let mut file = options.open(&path)?;
            file.write_all(bytes)?;
            drop(file);
            Self::open(path.clone(), bytes.len())
        })();
        if result.is_err() {
            let _ = fs::remove_file(path);
        }
        result
    }
    pub fn acquire(path: &Path, len: usize) -> io::Result<Self> {
        let retained = Self::name();
        fs::hard_link(path, &retained)?;
        let result = Self::open(retained.clone(), len);
        if result.is_err() {
            let _ = fs::remove_file(retained);
        }
        result
    }
    fn open(path: PathBuf, len: usize) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file: File = options.open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() != len as u64 {
            return Err(io::Error::other("invalid local image file"));
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let mapping = if len == 0 {
                None
            } else {
                // SAFETY: length was checked against the immutable regular file.
                let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0) };
                if ptr == libc::MAP_FAILED {
                    return Err(io::Error::last_os_error());
                }
                Some(std::ptr::NonNull::new(ptr).ok_or_else(|| io::Error::other("null image mapping"))?)
            };
            Ok(Self { path, mapping, len })
        }
        #[cfg(not(unix))]
        {
            use std::io::Read;
            let mut data = Vec::new();
            let mut file = file;
            file.read_to_end(&mut data)?;
            Ok(Self { path, data })
        }
    }
    fn bytes(&self) -> &[u8] {
        #[cfg(unix)]
        {
            self.mapping.map_or(&[], |ptr| unsafe { std::slice::from_raw_parts(ptr.as_ptr().cast::<u8>(), self.len) })
        }
        #[cfg(not(unix))]
        {
            &self.data
        }
    }
}
impl Drop for LocalImage {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(ptr) = self.mapping {
            unsafe {
                libc::munmap(ptr.as_ptr(), self.len);
            }
        }
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_names_share_content_and_cleanup_independently() {
        let original = LocalImage::create(b"pixels").unwrap();
        let a = LocalImage::acquire(&original.path, 6).unwrap();
        let b = LocalImage::acquire(&original.path, 6).unwrap();
        let original_path = original.path.clone();
        let a_path = a.path.clone();
        let b_path = b.path.clone();
        drop(original);
        assert!(!original_path.exists());
        assert_eq!(a.bytes(), b"pixels");
        assert_eq!(b.bytes(), b"pixels");
        drop(a);
        assert!(!a_path.exists());
        assert!(b_path.exists());
        drop(b);
        assert!(!b_path.exists());
    }
}
