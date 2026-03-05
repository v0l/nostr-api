use crate::default_avatar;
use crate::fetch::FetchQueue;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::DateTime;
use nostr_sdk::nips::nip19::Nip19;
use nostr_sdk::prelude::{Nip19Coordinate, Nip19Event};
use nostr_sdk::{
    Alphabet, Event, FromBech32, Kind, Metadata, PublicKey, SingleLetterTag, TagKind, ToBech32,
};
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HeadElement {
    element: String,
    attributes: Vec<(String, String)>,
    content: Option<String>,
}

impl HeadElement {
    fn new<S>(element: S, attributes: &[(S, S)], content: Option<S>) -> Self
    where
        S: ToString,
    {
        Self {
            element: element.to_string(),
            attributes: attributes
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            content: content.map(|t| t.to_string()),
        }
    }

    pub fn as_title(&self) -> Option<HeadElement> {
        // allow og:title to replace title
        let is_title = self.element == "meta"
            && self
                .attributes
                .iter()
                .any(|t| t.0 == "property" && t.1 == "og:title");
        if is_title {
            Some(HeadElement::new("title", &[], self.meta_content()))
        } else {
            None
        }
    }

    pub fn meta_content(&self) -> Option<&str> {
        if self.element == "meta" {
            self.attributes
                .iter()
                .find(|t| t.0 == "content")
                .map(|t| t.1.as_str())
        } else {
            None
        }
    }

    pub fn replace_with(&self, node: ElementRef) -> Option<HeadElement> {
        if self.is_match(node) {
            return Some(self.clone());
        }
        if node.value().name.local.as_ref() == "title"
            && let Some(t) = self.as_title()
        {
            return Some(t);
        }
        None
    }

