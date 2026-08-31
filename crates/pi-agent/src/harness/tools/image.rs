//! Rust 翻译自 packages/agent/src/harness/tools/image.ts

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// 对应 `detectSupportedImageMimeType`
pub fn detect_supported_image_mime_type(buffer: &[u8]) -> Option<&'static str> {
    if starts_with(buffer, &[0xff, 0xd8, 0xff]) {
        return if buffer.get(3) == Some(&0xf7) {
            None
        } else {
            Some("image/jpeg")
        };
    }
    if starts_with(buffer, &PNG_SIGNATURE) {
        return if is_png(buffer) && !is_animated_png(buffer) {
            Some("image/png")
        } else {
            None
        };
    }
    if starts_with_ascii(buffer, 0, "GIF") {
        return Some("image/gif");
    }
    if starts_with_ascii(buffer, 0, "RIFF") && starts_with_ascii(buffer, 8, "WEBP") {
        return Some("image/webp");
    }
    if starts_with_ascii(buffer, 0, "BM") && is_bmp(buffer) {
        return Some("image/bmp");
    }
    None
}

/// 对应 `encodeBase64`
pub fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    let mut index = 0;
    while index < bytes.len() {
        let first = bytes[index];
        let second = bytes.get(index + 1).copied();
        let third = bytes.get(index + 2).copied();
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output
            .push(ALPHABET[(((first & 0x03) << 4) | (second.unwrap_or(0) >> 4)) as usize] as char);
        match second {
            None => output.push('='),
            Some(second) => {
                output.push(
                    ALPHABET[(((second & 0x0f) << 2) | (third.unwrap_or(0) >> 6)) as usize] as char,
                );
            }
        }
        match third {
            None => output.push('='),
            Some(third) => output.push(ALPHABET[(third & 0x3f) as usize] as char),
        }
        index += 3;
    }
    output
}

fn is_png(buffer: &[u8]) -> bool {
    buffer.len() >= 16
        && read_uint32_be(buffer, PNG_SIGNATURE.len()) == 13
        && starts_with_ascii(buffer, 12, "IHDR")
}

fn is_animated_png(buffer: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= buffer.len() {
        let chunk_length = read_uint32_be(buffer, offset) as usize;
        let chunk_type_offset = offset + 4;
        if starts_with_ascii(buffer, chunk_type_offset, "acTL") {
            return true;
        }
        if starts_with_ascii(buffer, chunk_type_offset, "IDAT") {
            return false;
        }
        let next_offset = offset + 8 + chunk_length + 4;
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
    let declared_file_size = read_uint32_le(buffer, 2);
    let pixel_data_offset = read_uint32_le(buffer, 10);
    let dib_header_size = read_uint32_le(buffer, 14);
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if pixel_data_offset < 14 + dib_header_size {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }

    let (color_planes, bits_per_pixel): (u32, u32) = if dib_header_size == 12 {
        (
            read_uint16_le(buffer, 22) as u32,
            read_uint16_le(buffer, 24) as u32,
        )
    } else if (40..=124).contains(&dib_header_size) {
        if buffer.len() < 30 {
            return false;
        }
        (
            read_uint16_le(buffer, 26) as u32,
            read_uint16_le(buffer, 28) as u32,
        )
    } else {
        return false;
    };
    color_planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits_per_pixel)
}

fn read_uint16_le(buffer: &[u8], offset: usize) -> u16 {
    (buffer.get(offset).copied().unwrap_or(0) as u16)
        + ((buffer.get(offset + 1).copied().unwrap_or(0) as u16) << 8)
}

fn read_uint32_be(buffer: &[u8], offset: usize) -> u32 {
    (buffer.get(offset).copied().unwrap_or(0) as u32) * 0x1000000
        + ((buffer.get(offset + 1).copied().unwrap_or(0) as u32) << 16)
        + ((buffer.get(offset + 2).copied().unwrap_or(0) as u32) << 8)
        + buffer.get(offset + 3).copied().unwrap_or(0) as u32
}

fn read_uint32_le(buffer: &[u8], offset: usize) -> u32 {
    (buffer.get(offset).copied().unwrap_or(0) as u32)
        + ((buffer.get(offset + 1).copied().unwrap_or(0) as u32) << 8)
        + ((buffer.get(offset + 2).copied().unwrap_or(0) as u32) << 16)
        + (buffer.get(offset + 3).copied().unwrap_or(0) as u32) * 0x1000000
}

fn starts_with(buffer: &[u8], bytes: &[u8]) -> bool {
    buffer.len() >= bytes.len() && bytes.iter().enumerate().all(|(i, b)| buffer[i] == *b)
}

fn starts_with_ascii(buffer: &[u8], offset: usize, text: &str) -> bool {
    if buffer.len() < offset + text.len() {
        return false;
    }
    text.bytes()
        .enumerate()
        .all(|(i, b)| buffer[offset + i] == b)
}
