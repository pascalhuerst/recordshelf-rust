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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    /// Serves one canned HTTP response and returns the raw request it received.
    fn serve_once(status: &str, body: &str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        (base, handle)
    }

    fn client(base: &str) -> DiscogsClient {
        let mut c = DiscogsClient::new("test-agent", "tok", "EUR").unwrap();
        c.base = base.to_string();
        c
    }

    #[test]
    fn rejects_invalid_header_values() {
        assert!(DiscogsClient::new("bad\nagent", "tok", "EUR").is_err());
        assert!(DiscogsClient::new("ok", "bad\ntoken", "EUR").is_err());
    }

    #[test]
    fn release_request_has_auth_and_currency() {
        let (base, server) = serve_once("200 OK", r#"{"id": 42, "title": "Blue", "year": 1971}"#);
        let release = client(&base).release(42).unwrap();
        assert_eq!(release.id, Some(42));
        assert_eq!(release.title.as_deref(), Some("Blue"));
        let req = server.join().unwrap().to_lowercase();
        assert!(req.starts_with("get /releases/42?curr_abbr=eur"), "{req}");
        assert!(req.contains("authorization: discogs token=tok"), "{req}");
        assert!(req.contains("user-agent: test-agent"), "{req}");
    }

    #[test]
    fn http_errors_are_reported() {
        let (base, _server) = serve_once("404 Not Found", r#"{"message": "nope"}"#);
        assert!(client(&base).release(1).is_err());
    }

    #[test]
    fn add_to_collection_posts_to_folder() {
        let (base, server) = serve_once("201 Created", "{}");
        client(&base).add_to_collection("me", 7, 99).unwrap();
        let req = server.join().unwrap();
        assert!(
            req.starts_with("POST /users/me/collection/folders/7/releases/99?quantity=1"),
            "{req}"
        );
    }

    #[test]
    fn collection_instances_lists_every_copy() {
        let body = r#"{"pagination": {"items": 2}, "releases": [
            {"id": 5, "instance_id": 11, "folder_id": 1, "rating": 0},
            {"id": 5, "instance_id": 12, "folder_id": 3}
        ]}"#;
        let (base, server) = serve_once("200 OK", body);
        let owned = client(&base).collection_instances("me", 5).unwrap();
        assert_eq!(
            owned,
            vec![
                CollectionInstance {
                    instance_id: 11,
                    folder_id: 1
                },
                CollectionInstance {
                    instance_id: 12,
                    folder_id: 3
                },
            ]
        );
        assert!(
            server
                .join()
                .unwrap()
                .starts_with("GET /users/me/collection/releases/5")
        );
    }

    #[test]
    fn collection_instances_empty_when_not_owned() {
        let (base, _server) =
            serve_once("200 OK", r#"{"pagination": {"items": 0}, "releases": []}"#);
        assert!(
            client(&base)
                .collection_instances("me", 5)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn collection_folders_are_parsed() {
        let body = r#"{"folders": [
            {"id": 0, "name": "All", "count": 10, "resource_url": "x"},
            {"id": 1, "name": "Uncategorized", "count": 4}
        ]}"#;
        let (base, _server) = serve_once("200 OK", body);
        let folders = client(&base).collection_folders("me").unwrap();
        assert_eq!(folders.len(), 2);
        assert_eq!(
            folders[1],
            CollectionFolder {
                id: 1,
                name: "Uncategorized".into(),
                count: 4
            }
        );
    }

    #[test]
    fn search_sends_only_set_parameters() {
        let (base, server) = serve_once("200 OK", r#"{"results": []}"#);
        let query = SearchQuery {
            barcode: Some("5012345678900".into()),
            kind: Some("release".into()),
            per_page: Some(10),
            ..Default::default()
        };
        assert!(client(&base).search(&query).unwrap().results.is_empty());
        let req = server.join().unwrap();
        let line = req.lines().next().unwrap();
        assert!(line.contains("barcode=5012345678900"), "{line}");
        assert!(line.contains("type=release"), "{line}");
        assert!(line.contains("per_page=10"), "{line}");
        assert!(line.contains("curr_abbr=EUR"), "{line}");
        assert!(!line.contains("artist="), "{line}");
        assert!(!line.contains("&page="), "{line}");
    }

    #[test]
    fn search_response_tolerates_inconsistent_types() {
        let body = r#"{"results": [
            {"id": "123", "title": "Artist - Album", "year": "1999", "label": "Solo",
             "catno": 42, "format": ["Vinyl", "LP"], "country": "DE", "uri": "/release/123"},
            {"id": 456, "title": "Other", "year": 2001, "label": ["A", "B"]},
            {"title": "no id at all"}
        ]}"#;
        let (base, _server) = serve_once("200 OK", body);
        let results = client(&base)
            .search(&SearchQuery::default())
            .unwrap()
            .results;
        assert_eq!(results[0].id, Some(123));
        assert_eq!(results[0].year, Some(1999));
        assert_eq!(results[0].label, Some(vec!["Solo".to_string()]));
        assert_eq!(results[0].catno.as_deref(), Some("42"));
        assert_eq!(results[1].label_display(), "A, B");
        assert_eq!(results[2].id, None);
    }

    fn item(json: &str) -> SearchItem {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn artist_falls_back_to_title_prefix() {
        let it = item(r#"{"id": 1, "title": "Miles Davis - Kind of Blue"}"#);
        assert_eq!(it.artist_display(), "Miles Davis");
        let it = item(r#"{"id": 1, "title": "Kind of Blue", "artist": "Miles Davis"}"#);
        assert_eq!(it.artist_display(), "Miles Davis");
        let it = item(r#"{"id": 1, "title": "Untitled"}"#);
        assert_eq!(it.artist_display(), "");
    }

    #[test]
    fn display_helpers_join_fields() {
        let it = item(
            r#"{"id": 1, "label": ["Blue Note", "Other"], "catno": "BLP 1", "format": ["Vinyl", "LP"]}"#,
        );
        assert_eq!(it.label_display(), "Blue Note, Other (BLP 1)");
        assert_eq!(it.format_display(), "Vinyl, LP");
        assert_eq!(item(r#"{"id": 1}"#).label_display(), "");
    }

    #[test]
    fn release_model_decodes_nested_data() {
        let body = r#"{"id": 9, "title": "T", "artists": [{"name": "A"}],
            "labels": [{"name": "L", "catno": "C-1"}], "year": 1990,
            "formats": [{"name": "Vinyl", "descriptions": ["LP"]}],
            "genres": ["Jazz"], "styles": ["Bop"],
            "tracklist": [{"position": "A1", "title": "One", "duration": "3:00"}],
            "uri": "https://www.discogs.com/release/9"}"#;
        let r: Release = serde_json::from_str(body).unwrap();
        assert_eq!(r.artists.unwrap()[0].name.as_deref(), Some("A"));
        assert_eq!(r.labels.unwrap()[0].catno.as_deref(), Some("C-1"));
        assert_eq!(r.tracklist.unwrap()[0].duration.as_deref(), Some("3:00"));
    }
}
