use anyhow::{Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;

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
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
pub struct Artist {
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Label {
    pub name: Option<String>,
    pub catno: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Format {
    pub name: Option<String>,
    pub descriptions: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct Track {
    pub position: Option<String>,
    pub title: Option<String>,
    pub duration: Option<String>,
}
