// language: Rust, file: src/codec/screenshot.rs
//! PNG snapshot of a decoded VideoToolbox frame.
//!
//! Frames are NV12 (`420v`/`420f`), which the Metal renderer also consumes.
//! The encoder is deliberately dependency-free: filter-0 rows plus a zlib
//! stream from the `flate2` crate already used for negotiation blobs.

use crate::codec::decoder::DecodedFrame;
use crate::codec::videotoolbox_sys::*;
use anyhow::{bail, Context, Result};
use flate2::{write::ZlibEncoder, Compression};
use std::io::Write;
use std::path::Path;

const LOCK_READ_ONLY: u64 = 1;

pub fn save_frame_png(frame: &DecodedFrame, path: &Path) -> Result<()> {
    let width = frame.width();
    let height = frame.height();
    if width == 0 || height == 0 {
        bail!("Cannot save an empty frame");
    }

    let pixel_buffer = frame.as_ptr();
    let (planes, pixel_format) = unsafe {
        (
            CVPixelBufferGetPlaneCount(pixel_buffer),
            CVPixelBufferGetPixelFormatType(pixel_buffer),
        )
    };
    if planes != 2 {
        bail!("Expected an NV12 frame with two planes, got {planes}");
    }
    let full_range = matches!(&pixel_format.to_be_bytes(), b"420f");

    let status = unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, LOCK_READ_ONLY) };
    if status != 0 {
        bail!("CVPixelBufferLockBaseAddress failed with status {status}");
    }
    let _guard = UnlockGuard(pixel_buffer);

    let (luma, luma_stride, chroma, chroma_stride) = unsafe {
        let luma = CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 0).cast::<u8>();
        let chroma = CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 1).cast::<u8>();
        (
            luma,
            CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 0),
            chroma,
            CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 1),
        )
    };
    if luma.is_null() || chroma.is_null() {
        bail!("VideoToolbox returned a frame without readable planes");
    }

    let mut rgb = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        let luma_row = unsafe { std::slice::from_raw_parts(luma.add(y * luma_stride), width) };
        let chroma_row =
            unsafe { std::slice::from_raw_parts(chroma.add((y / 2) * chroma_stride), width) };
        for (x, &luma_sample) in luma_row.iter().enumerate() {
            let uv = x & !1;
            let cb = chroma_row[uv];
            let cr = chroma_row[uv + 1];
            rgb.extend_from_slice(&if full_range {
                yuv_to_rgb(
                    luma_sample as f32 / 255.0,
                    (cb as f32 - 128.0) / 255.0,
                    (cr as f32 - 128.0) / 255.0,
                )
            } else {
                yuv_to_rgb(
                    (luma_sample as f32 - 16.0) / 219.0,
                    (cb as f32 - 128.0) / 224.0,
                    (cr as f32 - 128.0) / 224.0,
                )
            });
        }
    }

    let file = std::fs::File::create(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    write_png(file, width as u32, height as u32, &rgb)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

fn yuv_to_rgb(y: f32, cb: f32, cr: f32) -> [u8; 3] {
    let r = y + 1.5748 * cr;
    let g = y - 0.1873 * cb - 0.4681 * cr;
    let b = y + 1.8556 * cb;
    [clamp(r), clamp(g), clamp(b)]
}

fn clamp(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

struct UnlockGuard(CVPixelBufferRef);

impl Drop for UnlockGuard {
    fn drop(&mut self) {
        unsafe {
            CVPixelBufferUnlockBaseAddress(self.0, LOCK_READ_ONLY);
        }
    }
}

fn write_png(mut output: impl Write, width: u32, height: u32, rgb: &[u8]) -> Result<()> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    output.write_all(&SIGNATURE)?;

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB, deflate, no filter set, no interlace
    write_chunk(&mut output, b"IHDR", &header)?;

    let stride = width as usize * 3;
    let mut filtered = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgb.chunks_exact(stride) {
        filtered.push(0); // PNG filter type: None
        filtered.extend_from_slice(row);
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&filtered)?;
    let compressed = encoder.finish()?;
    write_chunk(&mut output, b"IDAT", &compressed)?;
    write_chunk(&mut output, b"IEND", &[])?;
    Ok(())
}

fn write_chunk(output: &mut impl Write, kind: &[u8; 4], data: &[u8]) -> Result<()> {
    output.write_all(&(data.len() as u32).to_be_bytes())?;
    output.write_all(kind)?;
    output.write_all(data)?;
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    output.write_all(&crc.finish().to_be_bytes())?;
    Ok(())
}

struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.0 ^= byte as u32;
            for _ in 0..8 {
                let mask = (self.0 & 1).wrapping_neg();
                self.0 = (self.0 >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
    }

    fn finish(self) -> u32 {
        !self.0
    }
}
