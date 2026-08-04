use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::ETAG;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Known avatar set names — used for the allowlist check and at-startup scanning.
pub const AVATAR_SETS: &[&str] = &["cyberpunks", "robots", "zombies"];

/// A single avatar image: its raw bytes plus a stable ETag derived from the
/// bytes themselves. Both are computed once at startup so requests never hit
/// disk and can be served with conditional caching.
pub struct AvatarImage {
    /// Original file name, used only for deterministic ordering.
    path: PathBuf,
    bytes: Arc<Vec<u8>>,
    etag: String,
}

/// Pre-built, sorted lists of `.webp` files + their in-memory bytes for each
/// avatar set, populated once at startup to avoid per-request blocking I/O.
#[derive(Clone)]
pub struct AvatarSets {
    sets: Arc<HashMap<String, Vec<Arc<AvatarImage>>>>,
}

impl AvatarSets {
    /// Scan the `avatars/` directory at startup, load every `.webp` file into
    /// memory and build the sorted image lists (ordered by file name).
    pub fn load() -> Self {
        let mut sets = HashMap::new();
        for &set in AVATAR_SETS {
            let dir = PathBuf::from("avatars").join(set);
            let mut images: Vec<Arc<AvatarImage>> = match std::fs::read_dir(&dir) {
                Ok(entries) => entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("webp"))
                    .filter_map(|p| {
                        std::fs::read(&p).ok().map(|bytes| {
                            let etag = format!("\"{}\"", hex_sha256(&bytes));
                            Arc::new(AvatarImage {
                                path: p,
                                bytes: Arc::new(bytes),
                                etag,
                            })
                        })
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            images.sort_by_key(|img| img.path.clone());
            sets.insert(set.to_string(), images);
        }
        Self {
            sets: Arc::new(sets),
        }
    }

    /// Return the sorted image list for `set`, or `None` if unknown / empty.
    pub fn images_for(&self, set: &str) -> Option<&Vec<Arc<AvatarImage>>> {
        self.sets
            .get(set)
            .filter(|files| !files.is_empty())
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub async fn get_avatar(
    State(avatar_sets): State<AvatarSets>,
    Path((set, value)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    // Validate set against allowlist to prevent path traversal
    let images = match avatar_sets.images_for(&set) {
        Some(f) => f,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

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

    // Pick image based on hash
    let idx = hash_int.unsigned_abs() as usize % images.len();
    let img = &images[idx];

    // If the client already has this exact revision, respond 304 (Not Modified)
    // instead of resending the (possibly large) image body.
    let etag = HeaderValue::from_str(&img.etag)
        .unwrap_or_else(|_| HeaderValue::from_static("\"\""));
    if headers
        .get("if-none-match")
        .map(|v| v == etag)
        .unwrap_or(false)
    {
        return StatusCode::NOT_MODIFIED
            .into_response();
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(ETAG, etag)
        .header(axum::http::header::CONTENT_TYPE, "image/webp")
        .body(Body::from(img.bytes.as_ref().clone()))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_avatar_sets_load_does_not_panic() {
        // Even if the avatars/ directory doesn't exist, load() should not panic.
        let sets = AvatarSets::load();
        // All sets should be present in the map (possibly empty).
        for &name in AVATAR_SETS {
            // images_for returns None for empty sets, Some for non-empty
            let _ = sets.images_for(name);
        }
    }

    #[test]
    fn test_avatar_sets_images_for_unknown_set_returns_none() {
        let sets = AvatarSets::load();
        assert!(sets.images_for("../../etc").is_none());
        assert!(sets.images_for("").is_none());
        assert!(sets.images_for("unknown_set").is_none());
    }

    #[test]
    fn test_avatar_sets_known_sets_allowlist() {
        // Ensure each entry in AVATAR_SETS is a valid known name (not a path component)
        for &name in AVATAR_SETS {
            assert!(!name.contains('/'));
            assert!(!name.contains(".."));
        }
    }

    #[test]
    fn test_hex_sha256_known_vector() {
        // SHA-256 of the empty string.
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_avatar_image_etag_is_deterministic() {
        let bytes = b"fake webp bytes";
        let a = AvatarImage {
            path: PathBuf::from("a.webp"),
            bytes: Arc::new(bytes.to_vec()),
            etag: format!("\"{}\"", hex_sha256(bytes)),
        };
        let b = AvatarImage {
            path: PathBuf::from("b.webp"),
            bytes: Arc::new(bytes.to_vec()),
            etag: format!("\"{}\"", hex_sha256(bytes)),
        };
        assert_eq!(a.etag, b.etag);
        assert!(a.etag.starts_with('"') && a.etag.ends_with('"'));
    }

    fn handler_router() -> (axum::Router, String) {
        // Use a set with a single trivial image so the hash always picks it and
        // the ETag is known.
        let bytes = vec![1u8, 2, 3, 4];
        let img = AvatarImage {
            path: PathBuf::from("one.webp"),
            etag: format!("\"{}\"", hex_sha256(&bytes)),
            bytes: Arc::new(bytes.clone()),
        };
        let etag = format!("\"{}\"", hex_sha256(&bytes));
        let mut sets = HashMap::new();
        sets.insert("cyberpunks".to_string(), vec![Arc::new(img)]);
        let state = AvatarSets {
            sets: Arc::new(sets),
        };
        let app = axum::Router::new()
            .route("/avatar/{set}/{value}", axum::routing::get(get_avatar))
            .with_state(state);
        (app, etag)
    }

    #[tokio::test]
    async fn test_get_avatar_returns_image_and_etag() {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let (app, _) = handler_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/avatar/cyberpunks/abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            resp.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "image/webp"
        );
        assert!(resp.headers().get(axum::http::header::ETAG).is_some());
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], &[1u8, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_get_avatar_not_modified_when_etag_matches() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (app, etag) = handler_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/avatar/cyberpunks/abc")
                    .header("if-none-match", &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn test_get_avatar_unknown_set_is_not_found() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (app, _) = handler_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/avatar/nope/abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }
}
