//! Immutable, channel-scoped image generations. Render packets reference assets;
//! bounded chunks precede the render that commits them. Both peers retain exactly
//! the committed view's set, so a later cache miss is delivered again.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Weak},
};

use crate::{
    packet::{ImageChunk, PacketFrame, RenderPacket, MSG_SESSION_IMAGE, MSG_SESSION_RENDER},
    provider::{TerminalImageResource, TerminalRenderUpdate},
};

pub(crate) type ImageKey = (u32, u64);
use crate::image_backing::{ImageBacking, LocalImage, RetainedImage};
pub(crate) type Image = Arc<RetainedImage>;
pub(crate) const MAX_VIEW_IMAGE_BYTES: usize = 320 * 1024 * 1024;
pub(crate) const IMAGE_CHUNK_BYTES: usize = 64 * 1024;
fn key(image: &RetainedImage) -> ImageKey {
    (image.image_id, image.generation)
}
fn resource_key(image: &TerminalImageResource) -> ImageKey {
    (image.image_id, image.generation)
}

#[derive(Clone, Debug)]
pub(crate) struct RenderBundle {
    pub packet: RenderPacket,
    pub images: Vec<Image>,
}
impl RenderBundle {
    pub fn live(update: TerminalRenderUpdate, images: Vec<Image>) -> Self {
        Self { packet: RenderPacket::live(update), images }
    }
}

#[derive(Default)]
pub(crate) struct CaptureImages(HashMap<ImageKey, Weak<RetainedImage>>);
impl CaptureImages {
    /// Called in the actor's render command, before any further VT mutation.
    pub fn capture(
        &mut self,
        resources: &[TerminalImageResource],
        mut read: impl FnMut(u32, u64, &mut dyn FnMut(&[u8]) -> bool) -> Result<bool, String>,
    ) -> Result<Vec<Image>, String> {
        self.0.retain(|_, image| image.strong_count() != 0);
        let mut total = 0usize;
        let mut images = Vec::new();
        for resource in resources {
            total = total.checked_add(resource.data_len).ok_or("image size overflow")?;
            if total > MAX_VIEW_IMAGE_BYTES {
                return Err("view exceeds 320 MiB image budget".into());
            }
            if let Some(image) = self.0.get(&resource_key(resource)).and_then(Weak::upgrade) {
                images.push(image);
                continue;
            }
            let mut backing = None;
            let mut error = None;
            let found = read(resource.image_id, resource.generation, &mut |data| {
                if data.len() != resource.data_len {
                    error = Some("image length changed during capture".to_string());
                    return false;
                }
                backing = Some(ImageBacking::capture(data));
                true
            })?;
            if let Some(error) = error {
                return Err(error);
            }
            if !found {
                return Err("image generation unavailable during capture".into());
            }
            let image = Arc::new(RetainedImage {
                image_id: resource.image_id,
                generation: resource.generation,
                backing: backing.ok_or("image callback not invoked")?,
            });
            self.0.insert(key(&image), Arc::downgrade(&image));
            images.push(image);
        }
        Ok(images)
    }
}

