use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Known avatar set names — used for the allowlist check and at-startup scanning.
pub const AVATAR_SETS: &[&str] = &["cyberpunks", "robots", "zombies"];

/// Pre-built, sorted lists of `.webp` files for each avatar set.
/// Populated once at startup to avoid per-request blocking I/O.
#[derive(Clone)]
pub struct AvatarSets {
    sets: Arc<HashMap<String, Vec<PathBuf>>>,
}

impl AvatarSets {
    /// Scan the `avatars/` directory at startup and build the file lists.
    pub fn load() -> Self {
        let mut sets = HashMap::new();
        for &set in AVATAR_SETS {
            let dir = PathBuf::from("avatars").join(set);
            let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
                Ok(entries) => entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("webp"))
                    .collect(),
                Err(_) => Vec::new(),
            };
            files.sort();
            sets.insert(set.to_string(), files);
        }
        Self {
            sets: Arc::new(sets),
        }
    }

    /// Return the sorted file list for `set`, or `None` if unknown / empty.
    pub fn files_for(&self, set: &str) -> Option<&Vec<PathBuf>> {
        self.sets
            .get(set)
            .filter(|files| !files.is_empty())
    }
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
            // files_for returns None for empty sets, Some for non-empty
            let _ = sets.files_for(name);
        }
    }

    #[test]
    fn test_avatar_sets_files_for_unknown_set_returns_none() {
        let sets = AvatarSets::load();
        assert!(sets.files_for("../../etc").is_none());
        assert!(sets.files_for("").is_none());
        assert!(sets.files_for("unknown_set").is_none());
    }

    #[test]
    fn test_avatar_sets_known_sets_allowlist() {
        // Ensure each entry in AVATAR_SETS is a valid known name (not a path component)
        for &name in AVATAR_SETS {
            assert!(!name.contains('/'));
            assert!(!name.contains(".."));
        }
    }
}

pub async fn get_avatar(
    State(avatar_sets): State<AvatarSets>,
    Path((set, value)): Path<(String, String)>,
) -> Response {
    // Validate set against allowlist to prevent path traversal
    let files = match avatar_sets.files_for(&set) {
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
    let idx = hash_int.unsigned_abs() as usize % files.len();
    let pick_img = &files[idx];

    match tokio::fs::read(pick_img).await {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "image/webp")
            .body(Body::from(bytes))
            .unwrap(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