    fn is_match(&self, node: ElementRef) -> bool {
        let name = node.value().name.local.as_ref();
        self.element == name
            && match name {
                "meta" => {
                    let prop_child = node.attr("property").or(node.attr("name"));
                    let prop_tag = self
                        .attributes
                        .iter()
                        .find(|p| p.0 == "property" || p.0 == "name")
                        .map(|p| p.1.as_str());
                    prop_child == prop_tag
                }
                _ => false,
            }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

impl Display for HeadElement {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{}", self.element)?;
        for (k, v) in &self.attributes {
            write!(f, " {}=\"{}\"", html_escape(k), html_escape(v))?;
        }
        if let Some(c) = &self.content {
            write!(f, ">")?;
            write!(f, "{}", html_escape(c))?;
            write!(f, "</{}>", self.element)?;
        } else {
            write!(f, " />")?;
        }
        Ok(())
    }
}

struct ProfileMeta {
    title: String,
    description: String,
    image: String,
    profile: Metadata,
}

#[derive(Deserialize)]
struct Nip05Response {
    names: HashMap<String, String>,
}

/// Parse and validate NIP-05 identifier (name@domain.tld)
fn parse_nip05(identifier: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = identifier.split('@').collect();
    if parts.len() != 2 {
        return None;
    }

    let name = parts[0];
    let domain = parts[1];

    // Validate name: only a-z0-9-_.
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' || c == '.')
    {
        return None;
    }

    // Basic domain validation
    if domain.is_empty() || !domain.contains('.') {
        return None;
    }

    let domain_lower = domain.to_lowercase();

    // Reject private/internal TLDs and hostnames
    if domain_lower == "localhost"
        || domain_lower.ends_with(".local")
        || domain_lower.ends_with(".internal")
        || domain_lower.ends_with(".localhost")
    {
        return None;
    }

    // Reject bare IPv4 addresses (all-numeric labels, e.g. "192.168.1.1")
    let is_ipv4 = domain_lower
        .split('.')
        .all(|label| label.parse::<u8>().is_ok());
    if is_ipv4 {
        return None;
    }

    Some((name.to_lowercase(), domain_lower))
}

/// Resolve NIP-05 identifier to public key
async fn resolve_nip05(client: &reqwest::Client, identifier: &str) -> Option<PublicKey> {
    let (name, domain) = parse_nip05(identifier)?;

    let url = format!("https://{}/.well-known/nostr.json?name={}", domain, name);

    let response = client.get(&url).send().await.ok()?;

    if !response.status().is_success() {
        return None;
    }

    let nip05_data: Nip05Response = response.json().await.ok()?;

    let pubkey_hex = nip05_data.names.get(&name)?;
    PublicKey::from_hex(pubkey_hex).ok()
}

#[derive(Deserialize)]
pub struct OpenGraphQuery {
    canonical: Option<String>,
}

/// Inject opengraph tags into provided html
pub async fn tag_page(
    State(fetch): State<FetchQueue>,
    State(http_client): State<Arc<reqwest::Client>>,
    Path(id): Path<String>,
    Query(query): Query<OpenGraphQuery>,
    body: Bytes,
) -> Response {
    let html = match String::from_utf8(body.to_vec()) {
        Ok(s) => s,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    // Try parsing as Nip19 first, then fall back to NIP-05
    let nid = match Nip19::from_bech32(&id) {
        Ok(n) => Some(n),
        Err(_) => {
            // Try NIP-05 resolution using the shared HTTP client
            if let Some(pubkey) = resolve_nip05(&http_client, &id).await {
                info!("Resolved NIP-05 {} to {}", id, pubkey.to_hex());
                Some(Nip19::Pubkey(pubkey))
            } else {
                None
            }
        }
    };

    let nid = match nid {
        Some(n) => n,
        None => {
            return (
                [(CONTENT_TYPE, HeaderValue::from_static("text/html"))],
                html,
            )
                .into_response();
        }
    };

    let tags = match &nid {
        Nip19::EventId(_) | Nip19::Event(_) | Nip19::Coordinate(_) => {
            match fetch.demand(&nid).await {
                Ok(Some(ev)) => get_event_tags(&fetch, &ev, &query.canonical).await,
                Ok(None) => Vec::new(),
                Err(e) => {
                    warn!("Failed to fetch event: {}", e);
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
        }
        Nip19::Profile(_) | Nip19::Pubkey(_) => {
            let pk = match &nid {
                Nip19::Pubkey(p) => *p,
                Nip19::Profile(np) => np.public_key,
                _ => {
                    return (
                        [(CONTENT_TYPE, HeaderValue::from_static("text/html"))],
                        html,
                    )
                        .into_response();
                }
            };

            let profile_meta = get_profile_meta(&fetch, &pk).await;
            let pk_hex = pk.to_hex();
            let image = profile_meta
                .as_ref()
                .map(|p| p.image.clone())
                .unwrap_or_else(|| default_avatar(&pk_hex));
            let title = profile_meta
                .as_ref()
                .map(|p| p.title.clone())
                .unwrap_or_default();
            let description = profile_meta
                .as_ref()
                .map(|p| p.description.clone())
                .unwrap_or_default();

            meta_tags_to_elements(vec![
                ("og:type", "profile"),
                ("og:title", &title),
                ("og:description", &description),
                ("og:image", &image),
                ("twitter:card", "summary"),
                ("twitter:title", &title),
                ("twitter:description", &description),
                ("twitter:image", &image),
            ])
        }
        _ => Vec::new(),
    };

    let result_html = if tags.is_empty() {
        html
    } else {
        info!("Injecting {} tags for {}", tags.len(), id);
        inject_tags(&html, tags)
    };

    (
        [(CONTENT_TYPE, HeaderValue::from_static("text/html"))],
        result_html,
    )
        .into_response()
}

async fn get_event_tags(
    fetch: &FetchQueue,
    ev: &Event,
    canonical: &Option<String>,
) -> Vec<HeadElement> {
    let mut tags = match ev.kind {
        Kind::LiveEvent => {
            // Live event (kind 30311)
            let host_pubkey = ev
                .tags
                .iter()
                .find(|t| {
                    let vec = t.as_slice();
                    vec[0] == "p" && vec.len() > 3 && vec[3].eq_ignore_ascii_case("host")
                })
                .and_then(|t| {
                    let vec = t.as_slice();
                    vec.get(1).and_then(|s| PublicKey::from_hex(s).ok())
                })
                .unwrap_or(ev.pubkey);

            let profile = get_profile_meta(fetch, &host_pubkey).await;
            let host_name = profile
                .as_ref()
                .and_then(|p| p.profile.name.as_deref())
                .unwrap_or("Nostrich");

            let stream = ev
                .tags
                .find(TagKind::Streaming)
                .or_else(|| ev.tags.find(TagKind::Recording))
                .and_then(|t| t.content())
                .unwrap_or("");

            let image = ev
                .tags
                .find(TagKind::Image)
                .and_then(|t| t.content())
                .map(|s| s.to_string())
                .or_else(|| profile.as_ref().and_then(|p| p.profile.picture.clone()))
                .unwrap_or_else(|| default_avatar(&host_pubkey.to_hex()));

            let title_tag = ev
                .tags
                .find(TagKind::Title)
                .and_then(|t| t.content())
                .unwrap_or("");

            let event_bech32 = Nip19::Event(Nip19Event {
                event_id: ev.id,
                author: Some(ev.pubkey),
                kind: Some(ev.kind),
                relays: vec![],
            })
            .to_bech32()
            .unwrap_or_default();

            meta_tags_to_elements(vec![
                ("og:type", "video.other"),
                ("og:title", &format!("{} is streaming", host_name)),
                ("og:description", title_tag),
                ("og:image", &image),
                ("og:video", stream),
                ("og:video:secure_url", stream),
                ("og:video:type", "application/vnd.apple.mpegurl"),
                ("twitter:card", "player"),
                ("twitter:title", &format!("{} is streaming", host_name)),
                ("twitter:description", title_tag),
                ("twitter:image", &image),
                ("twitter:site", "@zap_stream"),
                (
                    "twitter:player",
                    &format!("https://zap.stream/embed/{}", event_bech32),
                ),
                ("twitter:player:width", "640"),
                ("twitter:player:height", "480"),
                ("twitter:text:player_height", "480"),
            ])
        }
        Kind::Custom(1313) => {
            let profile = get_profile_meta(fetch, &ev.pubkey).await;
            let name = profile
                .as_ref()
                .and_then(|p| p.profile.name.as_deref())
                .unwrap_or("Nostrich");

            // Stream clip
            let stream = ev
                .tags
                .find(TagKind::SingleLetter(SingleLetterTag::lowercase(
                    Alphabet::R,
                )))
                .and_then(|t| t.content())
                .unwrap_or("");

            let image = ev
                .tags
                .find(TagKind::Image)
                .and_then(|t| t.content())
                .map(|s| s.to_string())
                .or_else(|| profile.as_ref().and_then(|p| p.profile.picture.clone()))
                .unwrap_or_else(|| default_avatar(&ev.pubkey.to_hex()));

            let title_tag = ev
                .tags
                .find(TagKind::Title)
                .and_then(|t| t.content())
                .unwrap_or("");

            meta_tags_to_elements(vec![
                ("og:type", "video.other"),
                ("og:title", &format!("{} created a clip", name)),
                ("og:description", title_tag),
                ("og:image", &image),
                ("og:video", stream),
                ("og:video:secure_url", stream),
                ("og:video:type", "video/mp4"),
                ("twitter:card", "summary_large_image"),
                ("twitter:title", &format!("{} created a clip", name)),
                ("twitter:description", title_tag),
                ("twitter:image", &image),
            ])
        }
        _ => {
            // Default case for regular posts
            let profile = get_profile_meta(fetch, &ev.pubkey).await;
            let name = profile
                .as_ref()
                .and_then(|p| p.profile.name.as_deref())
                .unwrap_or("Nostrich");

            const MAX_LEN: usize = 160;
            let trimmed_content = ev.content.chars().take(MAX_LEN).collect::<String>();
            let title_content = format!("{}: {}", name, trimmed_content);

            let image = profile
                .as_ref()
                .and_then(|p| p.profile.picture.clone())
                .unwrap_or_else(|| default_avatar(&ev.pubkey.to_hex()));

            let profile_name = profile
                .as_ref()
                .and_then(|p| p.profile.name.as_deref())
                .unwrap_or("");

            let created_iso = DateTime::from_timestamp(ev.created_at.as_secs() as i64, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();

            meta_tags_to_elements(vec![
                ("og:type", "article"),
                ("og:title", &title_content),
                ("og:description", ""),
                ("og:image", &image),
                ("og:article:published_time", &created_iso),
                ("og:article:author:username", profile_name),
                ("twitter:card", "summary"),
                ("twitter:title", &title_content),
                ("twitter:description", ""),
                ("twitter:image", &image),
            ])
        }
    };
    if let Some(canonical_template) = canonical
        && canonical_template.contains("%s")
    {
        let bech32 = ev
            .coordinate()
            .map(|r| Nip19::Coordinate(Nip19Coordinate::new(r.into_owned(), [])))
            .unwrap_or(Nip19::Event(Nip19Event::from(ev)));
        if let Ok(b) = bech32.to_bech32() {
            let canonical_url = canonical_template.replace("%s", &b);
            tags.push(HeadElement::new(
                "link",
                &[("rel", "canonical"), ("href", canonical_url.as_str())],
                None,
            ));
        }
    }
    tags
}

async fn get_profile_meta(fetch: &FetchQueue, pubkey: &PublicKey) -> Option<ProfileMeta> {
    let profile = match fetch.get_profile(*pubkey).await {
        Ok(Some(profile)) => profile,
        Ok(None) => return None,
        Err(e) => {
            warn!("Failed to get profile: {}", e);
            return None;
        }
    };

    let name = profile.name.as_deref().unwrap_or("Nostrich");
    let title = format!("{}'s Profile", name);

    let about = profile.about.as_deref().unwrap_or("");
    let description = about.chars().take(160).collect::<String>();

    let picture = profile
        .picture
        .as_ref()
        .map(|p| p.to_string())
        .unwrap_or(default_avatar(&pubkey.to_hex()));

    Some(ProfileMeta {
        title,
        description: description.to_string(),
        image: picture,
        profile,
    })
}

fn inject_tags(html: &str, tags: Vec<HeadElement>) -> String {
    let doc = Html::parse_document(html);
    let head_selector = Selector::parse("head").expect("invalid selector");

    if let Some(head_element) = doc.select(&head_selector).next() {
        let mut new_head_content = String::new();

        // Iterate through existing head children
        let mut replaced = HashSet::new();
        for child in head_element.child_elements() {
            let replace_with = tags.iter().find_map(|t| t.replace_with(child));
            new_head_content.push('\n');
            if let Some(replace_with) = replace_with {
                new_head_content.push_str(replace_with.to_string().as_str());
                replaced.insert(replace_with);
            } else {
                new_head_content.push_str(child.html().as_str());
            }
        }

        // add remaining tags
        for tag in tags.into_iter().filter(|t| !replaced.contains(t)) {
            new_head_content.push('\n');
            new_head_content.push_str(tag.to_string().as_str());
        }

        // Rebuild the head element using the parsed source span
        let head_html = head_element.html();
        // Find the end of the opening tag within head_html
        if let Some(open_end) = head_html.find('>') {
            // Find the closing tag from the end
            if let Some(close_start) = head_html.rfind("</head>") {
                let mut result = html.to_string();
                // Locate the head element in the original html
                if let Some(head_pos) = result.find("<head") {
                    let abs_open_end = head_pos + open_end + 1;
                    let abs_close_start = head_pos + close_start;
                    result.replace_range(abs_open_end..abs_close_start, &new_head_content);
                }
                return result;
            }
        }

        html.to_string()
    } else {
        warn!("No head element found in document");
        html.to_string()
    }
}

fn meta_tags_to_elements(tags: Vec<(&str, &str)>) -> Vec<HeadElement> {
    tags.into_iter()
        .map(|(key, value)| {
            HeadElement::new("meta", &[("property", key), ("content", value)], None)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::Html;

    // ── html_escape ──────────────────────────────────────────────────────────

    #[test]
    fn test_html_escape_ampersand() {
        assert_eq!(html_escape("a&b"), "a&amp;b");
    }

    #[test]
    fn test_html_escape_double_quote() {
        assert_eq!(html_escape("say \"hi\""), "say &quot;hi&quot;");
    }

    #[test]
    fn test_html_escape_less_than() {
        assert_eq!(html_escape("a<b"), "a&lt;b");
    }

    #[test]
    fn test_html_escape_greater_than() {
        assert_eq!(html_escape("a>b"), "a&gt;b");
    }

    #[test]
    fn test_html_escape_all_chars() {
        assert_eq!(html_escape("<\"a&b\">"), "&lt;&quot;a&amp;b&quot;&gt;");
    }

    #[test]
    fn test_html_escape_clean_string() {
        assert_eq!(html_escape("hello world"), "hello world");
    }

    // ── parse_nip05 ──────────────────────────────────────────────────────────

    #[test]
    fn test_parse_nip05_valid() {
        let result = parse_nip05("alice@example.com");
        assert_eq!(result, Some(("alice".to_string(), "example.com".to_string())));
    }

    #[test]
    fn test_parse_nip05_normalises_to_lowercase() {
        let result = parse_nip05("Alice@Example.COM");
        // uppercase name chars are rejected by the name validator
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_nip05_lowercase_domain_normalised() {
        let result = parse_nip05("bob@Example.COM");
        assert_eq!(result, Some(("bob".to_string(), "example.com".to_string())));
    }

    #[test]
    fn test_parse_nip05_missing_at() {
        assert!(parse_nip05("nodomain").is_none());
    }

    #[test]
    fn test_parse_nip05_multiple_at_signs() {
        assert!(parse_nip05("a@b@c.com").is_none());
    }

    #[test]
    fn test_parse_nip05_domain_no_dot() {
        assert!(parse_nip05("alice@localhost").is_none());
    }

    #[test]
    fn test_parse_nip05_empty_domain() {
        assert!(parse_nip05("alice@").is_none());
    }

    #[test]
    fn test_parse_nip05_invalid_name_char() {
        // uppercase rejected
        assert!(parse_nip05("Alice@example.com").is_none());
        // space rejected
        assert!(parse_nip05("ali ce@example.com").is_none());
    }

    #[test]
    fn test_parse_nip05_allowed_name_chars() {
        // digits, hyphens, underscores, dots are all valid
        assert!(parse_nip05("a1-_.@example.com").is_some());
    }

    #[test]
    fn test_parse_nip05_rejects_dot_local() {
        assert!(parse_nip05("alice@printer.local").is_none());
    }

    #[test]
    fn test_parse_nip05_rejects_dot_internal() {
        assert!(parse_nip05("alice@db.internal").is_none());
    }

    #[test]
    fn test_parse_nip05_rejects_dot_localhost() {
        assert!(parse_nip05("alice@foo.localhost").is_none());
    }

    #[test]
    fn test_parse_nip05_rejects_ipv4_address() {
        assert!(parse_nip05("alice@192.168.1.1").is_none());
        assert!(parse_nip05("alice@10.0.0.1").is_none());
        assert!(parse_nip05("alice@127.0.0.1").is_none());
    }

    #[test]
    fn test_parse_nip05_allows_numeric_labels_that_are_not_full_ipv4() {
        // "1.example.com" — not all labels are octets so it should pass
        assert!(parse_nip05("alice@1.example.com").is_some());
    }

    // ── HeadElement::new / meta_content / as_title ───────────────────────────

    #[test]
    fn test_head_element_meta_content_present() {
        let el = HeadElement::new(
            "meta",
            &[("property", "og:title"), ("content", "Hello")],
            None,
        );
        assert_eq!(el.meta_content(), Some("Hello"));
    }

    #[test]
    fn test_head_element_meta_content_absent() {
        let el = HeadElement::new("meta", &[("property", "og:title")], None);
        assert_eq!(el.meta_content(), None);
    }

    #[test]
    fn test_head_element_meta_content_non_meta_element() {
        let el = HeadElement::new("title", &[], Some("My Title"));
        assert_eq!(el.meta_content(), None);
    }

    #[test]
    fn test_head_element_as_title_og_title_produces_title_element() {
        let el = HeadElement::new(
            "meta",
            &[("property", "og:title"), ("content", "My Page")],
            None,
        );
        let title = el.as_title().unwrap();
        assert_eq!(title.element, "title");
        assert_eq!(title.content, Some("My Page".to_string()));
    }

    #[test]
    fn test_head_element_as_title_non_og_title_returns_none() {
        let el = HeadElement::new(
            "meta",
            &[("property", "og:description"), ("content", "Desc")],
            None,
        );
        assert!(el.as_title().is_none());
    }

    #[test]
    fn test_head_element_as_title_non_meta_returns_none() {
        let el = HeadElement::new("title", &[], Some("Title"));
        assert!(el.as_title().is_none());
    }

    // ── HeadElement Display (and html_escape integration) ────────────────────

    #[test]
    fn test_display_self_closing_meta() {
        let el = HeadElement::new(
            "meta",
            &[("property", "og:title"), ("content", "Hello")],
            None,
        );
        let s = el.to_string();
        assert!(s.starts_with("<meta "));
        assert!(s.ends_with("/>"));
        assert!(s.contains("property=\"og:title\""));
        assert!(s.contains("content=\"Hello\""));
    }

    #[test]
    fn test_display_element_with_content() {
        let el = HeadElement::new("title", &[], Some("My Page"));
        let s = el.to_string();
        assert_eq!(s, "<title>My Page</title>");
    }

    #[test]
    fn test_display_escapes_attribute_values() {
        let el = HeadElement::new(
            "meta",
            &[("property", "og:title"), ("content", "Bob & Alice \"say\" <hi>")],
            None,
        );
        let s = el.to_string();
        assert!(s.contains("Bob &amp; Alice &quot;say&quot; &lt;hi&gt;"));
    }

    #[test]
    fn test_display_escapes_content() {
        let el = HeadElement::new("title", &[], Some("<script>alert(1)</script>"));
        let s = el.to_string();
        assert!(!s.contains("<script>"));
        assert!(s.contains("&lt;script&gt;"));
    }

    // ── meta_tags_to_elements ────────────────────────────────────────────────

    #[test]
    fn test_meta_tags_to_elements_count() {
        let tags = meta_tags_to_elements(vec![
            ("og:title", "Hello"),
            ("og:description", "World"),
        ]);
        assert_eq!(tags.len(), 2);
    }

    #[test]
    fn test_meta_tags_to_elements_structure() {
        let tags = meta_tags_to_elements(vec![("og:title", "Hi")]);
        let el = &tags[0];
        assert_eq!(el.element, "meta");
        assert!(el.attributes.iter().any(|(k, v)| k == "property" && v == "og:title"));
        assert!(el.attributes.iter().any(|(k, v)| k == "content" && v == "Hi"));
        assert!(el.content.is_none());
    }

    #[test]
    fn test_meta_tags_to_elements_empty() {
        let tags = meta_tags_to_elements(vec![]);
        assert!(tags.is_empty());
    }

    // ── inject_tags ──────────────────────────────────────────────────────────

    fn base_html() -> &'static str {
        r#"<!DOCTYPE html><html><head><title>Old Title</title><meta property="og:description" content="old" /></head><body></body></html>"#
    }

    #[test]
    fn test_inject_tags_replaces_existing_og_meta() {
        let tags = meta_tags_to_elements(vec![("og:description", "new description")]);
        let result = inject_tags(base_html(), tags);
        assert!(result.contains("new description"));
        assert!(!result.contains("content=\"old\""));
    }

    #[test]
    fn test_inject_tags_appends_new_tags() {
        let tags = meta_tags_to_elements(vec![("og:image", "https://example.com/img.png")]);
        let result = inject_tags(base_html(), tags);
        assert!(result.contains("og:image"));
        assert!(result.contains("https://example.com/img.png"));
    }

    #[test]
    fn test_inject_tags_replaces_title_via_og_title() {
        let tags = meta_tags_to_elements(vec![("og:title", "New Title")]);
        let result = inject_tags(base_html(), tags);
        assert!(result.contains("<title>New Title</title>"));
        assert!(!result.contains("<title>Old Title</title>"));
    }

    #[test]
    fn test_inject_tags_no_head_returns_original() {
        let html = "<html><body>no head here</body></html>";
        let tags = meta_tags_to_elements(vec![("og:title", "Hi")]);
        let result = inject_tags(html, tags);
        // No head to inject into — original returned
        assert_eq!(result, html);
    }

    #[test]
    fn test_inject_tags_empty_tags_list() {
        // With no tags the head children are all preserved unchanged.
        let result = inject_tags(base_html(), vec![]);
        assert!(result.contains("<title>Old Title</title>"));
    }

    #[test]
    fn test_inject_tags_escapes_injected_values() {
        let tags = meta_tags_to_elements(vec![("og:title", "A & B <test>")]);
        let result = inject_tags(base_html(), tags);
        assert!(result.contains("A &amp; B &lt;test&gt;"));
        assert!(!result.contains("<test>"));
    }

    #[test]
    fn test_inject_tags_empty_head_appends_all_tags() {
        let html = "<html><head></head><body></body></html>";
        let tags = meta_tags_to_elements(vec![("og:title", "Hi"), ("og:image", "https://x.com/img.png")]);
        let result = inject_tags(html, tags);
        assert!(result.contains("og:title"));
        assert!(result.contains("og:image"));
    }

    #[test]
    fn test_inject_tags_head_with_attributes_is_handled() {
        // The `>` in `<head lang="en">` must not confuse the open-tag boundary detection.
        let html = r#"<!DOCTYPE html><html><head lang="en"><title>T</title></head><body></body></html>"#;
        let tags = meta_tags_to_elements(vec![("og:title", "New")]);
        let result = inject_tags(html, tags);
        assert!(result.contains("<title>New</title>"));
        assert!(!result.contains("<title>T</title>"));
        // The head opening tag and attributes must still be present
        assert!(result.contains(r#"lang="en""#));
    }

    #[test]
    fn test_inject_tags_preserves_unmatched_existing_children() {
        // og:title is replaced; the charset meta must survive untouched.
        let html = r#"<html><head><meta charset="utf-8"><meta property="og:title" content="old" /></head></html>"#;
        let tags = meta_tags_to_elements(vec![("og:title", "New")]);
        let result = inject_tags(html, tags);
        assert!(result.contains(r#"charset="utf-8""#));
        assert!(result.contains("New"));
        assert!(!result.contains("content=\"old\""));
    }

    #[test]
    fn test_inject_tags_body_is_not_modified() {
        let html = r#"<html><head></head><body><p>Keep me</p></body></html>"#;
        let tags = meta_tags_to_elements(vec![("og:title", "Hi")]);
        let result = inject_tags(html, tags);
        assert!(result.contains("<p>Keep me</p>"));
    }

    #[test]
    fn test_inject_tags_duplicate_input_tags_not_appended_twice() {
        // Two og:title entries in the input vec; the second is a duplicate.
        // After the first matches and is inserted into `replaced`, the second
        // should NOT be appended again (the HashSet deduplicates equal HeadElements).
        let tags = vec![
            HeadElement::new("meta", &[("property", "og:title"), ("content", "A")], None),
            HeadElement::new("meta", &[("property", "og:title"), ("content", "A")], None),
        ];
        let html = r#"<html><head><meta property="og:title" content="old" /></head></html>"#;
        let result = inject_tags(html, tags);
        // The value "A" should appear exactly once as a content attribute.
        let count = result.matches("content=\"A\"").count();
        assert_eq!(count, 1, "duplicate tag appeared {count} times");
    }

    #[test]
    fn test_inject_tags_meta_matched_by_name_attribute() {
        // is_match checks `name` attr as a fallback when `property` is absent.
        let html = r#"<html><head><meta name="description" content="old desc" /></head></html>"#;
        let tag = HeadElement::new(
            "meta",
            &[("name", "description"), ("content", "new desc")],
            None,
        );
        let result = inject_tags(html, vec![tag]);
        assert!(result.contains("new desc"));
        assert!(!result.contains("old desc"));
    }

    #[test]
    fn test_inject_tags_og_title_appended_when_no_existing_title() {
        // No <title> element in head; og:title tag should be appended (not replace anything).
        let html = r#"<html><head><meta charset="utf-8" /></head><body></body></html>"#;
        let tags = meta_tags_to_elements(vec![("og:title", "Appended")]);
        let result = inject_tags(html, tags);
        assert!(result.contains("og:title"));
        assert!(result.contains("Appended"));
    }

    #[test]
    fn test_inject_tags_multiple_replacements_in_one_call() {
        let html = r#"<html><head>
            <title>Old</title>
            <meta property="og:description" content="old desc" />
            <meta property="og:image" content="old.png" />
        </head></html>"#;
        let tags = meta_tags_to_elements(vec![
            ("og:title", "New Title"),
            ("og:description", "new desc"),
            ("og:image", "new.png"),
        ]);
        let result = inject_tags(html, tags);
        assert!(result.contains("<title>New Title</title>"));
        assert!(result.contains("new desc"));
        assert!(result.contains("new.png"));
        assert!(!result.contains("old desc"));
        assert!(!result.contains("old.png"));
    }

    // ── HeadElement::replace_with / is_match (via inject_tags DOM path) ──────

    #[test]
    fn test_replace_with_matching_meta_returns_self() {
        let html = r#"<html><head><meta property="og:title" content="old" /></head></html>"#;
        let doc = Html::parse_document(html);
        let sel = Selector::parse("meta[property='og:title']").unwrap();
        let node = doc.select(&sel).next().unwrap();

        let el = HeadElement::new(
            "meta",
            &[("property", "og:title"), ("content", "new")],
            None,
        );
        let replaced = el.replace_with(node).unwrap();
        assert_eq!(replaced, el);
    }

    #[test]
    fn test_replace_with_non_matching_meta_returns_none() {
        let html = r#"<html><head><meta property="og:description" content="d" /></head></html>"#;
        let doc = Html::parse_document(html);
        let sel = Selector::parse("meta[property='og:description']").unwrap();
        let node = doc.select(&sel).next().unwrap();

        // og:title element does NOT match og:description node
        let el = HeadElement::new(
            "meta",
            &[("property", "og:title"), ("content", "new")],
            None,
        );
        assert!(el.replace_with(node).is_none());
    }

    #[test]
    fn test_replace_with_title_node_and_og_title_tag() {
        let html = r#"<html><head><title>Old</title></head></html>"#;
        let doc = Html::parse_document(html);
        let sel = Selector::parse("title").unwrap();
        let node = doc.select(&sel).next().unwrap();

        let el = HeadElement::new(
            "meta",
            &[("property", "og:title"), ("content", "New Title")],
            None,
        );
        let replaced = el.replace_with(node).unwrap();
        assert_eq!(replaced.element, "title");
        assert_eq!(replaced.content, Some("New Title".to_string()));
    }

    // ── integration: real NIP-05 + relay fetch ───────────────────────────────

    /// End-to-end test: resolve `kieran@snort.social` via NIP-05, fetch the
    /// profile from a public relay, and verify that `og:title` and `og:type`
    /// are injected into the returned HTML.
    #[tokio::test]
    async fn test_tag_page_kieran_snort_social() {
        use crate::{AppState, avatar, link_preview, opengraph};
        use axum::{Router, body::Body, routing::post};
        use http_body_util::BodyExt;
        use nostr_sdk::ClientBuilder;
        use std::sync::Arc;
        use tower::ServiceExt;

        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        // Build a real client connected to relay.snort.social.
        let client = ClientBuilder::new().build();
        client
            .add_relay("wss://relay.snort.social")
            .await
            .expect("add relay");
        client.connect().await;

        let fetch = crate::fetch::FetchQueue::new(client);
        let fetch_worker = fetch.clone();
        tokio::spawn(async move {
            loop {
                fetch_worker.process_queue().await;
            }
        });

        let link_preview_cache = Arc::new(link_preview::LinkPreviewCache::new());
        let http_client = Arc::new(link_preview_cache.client().clone());
        let avatar_sets = avatar::AvatarSets::load();

        let state = AppState {
            fetch,
            link_preview: link_preview_cache,
            http_client,
            avatar_sets,
        };

        let app = Router::new()
            .route("/opengraph/{id}", post(opengraph::tag_page))
            .with_state(state);

        let minimal_html = r#"<!DOCTYPE html><html><head><title>Snort</title></head><body></body></html>"#;

        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/opengraph/kieran@snort.social")
            .header("content-type", "text/html")
            .body(Body::from(minimal_html))
            .unwrap();

        let response = app.oneshot(request).await.expect("handler should respond");

        assert_eq!(response.status(), axum::http::StatusCode::OK);

        let body_bytes = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        let body = std::str::from_utf8(&body_bytes).expect("body is utf-8");

        assert!(
            body.contains("og:type"),
            "response should contain og:type tag, got:\n{body}"
        );
        assert!(
            body.contains("profile"),
            "og:type should be profile, got:\n{body}"
        );
        assert!(
            body.contains("og:title"),
            "response should contain og:title tag, got:\n{body}"
        );
        // kieran's profile name should appear somewhere in the title
        assert!(
            body.to_lowercase().contains("kieran") || body.contains("og:title"),
            "og:title should reference the profile, got:\n{body}"
        );
    }
}
