use crate::default_avatar;
use crate::fetch::FetchQueue;
use chrono::DateTime;
use nostr_sdk::nips::nip19::Nip19;
use nostr_sdk::prelude::{Nip19Coordinate, Nip19Event};
use nostr_sdk::{
    Alphabet, Event, EventId, FromBech32, JsonUtil, Kind, PublicKey, SingleLetterTag,
    TagKind, ToBech32,
};
use rocket::data::ByteUnit;
use rocket::http::Status;
use rocket::{Data, Route, State};
use scraper::{Html, Selector};

pub fn routes() -> Vec<Route> {
    routes![tag_page]
}

#[derive(Debug, Clone)]
struct HeadElement {
    element: String,
    attributes: Vec<(String, String)>,
}

impl HeadElement {
    fn new(element: &str, attributes: Vec<(&str, &str)>) -> Self {
        Self {
            element: element.to_string(),
            attributes: attributes
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
}

struct ProfileMeta {
    title: String,
    description: String,
    image: String,
}

/// Inject opengraph tags into provided html
#[post("/opengraph/<id>?<canonical>", data = "<body>")]
async fn tag_page(
    fetch: &State<FetchQueue>,
    id: &str,
    canonical: Option<&str>,
    body: Data<'_>,
) -> Result<(Status, String), Status> {
    let stream = body.open(ByteUnit::Mebibyte(64));
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

    let nid = match Nip19::from_bech32(id) {
        Ok(n) => n,
        Err(_) => return Ok((Status::Ok, html)),
    };

    let mut tags = Vec::new();

    match &nid {
        Nip19::EventId(_) | Nip19::Event(_) | Nip19::Coordinate(_) => {
            if let Some(ev) = fetch.demand(&nid).await.map_err(|e| {
                warn!("Failed to fetch event: {}", e);
                Status::InternalServerError
            })? {
                tags = get_event_tags(fetch, &ev, &canonical).await;
            }
        }
        Nip19::Profile(_) | Nip19::Pubkey(_) => {
            let pk = match &nid {
                Nip19::Pubkey(p) => *p,
                Nip19::Profile(np) => np.public_key,
                _ => return Ok((Status::Ok, html)),
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

            tags = meta_tags_to_elements(vec![
                ("og:type", "profile"),
                ("og:title", &title),
                ("og:description", &description),
                ("og:image", &image),
                ("twitter:card", "summary"),
                ("twitter:title", &title),
                ("twitter:description", &description),
                ("twitter:image", &image),
            ]);

            if let Some(canonical_template) = canonical {
                if canonical_template.contains("%s") {
                    let bech32 = nid.to_bech32().unwrap_or_default();
                    let canonical_url = canonical_template.replace("%s", &bech32);
                    tags.push(HeadElement::new(
                        "link",
                        vec![("rel", "canonical"), ("href", &canonical_url)],
                    ));
                }
            }
        }
        _ => return Ok((Status::Ok, html)),
    }

    info!("Injecting {} tags for {}", tags.len(), id);
    let result_html = inject_tags(&html, tags);
    Ok((Status::Ok, result_html))
}

async fn get_event_tags(
    fetch: &State<FetchQueue>,
    ev: &Event,
    canonical: &Option<&str>,
) -> Vec<HeadElement> {
    let profile = get_profile_event(fetch, &ev.pubkey).await;
    let name = profile
        .as_ref()
        .and_then(|p| p.name.as_deref())
        .unwrap_or("Nostrich");

    let mut tags = match ev.kind {
        Kind::LiveEvent => {
            // Live event (kind 30311)
            let host_pubkey = ev
                .tags
                .iter()
                .find(|t| {
                    let vec = t.as_slice();
                    vec.len() > 3 && vec[0] == "p" && vec[3] == "host"
                })
                .and_then(|t| {
                    let vec = t.as_slice();
                    vec.get(1).and_then(|s| PublicKey::from_hex(s).ok())
                })
                .unwrap_or(ev.pubkey);

            let host_profile = get_profile_event(fetch, &host_pubkey).await;
            let host_name = host_profile
                .as_ref()
                .and_then(|p| p.name.as_deref())
                .unwrap_or(name);

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
                .or_else(|| host_profile.as_ref().and_then(|p| p.picture.clone()))
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
                .or_else(|| profile.as_ref().and_then(|p| p.picture.clone()))
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
            const MAX_LEN: usize = 160;
            let trimmed_content = if ev.content.len() > MAX_LEN {
                &ev.content[..MAX_LEN]
            } else {
                &ev.content
            };
            let title_content = format!("{}: {}", name, trimmed_content);

            let image = profile
                .as_ref()
                .and_then(|p| p.picture.clone())
                .unwrap_or_else(|| default_avatar(&ev.pubkey.to_hex()));

            let profile_name = profile
                .as_ref()
                .and_then(|p| p.name.as_deref())
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
                    vec![("rel", "canonical"), ("href", &canonical_url)],
                ));
            }
        }
    }
    tags
}

