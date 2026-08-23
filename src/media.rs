use std::collections::HashSet;
use std::io::{BufReader, Cursor, Read};
use std::path::Path;

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::model::KakaoCredentials;

pub const MAX_IMAGE_INPUTS: usize = 10;
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
pub const MAX_IMAGE_BATCH_BYTES: u64 = 20 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const MAX_NORMALIZED_IMAGE_DIMENSION: u32 = 1_024;
const JPEG_START_OF_IMAGE: [u8; 2] = [0xff, 0xd8];
const JPEG_END_OF_IMAGE: [u8; 2] = [0xff, 0xd9];
const SEF_TAIL_HEADER_LENGTH: usize = 12;
const SEF_TAIL_FOOTER_LENGTH: usize = 8;
const SEF_SDR_LENGTH: usize = 12;
const MAX_SEF_SDR_COUNT: usize = 128;
const SEF_SAMSUNG_CAPTURE_INFO: u16 = 0x0c51;
const SEF_CAPTURED_APP_INFO: u16 = 0x0da1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageDownloadSource {
    pub url: String,
    /// Whether this locator was synthesized from a Kakao media key and needs
    /// the account Authorization header. Signed CDN URLs are intentionally
    /// fetched without credentials so local, read-only downloads never fall
    /// back to interactive credential prompts.
    pub requires_credentials: bool,
    pub declared_size: Option<u64>,
    pub declared_width: Option<u32>,
    pub declared_height: Option<u32>,
    pub expected_media_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedImageFile {
    pub size: u64,
    pub sha256: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
}

/// Detect media type from magic bytes, falling back to file extension.
/// Returns (kakao_msg_type, extension).
pub fn detect_media_type(data: &[u8], file_ext: &str) -> (i32, String) {
    // Magic bytes detection
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xD8 {
        return (2, "jpg".into());
    }
    if data.len() >= 8 && &data[..8] == b"\x89PNG\r\n\x1a\n" {
        return (2, "png".into());
    }
    if data.len() >= 4 && &data[..4] == b"GIF8" {
        return (14, "gif".into());
    }
    // Video: ftyp box (MP4/MOV/3GP)
    if data.len() >= 8 && &data[4..8] == b"ftyp" {
        return (3, if file_ext == "mov" { "mov" } else { "mp4" }.into());
    }
    // WebM
    if data.len() >= 4 && &data[..4] == b"\x1a\x45\xdf\xa3" {
        return (3, "webm".into());
    }

    // Fall back to extension
    match file_ext {
        "jpg" | "jpeg" => (2, "jpg".into()),
        "png" => (2, "png".into()),
        "gif" => (14, "gif".into()),
        "mp4" | "mov" | "avi" | "mkv" | "webm" => (3, file_ext.into()),
        "m4a" | "aac" | "mp3" | "wav" | "ogg" => (12, file_ext.into()),
        _ => (
            26,
            if file_ext.is_empty() { "bin" } else { file_ext }.into(),
        ),
    }
}

/// Extract JPEG dimensions from SOF marker.
pub fn jpeg_dimensions(data: &[u8]) -> Option<(i32, i32)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        i += 2;
        if i + 2 > data.len() {
            return None;
        }
        // SOF markers (C0-CF except C4, C8, CC)
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            if i + 7 > data.len() {
                return None;
            }
            let height = ((data[i + 3] as i32) << 8) | (data[i + 4] as i32);
            let width = ((data[i + 5] as i32) << 8) | (data[i + 6] as i32);
            return Some((width, height));
        }
        let len = ((data[i] as usize) << 8) | (data[i + 1] as usize);
        if len < 2 {
            return None;
        }
        i += len;
    }
    None
}

/// Extract PNG dimensions from IHDR chunk.
pub fn png_dimensions(data: &[u8]) -> Option<(i32, i32)> {
    if data.len() < 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = ((data[16] as i32) << 24)
        | ((data[17] as i32) << 16)
        | ((data[18] as i32) << 8)
        | (data[19] as i32);
    let height = ((data[20] as i32) << 24)
        | ((data[21] as i32) << 16)
        | ((data[22] as i32) << 8)
        | (data[23] as i32);
    Some((width, height))
}

fn safe_kakao_media_url(value: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(value)?;
    let host = parsed.host_str().unwrap_or("");
    if parsed.scheme() != "https"
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.port().is_some_and(|port| port != 443)
        || parsed.fragment().is_some()
        || (!host.ends_with(".kakao.com") && !host.ends_with(".kakaocdn.net"))
    {
        anyhow::bail!("Refusing unsafe media URL");
    }
    Ok(parsed.into())
}

fn safe_media_key(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > 4096
        || value.starts_with('/')
        || value.contains('\0')
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || value.contains("://")
        || value.contains('?')
        || value.contains('#')
    {
        anyhow::bail!("Malformed Kakao media key");
    }
    Ok(format!("https://dn-m.talk.kakao.com/talkm/{value}"))
}

fn safe_emoticon_path(value: &str) -> Result<String> {
    // Emoticon attachments carry a relative `path` under Kakao's public item
    // CDN rather than a Talk media `k` key.
    safe_media_key(value)?;
    Ok(format!("https://item.kakaocdn.net/dw/{value}"))
}

fn positive_u64(value: Option<&serde_json::Value>, label: &str, maximum: u64) -> Result<u64> {
    let number = value
        .and_then(serde_json::Value::as_u64)
        .filter(|number| *number > 0 && *number <= maximum)
        .ok_or_else(|| anyhow::anyhow!("Attachment has invalid {label}"))?;
    Ok(number)
}

fn positive_dimension(value: Option<&serde_json::Value>, label: &str) -> Result<u32> {
    let number = positive_u64(value, label, u64::from(MAX_IMAGE_DIMENSION))?;
    Ok(number as u32)
}

fn normalized_media_type(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" | "image/jpg" | "image/jpeg" => Some("jpeg"),
        "png" | "image/png" => Some("png"),
        "webp" | "image/webp" => Some("webp"),
        "gif" | "image/gif" => Some("gif"),
        _ => None,
    }
}

fn still_raster_media_type(value: &str) -> bool {
    matches!(value, "jpeg" | "png" | "webp")
}

fn attachment_media_type_accepts_payload(expected: &str, actual: &str) -> bool {
    // Kakao Talk CDN often serves PNG or WebP bytes while the local DB row
    // still says `mt=image/jpg` and a `.jpg` key. Trust the decoded payload
    // when both sides are still rasters this pipeline already allows.
    expected == actual || (still_raster_media_type(expected) && still_raster_media_type(actual))
}

fn locator_extension_media_type(value: &str) -> Option<&'static str> {
    let path = reqwest::Url::parse(value)
        .ok()
        .map(|url| url.path().to_string())
        .unwrap_or_else(|| value.to_string());
    let extension = Path::new(&path)
        .extension()
        .and_then(|extension| extension.to_str())?;
    normalized_media_type(extension)
}

fn image_locator(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<(String, String, bool)> {
    let direct = object
        .get("url")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(safe_kakao_media_url)
        .transpose()?;
    let key = object
        .get("k")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(safe_media_key)
        .transpose()?;
    let requires_credentials = direct.is_none();
    let url = direct
        .or(key)
        .ok_or_else(|| anyhow::anyhow!("Attachment has no full-resolution image locator"))?;
    let locator_hint = object
        .get("url")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| object.get("k").and_then(serde_json::Value::as_str))
        .unwrap_or("")
        .to_string();
    Ok((url, locator_hint, requires_credentials))
}

fn string_array<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<Vec<&'a str>>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    let array = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Attachment {key} must be an array"))?;
    let mut values = Vec::with_capacity(array.len());
    for item in array {
        let item = item
            .as_str()
            .filter(|item| !item.is_empty() && item.len() <= 4096)
            .ok_or_else(|| anyhow::anyhow!("Attachment {key} has an invalid item"))?;
        values.push(item);
    }
    Ok(Some(values))
}

fn positive_u64_array(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    count: usize,
    maximum: u64,
) -> Result<Vec<u64>> {
    let array = object
        .get(key)
        .and_then(serde_json::Value::as_array)
        .filter(|array| array.len() == count)
        .ok_or_else(|| anyhow::anyhow!("Attachment {key} has the wrong shape"))?;
    array
        .iter()
        .map(|value| positive_u64(Some(value), key, maximum))
        .collect()
}

fn validate_optional_string_array(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    count: usize,
    maximum_bytes: usize,
) -> Result<()> {
    if let Some(value) = object.get(key) {
        let array = value
            .as_array()
            .filter(|array| array.len() == count)
            .ok_or_else(|| anyhow::anyhow!("Attachment {key} has the wrong shape"))?;
        if array.iter().any(|item| {
            item.as_str()
                .is_none_or(|item| item.is_empty() || item.len() > maximum_bytes)
        }) {
            anyhow::bail!("Attachment {key} has an invalid item");
        }
    }
    Ok(())
}

