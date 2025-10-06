use crate::default_avatar;
use crate::fetch::FetchQueue;
use chrono::DateTime;
use nostr_sdk::nips::nip19::Nip19;
use nostr_sdk::prelude::{Nip19Coordinate, Nip19Event};
use nostr_sdk::{
    Alphabet, Event, FromBech32, Kind, Metadata, PublicKey, SingleLetterTag, TagKind, ToBech32,
};
use rocket::data::ByteUnit;
use rocket::http::{ContentType, Status};
use rocket::{Data, Route, State};
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::{Display, Formatter};

pub fn routes() -> Vec<Route> {
    routes![tag_page]
}

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

impl Display for HeadElement {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{}", self.element)?;
        for (k, v) in &self.attributes {
            write!(f, " {}=\"{}\"", k, v)?;
        }
        if let Some(c) = &self.content {
            write!(f, ">")?;
            write!(f, "{}", c)?;
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

    Some((name.to_lowercase(), domain.to_lowercase()))
}

/// Resolve NIP-05 identifier to public key
async fn resolve_nip05(identifier: &str) -> Option<PublicKey> {
    let (name, domain) = parse_nip05(identifier)?;

    let url = format!("https://{}/.well-known/nostr.json?name={}", domain, name);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;

    let response = client.get(&url).send().await.ok()?;

    if !response.status().is_success() {
        return None;
    }

    let nip05_data: Nip05Response = response.json().await.ok()?;

    let pubkey_hex = nip05_data.names.get(&name)?;
    PublicKey::from_hex(pubkey_hex).ok()
}

/// Inject opengraph tags into provided html
#[post("/opengraph/<id>?<canonical>", data = "<body>", format = "text/html")]
async fn tag_page(
    fetch: &State<FetchQueue>,
    id: &str,
    canonical: Option<&str>,
    body: Data<'_>,
) -> Result<(ContentType, String), Status> {
    let stream = body.open(ByteUnit::Mebibyte(1));
    let html = stream.into_string().await.map_err(|e| {
        warn!("Failed to read request body: {}", e);
        Status::InternalServerError
    })?;
    let html = if html.is_complete() {
        html.value
    } else {
        warn!("html is not complete, capped at {}", html.n);
        return Err(Status::InternalServerError);
    };

    // Try parsing as Nip19 first, then fall back to NIP-05
    let nid = match Nip19::from_bech32(id) {
        Ok(n) => Some(n),
        Err(_) => {
            // Try NIP-05 resolution
            if let Some(pubkey) = resolve_nip05(id).await {
                info!("Resolved NIP-05 {} to {}", id, pubkey.to_hex());
                Some(Nip19::Pubkey(pubkey))
            } else {
                None
            }
        }
    };

    let nid = match nid {
        Some(n) => n,
        None => return Ok((ContentType::HTML, html)),
    };

    let tags = match &nid {
        Nip19::EventId(_) | Nip19::Event(_) | Nip19::Coordinate(_) => {
            if let Some(ev) = fetch.demand(&nid).await.map_err(|e| {
                warn!("Failed to fetch event: {}", e);
                Status::InternalServerError
            })? {
                get_event_tags(fetch, &ev, &canonical).await
            } else {
                Vec::new()
            }
        }
        Nip19::Profile(_) | Nip19::Pubkey(_) => {
            let pk = match &nid {
                Nip19::Pubkey(p) => *p,
                Nip19::Profile(np) => np.public_key,
                _ => return Ok((ContentType::HTML, html)),
            };

            let profile_meta = get_profile_meta(fetch, &pk).await;
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

    if tags.is_empty() {
        Ok((ContentType::HTML, html))
    } else {
        info!("Injecting {} tags for {}", tags.len(), id);
        let result_html = inject_tags(&html, tags);
        Ok((ContentType::HTML, result_html))
    }
}

async fn get_event_tags(
    fetch: &State<FetchQueue>,
    ev: &Event,
    canonical: &Option<&str>,
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
            let trimmed_content = if ev.content.len() > MAX_LEN {
                &ev.content[..MAX_LEN]
            } else {
                &ev.content
            };
            let title_content = format!("{}: {}", name, trimmed_content);

            let image = profile
                .as_ref()
                .and_then(|p| p.profile.picture.clone())
                .unwrap_or_else(|| default_avatar(&ev.pubkey.to_hex()));

            let profile_name = profile
                .as_ref()
                .and_then(|p| p.profile.name.as_deref())
                .unwrap_or("");

            let created_iso = DateTime::from_timestamp(ev.created_at.as_u64() as i64, 0)
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
    if let Some(canonical_template) = canonical {
        if canonical_template.contains("%s") {
            let bech32 = ev
                .coordinate()
                .map(|r| Nip19::Coordinate(Nip19Coordinate::new(r.into_owned(), [])))
                .unwrap_or(Nip19::Event(Nip19Event::from_event(&ev)));
            if let Ok(b) = bech32.to_bech32() {
                let canonical_url = canonical_template.replace("%s", &b);
                tags.push(HeadElement::new(
                    "link",
                    &[("rel", "canonical"), ("href", canonical_url.as_str())],
                    None,
                ));
            }
        }
    }
    tags
}

async fn get_profile_meta(fetch: &State<FetchQueue>, pubkey: &PublicKey) -> Option<ProfileMeta> {
    let profile = match fetch.get_profile(pubkey.clone()).await {
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
    let description = if about.len() > 160 {
        &about[..160]
    } else {
        about
    };

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

        // Replace the entire head element
        let mut html_string = html.to_string();
        if let Some(hs) = html_string.find("<head")
            && let Some(head_start_end) = html_string[hs..].find(">")
            && let Some(head_close) = html_string[head_start_end..].find("</head>")
        {
            let head_start = hs + head_start_end + 1;
            let head_end = head_start_end + head_close;
            html_string.replace_range(head_start..head_end, &new_head_content);
        }

        html_string
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
