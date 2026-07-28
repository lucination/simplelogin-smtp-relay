//! Blocking HTTP client for the SimpleLogin API, mirroring utils.py's
//! SimpleLoginClient exactly: same endpoints, same pagination/caching
//! semantics, same error conditions.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Deserialize)]
struct AliasEntry {
    id: i64,
    email: String,
}

#[derive(Deserialize)]
struct AliasesResponse {
    #[serde(default)]
    aliases: Vec<AliasEntry>,
}

#[derive(Deserialize)]
struct ContactsResponse {
    reverse_alias: Option<String>,
}

pub struct SimpleLoginClient {
    api_url: String,
    api_key: String,
    client: &'static reqwest::blocking::Client,
    alias_cache: Mutex<HashMap<String, i64>>,
}

impl SimpleLoginClient {
    pub fn new(api_url: &str, api_key: &str) -> Result<Self> {
        let client = Box::leak(Box::new(
            reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
        ));
        Ok(SimpleLoginClient {
            api_url: api_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            client,
            alias_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Mirrors _get_alias_id: paginate GET /api/v2/aliases?page_id=&query=
    /// until an exact email match is found or an empty page is returned.
    fn get_alias_id(&self, alias_email: &str) -> Result<i64> {
        if let Some(id) = self.alias_cache.lock().unwrap().get(alias_email) {
            return Ok(*id);
        }
        let mut page_id: u64 = 0;
        loop {
            let resp = self
                .client
                .get(format!("{}/api/v2/aliases", self.api_url))
                .header("Authentication", &self.api_key)
                .query(&[
                    ("page_id", page_id.to_string()),
                    ("query", alias_email.to_string()),
                ])
                .send()?;
            let resp = resp.error_for_status()?;
            let body: AliasesResponse = resp.json()?;
            if body.aliases.is_empty() {
                break;
            }
            for alias in &body.aliases {
                if alias.email == alias_email {
                    self.alias_cache
                        .lock()
                        .unwrap()
                        .insert(alias_email.to_string(), alias.id);
                    return Ok(alias.id);
                }
            }
            page_id += 1;
        }
        Err(anyhow!("Alias not found: {}", alias_email))
    }

    /// Mirrors get_reverse_alias: resolve sender's alias id, then
    /// POST /api/aliases/{id}/contacts {"contact": recipient} and
    /// return the reverse_alias string.
    pub fn get_reverse_alias(&self, sender: &str, recipient: &str) -> Result<String> {
        let alias_id = self.get_alias_id(sender)?;
        let resp = self
            .client
            .post(format!(
                "{}/api/aliases/{}/contacts",
                self.api_url, alias_id
            ))
            .header("Authentication", &self.api_key)
            .json(&serde_json::json!({ "contact": recipient }))
            .send()?;
        let resp = resp.error_for_status()?;
        let body: ContactsResponse = resp.json()?;
        match body.reverse_alias {
            Some(ra) if !ra.is_empty() => Ok(ra),
            _ => Err(anyhow!("No reverse_alias in response for {}", recipient)),
        }
    }
}
