use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use moka::future::Cache;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkPreviewData {
    #[serde(rename = "ogTags")]
    pub og_tags: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

pub struct LinkPreviewCache {
    cache: Cache<String, Option<LinkPreviewData>>,
    empty_cache: Cache<String, bool>,
    client: reqwest::Client,
}

impl LinkPreviewCache {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/112.0.0.0 Safari/537.36 Snort/1.0 (LinkPreview; https://nostr-rs-api.v0l.io)")
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(24 * 60 * 60)) // 1 day
                .build(),
            empty_cache: Cache::builder()
                .time_to_live(Duration::from_secs(10 * 60)) // 10 minutes
                .build(),
            client,
        }
    }

    pub async fn get_preview(&self, url: &str) -> Option<LinkPreviewData> {
        let url_hash = {
            let mut hasher = Sha256::new();
            hasher.update(url.to_lowercase().as_bytes());
            hex::encode(hasher.finalize())
        };

        // Check empty cache
        if self.empty_cache.get(&url_hash).await.is_some() {
            return None;
        }

        // Check main cache
        if let Some(cached) = self.cache.get(&url_hash).await {
            return cached;
        }

        // Fetch the URL
        match self.fetch_and_parse(url).await {
            Ok(Some(data)) => {
                self.cache
                    .insert(url_hash.clone(), Some(data.clone()))
                    .await;
                Some(data)
            }
            Ok(None) => {
                self.empty_cache.insert(url_hash, true).await;
                None
            }
            Err(e) => {
                warn!("Failed to fetch preview for {}: {}", url, e);
                self.empty_cache.insert(url_hash, true).await;
                None
            }
        }
    }

    async fn fetch_and_parse(&self, url: &str) -> anyhow::Result<Option<LinkPreviewData>> {
        let response = self.client.get(url).send().await?;

        if !response.status().is_success() {
            warn!("{} returned {}", url, response.status());
            return Ok(None);
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !content_type.starts_with("text/html") {
            return Ok(None);
        }

        let body = response.text().await?;
        let document = Html::parse_document(&body);

        // Parse OpenGraph tags
        let og_selector = Selector::parse("meta[property^='og:']").expect("invalid selector");
        let mut og_tags = Vec::new();

        for element in document.select(&og_selector) {
            if let (Some(property), Some(content)) = (
                element.value().attr("property"),
                element.value().attr("content"),
            ) {
                if !property.is_empty() && !content.is_empty() {
                    og_tags.push((property.to_string(), content.to_string()));
                }
            }
        }

        // Extract specific fields
        let title = og_tags
            .iter()
            .find(|(k, _)| k == "og:title")
            .map(|(_, v)| v.clone())
            .or_else(|| {
                let title_selector = Selector::parse("title").unwrap();
                document
                    .select(&title_selector)
                    .next()
                    .map(|e| e.text().collect::<String>())
            });

        let description = og_tags
            .iter()
            .find(|(k, _)| k == "og:description")
            .map(|(_, v)| v.clone())
            .or_else(|| {
                let desc_selector = Selector::parse("meta[name='description']").unwrap();
                document
                    .select(&desc_selector)
                    .next()
                    .and_then(|e| e.value().attr("content"))
                    .map(|s| s.to_string())
            });

        let image = og_tags
            .iter()
            .find(|(k, _)| k == "og:image")
            .map(|(_, v)| v.clone());

        Ok(Some(LinkPreviewData {
            og_tags,
            title,
            description,
            image,
        }))
    }
}

#[derive(Deserialize)]
pub struct PreviewQuery {
    url: String,
}

pub async fn get_preview(
    State(cache): State<Arc<LinkPreviewCache>>,
    Query(q): Query<PreviewQuery>,
) -> Response {
    match cache.get_preview(&q.url).await {
        Some(data) => Json(data).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
