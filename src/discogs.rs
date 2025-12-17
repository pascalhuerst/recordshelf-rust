use anyhow::{Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

#[derive(Clone)]
pub struct DiscogsClient {
    http: Client,
    base: String,
    currency: String,
}

impl DiscogsClient {
    pub fn new(user_agent: &str, token: &str, currency: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(user_agent).context("invalid user-agent header")?,
        );
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Discogs token={}", token))
                .context("invalid authorization header")?,
        );

        let http = Client::builder()
            .default_headers(headers)
            .build()
            .context("build HTTP client")?;

        Ok(Self {
            http,
            base: "https://api.discogs.com".to_string(),
            currency: currency.to_string(),
        })
    }

    pub fn release(&self, id: u32) -> Result<Release> {
        let url = format!("{}/releases/{}", self.base, id);
        let resp = self
            .http
            .get(url)
            .query(&[("curr_abbr", &self.currency)])
            .send()
            .context("send request")?
            .error_for_status()
            .context("Discogs error response")?;

        let release: Release = resp.json().context("decode release response")?;
        Ok(release)
    }

    pub fn search(&self, query: &SearchQuery) -> Result<SearchResponse> {
        let url = format!("{}/database/search", self.base);
        let mut params: Vec<(&str, String)> = Vec::new();
        params.push(("curr_abbr", self.currency.clone()));

        if let Some(q) = &query.query {
            params.push(("q", q.clone()));
        }
        if let Some(artist) = &query.artist {
            params.push(("artist", artist.clone()));
        }
        if let Some(title) = &query.release_title {
            params.push(("release_title", title.clone()));
        }
        if let Some(catno) = &query.catno {
            params.push(("catno", catno.clone()));
        }
        if let Some(page) = query.page {
            params.push(("page", page.to_string()));
        }
        if let Some(per_page) = query.per_page {
            params.push(("per_page", per_page.to_string()));
        }

        let resp = self
            .http
            .get(url)
            .query(&params)
            .send()
            .context("send search request")?
            .error_for_status()
            .context("Discogs search error response")?;

        let raw: serde_json::Value = resp.json().context("decode search response body")?;
        eprintln!("discogs search raw response: {raw}");
        let mut out = SearchResponse::default();
        let mut total = 0usize;
        let mut kept = 0usize;

        if let Some(results) = raw.get("results").and_then(|v| v.as_array()) {
            total = results.len();
            for v in results {
                let id = v.get("id").and_then(val_to_u32);
                let type_field = v.get("type").and_then(val_to_string);
                let title = v.get("title").and_then(val_to_string);
                let artist = v.get("artist").and_then(val_to_string);
                let year = v.get("year").and_then(val_to_u32);
                let country = v.get("country").and_then(val_to_string);
                let label = v.get("label").and_then(val_to_string_vec);
                let catno = v.get("catno").and_then(val_to_string);
                let format_descriptions = v.get("format").and_then(val_to_string_vec);
                let uri = v.get("uri").and_then(val_to_string);

                if let Some(id) = id {
                    kept += 1;
                    out.results.push(SearchItem {
                        id: Some(id),
                        type_field,
                        title,
                        artist,
                        year,
                        country,
                        label,
                        catno,
                        format_descriptions,
                        uri,
                    });
                }
            }
        }
        let _ = eprintln!("discogs search parsed: total={}, kept={}", total, kept);
        if let Some(pag) = raw.get("pagination") {
            out.pagination = serde_json::from_value(pag.clone()).ok();
        }

        Ok(out)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub query: Option<String>,
    pub artist: Option<String>,
    pub release_title: Option<String>,
    pub catno: Option<String>,
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SearchResponse {
    #[serde(default)]
    pub results: Vec<SearchItem>,
    pub pagination: Option<SearchPagination>,
}

impl Default for SearchResponse {
    fn default() -> Self {
        Self {
            results: Vec::new(),
            pagination: None,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct SearchPagination {
    pub page: Option<u32>,
    pub pages: Option<u32>,
    pub per_page: Option<u32>,
    pub items: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SearchItem {
    #[serde(deserialize_with = "deserialize_opt_u32")]
    pub id: Option<u32>,
    #[serde(rename = "type")]
    pub type_field: Option<String>,
    #[serde(deserialize_with = "deserialize_opt_string")]
    pub title: Option<String>,
    #[serde(deserialize_with = "deserialize_opt_string")]
    pub artist: Option<String>,
    #[serde(deserialize_with = "deserialize_opt_u32")]
    pub year: Option<u32>,
    #[serde(deserialize_with = "deserialize_opt_string")]
    pub country: Option<String>,
    #[serde(deserialize_with = "deserialize_opt_string_vec")]
    pub label: Option<Vec<String>>,
    #[serde(deserialize_with = "deserialize_opt_string")]
    pub catno: Option<String>,
    #[serde(rename = "format", deserialize_with = "deserialize_opt_string_vec")]
    pub format_descriptions: Option<Vec<String>>,
    #[serde(deserialize_with = "deserialize_opt_string")]
    pub uri: Option<String>,
}
impl SearchItem {
    pub fn title_display(&self) -> String {
        self.title.clone().unwrap_or_default()
    }

    pub fn artist_display(&self) -> String {
        if let Some(a) = &self.artist {
            if !a.is_empty() {
                return a.clone();
            }
        }
        // Fallback: derive from title "Artist - Title"
        if let Some(t) = &self.title {
            if let Some((artist, _)) = t.split_once(" - ") {
                return artist.trim().to_string();
            }
        }
        String::new()
    }

    pub fn label_display(&self) -> String {
        let mut parts = Vec::new();
        if let Some(labels) = &self.label {
            if !labels.is_empty() {
                parts.push(labels.join(", "));
            }
        }
        if let Some(cat) = &self.catno {
            if !cat.is_empty() {
                parts.push(format!("({})", cat));
            }
        }
        parts.join(" ").trim().to_string()
    }

    pub fn format_display(&self) -> String {
        self.format_descriptions
            .as_ref()
            .map(|f| f.join(", "))
            .unwrap_or_default()
    }

    pub fn uri_display(&self) -> String {
        if let Some(uri) = &self.uri {
            if uri.starts_with("http") {
                uri.clone()
            } else {
                format!("https://www.discogs.com{uri}")
            }
        } else {
            String::new()
        }
    }
}

fn val_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn val_to_u32(v: &serde_json::Value) -> Option<u32> {
    match v {
        serde_json::Value::Number(n) => n.as_u64().and_then(|u| u.try_into().ok()),
        serde_json::Value::String(s) => s.parse::<u32>().ok(),
        serde_json::Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        _ => None,
    }
}

fn val_to_string_vec(v: &serde_json::Value) -> Option<Vec<String>> {
    match v {
        serde_json::Value::Array(arr) => {
            let mut out = Vec::new();
            for item in arr {
                if let Some(s) = val_to_string(item) {
                    out.push(s);
                }
            }
            Some(out)
        }
        _ => val_to_string(v).map(|s| vec![s]),
    }
}

fn deserialize_opt_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Option::<Value>::deserialize(deserializer)?;
    Ok(match v {
        None => None,
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    })
}

fn deserialize_opt_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Option::<Value>::deserialize(deserializer)?;
    Ok(match v {
        None => None,
        Some(Value::Number(n)) => n.as_u64().and_then(|u| u.try_into().ok()),
        Some(Value::String(s)) => s.parse::<u32>().ok(),
        Some(Value::Bool(b)) => Some(if b { 1 } else { 0 }),
        _ => None,
    })
}

fn deserialize_opt_string_vec<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Option::<Value>::deserialize(deserializer)?;
    Ok(match v {
        None => None,
        Some(Value::Array(arr)) => {
            let mut out = Vec::new();
            for val in arr {
                match val {
                    Value::String(s) => out.push(s),
                    Value::Number(n) => out.push(n.to_string()),
                    Value::Bool(b) => out.push(b.to_string()),
                    _ => {}
                }
            }
            Some(out)
        }
        Some(Value::String(s)) => Some(vec![s]),
        Some(Value::Number(n)) => Some(vec![n.to_string()]),
        Some(Value::Bool(b)) => Some(vec![b.to_string()]),
        _ => None,
    })
}

#[derive(Debug, Deserialize, Clone)]
pub struct Release {
    pub title: Option<String>,
    pub artists: Option<Vec<Artist>>,
    pub labels: Option<Vec<Label>>,
    pub year: Option<u32>,
    pub country: Option<String>,
    pub formats: Option<Vec<Format>>,
    pub genres: Option<Vec<String>>,
    pub styles: Option<Vec<String>>,
    pub tracklist: Option<Vec<Track>>,
    pub uri: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Artist {
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Label {
    pub name: Option<String>,
    pub catno: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Format {
    pub name: Option<String>,
    pub descriptions: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Track {
    pub position: Option<String>,
    pub title: Option<String>,
    pub duration: Option<String>,
}