/// Holds at most one view transfer per channel. Chunks are encoded on demand,
/// not queued as a second whole-image copy behind a blocked socket.
pub(crate) struct ImageTransfer {
    images: VecDeque<Image>,
    offset: usize,
    render: Option<PacketFrame>,
    offered: bool,
    waiting: bool,
    local: bool,
}
impl ImageTransfer {
    pub fn new(channel: u32, bundle: RenderBundle, resident: &mut HashSet<ImageKey>) -> Result<Self, String> {
        let wanted: HashSet<_> = bundle.packet.update.image_resources.iter().map(resource_key).collect();
        let available: HashSet<_> = bundle.images.iter().map(|image| key(image)).collect();
        if !wanted.is_subset(&available) {
            return Err("render references unavailable image generation".into());
        }
        let total = bundle.images.iter().try_fold(0usize, |n, image| n.checked_add(image.bytes().len())).ok_or("image size overflow")?;
        if total > MAX_VIEW_IMAGE_BYTES {
            return Err("view exceeds 320 MiB image budget".into());
        }
        let render = PacketFrame::new(channel, MSG_SESSION_RENDER, &bundle.packet).map_err(|e| e.to_string())?;
        let images = bundle.images.into_iter().filter(|image| !resident.contains(&key(image))).collect();
        *resident = wanted;
        Ok(Self { images, offset: 0, render: Some(render), offered: false, waiting: false, local: true })
    }
    pub fn local_files(mut self, enabled: bool) -> Self {
        self.local = enabled;
        self
    }
    pub fn complete(&self) -> bool {
        self.render.is_none()
    }
    pub fn file_result(&mut self, result: crate::packet::ImageFileResult) -> Result<(), String> {
        let image = self.images.front().ok_or("unexpected image file response")?;
        if !self.waiting || key(image) != (result.image_id, result.generation) {
            return Err("unexpected image file response".into());
        }
        self.waiting = false;
        if result.acquired {
            self.images.pop_front();
            self.offered = false;
        } else {
            self.local = false;
        }
        Ok(())
    }
    pub fn next(&mut self, channel: u32) -> Result<Option<PacketFrame>, String> {
        if self.waiting {
            return Ok(None);
        }
        if let Some(image) = self.images.front() {
            if self.local && !self.offered {
                self.offered = true;
                if let Some(path) = image.backing.path().and_then(|p| p.to_str()) {
                    self.waiting = true;
                    return PacketFrame::new(channel, crate::packet::MSG_SESSION_IMAGE_FILE, &crate::packet::ImageFile {
                        image_id: image.image_id,
                        generation: image.generation,
                        len: image.bytes().len() as u64,
                        path: path.to_string(),
                    })
                    .map(Some)
                    .map_err(|e| e.to_string());
                }
            }
            let end = (self.offset + IMAGE_CHUNK_BYTES).min(image.bytes().len());
            let frame = PacketFrame::new(channel, MSG_SESSION_IMAGE, &ImageChunk {
                image_id: image.image_id,
                generation: image.generation,
                total_len: image.bytes().len() as u64,
                offset: self.offset as u64,
                bytes: image.bytes()[self.offset..end].to_vec(),
            })
            .map_err(|e| e.to_string())?;
            self.offset = end;
            if end == image.bytes().len() {
                self.images.pop_front();
                self.offset = 0;
                self.offered = false;
            }
            return Ok(Some(frame));
        }
        Ok(self.render.take())
    }
}