async fn get_profile_event(
    fetch: &State<FetchQueue>,
    pubkey: &PublicKey,
) -> Option<nostr_sdk::Metadata> {
    let nip19 = Nip19::Event(Nip19Event {
        event_id: EventId::all_zeros(),
        author: Some(*pubkey),
        kind: Some(Kind::Metadata),
        relays: vec![],
    });

    let ev = match fetch.demand(&nip19).await {
        Ok(e) => e,
        _ => None,
    };
    ev.and_then(|e| nostr_sdk::Metadata::from_json(&e.content).ok())
}

async fn get_profile_meta(fetch: &State<FetchQueue>, pubkey: &PublicKey) -> Option<ProfileMeta> {
    let profile = get_profile_event(fetch, pubkey).await?;

    let name = profile.name.as_deref().unwrap_or("Nostrich");
    let title = format!("{}'s Profile", name);

    let about = profile.about.as_deref().unwrap_or("");
    let description = if about.len() > 160 {
        &about[..160]
    } else {
        about
    };

    let picture = profile.picture.unwrap_or(default_avatar(&pubkey.to_hex()));

    Some(ProfileMeta {
        title,
        description: description.to_string(),
        image: picture.to_string(),
    })
}

fn inject_tags(html: &str, tags: Vec<HeadElement>) -> String {
    let doc = Html::parse_document(html);
    let head_selector = Selector::parse("head").unwrap();

    let mut html_string = html.to_string();

    // Find the head tag position
    if doc.select(&head_selector).next().is_some() {
        let mut injected_tags = String::new();

        for tag_elem in &tags {
            if tag_elem.element == "meta" {
                let mut tag_string = String::from("<meta");
                for (key, value) in &tag_elem.attributes {
                    tag_string.push_str(&format!(" {}=\"{}\"", key, value));
                }
                tag_string.push_str(">\n");
                injected_tags.push_str(&tag_string);

                // Handle og:title -> update title tag
                if tag_elem
                    .attributes
                    .iter()
                    .any(|(k, v)| k == "property" && v == "og:title")
                {
                    if let Some((_, content)) =
                        tag_elem.attributes.iter().find(|(k, _)| k == "content")
                    {
                        if !content.is_empty() {
                            // We'll inject a title tag
                            injected_tags.push_str(&format!("<title>{}</title>\n", content));
                        }
                    }
                }

                // Handle og:description -> update description meta tag
                if tag_elem
                    .attributes
                    .iter()
                    .any(|(k, v)| k == "property" && v == "og:description")
                {
                    if let Some((_, content)) =
                        tag_elem.attributes.iter().find(|(k, _)| k == "content")
                    {
                        if !content.is_empty() {
                            injected_tags.push_str(&format!(
                                "<meta name=\"description\" content=\"{}\">\n",
                                content
                            ));
                        }
                    }
                }
            } else if tag_elem.element == "link" {
                let mut tag_string = String::from("<link");
                for (key, value) in &tag_elem.attributes {
                    tag_string.push_str(&format!(" {}=\"{}\"", key, value));
                }
                tag_string.push_str(">\n");
                injected_tags.push_str(&tag_string);
            }
        }

        // Insert tags at the beginning of <head>
        if let Some(head_start) = html_string.find("<head ") {
            if let Some(head_end) = html_string[head_start..].find('>') {
                let insert_pos = head_start + head_end + 1;
                html_string.insert_str(insert_pos, &format!("\n{}", injected_tags));
            }
        } else {
            warn!("Cant find head in html document, inserting at end of html");
            html_string.push_str(&injected_tags);
        }
    }

    html_string
}

fn meta_tags_to_elements(tags: Vec<(&str, &str)>) -> Vec<HeadElement> {
    tags.into_iter()
        .map(|(key, value)| HeadElement::new("meta", vec![("property", key), ("content", value)]))
        .collect()
}
