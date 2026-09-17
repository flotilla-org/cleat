//! Per-terminal residency and delivery. Producer replies never pass through here.
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    time::{Duration, Instant},
};

use crate::{
    image_delivery::{Image, ImageKey},
    provider::{TerminalImagePlacement, TerminalImageResource, TerminalRenderUpdate},
};

#[derive(Debug)]
struct Upload {
    image: Image,
    resource: TerminalImageResource,
    file: bool,
    started: Instant,
}
#[derive(Debug, Default)]
pub(crate) struct KittyOutput {
    assets: HashMap<ImageKey, Image>,
    ids: HashMap<ImageKey, u32>,
    pending: HashMap<u32, Upload>,
    resident: HashSet<u32>,
    next_id: u32,
    file_failed: bool,
    disabled: bool,
}
impl KittyOutput {
    pub fn set_assets(&mut self, images: Vec<Image>) {
        self.assets = images.into_iter().map(|i| ((i.image_id, i.generation), i)).collect();
    }
    pub fn reply(&mut self, writer: &mut impl Write, reply: &[u8]) -> Result<(), String> {
        let Some(body) = reply.strip_prefix(b"\x1b_G").and_then(|s| s.strip_suffix(b"\x1b\\")) else { return Ok(()) };
        let Some(split) = body.iter().position(|b| *b == b';') else { return Ok(()) };
        let id = std::str::from_utf8(&body[..split])
            .ok()
            .and_then(|s| s.split(',').find_map(|part| part.strip_prefix("i=").and_then(|s| s.parse::<u32>().ok())));
        let Some(id) = id else { return Ok(()) };
        let Some(mut upload) = self.pending.remove(&id) else { return Ok(()) };
        if !self.ids.values().any(|current| *current == id) {
            write!(writer, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\").map_err(|e| e.to_string())?;
            return Ok(());
        }
        if &body[split + 1..] == b"OK" {
            self.resident.insert(id);
        } else if upload.file {
            self.file_failed = true;
            upload.file = false;
            upload.started = Instant::now();
            emit_upload(writer, id, &upload)?;
            self.pending.insert(id, upload);
        } else {
            self.disabled = true;
        }
        Ok(())
    }
    pub fn expire(&mut self, writer: &mut impl Write) -> Result<(), String> {
        if self.pending.values().any(|u| u.started.elapsed() > Duration::from_secs(5)) {
            // No support or an unresponsive terminal. Bound holds and stop
            // uploading until reconnect; never forward responses to the PTY.
            for id in self.pending.keys() {
                write!(writer, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\").map_err(|e| e.to_string())?;
            }
            self.disabled = true;
            self.pending.clear();
        }
        Ok(())
    }
    pub fn render(
        &mut self,
        writer: &mut impl Write,
        update: &TerminalRenderUpdate,
        origin: (u16, u16),
        viewport: (u16, u16),
    ) -> Result<(), String> {
        if self.disabled {
            return Ok(());
        }
        let wanted: HashSet<_> = update.image_resources.iter().map(|r| (r.image_id, r.generation)).collect();
        let retired: Vec<_> = self.ids.iter().filter(|(key, _)| !wanted.contains(key)).map(|(key, id)| (*key, *id)).collect();
        for (key, id) in retired {
            write!(writer, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\").map_err(|e| e.to_string())?;
            self.ids.remove(&key);
            self.resident.remove(&id);
            // Pending uploads keep their file until a reply or timeout.
        }
        for resource in &update.image_resources {
            let key = (resource.image_id, resource.generation);
            let Some(image) = self.assets.get(&key) else { continue };
            if !matches!(resource.format, 0..=2) || resource.compression != 0 {
                continue;
            }
            let id = *self.ids.entry(key).or_insert_with(|| {
                self.next_id = self.next_id.wrapping_add(1).max(1);
                self.next_id
            });
            if !self.resident.contains(&id) && !self.pending.contains_key(&id) {
                // Bound outstanding uploads when a terminal isn't reading.
                if self.pending.len() >= 8
                    || self.pending.values().map(|u| u.image.bytes().len()).sum::<usize>().saturating_add(image.bytes().len())
                        > crate::image_delivery::MAX_VIEW_IMAGE_BYTES
                {
                    continue;
                }
                let upload = Upload {
                    image: image.clone(),
                    resource: resource.clone(),
                    file: !self.file_failed && image.backing.path().is_some(),
                    started: Instant::now(),
                };
                emit_upload(writer, id, &upload)?;
                self.pending.insert(id, upload);
            }
            // Explicit resolved fragments also represent Unicode placeholders;
            // the cell renderer suppresses placeholder codepoints.
            write!(writer, "\x1b_Ga=d,d=i,i={id},q=2;\x1b\\").map_err(|e| e.to_string())?;
        }
        for (index, placement) in update.image_placements.iter().enumerate() {
            let Some(id) = self.ids.get(&(placement.image_id, placement.generation)) else { continue };
            if !self.resident.contains(id) {
                continue;
            }
            let Some(rect) = clipped(placement, update, origin, viewport) else { continue };
            write!(
                writer,
                "\x1b[{};{}H\x1b_Ga=p,C=1,q=2,i={},p={},z={},c={},r={},x={},y={},w={},h={},X={},Y={};\x1b\\",
                rect.row + 1,
                rect.col + 1,
                id,
                index + 1,
                placement.z,
                rect.cols,
                rect.rows,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                rect.offset_x,
                rect.offset_y
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
fn emit_upload(writer: &mut impl Write, id: u32, upload: &Upload) -> Result<(), String> {
    let format = match upload.resource.format {
        0 => 24,
        1 => 32,
        _ => 100,
    };
    let header = format!("a=t,i={id},q=0,f={format},s={},v={}", upload.resource.width_px, upload.resource.height_px);
    if upload.file {
        let path = upload.image.backing.path().and_then(|p| p.to_str()).ok_or("image path is not UTF-8")?;
        write!(writer, "\x1b_G{header},t=f;{}\x1b\\", base64(path.as_bytes())).map_err(|e| e.to_string())?;
    } else {
        let chunks = upload.image.bytes().chunks(3072);
        let count = chunks.len();
        for (index, chunk) in chunks.enumerate() {
            let more = u8::from(index + 1 < count);
            if index == 0 {
                write!(writer, "\x1b_G{header},t=d,m={more};").map_err(|e| e.to_string())?;
            } else {
                write!(writer, "\x1b_Gm={more},q=0;").map_err(|e| e.to_string())?;
            }
            write!(writer, "{}\x1b\\", base64(chunk)).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
pub(crate) fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}
#[derive(Debug, PartialEq)]
struct Rect {
    col: u32,
    row: u32,
    cols: u32,
    rows: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    offset_x: u32,
    offset_y: u32,
}
fn clipped(p: &TerminalImagePlacement, update: &TerminalRenderUpdate, origin: (u16, u16), viewport: (u16, u16)) -> Option<Rect> {
    let cw = if update.geometry.cell_width_px > 0.0 {
        f64::from(update.geometry.cell_width_px)
    } else {
        f64::from(p.pixel_width) / f64::from(p.grid_cols.max(1))
    }
    .max(1.0);
    let ch = if update.geometry.cell_height_px > 0.0 {
        f64::from(update.geometry.cell_height_px)
    } else {
        f64::from(p.pixel_height) / f64::from(p.grid_rows.max(1))
    }
    .max(1.0);
    let left = f64::from(p.viewport_col - i32::from(origin.0)) * cw + f64::from(p.x_offset_px);
    let top = f64::from(p.viewport_row - i32::from(origin.1)) * ch + f64::from(p.y_offset_px);
    let width = if p.pixel_width > 0 { f64::from(p.pixel_width) } else { f64::from(p.grid_cols) * cw };
    let height = if p.pixel_height > 0 { f64::from(p.pixel_height) } else { f64::from(p.grid_rows) * ch };
    let right = (left + width).min(f64::from(viewport.0.min(update.cols.saturating_sub(origin.0))) * cw);
    let bottom = (top + height).min(f64::from(viewport.1.min(update.rows.saturating_sub(origin.1))) * ch);
    let x0 = left.max(0.0);
    let y0 = top.max(0.0);
    if width <= 0.0 || height <= 0.0 || right <= x0 || bottom <= y0 {
        return None;
    }
    let sx = f64::from(p.source_width);
    let sy = f64::from(p.source_height);
    let crop_x = ((x0 - left) / width * sx).round() as u32;
    let crop_y = ((y0 - top) / height * sy).round() as u32;
    let source_width = ((right - x0) / width * sx).round() as u32;
    let source_height = ((bottom - y0) / height * sy).round() as u32;
    if source_width == 0 || source_height == 0 {
        return None;
    }
    Some(Rect {
        col: (x0 / cw).floor() as u32,
        row: (y0 / ch).floor() as u32,
        cols: ((right - x0) / cw).ceil() as u32,
        rows: ((bottom - y0) / ch).ceil() as u32,
        x: p.source_x + crop_x,
        y: p.source_y + crop_y,
        width: source_width,
        height: source_height,
        offset_x: (x0 % cw) as u32,
        offset_y: (y0 % ch) as u32,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{image_backing::RetainedImage, provider::*};
    #[test]
    fn file_upload_error_falls_back_and_success_does_not_reupload() {
        let image = RetainedImage::from_owned(TerminalImageBytes { image_id: 7, generation: 1, bytes: vec![1, 2, 3] });
        let update = TerminalRenderUpdate {
            image_resources: vec![TerminalImageResource {
                image_id: 7,
                generation: 1,
                format: 0,
                width_px: 1,
                height_px: 1,
                data_len: 3,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut output = KittyOutput::default();
        output.set_assets(vec![image]);
        let mut bytes = Vec::new();
        output.render(&mut bytes, &update, (0, 0), (80, 24)).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("t=f"));
        bytes.clear();
        output.reply(&mut bytes, b"\x1b_Gi=1;ENOENT\x1b\\").unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("t=d"));
        output.reply(&mut bytes, b"\x1b_Gi=1;OK\x1b\\").unwrap();
        bytes.clear();
        output.render(&mut bytes, &update, (0, 0), (80, 24)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("a=t"));
    }
    #[test]
    fn crop_tracks_pan_and_clips_to_grid_not_spare_chrome() {
        let update = TerminalRenderUpdate {
            cols: 10,
            rows: 10,
            geometry: TerminalGeometry::from_cell_size(10, 10, 10.0, 20.0),
            ..Default::default()
        };
        let p = TerminalImagePlacement {
            viewport_col: 0,
            viewport_row: 0,
            grid_cols: 10,
            grid_rows: 10,
            pixel_width: 100,
            pixel_height: 200,
            source_width: 100,
            source_height: 200,
            ..Default::default()
        };
        let r = clipped(&p, &update, (3, 2), (20, 20)).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height, r.cols, r.rows), (30, 40, 70, 160, 7, 8));
    }
}