#[derive(Debug, Default)]
pub(crate) struct ImageReceiver {
    resident: HashMap<ImageKey, Image>,
    incoming: HashMap<ImageKey, Image>,
    partial: Option<(ImageKey, usize, Vec<u8>)>,
    incoming_bytes: usize,
}
impl ImageReceiver {
    pub fn file(&mut self, file: &crate::packet::ImageFile) -> bool {
        let id = (file.image_id, file.generation);
        let Ok(len) = usize::try_from(file.len) else { return false };
        if self.partial.is_some()
            || self.incoming.contains_key(&id)
            || self.resident.contains_key(&id)
            || len > MAX_VIEW_IMAGE_BYTES.saturating_sub(self.incoming_bytes)
        {
            return false;
        }
        let Ok(local) = LocalImage::acquire(std::path::Path::new(&file.path), len) else {
            return false;
        };
        self.incoming_bytes += len;
        self.incoming.insert(
            id,
            Arc::new(RetainedImage { image_id: file.image_id, generation: file.generation, backing: ImageBacking::Local(local) }),
        );
        true
    }
    pub fn chunk(&mut self, chunk: ImageChunk) -> Result<(), String> {
        let id = (chunk.image_id, chunk.generation);
        let total = usize::try_from(chunk.total_len).map_err(|e| e.to_string())?;
        if chunk.bytes.len() > IMAGE_CHUNK_BYTES || total > MAX_VIEW_IMAGE_BYTES {
            return Err("image chunk exceeds budget".into());
        }
        if self.partial.is_none() {
            if chunk.offset != 0 || self.incoming.contains_key(&id) || self.resident.contains_key(&id) {
                return Err("unexpected image start".into());
            }
            self.incoming_bytes = self.incoming_bytes.checked_add(total).ok_or("image size overflow")?;
            if self.incoming_bytes > MAX_VIEW_IMAGE_BYTES {
                return Err("incoming view exceeds image budget".into());
            }
            self.partial = Some((id, total, Vec::new()));
        }
        let (expected, expected_total, bytes) = self.partial.as_mut().unwrap();
        if *expected != id
            || *expected_total != total
            || chunk.offset != bytes.len() as u64
            || chunk.bytes.len() > total.saturating_sub(bytes.len())
            || (chunk.bytes.is_empty() && total != 0)
        {
            return Err("out-of-order or invalid image chunk".into());
        }
        bytes.try_reserve(chunk.bytes.len()).map_err(|e| e.to_string())?;
        bytes.extend_from_slice(&chunk.bytes);
        if bytes.len() == total {
            let (_, _, bytes) = self.partial.take().unwrap();
            self.incoming.insert(id, Arc::new(RetainedImage { image_id: id.0, generation: id.1, backing: ImageBacking::Bytes(bytes) }));
        }
        Ok(())
    }
    pub fn commit(&mut self, resources: &[TerminalImageResource]) -> Result<Vec<Image>, String> {
        if self.partial.is_some() {
            return Err("render interrupted image transfer".into());
        }
        let mut next = HashMap::new();
        let mut total = 0usize;
        for resource in resources {
            let id = resource_key(resource);
            let image = self.incoming.get(&id).or_else(|| self.resident.get(&id)).ok_or("render references missing image")?;
            if image.bytes().len() != resource.data_len {
                return Err("image descriptor length mismatch".into());
            }
            total = total.checked_add(image.bytes().len()).ok_or("image size overflow")?;
            if total > MAX_VIEW_IMAGE_BYTES {
                return Err("committed view exceeds image budget".into());
            }
            next.insert(id, Arc::clone(image));
        }
        self.resident = next;
        self.incoming.clear();
        self.incoming_bytes = 0;
        Ok(self.resident.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bundle(generation: u64, size: usize) -> RenderBundle {
        let image = Arc::new(RetainedImage { image_id: 7, generation, backing: ImageBacking::Bytes(vec![generation as u8; size]) });
        RenderBundle::live(
            TerminalRenderUpdate {
                image_resources: vec![TerminalImageResource { image_id: 7, generation, data_len: size, ..Default::default() }],
                ..Default::default()
            },
            vec![image],
        )
    }
    fn deliver(bundle: RenderBundle, sender: &mut HashSet<ImageKey>, receiver: &mut ImageReceiver) -> (usize, Vec<Image>) {
        let mut transfer = ImageTransfer::new(3, bundle, sender).unwrap();
        let mut chunks = 0;
        let mut images = Vec::new();
        while let Some(frame) = transfer.next(3).unwrap() {
            assert!(frame.payload.len() < crate::packet::MAX_PACKET_PAYLOAD_LEN);
            if frame.msg_type == MSG_SESSION_IMAGE {
                receiver.chunk(frame.decode().unwrap()).unwrap();
                chunks += 1;
            } else {
                images = receiver.commit(&frame.decode::<RenderPacket>().unwrap().update.image_resources).unwrap();
            }
        }
        (chunks, images)
    }
    #[test]
    fn large_assets_reuse_replace_and_return_after_eviction() {
        let mut sender = HashSet::new();
        let mut receiver = ImageReceiver::default();
        let (count, old) = deliver(bundle(1, 1920 * 1080 * 4), &mut sender, &mut receiver);
        assert!(count > 1);
        assert_eq!(old[0].bytes().len(), 1920 * 1080 * 4);
        assert_eq!(deliver(bundle(1, 1920 * 1080 * 4), &mut sender, &mut receiver).0, 0);
        let (_, new) = deliver(bundle(2, 17), &mut sender, &mut receiver);
        assert_eq!(new[0].bytes(), vec![2; 17]);
        assert_eq!(old[0].bytes()[0], 1);
        deliver(RenderBundle::live(Default::default(), vec![]), &mut sender, &mut receiver);
        assert_eq!(deliver(bundle(2, 17), &mut sender, &mut receiver).0, 1);
        assert_eq!(deliver(bundle(2, 17), &mut HashSet::new(), &mut ImageReceiver::default()).0, 1);
    }
    #[test]
    fn capture_owns_bytes_and_reuses_immutable_generations() {
        let mut capture = CaptureImages::default();
        let resource = bundle(1, 3).packet.update.image_resources;
        let mut source = vec![1; 3];
        let first = capture.capture(&resource, |_, _, copy| Ok(copy(&source))).unwrap();
        source.fill(2);
        let second = capture.capture(&resource, |_, _, _| panic!("cached generation reread")).unwrap();
        assert!(Arc::ptr_eq(&first[0], &second[0]));
        assert_eq!(first[0].bytes(), vec![1; 3]);
    }
    #[test]
    fn local_offer_is_shared_without_pixel_chunks_and_failure_falls_back() {
        let original = bundle(1, 17);
        let retained = RetainedImage::from_owned(crate::provider::TerminalImageBytes { image_id: 7, generation: 1, bytes: vec![1; 17] });
        let original_path = retained.backing.path().unwrap().to_path_buf();
        let mut receivers = [ImageReceiver::default(), ImageReceiver::default()];
        for receiver in &mut receivers {
            let mut transfer =
                ImageTransfer::new(3, RenderBundle::live(original.packet.update.clone(), vec![retained.clone()]), &mut HashSet::new())
                    .unwrap();
            let offer = transfer.next(3).unwrap().unwrap();
            assert_eq!(offer.msg_type, crate::packet::MSG_SESSION_IMAGE_FILE);
            assert!(transfer.next(3).unwrap().is_none());
            assert!(!transfer.complete());
            let file = offer.decode::<crate::packet::ImageFile>().unwrap();
            assert!(receiver.file(&file));
            transfer.file_result(crate::packet::ImageFileResult { image_id: 7, generation: 1, acquired: true }).unwrap();
            assert_eq!(transfer.next(3).unwrap().unwrap().msg_type, MSG_SESSION_RENDER);
            receiver.commit(&original.packet.update.image_resources).unwrap();
        }
        drop(retained);
        assert!(!original_path.exists());
        let first = receivers[0].commit(&original.packet.update.image_resources).unwrap();
        receivers[1] = ImageReceiver::default();
        assert_eq!(first[0].bytes(), &[1; 17]);
        let retained = RetainedImage::from_owned(crate::provider::TerminalImageBytes { image_id: 7, generation: 1, bytes: vec![1; 17] });
        let mut transfer = ImageTransfer::new(3, RenderBundle::live(original.packet.update, vec![retained]), &mut HashSet::new()).unwrap();
        transfer.next(3).unwrap();
        transfer.file_result(crate::packet::ImageFileResult { image_id: 7, generation: 1, acquired: false }).unwrap();
        assert_eq!(transfer.next(3).unwrap().unwrap().msg_type, MSG_SESSION_IMAGE);
    }

    #[cfg(all(unix, feature = "ghostty-vt"))]
    #[test]
    fn direct_file_and_shm_inputs_survive_source_removal_and_late_capture() {
        use std::os::fd::{AsRawFd, FromRawFd};
        struct ShmName(std::ffi::CString);
        impl Drop for ShmName {
            fn drop(&mut self) {
                unsafe {
                    libc::shm_unlink(self.0.as_ptr());
                }
            }
        }

        use crate::vt::{ghostty::GhosttyVtEngine, VtEngine};
        for medium in ["d", "f", "s"] {
            let bytes = [11, 22, 33, 255];
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), bytes).unwrap();
            let name = std::ffi::CString::new(format!("/c206-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])).unwrap();
            let _shm_cleanup = ShmName(name.clone());
            let payload = match medium {
                "d" => crate::kitty_output::base64(&bytes),
                "f" => crate::kitty_output::base64(file.path().to_str().unwrap().as_bytes()),
                _ => {
                    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600) };
                    assert!(fd >= 0);
                    let shm = unsafe { std::fs::File::from_raw_fd(fd) };
                    assert_eq!(unsafe { libc::ftruncate(shm.as_raw_fd(), bytes.len() as libc::off_t) }, 0);
                    let mapping = unsafe {
                        libc::mmap(
                            std::ptr::null_mut(),
                            bytes.len(),
                            libc::PROT_READ | libc::PROT_WRITE,
                            libc::MAP_SHARED,
                            shm.as_raw_fd(),
                            0,
                        )
                    };
                    assert_ne!(mapping, libc::MAP_FAILED);
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapping.cast::<u8>(), bytes.len());
                        libc::munmap(mapping, bytes.len());
                    }
                    crate::kitty_output::base64(name.as_bytes())
                }
            };
            let mut source = GhosttyVtEngine::new(10, 10);
            source.feed(format!("\x1b_Ga=T,C=1,q=0,i=7,f=32,s=1,v=1,c=1,r=1,t={medium};{payload}\x1b\\").as_bytes()).unwrap();
            assert!(String::from_utf8_lossy(&source.drain_replies()).contains("OK"));
            if medium == "s" {
                assert_eq!(unsafe { libc::shm_unlink(name.as_ptr()) }, -1, "engine must consume the original shm name");
            }
            drop(file);
            let update = source.render_update(crate::provider::DirtyState::Full).unwrap();
            assert_eq!(update.image_resources.len(), 1);
            let mut capture = CaptureImages::default();
            let assets = capture
                .capture(&update.image_resources, |id, generation, copy| source.with_image_resource_data(id, generation, copy))
                .unwrap();
            drop(source);
            assert_eq!(assets[0].bytes(), &bytes);
            let mut receiver = ImageReceiver::default();
            let mut transfer = ImageTransfer::new(3, RenderBundle::live(update.clone(), assets), &mut HashSet::new()).unwrap();
            let offer = transfer.next(3).unwrap().unwrap().decode::<crate::packet::ImageFile>().unwrap();
            assert!(receiver.file(&offer));
            transfer.file_result(crate::packet::ImageFileResult { image_id: 7, generation: offer.generation, acquired: true }).unwrap();
            drop(transfer);
            assert_eq!(receiver.commit(&update.image_resources).unwrap()[0].bytes(), &bytes);
        }
    }

    #[test]
    fn rejects_missing_partial_out_of_order_and_oversized_assets() {
        let mut receiver = ImageReceiver::default();
        assert!(receiver.commit(&bundle(1, 3).packet.update.image_resources).is_err());
        let chunk = ImageChunk { image_id: 7, generation: 1, total_len: 3, offset: 0, bytes: vec![1] };
        receiver.chunk(chunk.clone()).unwrap();
        assert!(receiver.commit(&[]).is_err());
        assert!(receiver.chunk(chunk).is_err());
        assert!(ImageReceiver::default()
            .chunk(ImageChunk { image_id: 7, generation: 1, total_len: MAX_VIEW_IMAGE_BYTES as u64 + 1, offset: 0, bytes: vec![1] })
            .is_err());
    }
}
