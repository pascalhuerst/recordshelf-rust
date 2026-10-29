use anyhow::{Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Deserializer, de::DeserializeOwned};
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

    fn get<T: DeserializeOwned>(
        &self,
        url: &str,
        params: &[(&str, String)],
        what: &str,
    ) -> Result<T> {
        self.http
            .get(url)
            .query(params)
            .send()
            .with_context(|| format!("send {what} request"))?
            .error_for_status()
            .with_context(|| format!("Discogs {what} error response"))?
            .json()
            .with_context(|| format!("decode {what} response"))
    }

    pub fn release(&self, id: u32) -> Result<Release> {
        let url = format!("{}/releases/{}", self.base, id);
        self.get(&url, &[("curr_abbr", self.currency.clone())], "release")
    }

    pub fn add_to_collection(&self, username: &str, folder_id: u32, release_id: u32) -> Result<()> {
        let url = format!(
            "{}/users/{}/collection/folders/{}/releases/{}",
            self.base, username, folder_id, release_id
        );
        self.http
            .post(url)
            .query(&[("quantity", 1u32)])
            .send()
            .context("send add-to-collection request")?
            .error_for_status()
            .context("Discogs collection error response")?;
        Ok(())
    }

    pub fn collection_folders(&self, username: &str) -> Result<Vec<CollectionFolder>> {
        let url = format!("{}/users/{}/collection/folders", self.base, username);
        let resp: CollectionFoldersResponse = self.get(&url, &[], "collection folders")?;
        Ok(resp.folders)
    }

    /// Every copy of `release_id` in the user's collection (empty if not owned).
    pub fn collection_instances(
        &self,
        username: &str,
        release_id: u32,
    ) -> Result<Vec<CollectionInstance>> {
        let url = format!(
            "{}/users/{}/collection/releases/{}",
            self.base, username, release_id
        );
        let resp: CollectionInstancesResponse = self.get(&url, &[], "collection instances")?;
        Ok(resp.releases)
    }

    pub fn search(&self, query: &SearchQuery) -> Result<SearchResponse> {
        let url = format!("{}/database/search", self.base);
        self.get(&url, &query.params(&self.currency), "search")
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub query: Option<String>,
    pub artist: Option<String>,
    pub release_title: Option<String>,
    pub catno: Option<String>,
    pub barcode: Option<String>,
    /// Restrict results to a Discogs entity type, e.g. "release".
    pub kind: Option<String>,
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

impl SearchQuery {
    fn params(&self, currency: &str) -> Vec<(&'static str, String)> {
        let mut params = vec![("curr_abbr", currency.to_string())];
        let optional = [
            ("q", self.query.clone()),
            ("artist", self.artist.clone()),
            ("release_title", self.release_title.clone()),
            ("catno", self.catno.clone()),
            ("barcode", self.barcode.clone()),
            ("type", self.kind.clone()),
            ("page", self.page.map(|p| p.to_string())),
            ("per_page", self.per_page.map(|p| p.to_string())),
        ];
        params.extend(optional.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))));
        params
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct SearchResponse {
    #[serde(default)]
    pub results: Vec<SearchItem>,
}

#[derive(Debug, Deserialize)]
struct CollectionFoldersResponse {
    #[serde(default)]
    folders: Vec<CollectionFolder>,
}

#[derive(Debug, Deserialize)]
struct CollectionInstancesResponse {
    #[serde(default)]
    releases: Vec<CollectionInstance>,
}

/// One copy of a release in the user's collection.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct CollectionInstance {
    pub instance_id: u32,
    pub folder_id: u32,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct CollectionFolder {
    pub id: u32,
    pub name: String,
    #[serde(default)]
    pub count: u32,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct SearchItem {
    #[serde(deserialize_with = "deserialize_opt_u32")]
    pub id: Option<u32>,
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
    pub id: Option<u32>,
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