fn validate_optional_dimension_array(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    count: usize,
) -> Result<()> {
    if object.get(key).is_some() {
        positive_u64_array(object, key, count, u64::from(MAX_IMAGE_DIMENSION))?;
    }
    Ok(())
}

/// Parse only image-bearing Kakao attachment shapes used by automatic reply.
/// Every returned source is full-resolution and preserves the sender's order.
pub fn parse_image_download_sources(
    attachment: &str,
    message_type: i32,
) -> Result<Vec<ImageDownloadSource>> {
    let value: serde_json::Value = serde_json::from_str(attachment)?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Image attachment must be a JSON object"))?;
    match message_type {
        2 | 14 => {
            let emoticon_path = object
                .get("path")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty());
            let has_talk_locator = object
                .get("url")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.is_empty())
                || object
                    .get("k")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.is_empty());
            let (url, locator_hint, requires_credentials) = if message_type == 14 {
                match (emoticon_path, has_talk_locator) {
                    (Some(_), true) => {
                        anyhow::bail!("Emoticon attachment has ambiguous locators")
                    }
                    (Some(path), false) => (safe_emoticon_path(path)?, path.to_string(), false),
                    (None, _) => image_locator(object)?,
                }
            } else {
                if emoticon_path.is_some() {
                    anyhow::bail!("Photo attachment has an unexpected emoticon path");
                }
                image_locator(object)?
            };
            let (declared_size, declared_width, declared_height) = if message_type == 2 {
                (
                    Some(positive_u64(object.get("s"), "s", MAX_IMAGE_BYTES)?),
                    Some(positive_dimension(object.get("w"), "w")?),
                    Some(positive_dimension(object.get("h"), "h")?),
                )
            } else {
                let width = object.get("width");
                let height = object.get("height");
                if width.is_some() != height.is_some() {
                    anyhow::bail!("Emoticon dimensions are incomplete");
                }
                (
                    None,
                    width
                        .map(|value| positive_dimension(Some(value), "width"))
                        .transpose()?,
                    height
                        .map(|value| positive_dimension(Some(value), "height"))
                        .transpose()?,
                )
            };
            let declared_kind = object
                .get(if message_type == 2 { "mt" } else { "type" })
                .and_then(serde_json::Value::as_str)
                .and_then(normalized_media_type);
            let extension_kind = locator_extension_media_type(&locator_hint);
            if declared_kind.is_some()
                && extension_kind.is_some()
                && declared_kind != extension_kind
            {
                anyhow::bail!("Image attachment media type conflicts with its locator");
            }
            Ok(vec![ImageDownloadSource {
                url,
                requires_credentials,
                declared_size,
                declared_width,
                declared_height,
                expected_media_type: declared_kind.or(extension_kind).map(str::to_string),
            }])
        }
        27 => {
            if object.get("url").is_some() || object.get("k").is_some() {
                anyhow::bail!("Multi-photo attachment has ambiguous scalar locators");
            }
            let keys = string_array(object, "kl")?;
            let urls = string_array(object, "imageUrls")?;
            let count = urls
                .as_ref()
                .map(Vec::len)
                .or_else(|| keys.as_ref().map(Vec::len))
                .ok_or_else(|| anyhow::anyhow!("Multi-photo attachment has no locators"))?;
            if !(2..=MAX_IMAGE_INPUTS).contains(&count)
                || keys.as_ref().is_some_and(|values| values.len() != count)
                || urls.as_ref().is_some_and(|values| values.len() != count)
            {
                anyhow::bail!("Multi-photo attachment locator count is invalid");
            }
            let sizes = positive_u64_array(object, "sl", count, MAX_IMAGE_BYTES)?;
            let widths = positive_u64_array(object, "wl", count, u64::from(MAX_IMAGE_DIMENSION))?;
            let heights = positive_u64_array(object, "hl", count, u64::from(MAX_IMAGE_DIMENSION))?;
            if sizes.iter().sum::<u64>() > MAX_IMAGE_BATCH_BYTES {
                anyhow::bail!("Multi-photo attachment exceeds the aggregate byte cap");
            }
            validate_optional_string_array(object, "csl", count, 128)?;
            validate_optional_string_array(object, "thumbnailUrls", count, 4096)?;
            validate_optional_dimension_array(object, "thumbnailWidths", count)?;
            validate_optional_dimension_array(object, "thumbnailHeights", count)?;
            let media_types = match string_array(object, "mtl")? {
                Some(values) if values.len() == count => values
                    .into_iter()
                    .map(|value| {
                        normalized_media_type(value)
                            .map(str::to_string)
                            .ok_or_else(|| anyhow::anyhow!("Attachment mtl has an invalid item"))
                    })
                    .collect::<Result<Vec<_>>>()?,
                Some(_) => anyhow::bail!("Attachment mtl has the wrong shape"),
                None => Vec::new(),
            };
            let mut seen_urls = HashSet::new();
            let mut seen_keys = HashSet::new();
            let mut sources = Vec::with_capacity(count);
            for index in 0..count {
                let direct = urls
                    .as_ref()
                    .map(|values| safe_kakao_media_url(values[index]))
                    .transpose()?;
                let key_url = keys
                    .as_ref()
                    .map(|values| safe_media_key(values[index]))
                    .transpose()?;
                if direct
                    .as_ref()
                    .is_some_and(|value| !seen_urls.insert(value.clone()))
                    || key_url
                        .as_ref()
                        .is_some_and(|value| !seen_keys.insert(value.clone()))
                {
                    anyhow::bail!("Multi-photo attachment contains duplicate locators");
                }
                let url = direct
                    .clone()
                    .or(key_url.clone())
                    .ok_or_else(|| anyhow::anyhow!("Multi-photo item has no locator"))?;
                let locator_hint = urls
                    .as_ref()
                    .map(|values| values[index])
                    .or_else(|| keys.as_ref().map(|values| values[index]))
                    .unwrap_or("");
                let expected = media_types
                    .get(index)
                    .cloned()
                    .or_else(|| locator_extension_media_type(locator_hint).map(str::to_string));
                sources.push(ImageDownloadSource {
                    url,
                    requires_credentials: direct.is_none(),
                    declared_size: Some(sizes[index]),
                    declared_width: Some(widths[index] as u32),
                    declared_height: Some(heights[index] as u32),
                    expected_media_type: expected,
                });
            }
            Ok(sources)
        }
        _ => anyhow::bail!("Message type is not a supported image attachment"),
    }
}

/// Parse attachment JSON to extract download URL and filename.
/// Returns (url, filename) or None if unparseable.
pub fn parse_attachment_url(attachment: &str, msg_type: i32) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(attachment).ok()?;

    // Try direct "url" field first
    if let Some(url) = v.get("url").and_then(|u| u.as_str()) {
        if !url.is_empty() {
            let filename = v
                .get("name")
                .and_then(|n| n.as_str())
                .filter(|n| !n.is_empty() && *n != "(Emoticons)")
                .map(String::from)
                .or_else(|| {
                    // Try to extract filename from "k" field
                    v.get("k")
                        .and_then(|k| k.as_str())
                        .and_then(|k| k.rsplit('/').next())
                        .filter(|n| n.contains('.'))
                        .map(String::from)
                })
                .unwrap_or_else(|| {
                    let ext = media_extension(msg_type);
                    format!("media.{}", ext)
                });
            return Some((url.to_string(), filename));
        }
    }

    // Try "k" field (photo/video key): https://dn-m.talk.kakao.com/talkm/{k}
    if let Some(k) = v.get("k").and_then(|k| k.as_str()) {
        if !k.is_empty() {
            let url = format!("https://dn-m.talk.kakao.com/talkm/{}", k);
            // Use the key's last segment as filename base
            let key_name = k.rsplit('/').next().unwrap_or(k);
            let ext = media_extension(msg_type);
            let filename = if key_name.contains('.') {
                key_name.to_string()
            } else {
                format!("{}.{}", key_name, ext)
            };
            return Some((url, filename));
        }
    }

    None
}

pub fn media_extension(msg_type: i32) -> &'static str {
    match msg_type {
        2 | 27 => "jpg",
        3 => "mp4",
        12 => "m4a",
        14 => "gif",
        26 => "bin",
        _ => "dat",
    }
}

/// Sanitize a filename by stripping path components and dangerous characters.
pub fn sanitize_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");
    let sanitized: String = base
        .chars()
        .filter(|c| *c != '\0' && *c != '/' && *c != '\\')
        .collect();
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        "download".to_string()
    } else {
        sanitized
    }
}

const MAX_MEDIA_BYTES: u64 = MAX_IMAGE_BYTES;

