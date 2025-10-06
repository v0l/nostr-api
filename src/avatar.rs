use rocket::Request;
use rocket::fs::NamedFile;
use rocket::http::Status;
use rocket::response::Responder;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub enum AvatarResponse {
    File(NamedFile),
    NotFound,
}

impl<'r> Responder<'r, 'static> for AvatarResponse {
    fn respond_to(self, req: &'r Request<'_>) -> rocket::response::Result<'static> {
        match self {
            AvatarResponse::File(f) => f.respond_to(req),
            AvatarResponse::NotFound => Err(Status::NotFound),
        }
    }
}

/// Robohash avatar endpoint
///
/// Available sets: `cyberpunks` / `robots` / `zombies`
#[get("/avatar/<set>/<value>")]
pub async fn get_avatar(set: &str, value: &str) -> AvatarResponse {
    // Remove file extension from value
    let value_no_ext = Path::new(&value)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&value);

    // Hash the value
    let mut hasher = Sha256::new();
    hasher.update(value_no_ext.as_bytes());
    let hash = hasher.finalize();

    // Convert first 4 bytes to i32
    let hash_int = i32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]);

    // Build directory path
    let dir = PathBuf::from("avatars").join(&set);

    // Get list of .webp files
    let file_list: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("webp"))
            .collect(),
        Err(_) => return AvatarResponse::NotFound,
    };

    if file_list.is_empty() {
        return AvatarResponse::NotFound;
    }

    // Pick image based on hash
    let idx = hash_int.abs() as usize % file_list.len();
    let pick_img = &file_list[idx];

    match NamedFile::open(pick_img).await {
        Ok(file) => AvatarResponse::File(file),
        Err(_) => AvatarResponse::NotFound,
    }
}

pub fn routes() -> Vec<rocket::Route> {
    routes![get_avatar]
}
