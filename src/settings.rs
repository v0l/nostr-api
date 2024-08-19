use nostr_sdk::Url;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Listen address for web API
    pub listen: Option<String>,

    /// List of relays to connect to for fetching data
    pub relays: Vec<Url>
}