fn checked_dimensions(width: u32, height: u32) -> Result<(u32, u32)> {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || pixels > MAX_IMAGE_PIXELS
    {
        anyhow::bail!("Image dimensions are invalid");
    }
    Ok((width, height))
}

fn jpeg_image_dimensions(data: &[u8]) -> Result<(u32, u32)> {
    if !data.ends_with(&JPEG_END_OF_IMAGE) {
        anyhow::bail!("JPEG is truncated");
    }
    let (width, height) = jpeg_dimensions(data)
        .filter(|(width, height)| *width > 0 && *height > 0)
        .ok_or_else(|| anyhow::anyhow!("JPEG dimensions are unavailable"))?;
    checked_dimensions(width as u32, height as u32)
}

fn little_endian_u16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn little_endian_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Return the JPEG prefix only when every byte after its unique EOI is a
/// structurally complete Samsung Extension Format trailer.
///
/// This follows AOSP's `SefReader`: the final little-endian offset plus the
/// eight-byte footer locates an `SEFH` header followed by `count` twelve-byte
/// SDR records and a `SEFT` footer.  We additionally require those records to
/// reference a sorted, non-overlapping, gapless cover of exactly the bytes
/// between JPEG EOI and the tail table; unknown or partial trailers fail shut.
fn jpeg_prefix_with_exact_sef_trailer(data: &[u8]) -> Option<&[u8]> {
    if data.len() < SEF_TAIL_HEADER_LENGTH + SEF_TAIL_FOOTER_LENGTH
        || data.get(data.len() - 4..)? != b"SEFT"
    {
        return None;
    }
    let footer_start = data.len().checked_sub(SEF_TAIL_FOOTER_LENGTH)?;
    let tail_offset = usize::try_from(little_endian_u32(data, footer_start)?).ok()?;
    let tail_length = tail_offset.checked_add(SEF_TAIL_FOOTER_LENGTH)?;
    let tail_start = data.len().checked_sub(tail_length)?;
    if data.get(tail_start..tail_start + 4)? != b"SEFH" {
        return None;
    }
    // The version is intentionally opaque, but it must be present and nonzero.
    if little_endian_u32(data, tail_start + 4)? == 0 {
        return None;
    }
    let count = usize::try_from(little_endian_u32(data, tail_start + 8)?).ok()?;
    if count == 0 || count > MAX_SEF_SDR_COUNT {
        return None;
    }
    let sdr_bytes = count.checked_mul(SEF_SDR_LENGTH)?;
    if tail_start
        .checked_add(SEF_TAIL_HEADER_LENGTH)?
        .checked_add(sdr_bytes)?
        != footer_start
    {
        return None;
    }

    let eoi_offsets = data
        .windows(JPEG_END_OF_IMAGE.len())
        .enumerate()
        .filter_map(|(offset, window)| (window == JPEG_END_OF_IMAGE).then_some(offset))
        .collect::<Vec<_>>();
    let [eoi_offset] = eoi_offsets.as_slice() else {
        return None;
    };
    let eoi_end = eoi_offset.checked_add(JPEG_END_OF_IMAGE.len())?;
    if eoi_end > tail_start {
        return None;
    }

    let mut intervals = Vec::with_capacity(count);
    let mut seen_types = HashSet::with_capacity(count);
    for index in 0..count {
        let offset = tail_start
            .checked_add(SEF_TAIL_HEADER_LENGTH)?
            .checked_add(index.checked_mul(SEF_SDR_LENGTH)?)?;
        if little_endian_u16(data, offset)? != 0 {
            return None;
        }
        let data_type = little_endian_u16(data, offset + 2)?;
        let expected_name: &[u8] = match data_type {
            SEF_SAMSUNG_CAPTURE_INFO => b"Samsung_Capture_Info",
            SEF_CAPTURED_APP_INFO => b"Captured_App_Info",
            _ => return None,
        };
        if !seen_types.insert(data_type) {
            return None;
        }
        let negative_offset = usize::try_from(little_endian_u32(data, offset + 4)?).ok()?;
        let size = usize::try_from(little_endian_u32(data, offset + 8)?).ok()?;
        if data_type == 0 || negative_offset == 0 || size == 0 {
            return None;
        }
        let start = tail_start.checked_sub(negative_offset)?;
        let end = start.checked_add(size)?;
        if start < eoi_end || end > tail_start {
            return None;
        }
        let record = data.get(start..end)?;
        let name_length = usize::try_from(little_endian_u32(record, 4)?).ok()?;
        let name_end = 8usize.checked_add(name_length)?;
        if little_endian_u16(record, 0)? != 0
            || little_endian_u16(record, 2)? != data_type
            || name_length != expected_name.len()
            || record.get(8..name_end)? != expected_name
            || name_end >= record.len()
        {
            return None;
        }
        intervals.push((start, end));
    }
    intervals.sort_unstable();
    let mut cursor = eoi_end;
    for (start, end) in intervals {
        if start != cursor || end <= start {
            return None;
        }
        cursor = end;
    }
    (cursor == tail_start).then(|| &data[..eoi_end])
}

fn attested_image_bytes_for_decode<'a>(
    data: &'a [u8],
    declared_size: Option<u64>,
    expected_media_type: Option<&str>,
) -> Result<&'a [u8]> {
    let original_size = u64::try_from(data.len())?;
    if original_size == 0 || original_size > MAX_IMAGE_BYTES {
        anyhow::bail!("Downloaded image size is invalid");
    }
    if declared_size.is_some_and(|expected| expected != original_size) {
        anyhow::bail!("Downloaded image does not match its declared size");
    }

    if !data.starts_with(&JPEG_START_OF_IMAGE) || data.ends_with(&JPEG_END_OF_IMAGE) {
        return Ok(data);
    }
    if declared_size == Some(original_size) && expected_media_type == Some("jpeg") {
        if let Some(jpeg) = jpeg_prefix_with_exact_sef_trailer(data) {
            return Ok(jpeg);
        }
    }
    anyhow::bail!("JPEG terminal marker is invalid");
}

fn png_image_dimensions(data: &[u8]) -> Result<(u32, u32)> {
    if data.len() < 45 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        anyhow::bail!("PNG signature is invalid");
    }
    if u32::from_be_bytes(data[8..12].try_into()?) != 13 || &data[12..16] != b"IHDR" {
        anyhow::bail!("PNG IHDR is invalid");
    }
    let width = u32::from_be_bytes(data[16..20].try_into()?);
    let height = u32::from_be_bytes(data[20..24].try_into()?);
    let mut offset = 8usize;
    let mut saw_ihdr = false;
    let mut saw_iend = false;
    while offset < data.len() {
        if offset + 12 > data.len() {
            anyhow::bail!("PNG chunk is truncated");
        }
        let length = u32::from_be_bytes(data[offset..offset + 4].try_into()?) as usize;
        let chunk_type = &data[offset + 4..offset + 8];
        let next = offset
            .checked_add(12)
            .and_then(|value| value.checked_add(length))
            .filter(|next| *next <= data.len())
            .ok_or_else(|| anyhow::anyhow!("PNG chunk is truncated"))?;
        if chunk_type == b"IHDR" {
            if saw_ihdr || offset != 8 || length != 13 {
                anyhow::bail!("PNG IHDR is invalid");
            }
            saw_ihdr = true;
        } else if chunk_type == b"acTL" {
            anyhow::bail!("Animated PNG inputs are not supported");
        } else if chunk_type == b"IEND" {
            if length != 0 || next != data.len() {
                anyhow::bail!("PNG IEND is invalid");
            }
            saw_iend = true;
        }
        offset = next;
    }
    if !saw_ihdr || !saw_iend {
        anyhow::bail!("PNG is incomplete");
    }
    checked_dimensions(width, height)
}

