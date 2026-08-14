use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::loco_helpers::{check_loco_status, loco_connect_with_auto_refresh};
use crate::media::{
    download_media_file, download_media_file_into_existing_directory,
    download_public_media_file_into_existing_directory, normalize_downloaded_image,
    parse_attachment_url, parse_image_download_sources, sanitize_filename,
    validate_normalized_image, ValidatedImageFile, MAX_IMAGE_BATCH_BYTES,
};
use crate::util::{get_bson_i32, get_bson_i64, get_bson_str, get_creds};

#[derive(Debug, Clone, Serialize)]
struct MediaManifestFile {
    index: usize,
    size: u64,
    sha256: String,
    media_type: String,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Serialize)]
struct MediaManifest {
    schema_version: u32,
    message_type: i32,
    expected_count: usize,
    total_bytes: u64,
    bundle_sha256: String,
    files: Vec<MediaManifestFile>,
}

fn canonical_manifest_files(files: &[MediaManifestFile]) -> Result<Vec<u8>> {
    let mut output = String::from("[");
    for (position, file) in files.iter().enumerate() {
        if position != 0 {
            output.push(',');
        }
        // Python's producer contract hashes json.dumps(..., sort_keys=True,
        // separators=(",", ":")); keep this exact lexicographic key order.
        output.push_str(&format!(
            "{{\"height\":{},\"index\":{},\"media_type\":{},\"sha256\":{},\"size\":{},\"width\":{}}}",
            file.height,
            file.index,
            serde_json::to_string(&file.media_type)?,
            serde_json::to_string(&file.sha256)?,
            file.size,
            file.width,
        ));
    }
    output.push(']');
    Ok(output.into_bytes())
}

fn private_existing_output_directory(path: &Path) -> Result<PathBuf> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| "Local media output directory must already exist")?;
    if !metadata.file_type().is_dir() {
        anyhow::bail!("Local media output path is not a real directory");
    }
    #[cfg(unix)]
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
        anyhow::bail!("Local media output directory must be private mode 0700");
    }
    let canonical = path.canonicalize()?;
    if canonical == Path::new("/") {
        anyhow::bail!("Refusing broad local media output directory");
    }
    Ok(canonical)
}

