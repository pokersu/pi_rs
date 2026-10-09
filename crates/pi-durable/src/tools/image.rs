//! 对应 `tools/image.ts`：检测受支持图片的 MIME 类型（只读头部与 PNG 块头）。

use crate::chord::context::Context;
use crate::env::BinaryReader;
use crate::env::FileError;

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
/// 除 APNG 块遍历外每个检查需要的字节：BMP 最多读到偏移 29。
const HEADER_BYTES: usize = 32;
const BLOCK_BYTES: usize = 64 * 1024;

/// 位置读取一个 `size` 字节的文件。
pub struct ByteSource<'a> {
    pub size: u64,
    pub reader: &'a dyn BinaryReader,
    pub context: &'a dyn Context,
}

/// 对应 `detectSupportedImageMimeTypeOf`：只读头部，PNG 再读到首个 `acTL` / `IDAT`。
pub async fn detect_supported_image_mime_type_of(
    source: &ByteSource<'_>,
) -> Result<Option<String>, FileError> {
    let header = source.reader.read(0, HEADER_BYTES, source.context).await?;
    if !starts_with(&header, &PNG_SIGNATURE) {
        return Ok(detect_supported_image_mime_type(&header));
    }
    Ok(if is_png(&header) && !is_animated_png_of(source).await? {
        Some("image/png".to_string())
    } else {
        None
    })
}

/// `isAnimatedPng` 的分块读取版。
async fn is_animated_png_of(source: &ByteSource<'_>) -> Result<bool, FileError> {
    let mut block: Vec<u8> = Vec::new();
    let mut block_start: u64 = 0;
    let mut offset = PNG_SIGNATURE.len() as u64;
    while offset + 8 <= source.size {
        if offset < block_start || offset + 8 > block_start + block.len() as u64 {
            block_start = offset;
            block = source
                .reader
                .read(offset, BLOCK_BYTES, source.context)
                .await?;
        }
        let start = (offset - block_start) as usize;
        let chunk_header = &block[start..start + 8];
        let chunk_length = read_u32_be(chunk_header, 0);
        if starts_with_ascii(chunk_header, 4, "acTL") {
            return Ok(true);
        }
        if starts_with_ascii(chunk_header, 4, "IDAT") {
            return Ok(false);
        }
        let next_offset = offset + 8 + chunk_length as u64 + 4;
        if next_offset <= offset || next_offset > source.size {
            return Ok(false);
        }
        offset = next_offset;
    }
    Ok(false)
}

/// 对应 `detectSupportedImageMimeType`。
pub fn detect_supported_image_mime_type(buffer: &[u8]) -> Option<String> {
    if starts_with(buffer, &[0xff, 0xd8, 0xff]) {
        return (buffer.get(3).copied() != Some(0xf7)).then(|| "image/jpeg".to_string());
    }
    if starts_with(buffer, &PNG_SIGNATURE) {
        return (is_png(buffer) && !is_animated_png(buffer)).then(|| "image/png".to_string());
    }
    if starts_with_ascii(buffer, 0, "GIF87a") || starts_with_ascii(buffer, 0, "GIF89a") {
        return Some("image/gif".to_string());
    }
    if starts_with_ascii(buffer, 0, "RIFF") && starts_with_ascii(buffer, 8, "WEBP") {
        return Some("image/webp".to_string());
    }
    if starts_with_ascii(buffer, 0, "BM") && is_bmp(buffer) {
        return Some("image/bmp".to_string());
    }
    None
}

fn is_png(buffer: &[u8]) -> bool {
    buffer.len() >= 16
        && read_u32_be(buffer, PNG_SIGNATURE.len()) == 13
        && starts_with_ascii(buffer, 12, "IHDR")
}

fn is_animated_png(buffer: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= buffer.len() {
        let chunk_length = read_u32_be(buffer, offset);
        let chunk_type_offset = offset + 4;
        if starts_with_ascii(buffer, chunk_type_offset, "acTL") {
            return true;
        }
        if starts_with_ascii(buffer, chunk_type_offset, "IDAT") {
            return false;
        }
        let next_offset = offset + 8 + chunk_length as usize + 4;
        if next_offset <= offset || next_offset > buffer.len() {
            return false;
        }
        offset = next_offset;
    }
    false
}

fn is_bmp(buffer: &[u8]) -> bool {
    if buffer.len() < 26 {
        return false;
    }
    let declared_file_size = read_u32_le(buffer, 2);
    let pixel_data_offset = read_u32_le(buffer, 10);
    let dib_header_size = read_u32_le(buffer, 14);
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if pixel_data_offset < 14 + dib_header_size {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }
    let (color_planes, bits_per_pixel) = if dib_header_size == 12 {
        (read_u16_le(buffer, 22), read_u16_le(buffer, 24))
    } else if (40..=124).contains(&dib_header_size) {
        if buffer.len() < 30 {
            return false;
        }
        (read_u16_le(buffer, 26), read_u16_le(buffer, 28))
    } else {
        return false;
    };
    color_planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits_per_pixel)
}

fn read_u16_le(buffer: &[u8], offset: usize) -> u32 {
    u32::from(buffer.get(offset).copied().unwrap_or(0))
        + (u32::from(buffer.get(offset + 1).copied().unwrap_or(0)) << 8)
}

fn read_u32_be(buffer: &[u8], offset: usize) -> u32 {
    (u32::from(buffer.get(offset).copied().unwrap_or(0)) << 24)
        + (u32::from(buffer.get(offset + 1).copied().unwrap_or(0)) << 16)
        + (u32::from(buffer.get(offset + 2).copied().unwrap_or(0)) << 8)
        + u32::from(buffer.get(offset + 3).copied().unwrap_or(0))
}

fn read_u32_le(buffer: &[u8], offset: usize) -> u32 {
    u32::from(buffer.get(offset).copied().unwrap_or(0))
        + (u32::from(buffer.get(offset + 1).copied().unwrap_or(0)) << 8)
        + (u32::from(buffer.get(offset + 2).copied().unwrap_or(0)) << 16)
        + (u32::from(buffer.get(offset + 3).copied().unwrap_or(0)) << 24)
}

fn starts_with(buffer: &[u8], bytes: &[u8]) -> bool {
    buffer.len() >= bytes.len() && buffer[..bytes.len()] == *bytes
}

fn starts_with_ascii(buffer: &[u8], offset: usize, text: &str) -> bool {
    let text = text.as_bytes();
    buffer.len() >= offset + text.len() && buffer[offset..offset + text.len()] == *text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_image_mime_types() {
        let png: Vec<u8> = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13]
            .iter()
            .chain(b"IHDR")
            .copied()
            .collect();
        assert_eq!(
            detect_supported_image_mime_type(&png),
            Some("image/png".to_string())
        );

        let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00];
        assert_eq!(
            detect_supported_image_mime_type(&jpeg),
            Some("image/jpeg".to_string())
        );

        let gif = b"GIF89a".to_vec();
        assert_eq!(
            detect_supported_image_mime_type(&gif),
            Some("image/gif".to_string())
        );
    }

    #[test]
    fn rejects_unknown_bytes() {
        assert_eq!(detect_supported_image_mime_type(b"hello world"), None);
    }
}