fn gif_static_dimensions(data: &[u8]) -> Result<(u32, u32)> {
    if data.len() < 14 || !matches!(&data[..6], b"GIF87a" | b"GIF89a") {
        anyhow::bail!("GIF signature is invalid");
    }
    let width = u16::from_le_bytes([data[6], data[7]]) as u32;
    let height = u16::from_le_bytes([data[8], data[9]]) as u32;
    let packed = data[10];
    let mut offset = 13usize;
    if packed & 0x80 != 0 {
        let table_size = 3usize
            .checked_mul(1usize << (usize::from(packed & 0x07) + 1))
            .ok_or_else(|| anyhow::anyhow!("GIF color table is invalid"))?;
        offset = offset
            .checked_add(table_size)
            .ok_or_else(|| anyhow::anyhow!("GIF color table is invalid"))?;
    }

    fn skip_sub_blocks(data: &[u8], offset: &mut usize) -> Result<()> {
        loop {
            let length = *data
                .get(*offset)
                .ok_or_else(|| anyhow::anyhow!("GIF sub-block is truncated"))?
                as usize;
            *offset += 1;
            if length == 0 {
                return Ok(());
            }
            *offset = offset
                .checked_add(length)
                .filter(|next| *next <= data.len())
                .ok_or_else(|| anyhow::anyhow!("GIF sub-block is truncated"))?;
        }
    }

    let mut frames = 0usize;
    let mut terminated = false;
    while offset < data.len() {
        match data[offset] {
            0x2c => {
                frames += 1;
                if frames > 1 {
                    anyhow::bail!("Animated GIF inputs are not supported");
                }
                if offset + 10 > data.len() {
                    anyhow::bail!("GIF image descriptor is truncated");
                }
                let local_packed = data[offset + 9];
                offset += 10;
                if local_packed & 0x80 != 0 {
                    let table_size = 3usize
                        .checked_mul(1usize << (usize::from(local_packed & 0x07) + 1))
                        .ok_or_else(|| anyhow::anyhow!("GIF color table is invalid"))?;
                    offset = offset
                        .checked_add(table_size)
                        .filter(|next| *next <= data.len())
                        .ok_or_else(|| anyhow::anyhow!("GIF color table is truncated"))?;
                }
                // LZW minimum code size byte.
                offset = offset
                    .checked_add(1)
                    .filter(|next| *next <= data.len())
                    .ok_or_else(|| anyhow::anyhow!("GIF image data is truncated"))?;
                skip_sub_blocks(data, &mut offset)?;
            }
            0x21 => {
                // Extension introducer plus extension label.
                offset = offset
                    .checked_add(2)
                    .filter(|next| *next <= data.len())
                    .ok_or_else(|| anyhow::anyhow!("GIF extension is truncated"))?;
                skip_sub_blocks(data, &mut offset)?;
            }
            0x3b => {
                offset += 1;
                terminated = true;
                break;
            }
            _ => anyhow::bail!("GIF block structure is invalid"),
        }
    }
    if frames != 1 || !terminated || offset != data.len() {
        anyhow::bail!("GIF must contain exactly one complete image frame");
    }
    checked_dimensions(width, height)
}

fn webp_image_dimensions(data: &[u8]) -> Result<(u32, u32)> {
    if data.len() < 30 || &data[..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        anyhow::bail!("WEBP signature is invalid");
    }
    let declared_riff_size = u32::from_le_bytes(data[4..8].try_into()?) as usize + 8;
    if declared_riff_size != data.len() {
        anyhow::bail!("WEBP container size is invalid");
    }
    match &data[12..16] {
        b"VP8X" => {
            if data[20] & 0x02 != 0 {
                anyhow::bail!("Animated WEBP inputs are not supported");
            }
            let width = 1 + u32::from_le_bytes([data[24], data[25], data[26], 0]);
            let height = 1 + u32::from_le_bytes([data[27], data[28], data[29], 0]);
            checked_dimensions(width, height)
        }
        b"VP8L" => {
            if data[20] != 0x2f || data.len() < 25 {
                anyhow::bail!("WEBP lossless header is invalid");
            }
            let width = 1 + (((u32::from(data[22]) & 0x3f) << 8) | u32::from(data[21]));
            let height = 1
                + (((u32::from(data[24]) & 0x0f) << 10)
                    | (u32::from(data[23]) << 2)
                    | (u32::from(data[22]) >> 6));
            checked_dimensions(width, height)
        }
        b"VP8 " => {
            if data.len() < 30 || &data[23..26] != b"\x9d\x01\x2a" {
                anyhow::bail!("WEBP lossy header is invalid");
            }
            let width = u16::from_le_bytes([data[26], data[27]]) & 0x3fff;
            let height = u16::from_le_bytes([data[28], data[29]]) & 0x3fff;
            checked_dimensions(u32::from(width), u32::from(height))
        }
        _ => anyhow::bail!("WEBP image chunk is unsupported"),
    }
}

fn image_metadata(data: &[u8]) -> Result<(&'static str, u32, u32)> {
    let (media_type, dimensions) = if data.starts_with(&[0xff, 0xd8]) {
        ("jpeg", jpeg_image_dimensions(data)?)
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        ("png", png_image_dimensions(data)?)
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        ("gif", gif_static_dimensions(data)?)
    } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(&b"WEBP"[..]) {
        ("webp", webp_image_dimensions(data)?)
    } else {
        anyhow::bail!("Downloaded media is not a supported image");
    };
    Ok((media_type, dimensions.0, dimensions.1))
}

fn read_private_image_file(path: &Path) -> Result<Vec<u8>> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let path_metadata = std::fs::symlink_metadata(path)?;
    if !path_metadata.file_type().is_file() {
        anyhow::bail!("Downloaded image path is not a regular file");
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(unix)]
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
        || (metadata.dev(), metadata.ino()) != (path_metadata.dev(), path_metadata.ino())
    {
        anyhow::bail!("Downloaded image ownership is invalid");
    }
    if metadata.len() == 0 || metadata.len() > MAX_IMAGE_BYTES {
        anyhow::bail!("Downloaded image size is invalid");
    }
    let mut data = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut data)?;
    if data.len() as u64 != metadata.len() {
        anyhow::bail!("Downloaded image changed while being validated");
    }
    let after = std::fs::symlink_metadata(path)?;
    #[cfg(unix)]
    if (after.dev(), after.ino()) != (metadata.dev(), metadata.ino())
        || after.len() != metadata.len()
    {
        anyhow::bail!("Downloaded image path changed while being validated");
    }
    Ok(data)
}

fn image_format(media_type: &str) -> Result<image::ImageFormat> {
    match media_type {
        "jpeg" => Ok(image::ImageFormat::Jpeg),
        "png" => Ok(image::ImageFormat::Png),
        "webp" => Ok(image::ImageFormat::WebP),
        "gif" => Ok(image::ImageFormat::Gif),
        _ => anyhow::bail!("Unsupported image decoder format"),
    }
}

fn decode_image(data: &[u8], media_type: &str) -> Result<(image::DynamicImage, u32, u32)> {
    use image::ImageDecoder;

    let reader = BufReader::new(Cursor::new(data));
    let mut image_reader = image::ImageReader::with_format(reader, image_format(media_type)?);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_PIXELS * 8);
    image_reader.limits(limits);
    let mut decoder = image_reader.into_decoder()?;
    let decoded_dimensions = decoder.dimensions();
    checked_dimensions(decoded_dimensions.0, decoded_dimensions.1)?;
    let orientation = decoder.orientation()?;
    let mut decoded = image::DynamicImage::from_decoder(decoder)?;
    decoded.apply_orientation(orientation);
    checked_dimensions(decoded.width(), decoded.height())?;
    Ok((decoded, decoded_dimensions.0, decoded_dimensions.1))
}

fn strict_decode_attested_jpeg(data: &[u8]) -> Result<()> {
    let options = zune_core::options::DecoderOptions::new_safe()
        .set_strict_mode(true)
        .set_max_width(MAX_IMAGE_DIMENSION as usize)
        .set_max_height(MAX_IMAGE_DIMENSION as usize);
    let cursor = zune_core::bytestream::ZCursor::new(data);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(cursor, options);
    decoder.decode_headers()?;
    let (width, height) = decoder
        .dimensions()
        .ok_or_else(|| anyhow::anyhow!("Strict JPEG dimensions are unavailable"))?;
    checked_dimensions(width as u32, height as u32)?;
    let output_size = decoder
        .output_buffer_size()
        .filter(|size| *size > 0 && (*size as u64) <= MAX_IMAGE_PIXELS.saturating_mul(4))
        .ok_or_else(|| anyhow::anyhow!("Strict JPEG decoded size is invalid"))?;
    let mut decoded = vec![0u8; output_size];
    decoder.decode_into(&mut decoded)?;
    if decoded.len() != output_size {
        anyhow::bail!("Strict JPEG decoded size is invalid");
    }
    Ok(())
}

fn encode_normalized_png(decoded: image::DynamicImage) -> Result<(Vec<u8>, u32, u32)> {
    let normalized = decoded
        .thumbnail(
            MAX_NORMALIZED_IMAGE_DIMENSION,
            MAX_NORMALIZED_IMAGE_DIMENSION,
        )
        .to_rgba8();
    let width = normalized.width();
    let height = normalized.height();
    checked_dimensions(width, height)?;
    let mut cursor = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(normalized).write_to(&mut cursor, image::ImageFormat::Png)?;
    let data = cursor.into_inner();
    if data.is_empty() || data.len() as u64 > MAX_IMAGE_BYTES {
        anyhow::bail!("Normalized PNG exceeds the image byte cap");
    }
    Ok((data, width, height))
}

