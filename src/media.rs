use std::path::Path;

use anyhow::Result;

use crate::model::KakaoCredentials;

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

const MAX_MEDIA_BYTES: u64 = 5 * 1024 * 1024;

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
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
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

/// Download a media file from KakaoTalk CDN.
pub fn download_media_file(creds: &KakaoCredentials, url: &str, path: &Path) -> Result<u64> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

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

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()?;

    // Validate URL domain before sending credentials
    let parsed_url = reqwest::Url::parse(url)?;
    let host = parsed_url.host_str().unwrap_or("");
    if parsed_url.scheme() != "https"
        || parsed_url.username() != ""
        || parsed_url.password().is_some()
        || parsed_url.port().is_some_and(|port| port != 443)
        || (!host.ends_with(".kakao.com") && !host.ends_with(".kakaocdn.net"))
    {
        anyhow::bail!("Refusing unsafe media URL");
    }

    let mut response = client
        .get(url)
        .header("A", &a_header)
        .header("User-Agent", &user_agent)
        .header(
            "Authorization",
            format!("{}-{}", creds.oauth_token, creds.device_uuid),
        )
        .send()?;

    if !response.status().is_success() {
        anyhow::bail!("HTTP {}", response.status());
    }

    let content_length = parse_content_length(response.headers())?;
    write_bounded_file(&mut response, path, content_length)
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
