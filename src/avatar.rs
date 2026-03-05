use axum::body::Body;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub async fn get_avatar(Path((set, value)): Path<(String, String)>) -> Response {
    // Validate set against allowlist to prevent path traversal
    if !matches!(set.as_str(), "cyberpunks" | "robots" | "zombies") {
        return StatusCode::NOT_FOUND.into_response();
    }

    // Remove file extension from value
    let value_no_ext = PathBuf::from(&value)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&value)
        .to_string();

    // Hash the value
    let mut hasher = Sha256::new();
    hasher.update(value_no_ext.as_bytes());
    let hash = hasher.finalize();

    // Convert first 4 bytes to i32
    let hash_int = i32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]);

    // Build directory path
    let dir = PathBuf::from("avatars").join(&set);

    // Get list of .webp files
    let mut file_list: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("webp"))
            .collect(),
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    if file_list.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }

    // Sort for determinism across runs
    file_list.sort();

    // Pick image based on hash
    let idx = hash_int.unsigned_abs() as usize % file_list.len();
    let pick_img = &file_list[idx];

    match tokio::fs::read(pick_img).await {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "image/webp")
            .body(Body::from(bytes))
            .unwrap(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