pub fn validate_normalized_image(path: &Path) -> Result<ValidatedImageFile> {
    let data = read_private_image_file(path)?;
    let (media_type, width, height) = image_metadata(&data)?;
    if media_type != "png" {
        anyhow::bail!("Normalized image is not PNG");
    }
    let (decoded, raw_width, raw_height) = decode_image(&data, media_type)?;
    if (raw_width, raw_height) != (width, height)
        || (decoded.width(), decoded.height()) != (width, height)
    {
        anyhow::bail!("Normalized PNG dimensions are inconsistent");
    }
    Ok(ValidatedImageFile {
        size: data.len() as u64,
        sha256: hex::encode(Sha256::digest(&data)),
        media_type: media_type.to_string(),
        width,
        height,
    })
}

/// Fully decode an attested image and write a metadata-free, bounded PNG.
pub fn normalize_downloaded_image(
    source_path: &Path,
    normalized_path: &Path,
    message_type: i32,
    source: &ImageDownloadSource,
) -> Result<ValidatedImageFile> {
    let data = read_private_image_file(source_path)?;
    let decode_data = attested_image_bytes_for_decode(
        &data,
        source.declared_size,
        source.expected_media_type.as_deref(),
    )?;
    if decode_data.len() != data.len() {
        strict_decode_attested_jpeg(decode_data)?;
    }
    let (media_type, raw_width, raw_height) = image_metadata(decode_data)?;
    if message_type != 14 && media_type == "gif" {
        anyhow::bail!("GIF content does not match the Kakao message type");
    }
    if source
        .expected_media_type
        .as_deref()
        .is_some_and(|expected| !attachment_media_type_accepts_payload(expected, media_type))
    {
        anyhow::bail!("Downloaded image media type does not match its attachment");
    }
    let (decoded, decoded_raw_width, decoded_raw_height) = decode_image(decode_data, media_type)?;
    if (decoded_raw_width, decoded_raw_height) != (raw_width, raw_height) {
        anyhow::bail!("Decoded image dimensions conflict with its container");
    }
    let decoded_dimensions = (decoded.width(), decoded.height());
    let declared_dimensions = source.declared_width.zip(source.declared_height);
    if declared_dimensions.is_some_and(|declared| {
        declared != (raw_width, raw_height) && declared != decoded_dimensions
    }) {
        anyhow::bail!("Downloaded image does not match its declared dimensions");
    }
    let (normalized, normalized_width, normalized_height) = encode_normalized_png(decoded)?;
    let result = (|| -> Result<ValidatedImageFile> {
        let mut cursor = Cursor::new(normalized.as_slice());
        write_bounded_file(&mut cursor, normalized_path, normalized.len() as u64)?;
        let validated = validate_normalized_image(normalized_path)?;
        if validated.width != normalized_width || validated.height != normalized_height {
            anyhow::bail!("Normalized PNG dimensions changed during write");
        }
        Ok(validated)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(normalized_path);
    }
    result
}

fn parse_content_length(headers: &reqwest::header::HeaderMap) -> Result<u64> {
    let mut values = headers.get_all(reqwest::header::CONTENT_LENGTH).iter();
    let value = values
        .next()
        .ok_or_else(|| anyhow::anyhow!("media response has unknown Content-Length"))?;
    if values.next().is_some() {
        anyhow::bail!("media response has multiple Content-Length values");
    }

    let value = value
        .to_str()
        .map_err(|_| anyhow::anyhow!("media response has malformed Content-Length"))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        anyhow::bail!("media response has malformed Content-Length");
    }

    let content_length = value
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("media response has malformed Content-Length"))?;
    if content_length > MAX_MEDIA_BYTES {
        anyhow::bail!("media response exceeds {} byte limit", MAX_MEDIA_BYTES);
    }
    Ok(content_length)
}

fn copy_bounded<R: std::io::Read, W: std::io::Write>(
    reader: &mut R,
    writer: &mut W,
    content_length: u64,
) -> Result<u64> {
    if content_length > MAX_MEDIA_BYTES {
        anyhow::bail!("media response exceeds {} byte limit", MAX_MEDIA_BYTES);
    }

    let mut copied = 0u64;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        if copied == content_length {
            // Probe one byte after the declared body. This keeps the output at
            // or below the cap while detecting an overlong/malformed stream.
            let probe = reader.read(&mut buffer[..1])?;
            if probe != 0 {
                anyhow::bail!("media response exceeds declared Content-Length");
            }
            return Ok(copied);
        }

        let remaining = content_length - copied;
        let read_len = remaining.min(buffer.len() as u64) as usize;
        let read = reader.read(&mut buffer[..read_len])?;
        if read == 0 {
            anyhow::bail!("media response ended before declared Content-Length");
        }
        writer.write_all(&buffer[..read])?;
        copied += read as u64;
    }
}

fn write_bounded_file<R: std::io::Read>(
    reader: &mut R,
    path: &Path,
    content_length: u64,
) -> Result<u64> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .mode(0o600);
    let mut file = options.open(path)?;
    #[cfg(unix)]
    let file_identity = (file.metadata()?.dev(), file.metadata()?.ino());
    let result = copy_bounded(reader, &mut file, content_length);
    #[cfg(unix)]
    let path_matches_file = std::fs::symlink_metadata(path)
        .map(|metadata| {
            metadata.file_type().is_file() && (metadata.dev(), metadata.ino()) == file_identity
        })
        .unwrap_or(false);
    #[cfg(not(unix))]
    let path_matches_file = path.is_file();
    if result.is_err() || !path_matches_file {
        drop(file);
        if path_matches_file {
            let _ = std::fs::remove_file(path);
        }
        if result.is_ok() {
            anyhow::bail!("media output path changed during download");
        }
    }
    result
}

fn download_media_file_without_directory_creation(
    creds: Option<&KakaoCredentials>,
    url: &str,
    path: &Path,
) -> Result<u64> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()?;

    // Validate URL domain before sending credentials
    let parsed_url = safe_kakao_media_url(url)?;

    let mut request = client.get(parsed_url);
    if let Some(creds) = creds {
        let a_header = if creds.a_header.is_empty() {
            format!("mac/{}/ko", creds.app_version)
        } else {
            creds.a_header.clone()
        };
        let user_agent = if creds.user_agent.is_empty() {
            format!("KT/{} Mc/10.15.7 ko", creds.app_version)
        } else {
            creds.user_agent.clone()
        };
        request = request
            .header("A", a_header)
            .header("User-Agent", user_agent)
            .header(
                "Authorization",
                format!("{}-{}", creds.oauth_token, creds.device_uuid),
            );
    }
    let mut response = request.send()?;

    if !response.status().is_success() {
        anyhow::bail!("HTTP {}", response.status());
    }

    let content_length = parse_content_length(response.headers())?;
    write_bounded_file(&mut response, path, content_length)
}

/// Download a media file from KakaoTalk CDN.
pub fn download_media_file(creds: &KakaoCredentials, url: &str, path: &Path) -> Result<u64> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    download_media_file_without_directory_creation(Some(creds), url, path)
}

/// Download into an already validated directory without creating ancestors.
pub fn download_media_file_into_existing_directory(
    creds: &KakaoCredentials,
    url: &str,
    path: &Path,
) -> Result<u64> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Media output has no parent directory"))?;
    if !parent.is_dir() {
        anyhow::bail!("Media output directory does not exist");
    }
    download_media_file_without_directory_creation(Some(creds), url, path)
}

