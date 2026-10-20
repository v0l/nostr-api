use nostr_sdk::Url;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Listen address for web API
    pub listen: Option<String>,

    /// List of relays to connect to for fetching data
    pub relays: Vec<Url>,

    #[serde(default)]
    pub translate: Option<TranslateSettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslateSettings {
    pub api_base: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
}
#[cfg(test)]
mod tests {
    use super::*;
    use config::{Config, File, FileFormat};

    #[test]
    fn translate_section_is_optional() {
        let cfg = Config::builder()
            .add_source(File::from_str("relays: []", FileFormat::Yaml))
            .build()
            .unwrap();
        let s: Settings = cfg.try_deserialize().unwrap();
        assert!(s.translate.is_none());
    }

    #[test]
    fn translate_section_parses_from_yaml() {
        let yaml = "relays: []\ntranslate:\n  api_base: http://x/v1\n  model: m\n";
        let cfg = Config::builder()
            .add_source(File::from_str(yaml, FileFormat::Yaml))
            .build()
            .unwrap();
        let s: Settings = cfg.try_deserialize().unwrap();
        let t = s.translate.unwrap();
        assert_eq!((t.api_base.as_str(), t.model.as_str()), ("http://x/v1", "m"));
        assert!(t.api_key.is_none());
    }
}