fn cleanup_created_files(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

fn opaque_image_filename(index: usize, media_type: &str) -> Result<String> {
    let extension = match media_type {
        "jpeg" => "jpg",
        "png" => "png",
        "webp" => "webp",
        "gif" => "gif",
        _ => anyhow::bail!("Unsupported validated image type"),
    };
    Ok(format!("image-{index:02}.{extension}"))
}

fn manifest_file(index: usize, image: ValidatedImageFile) -> MediaManifestFile {
    MediaManifestFile {
        index,
        size: image.size,
        sha256: image.sha256,
        media_type: image.media_type,
        width: image.width,
        height: image.height,
    }
}

fn cmd_download_local(
    chat_id: i64,
    log_id: i64,
    output_dir: Option<&str>,
    expected_author_id: Option<i64>,
    json: bool,
) -> Result<()> {
    let output_dir = output_dir
        .map(Path::new)
        .ok_or_else(|| anyhow::anyhow!("--local requires an explicit --output-dir"))?;
    let output_dir = private_existing_output_directory(output_dir)?;
    let reader = crate::local_db::LocalDbReader::open_no_mutation()?;
    let attachment = reader.exact_media_attachment(chat_id, log_id)?;
    let expected_author_id = expected_author_id
        .filter(|author_id| *author_id > 0)
        .ok_or_else(|| anyhow::anyhow!("--local requires a positive --expected-author-id"))?;
    if attachment.is_self || attachment.author_id != expected_author_id {
        anyhow::bail!("Exact local media author identity mismatch");
    }
    let sources = parse_image_download_sources(&attachment.attachment, attachment.message_type)?;
    let credentials = sources
        .iter()
        .any(|source| source.requires_credentials)
        .then(crate::auth_flow::resolve_base_credentials_noninteractive)
        .transpose()?;
    let attachment_sha256 = hex::encode(Sha256::digest(attachment.attachment.as_bytes()));

    let mut created_paths = Vec::with_capacity(sources.len() * 2);
    let mut paths = Vec::with_capacity(sources.len());
    let result = (|| -> Result<(Vec<MediaManifestFile>, u64)> {
        let mut files = Vec::with_capacity(sources.len());
        let mut total_bytes = 0u64;
        for (index, source) in sources.iter().enumerate() {
            let download_path = output_dir.join(format!(".image-{index:02}.download"));
            let normalized_path = output_dir.join(format!(".image-{index:02}.part"));
            if download_path.exists()
                || std::fs::symlink_metadata(&download_path).is_ok()
                || normalized_path.exists()
                || std::fs::symlink_metadata(&normalized_path).is_ok()
            {
                anyhow::bail!("Local media output file already exists");
            }
            if source.requires_credentials {
                let credentials = credentials
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Kakao media credentials are unavailable"))?;
                download_media_file_into_existing_directory(
                    credentials,
                    &source.url,
                    &download_path,
                )?;
            } else {
                download_public_media_file_into_existing_directory(&source.url, &download_path)?;
            }
            created_paths.push(download_path.clone());
            created_paths.push(normalized_path.clone());
            let image = normalize_downloaded_image(
                &download_path,
                &normalized_path,
                attachment.message_type,
                source,
            )?;
            std::fs::remove_file(&download_path)?;
            let final_path = output_dir.join(opaque_image_filename(index, &image.media_type)?);
            if final_path.exists() || std::fs::symlink_metadata(&final_path).is_ok() {
                anyhow::bail!("Local media output file already exists");
            }
            // A private same-filesystem hard link supplies no-replace
            // semantics. Unlinking the temporary name then atomically exposes
            // only the validated opaque extension while preserving the inode.
            std::fs::hard_link(&normalized_path, &final_path)?;
            created_paths.push(final_path.clone());
            std::fs::remove_file(&normalized_path)?;
            let final_image = validate_normalized_image(&final_path)?;
            if final_image != image {
                anyhow::bail!("Validated image changed during finalization");
            }
            paths.push(final_path);
            total_bytes = total_bytes
                .checked_add(image.size)
                .filter(|total| *total <= MAX_IMAGE_BATCH_BYTES)
                .ok_or_else(|| anyhow::anyhow!("Downloaded image batch exceeds its byte cap"))?;
            files.push(manifest_file(index, image));
        }
        Ok((files, total_bytes))
    })();
    let (files, total_bytes) = match result {
        Ok(result) => result,
        Err(error) => {
            cleanup_created_files(&created_paths);
            return Err(error);
        }
    };
    let bundle_sha256 = hex::encode(Sha256::digest(canonical_manifest_files(&files)?));
    let manifest = MediaManifest {
        schema_version: 1,
        message_type: attachment.message_type,
        expected_count: paths.len(),
        total_bytes,
        bundle_sha256,
        files,
    };
    let path_strings = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if json {
        crate::util::output_json(&serde_json::json!({
            "status": "ok",
            "chat_id": chat_id,
            "log_id": log_id,
            "message_type": attachment.message_type,
            "attachment_sha256": attachment_sha256,
            "path": path_strings[0],
            "paths": path_strings,
            "media_type": crate::util::message_type_label(attachment.message_type),
            "size": total_bytes,
            "media_manifest": manifest,
        }))?;
    } else {
        for path in &paths {
            println!("Saved: {}", path.display());
        }
    }
    Ok(())
}

pub fn cmd_download(
    chat_id: i64,
    log_id: i64,
    output_dir: Option<&str>,
    local: bool,
    expected_author_id: Option<i64>,
    json: bool,
) -> Result<()> {
    if local {
        return cmd_download_local(chat_id, log_id, output_dir, expected_author_id, json);
    }
    if expected_author_id.is_some() {
        anyhow::bail!("--expected-author-id requires --local");
    }
    let creds = get_creds()?;
    let out_dir = output_dir.unwrap_or("downloads");

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let mut client = crate::loco::client::LocoClient::new(creds.clone());
        eprintln!("Connecting via LOCO...");
        loco_connect_with_auto_refresh(&mut client).await?;

        // Get lastLogId via CHATONROOM (required as max for SYNCMSG)
        let room_info = client
            .send_command("CHATONROOM", bson::doc! { "chatId": chat_id })
            .await?;
        check_loco_status("CHATONROOM", &room_info)?;
        let last_log_id = room_info.body.get_i64("l").unwrap_or(0);

        // Scan via SYNCMSG pagination to find the target message.
        let mut cur = 0_i64;
        let mut target_doc: Option<bson::Document> = None;

        eprintln!("[download] Scanning for logId={}...", log_id);
        loop {
            let response = client
                .send_command(
                    "SYNCMSG",
                    bson::doc! {
                        "chatId": chat_id,
                        "cur": cur,
                        "cnt": 50_i32,
                        "max": last_log_id,
                    },
                )
                .await?;

            check_loco_status("SYNCMSG", &response)?;

            let chat_logs = response
                .body
                .get_array("chatLogs")
                .map(|a| a.to_vec())
                .unwrap_or_default();

            let is_ok = response.body.get_bool("isOK").unwrap_or(true);

            if chat_logs.is_empty() {
                break;
            }

            let mut max_in_batch = 0_i64;
            for log in &chat_logs {
                if let Some(doc) = log.as_document() {
                    let lid = get_bson_i64(doc, &["logId"]);
                    if lid > max_in_batch {
                        max_in_batch = lid;
                    }
                    if lid == log_id {
                        target_doc = Some(doc.clone());
                    }
                }
            }

            if target_doc.is_some() || is_ok || max_in_batch == 0 {
                break;
            }

            // Skip ahead if we've already passed the target
            if max_in_batch > log_id {
                break;
            }

            cur = max_in_batch;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let target_log = match &target_doc {
            Some(doc) => doc,
            None => {
                anyhow::bail!("Message logId={} not found in chat {}", log_id, chat_id);
            }
        };

        let msg_type = get_bson_i32(target_log, &["type"]);
        let attachment = get_bson_str(target_log, &["attachment"]);

        if attachment.is_empty() {
            anyhow::bail!("Message logId={} has no attachment", log_id);
        }

        match parse_attachment_url(&attachment, msg_type) {
            Some((url, filename)) => {
                let output_dir = Path::new(out_dir);
                let dir = if output_dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("bujamentor-db-media-"))
                {
                    output_dir.to_path_buf()
                } else {
                    output_dir.join(chat_id.to_string())
                };
                let save_name = format!("{}_{}", log_id, sanitize_filename(&filename));
                let save_path = dir.join(&save_name);

                eprintln!("Downloading media attachment");
                let bytes = download_media_file(&creds, &url, &save_path)?;
                if json {
                    crate::util::output_json(&serde_json::json!({
                        "status": "ok",
                        "path": save_path.display().to_string(),
                        "paths": [save_path.display().to_string()],
                        "media_type": crate::util::message_type_label(msg_type),
                        "size": bytes,
                    }))?;
                } else {
                    println!("Saved: {} ({} bytes)", save_path.display(), bytes);
                }
            }
            None => {
                anyhow::bail!("Cannot parse attachment for message logId={}", log_id);
            }
        }

        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_bundle_hash_uses_the_python_canonical_json_contract() {
        let files = vec![
            MediaManifestFile {
                index: 0,
                size: 123,
                sha256: "a".repeat(64),
                media_type: "jpeg".to_string(),
                width: 640,
                height: 480,
            },
            MediaManifestFile {
                index: 1,
                size: 456,
                sha256: "b".repeat(64),
                media_type: "png".to_string(),
                width: 800,
                height: 600,
            },
        ];
        let canonical = canonical_manifest_files(&files).expect("canonical JSON");
        assert_eq!(
            String::from_utf8(canonical.clone()).unwrap(),
            format!(
                "[{{\"height\":480,\"index\":0,\"media_type\":\"jpeg\",\"sha256\":\"{}\",\"size\":123,\"width\":640}},{{\"height\":600,\"index\":1,\"media_type\":\"png\",\"sha256\":\"{}\",\"size\":456,\"width\":800}}]",
                "a".repeat(64),
                "b".repeat(64),
            )
        );
        assert_eq!(hex::encode(Sha256::digest(canonical)).len(), 64);
    }

    #[test]
    fn normalized_output_names_are_opaque_supported_png_paths() {
        let name = opaque_image_filename(0, "png").expect("supported normalized type");
        assert_eq!(name, "image-00.png");
        assert!(!name.contains("sender"));
        assert!(!name.contains("photo.jpg"));
        assert!(opaque_image_filename(0, "unknown").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn local_output_directory_must_be_preexisting_private_and_not_a_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        let private = root.path().join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            private_existing_output_directory(&private).unwrap(),
            private.canonicalize().unwrap()
        );

        let public = root.path().join("public");
        std::fs::create_dir(&public).unwrap();
        std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_existing_output_directory(&public).is_err());

        let link = root.path().join("link");
        symlink(&private, &link).unwrap();
        assert!(private_existing_output_directory(&link).is_err());
        assert!(private_existing_output_directory(&root.path().join("missing")).is_err());
    }
}