/// Download a signed Kakao CDN URL into an already validated directory
/// without attaching account credentials or creating ancestors.
pub fn download_public_media_file_into_existing_directory(url: &str, path: &Path) -> Result<u64> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Media output has no parent directory"))?;
    if !parent.is_dir() {
        anyhow::bail!("Media output directory does not exist");
    }
    download_media_file_without_directory_creation(None, url, path)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kakao_attachment_key() {
        let result = parse_attachment_url(r#"{"k":"abc/photo.jpg"}"#, 2);
        assert_eq!(
            result,
            Some((
                "https://dn-m.talk.kakao.com/talkm/abc/photo.jpg".into(),
                "photo.jpg".into()
            ))
        );
    }

    #[test]
    fn rejects_invalid_attachment_json() {
        assert!(parse_attachment_url("not-json", 2).is_none());
    }

    #[test]
    fn strict_single_photo_parser_accepts_the_local_db_shape() {
        let sources = parse_image_download_sources(
            r#"{
                "url":"https://talk.kakaocdn.net/dn/photo.jpg?token=signed",
                "k":"safe/photo.jpg",
                "s":1234,
                "w":640,
                "h":480,
                "mt":"image/jpg"
            }"#,
            2,
        )
        .expect("valid photo attachment");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].declared_size, Some(1234));
        assert_eq!(sources[0].declared_width, Some(640));
        assert_eq!(sources[0].declared_height, Some(480));
        assert_eq!(sources[0].expected_media_type.as_deref(), Some("jpeg"));
        assert!(!sources[0].requires_credentials);
        assert!(sources[0].url.starts_with("https://talk.kakaocdn.net/"));

        let key_only = parse_image_download_sources(
            r#"{"k":"safe/photo.jpg","s":1234,"w":640,"h":480,"mt":"image/jpg"}"#,
            2,
        )
        .expect("valid keyed photo attachment");
        assert!(key_only[0].requires_credentials);
    }

    #[test]
    fn strict_photo_parser_rejects_unsafe_or_inconsistent_metadata() {
        assert!(parse_image_download_sources(
            r#"{"url":"https://example.com/photo.jpg","s":1,"w":1,"h":1}"#,
            2,
        )
        .is_err());
        assert!(
            parse_image_download_sources(r#"{"k":"../photo.jpg","s":1,"w":1,"h":1}"#, 2,).is_err()
        );
        assert!(
            parse_image_download_sources(r#"{"k":"safe/photo.jpg","s":0,"w":1,"h":1}"#, 2,)
                .is_err()
        );
        assert!(parse_image_download_sources(
            r#"{"k":"safe/photo.jpg","s":1,"w":1,"h":1,"mt":"image/png"}"#,
            2,
        )
        .is_err());
    }

    #[test]
    fn strict_emoticon_parser_uses_only_the_safe_item_cdn_path() {
        let sources = parse_image_download_sources(
            r#"{"path":"12345.emot_001.png","type":"png","width":360,"height":360}"#,
            14,
        )
        .expect("valid static emoticon attachment");
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].url,
            "https://item.kakaocdn.net/dw/12345.emot_001.png"
        );
        assert_eq!(sources[0].expected_media_type.as_deref(), Some("png"));
        assert!(!sources[0].requires_credentials);
        assert!(
            parse_image_download_sources(r#"{"path":"../secret.png","type":"png"}"#, 14,).is_err()
        );
        assert!(parse_image_download_sources(
            r#"{"path":"safe.png","url":"https://item.kakaocdn.net/dw/safe.png","type":"png"}"#,
            14,
        )
        .is_err());
    }

    #[test]
    fn strict_multi_photo_parser_preserves_order_and_exact_shapes() {
        let attachment = r#"{
            "kl":["safe/one.jpg","safe/two.png"],
            "imageUrls":[
                "https://talk.kakaocdn.net/dn/one.jpg?token=1",
                "https://talk.kakaocdn.net/dn/two.png?token=2"
            ],
            "wl":[640,800],
            "hl":[480,600],
            "sl":[1000,2000],
            "mtl":["jpg","png"]
        }"#;
        let sources =
            parse_image_download_sources(attachment, 27).expect("valid multi-photo attachment");
        assert_eq!(sources.len(), 2);
        assert!(sources[0].url.contains("one.jpg"));
        assert!(sources[1].url.contains("two.png"));
        assert_eq!(sources[0].expected_media_type.as_deref(), Some("jpeg"));
        assert_eq!(sources[1].expected_media_type.as_deref(), Some("png"));
        assert_eq!(sources[0].declared_size, Some(1000));
        assert_eq!(sources[1].declared_width, Some(800));
        assert!(sources.iter().all(|source| !source.requires_credentials));
    }

    #[test]
    fn strict_multi_photo_parser_rejects_truncation_ambiguity_and_duplicates() {
        let wrong_shape = r#"{
            "kl":["safe/one.jpg","safe/two.jpg"],
            "wl":[640],"hl":[480,480],"sl":[1000,1000]
        }"#;
        assert!(parse_image_download_sources(wrong_shape, 27).is_err());

        let duplicate = r#"{
            "kl":["safe/one.jpg","safe/one.jpg"],
            "wl":[640,640],"hl":[480,480],"sl":[1000,1000]
        }"#;
        assert!(parse_image_download_sources(duplicate, 27).is_err());

        let scalar_ambiguity = r#"{
            "url":"https://talk.kakaocdn.net/dn/one.jpg",
            "kl":["safe/one.jpg","safe/two.jpg"],
            "wl":[640,640],"hl":[480,480],"sl":[1000,1000]
        }"#;
        assert!(parse_image_download_sources(scalar_ambiguity, 27).is_err());

        let eleven = (0..11)
            .map(|index| format!("safe/{index}.jpg"))
            .collect::<Vec<_>>();
        let value = serde_json::json!({
            "kl": eleven,
            "wl": vec![1; 11],
            "hl": vec![1; 11],
            "sl": vec![1; 11],
        });
        assert!(parse_image_download_sources(&value.to_string(), 27).is_err());
    }

    fn static_gif() -> Vec<u8> {
        let mut data = b"GIF89a\x02\x00\x03\x00\x00\x00\x00".to_vec();
        data.extend_from_slice(b"\x2c\x00\x00\x00\x00\x02\x00\x03\x00\x00\x02\x01\x00\x00\x3b");
        data
    }

    fn encoded_test_jpeg(width: u32, height: u32) -> Vec<u8> {
        let original = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_pixel(
            width,
            height,
            image::Rgb([12, 34, 56]),
        ));
        let mut encoded = Cursor::new(Vec::new());
        original
            .write_to(&mut encoded, image::ImageFormat::Jpeg)
            .unwrap();
        let encoded = encoded.into_inner();
        assert!(encoded.starts_with(&JPEG_START_OF_IMAGE));
        assert!(encoded.ends_with(&JPEG_END_OF_IMAGE));
        encoded
    }

    fn encoded_test_png(width: u32, height: u32) -> Vec<u8> {
        let original = image::DynamicImage::ImageRgba8(image::ImageBuffer::from_pixel(
            width,
            height,
            image::Rgba([12, 34, 56, 255]),
        ));
        let mut encoded = Cursor::new(Vec::new());
        original
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let encoded = encoded.into_inner();
        assert!(encoded.starts_with(b"\x89PNG\r\n\x1a\n"));
        encoded
    }

    #[cfg(unix)]
    fn write_private_test_image(path: &Path, data: &[u8]) {
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        std::io::Write::write_all(&mut file, data).unwrap();
    }

    fn test_image_source(
        declared_size: u64,
        width: u32,
        height: u32,
        media_type: &str,
    ) -> ImageDownloadSource {
        ImageDownloadSource {
            url: format!("https://talk.kakaocdn.net/dn/source.{media_type}"),
            requires_credentials: false,
            declared_size: Some(declared_size),
            declared_width: Some(width),
            declared_height: Some(height),
            expected_media_type: Some(media_type.to_string()),
        }
    }

    fn exact_sef_trailer(jpeg: &[u8]) -> Vec<u8> {
        assert!(jpeg.ends_with(&JPEG_END_OF_IMAGE));
        let mut data = jpeg.to_vec();
        let records = [
            (
                SEF_SAMSUNG_CAPTURE_INFO,
                b"Samsung_Capture_Info".as_slice(),
                b"one".as_slice(),
            ),
            (
                SEF_CAPTURED_APP_INFO,
                b"Captured_App_Info".as_slice(),
                b"second".as_slice(),
            ),
        ];
        let mut record_sizes = Vec::with_capacity(records.len());
        for (data_type, name, payload) in records {
            let start = data.len();
            data.extend_from_slice(&0u16.to_le_bytes());
            data.extend_from_slice(&data_type.to_le_bytes());
            data.extend_from_slice(&(name.len() as u32).to_le_bytes());
            data.extend_from_slice(name);
            data.extend_from_slice(payload);
            record_sizes.push(data.len() - start);
        }
        let tail_start = data.len();
        data.extend_from_slice(b"SEFH");
        data.extend_from_slice(&107u32.to_le_bytes());
        data.extend_from_slice(&(records.len() as u32).to_le_bytes());
        let total_payload_size = record_sizes.iter().sum::<usize>();
        let mut consumed = 0usize;
        for (index, ((data_type, _, _), size)) in records.iter().zip(&record_sizes).enumerate() {
            data.extend_from_slice(&0u16.to_le_bytes());
            data.extend_from_slice(&data_type.to_le_bytes());
            data.extend_from_slice(&((total_payload_size - consumed) as u32).to_le_bytes());
            data.extend_from_slice(&(*size as u32).to_le_bytes());
            consumed += size;
            assert!(index < MAX_SEF_SDR_COUNT);
        }
        let tail_offset = data.len() - tail_start;
        data.extend_from_slice(&(tail_offset as u32).to_le_bytes());
        data.extend_from_slice(b"SEFT");
        data
    }

    #[test]
    fn image_magic_validation_accepts_static_gif_and_rejects_animation() {
        let data = static_gif();
        assert_eq!(image_metadata(&data).unwrap(), ("gif", 2, 3));

        let mut animated = data[..data.len() - 1].to_vec();
        animated.extend_from_slice(b"\x2c\x00\x00\x00\x00\x02\x00\x03\x00\x00\x02\x01\x00\x00\x3b");
        assert!(image_metadata(&animated).is_err());
    }

    #[test]
    fn image_magic_validation_rejects_truncated_png() {
        let mut truncated = b"\x89PNG\r\n\x1a\n".to_vec();
        truncated.extend_from_slice(&13u32.to_be_bytes());
        truncated.extend_from_slice(b"IHDR");
        truncated.extend_from_slice(&2u32.to_be_bytes());
        truncated.extend_from_slice(&3u32.to_be_bytes());
        assert!(image_metadata(&truncated).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn jpeg_missing_eoi_is_rejected_without_mutating_source() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.download");
        let normalized_path = directory.path().join("normalized.part");
        let mut original_body = encoded_test_jpeg(1_200, 800);
        original_body.truncate(original_body.len() - JPEG_END_OF_IMAGE.len());
        write_private_test_image(&source_path, &original_body);
        let source = test_image_source(original_body.len() as u64, 1_200, 800, "jpeg");

        let error = normalize_downloaded_image(&source_path, &normalized_path, 2, &source)
            .expect_err("JPEG EOI is never synthesized");

        assert_eq!(error.to_string(), "JPEG terminal marker is invalid");
        assert_eq!(std::fs::read(&source_path).unwrap(), original_body);
        assert!(!normalized_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn declared_size_mismatch_is_rejected_before_jpeg_terminal_validation() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.download");
        let normalized_path = directory.path().join("normalized.part");
        let mut original_body = encoded_test_jpeg(3, 2);
        original_body.truncate(original_body.len() - JPEG_END_OF_IMAGE.len());
        write_private_test_image(&source_path, &original_body);
        let source = test_image_source(original_body.len() as u64 + 2, 3, 2, "jpeg");

        let error = normalize_downloaded_image(&source_path, &normalized_path, 2, &source)
            .expect_err("the original body length must be attested first");

        assert_eq!(
            error.to_string(),
            "Downloaded image does not match its declared size"
        );
        assert_eq!(std::fs::read(&source_path).unwrap(), original_body);
        assert!(!normalized_path.exists());
    }

    #[test]
    fn complete_jpeg_with_eoi_is_unchanged() {
        let encoded = encoded_test_jpeg(3, 2);

        let decode_data =
            attested_image_bytes_for_decode(&encoded, Some(encoded.len() as u64), Some("jpeg"))
                .unwrap();

        assert_eq!(decode_data, encoded);
        assert_eq!(image_metadata(decode_data).unwrap(), ("jpeg", 3, 2));
        let (decoded, raw_width, raw_height) = decode_image(decode_data, "jpeg").unwrap();
        assert_eq!((raw_width, raw_height), (3, 2));
        assert_eq!((decoded.width(), decoded.height()), (3, 2));
    }

    #[test]
    fn missing_eoi_is_rejected_for_every_attachment_metadata_shape() {
        let mut original_body = encoded_test_jpeg(3, 2);
        original_body.truncate(original_body.len() - JPEG_END_OF_IMAGE.len());
        let original_size = original_body.len() as u64;

        for (declared_size, expected_media_type) in [
            (None, Some("jpeg")),
            (Some(original_size), None),
            (Some(original_size), Some("png")),
            (Some(original_size), Some("jpeg")),
        ] {
            let error =
                attested_image_bytes_for_decode(&original_body, declared_size, expected_media_type)
                    .expect_err("missing EOI must never be synthesized");
            assert_eq!(error.to_string(), "JPEG terminal marker is invalid");
        }
    }

    #[test]
    fn exact_sef_trailer_is_stripped_only_after_gapless_reference_validation() {
        let jpeg = encoded_test_jpeg(3, 2);
        let sef = exact_sef_trailer(&jpeg);

        let prefix = jpeg_prefix_with_exact_sef_trailer(&sef).unwrap();

        assert_eq!(prefix, jpeg);
        assert_eq!(image_metadata(prefix).unwrap(), ("jpeg", 3, 2));
        strict_decode_attested_jpeg(prefix).unwrap();
        assert!(attested_image_bytes_for_decode(&sef, None, Some("jpeg")).is_err());
        assert!(
            attested_image_bytes_for_decode(&sef, Some(sef.len() as u64), Some("png")).is_err()
        );

        let mut bad_signature = sef.clone();
        let last = bad_signature.len() - 1;
        bad_signature[last] ^= 1;
        assert!(jpeg_prefix_with_exact_sef_trailer(&bad_signature).is_none());

        let mut unknown_type = sef.clone();
        let tail_offset =
            little_endian_u32(&unknown_type, unknown_type.len() - 8).unwrap() as usize;
        let tail_start = unknown_type.len() - (tail_offset + 8);
        let first_type_offset = tail_start + SEF_TAIL_HEADER_LENGTH + 2;
        unknown_type[first_type_offset..first_type_offset + 2]
            .copy_from_slice(&0x9999u16.to_le_bytes());
        assert!(jpeg_prefix_with_exact_sef_trailer(&unknown_type).is_none());

        let mut bad_record_name = sef.clone();
        let first_record_start = jpeg.len();
        bad_record_name[first_record_start + 8] ^= 1;
        assert!(jpeg_prefix_with_exact_sef_trailer(&bad_record_name).is_none());

        let mut gap = sef.clone();
        let tail_offset = little_endian_u32(&gap, gap.len() - 8).unwrap() as usize;
        let tail_start = gap.len() - (tail_offset + 8);
        let first_size_offset = tail_start + SEF_TAIL_HEADER_LENGTH + 8;
        let first_size = little_endian_u32(&gap, first_size_offset).unwrap();
        gap[first_size_offset..first_size_offset + 4]
            .copy_from_slice(&(first_size - 1).to_le_bytes());
        assert!(jpeg_prefix_with_exact_sef_trailer(&gap).is_none());

        let mut overlap = sef;
        let tail_offset = little_endian_u32(&overlap, overlap.len() - 8).unwrap() as usize;
        let tail_start = overlap.len() - (tail_offset + 8);
        let second_negative_offset = tail_start + SEF_TAIL_HEADER_LENGTH + SEF_SDR_LENGTH + 4;
        let value = little_endian_u32(&overlap, second_negative_offset).unwrap();
        overlap[second_negative_offset..second_negative_offset + 4]
            .copy_from_slice(&(value + 1).to_le_bytes());
        assert!(jpeg_prefix_with_exact_sef_trailer(&overlap).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn complete_jpeg_with_exact_sef_trailer_normalizes_without_mutating_source() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.download");
        let normalized_path = directory.path().join("normalized.part");
        let original_body = exact_sef_trailer(&encoded_test_jpeg(1_200, 800));
        write_private_test_image(&source_path, &original_body);
        let source = test_image_source(original_body.len() as u64, 1_200, 800, "jpeg");

        let manifest = normalize_downloaded_image(&source_path, &normalized_path, 2, &source)
            .expect("an exact SEF trailer should be excluded from JPEG decoding");

        assert_eq!((manifest.width, manifest.height), (1_024, 683));
        assert_eq!(manifest.media_type, "png");
        assert_eq!(std::fs::read(&source_path).unwrap(), original_body);
        assert_eq!(
            manifest,
            validate_normalized_image(&normalized_path).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn entropy_truncated_jpeg_is_rejected_before_any_decoder() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.download");
        let normalized_path = directory.path().join("normalized.part");
        let encoded = encoded_test_jpeg(1_200, 800);
        let truncated = encoded[..encoded.len() - 1_026].to_vec();
        assert!(truncated.starts_with(&JPEG_START_OF_IMAGE));
        assert!(!truncated.ends_with(&JPEG_END_OF_IMAGE));
        assert!(!truncated.ends_with(&[JPEG_END_OF_IMAGE[0]]));
        write_private_test_image(&source_path, &truncated);
        let source = test_image_source(truncated.len() as u64, 1_200, 800, "jpeg");

        let error = normalize_downloaded_image(&source_path, &normalized_path, 2, &source)
            .expect_err("missing EOI rejects all entropy-truncated inputs");

        assert_eq!(error.to_string(), "JPEG terminal marker is invalid");
        assert!(!normalized_path.exists());
        assert_eq!(std::fs::read(&source_path).unwrap(), truncated);
    }

    #[test]
    fn non_jpeg_missing_soi_and_invalid_jpeg_remain_rejected() {
        let truncated_png = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_pixel(
            2,
            2,
            image::Rgb([1, 2, 3]),
        ));
        let mut png_cursor = Cursor::new(Vec::new());
        truncated_png
            .write_to(&mut png_cursor, image::ImageFormat::Png)
            .unwrap();
        let mut truncated_png = png_cursor.into_inner();
        truncated_png.truncate(truncated_png.len() - 12);
        let png_decode = attested_image_bytes_for_decode(
            &truncated_png,
            Some(truncated_png.len() as u64),
            Some("png"),
        )
        .unwrap();
        assert!(image_metadata(png_decode).is_err());

        let encoded = encoded_test_jpeg(3, 2);
        let missing_soi_and_eoi =
            &encoded[JPEG_START_OF_IMAGE.len()..encoded.len() - JPEG_END_OF_IMAGE.len()];
        let missing_soi_decode = attested_image_bytes_for_decode(
            missing_soi_and_eoi,
            Some(missing_soi_and_eoi.len() as u64),
            Some("jpeg"),
        )
        .unwrap();
        assert!(image_metadata(missing_soi_decode).is_err());

        let invalid_jpeg = [0xff, 0xd8, 0x00, 0x01, 0x02, 0x03];
        assert!(attested_image_bytes_for_decode(
            &invalid_jpeg,
            Some(invalid_jpeg.len() as u64),
            Some("jpeg"),
        )
        .is_err());
    }

    #[test]
    fn missing_eoi_at_and_above_original_cap_is_rejected() {
        let mut at_cap = vec![0u8; MAX_IMAGE_BYTES as usize];
        at_cap[..JPEG_START_OF_IMAGE.len()].copy_from_slice(&JPEG_START_OF_IMAGE);

        assert!(
            attested_image_bytes_for_decode(&at_cap, Some(MAX_IMAGE_BYTES), Some("jpeg")).is_err()
        );

        let mut over_cap = at_cap;
        over_cap.push(0);
        assert!(attested_image_bytes_for_decode(
            &over_cap,
            Some(MAX_IMAGE_BYTES + 1),
            Some("jpeg")
        )
        .is_err());
    }

    #[test]
    fn arbitrary_trailing_data_and_partial_eoi_are_rejected() {
        let mut trailing_data = encoded_test_jpeg(3, 2);
        trailing_data.extend_from_slice(b"not-an-eoi-omission");
        assert!(attested_image_bytes_for_decode(
            &trailing_data,
            Some(trailing_data.len() as u64),
            Some("jpeg")
        )
        .is_err());

        let mut partial_marker = encoded_test_jpeg(3, 2);
        partial_marker.pop();
        assert_eq!(partial_marker.last(), Some(&JPEG_END_OF_IMAGE[0]));
        assert!(attested_image_bytes_for_decode(
            &partial_marker,
            Some(partial_marker.len() as u64),
            Some("jpeg")
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn full_decode_normalizes_and_resizes_to_metadata_free_png() {
        use std::os::unix::fs::OpenOptionsExt;

        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.download");
        let normalized_path = directory.path().join("normalized.part");
        let original = image::DynamicImage::ImageRgba8(image::ImageBuffer::from_pixel(
            2_000,
            1_000,
            image::Rgba([12, 34, 56, 255]),
        ));
        let mut encoded = Cursor::new(Vec::new());
        original
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let encoded = encoded.into_inner();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&source_path)
            .unwrap();
        std::io::Write::write_all(&mut file, &encoded).unwrap();
        drop(file);
        let source = ImageDownloadSource {
            url: "https://talk.kakaocdn.net/dn/source.png".to_string(),
            requires_credentials: false,
            declared_size: Some(encoded.len() as u64),
            declared_width: Some(2_000),
            declared_height: Some(1_000),
            expected_media_type: Some("png".to_string()),
        };
        let manifest = normalize_downloaded_image(&source_path, &normalized_path, 2, &source)
            .expect("normalization succeeds");
        assert_eq!(manifest.media_type, "png");
        assert_eq!((manifest.width, manifest.height), (1_024, 512));
        assert!(manifest.size <= MAX_IMAGE_BYTES);
        assert_eq!(
            manifest,
            validate_normalized_image(&normalized_path).unwrap()
        );
    }

    #[test]
    fn attachment_accepts_kakao_jpg_label_for_still_raster_payloads() {
        assert!(attachment_media_type_accepts_payload("jpeg", "png"));
        assert!(attachment_media_type_accepts_payload("jpeg", "webp"));
        assert!(attachment_media_type_accepts_payload("png", "jpeg"));
        assert!(attachment_media_type_accepts_payload("jpeg", "jpeg"));
        assert!(!attachment_media_type_accepts_payload("jpeg", "gif"));
        assert!(!attachment_media_type_accepts_payload("gif", "png"));
    }

    #[cfg(unix)]
    #[test]
    fn kakao_jpg_attachment_normalizes_png_payload() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.download");
        let normalized_path = directory.path().join("normalized.part");
        let encoded = encoded_test_png(2_000, 1_000);
        write_private_test_image(&source_path, &encoded);
        let source = ImageDownloadSource {
            url: "https://talk.kakaocdn.net/dna/photo.jpg".to_string(),
            requires_credentials: false,
            declared_size: Some(encoded.len() as u64),
            declared_width: Some(2_000),
            declared_height: Some(1_000),
            expected_media_type: Some("jpeg".to_string()),
        };
        let manifest = normalize_downloaded_image(&source_path, &normalized_path, 2, &source)
            .expect("Kakao jpg-labeled PNG payload should normalize");
        assert_eq!(manifest.media_type, "png");
        assert_eq!((manifest.width, manifest.height), (1_024, 512));
    }

    #[cfg(unix)]
    #[test]
    fn kakao_jpg_attachment_still_rejects_png_with_wrong_dimensions() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.download");
        let normalized_path = directory.path().join("normalized.part");
        let encoded = encoded_test_png(32, 24);
        write_private_test_image(&source_path, &encoded);
        let source = ImageDownloadSource {
            url: "https://talk.kakaocdn.net/dna/photo.jpg".to_string(),
            requires_credentials: false,
            declared_size: Some(encoded.len() as u64),
            declared_width: Some(1440),
            declared_height: Some(1440),
            expected_media_type: Some("jpeg".to_string()),
        };
        let error = normalize_downloaded_image(&source_path, &normalized_path, 2, &source)
            .expect_err("dimension mismatch must stay fatal");
        assert_eq!(
            error.to_string(),
            "Downloaded image does not match its declared dimensions"
        );
    }

    #[test]
    fn decoded_pixel_cap_rejects_decompression_bomb_dimensions() {
        assert!(checked_dimensions(10_000, 4_000).is_ok());
        assert!(checked_dimensions(10_000, 4_001).is_err());
    }

    #[test]
    fn bounded_copy_accepts_exact_media_cap() {
        let mut source = std::io::Cursor::new(vec![0xA5; MAX_MEDIA_BYTES as usize]);
        let mut output = Vec::new();

        let copied = copy_bounded(&mut source, &mut output, MAX_MEDIA_BYTES).unwrap();

        assert_eq!(copied, MAX_MEDIA_BYTES);
        assert_eq!(output.len(), MAX_MEDIA_BYTES as usize);
    }

    #[test]
    fn bounded_copy_rejects_cap_plus_one_without_extra_write() {
        let mut source = std::io::Cursor::new(vec![0x5A; MAX_MEDIA_BYTES as usize + 1]);
        let mut output = Vec::new();

        let result = copy_bounded(&mut source, &mut output, MAX_MEDIA_BYTES);

        assert!(result.is_err());
        assert_eq!(output.len(), MAX_MEDIA_BYTES as usize);
    }

    #[test]
    fn bounded_copy_removes_partial_output_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.bin");
        let mut source = std::io::Cursor::new(vec![0x3C; MAX_MEDIA_BYTES as usize + 1]);

        let result = write_bounded_file(&mut source, &path, MAX_MEDIA_BYTES);

        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn content_length_is_required_and_bounded() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert!(parse_content_length(&headers).is_err());

        headers.insert(
            reqwest::header::CONTENT_LENGTH,
            reqwest::header::HeaderValue::from_static("5242881"),
        );
        assert!(parse_content_length(&headers).is_err());

        headers.insert(
            reqwest::header::CONTENT_LENGTH,
            reqwest::header::HeaderValue::from_static("5242880"),
        );
        assert_eq!(parse_content_length(&headers).unwrap(), MAX_MEDIA_BYTES);
    }

    #[test]
    fn rejects_non_kakao_download_domains() {
        let creds = KakaoCredentials {
            oauth_token: String::new(),
            device_uuid: String::new(),
            device_name: String::new(),
            a_header: String::new(),
            user_agent: String::new(),
            app_version: String::new(),
            user_id: 0,
            refresh_token: None,
            email: None,
            rest_token: None,
        };
        let path = std::env::temp_dir().join("openkakao-media-test.bin");
        let result = download_media_file(&creds, "https://example.com/image.jpg", &path);
        assert!(result.is_err());
        let _ = std::fs::remove_file(path);
    }
}
