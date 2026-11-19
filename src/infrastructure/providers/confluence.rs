use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use url::Url;

use crate::infrastructure::providers::{
    DocumentationProvider, build_http_client, ensure_success_json,
};
use crate::types::DocPageData;

/// REST client for Atlassian Confluence Cloud / Data Center documentation pages.
pub struct ConfluenceClient {
    client: reqwest::Client,
    url: String,
    username: String,
    token: String,
}

impl ConfluenceClient {
    pub fn new(
        url: impl Into<String>,
        username: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            client: build_http_client(),
            url: url.into().trim().to_string(),
            username: username.into().trim().to_string(),
            token: token.into().trim().to_string(),
        }
    }

    pub fn normalize_base_url(raw: &str) -> String {
        let mut base = raw.trim().to_string();
        if base.is_empty() {
            return base;
        }
        if !base.starts_with("http://") && !base.starts_with("https://") {
            base = format!("https://{base}");
        }
        base = base.trim_end_matches('/').to_string();
        if !base.contains("/wiki") {
            base.push_str("/wiki");
        }
        base
    }

    fn auth_header(&self) -> String {
        if !self.username.is_empty() {
            let encoded = STANDARD.encode(format!("{}:{}", self.username, self.token));
            format!("Basic {encoded}")
        } else {
            format!("Bearer {}", self.token)
        }
    }

    fn validate_credentials(&self) -> anyhow::Result<String> {
        if self.url.is_empty() || self.token.is_empty() {
            anyhow::bail!("Confluence URL and API Token are required.");
        }
        let normalized = Self::normalize_base_url(&self.url);
        if normalized.is_empty() {
            anyhow::bail!("Confluence URL is invalid.");
        }
        Ok(normalized)
    }

    fn build_api_url(
        &self,
        segments: &[&str],
        query_pairs: &[(&str, &str)],
    ) -> anyhow::Result<Url> {
        let base = self.validate_credentials()?;
        let mut url = Url::parse(&base)
            .map_err(|err| anyhow::anyhow!("Invalid Confluence URL '{base}': {err}"))?;
        {
            let mut path_segments = url
                .path_segments_mut()
                .map_err(|()| anyhow::anyhow!("Confluence URL cannot be a base URL: '{base}'"))?;
            path_segments.pop_if_empty();
            for segment in segments {
                path_segments.push(segment);
            }
        }
        if !query_pairs.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query_pairs {
                pairs.append_pair(key, value);
            }
        }
        Ok(url)
    }
}

#[async_trait]
impl DocumentationProvider for ConfluenceClient {
    async fn fetch_page(&self, page_id: &str) -> anyhow::Result<DocPageData> {
        let clean_id = page_id.trim();
        if clean_id.is_empty() {
            anyhow::bail!("Confluence page ID cannot be empty.");
        }

        let api_url = self.build_api_url(
            &["rest", "api", "content", clean_id],
            &[("expand", "body.storage")],
        )?;

        let response = self
            .client
            .get(api_url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("Failed to connect to Confluence: {err}"))?;

        let raw: ConfluenceContentItem = ensure_success_json(response, "Confluence").await?;
        Ok(raw.into_doc_page())
    }

    async fn search_pages(&self, query: &str) -> anyhow::Result<Vec<DocPageData>> {
        self.validate_credentials()?;
        let clean = query.trim();
        if clean.is_empty() {
            return Ok(Vec::new());
        }

        let exact_match = if clean.chars().all(|c| c.is_ascii_digit()) {
            self.fetch_page(clean).await.ok()
        } else {
            None
        };

        let cql = build_cql_query(clean);
        let api_url = self.build_api_url(
            &["rest", "api", "content", "search"],
            &[("cql", &cql), ("expand", "body.storage"), ("limit", "20")],
        )?;

        let response = self
            .client
            .get(api_url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("Failed to search Confluence pages: {err}"))?;

        let payload: ConfluenceSearchResponse =
            ensure_success_json(response, "Confluence search").await?;

        let mut results = Vec::new();
        let exact_id = exact_match.as_ref().map(|p| p.id.clone());
        if let Some(exact) = exact_match {
            results.push(exact);
        }

        for item in payload.results {
            let page = item.into_doc_page();
            if exact_id.as_deref() == Some(page.id.as_str()) {
                continue;
            }
            results.push(page);
            if results.len() >= 20 {
                break;
            }
        }

        Ok(results)
    }

    fn is_doc_page_url(&self, raw_url: &str) -> bool {
        if self.url.is_empty() || raw_url.trim().is_empty() {
            return false;
        }
        let base_str = Self::normalize_base_url(&self.url);
        let Ok(config_url) = Url::parse(&base_str) else {
            return false;
        };
        let Ok(target_url) = Url::parse(raw_url.trim()) else {
            return false;
        };
        let hosts_match = match (config_url.host_str(), target_url.host_str()) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        };
        hosts_match && self.extract_page_id(raw_url).is_some()
    }

    fn extract_page_id(&self, raw_url: &str) -> Option<String> {
        let trimmed = raw_url.trim();
        if trimmed.is_empty() {
            return None;
        }
        let parsed = Url::parse(trimmed).ok()?;

        if let Some((_, value)) = parsed.query_pairs().find(|(k, _)| k == "pageId") {
            let v = value.trim();
            if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()) {
                return Some(v.to_string());
            }
        }

        if let Some(mut segments) = parsed.path_segments() {
            while let Some(seg) = segments.next() {
                if seg == "pages" {
                    if let Some(next) = segments.next() {
                        if !next.is_empty() && next.chars().all(|c| c.is_ascii_digit()) {
                            return Some(next.to_string());
                        }
                    }
                }
            }
        }

        None
    }
}

pub fn build_cql_query(query: &str) -> String {
    let escaped = query.trim().replace('\\', "\\\\").replace('"', "\\\"");
    format!("title ~ \"{escaped}\" and type = \"page\"")
}

/// Converts Confluence storage XHTML markup into a readable plain-text snippet for UI previews.
pub fn strip_html_for_preview(html: &str) -> String {
    if html.trim().is_empty() {
        return String::new();
    }

    let mut with_breaks = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(lt) = rest.find('<') {
        with_breaks.push_str(&rest[..lt]);
        let after_lt = &rest[lt + 1..];
        if let Some(gt) = after_lt.find('>') {
            let tag = after_lt[..gt].trim().to_ascii_lowercase();
            let is_break_tag = tag.starts_with("br")
                || tag.starts_with("/p")
                || tag.starts_with("/div")
                || tag.starts_with("/li")
                || tag.starts_with("/tr")
                || tag.starts_with("/h1")
                || tag.starts_with("/h2")
                || tag.starts_with("/h3")
                || tag.starts_with("/h4")
                || tag.starts_with("/h5")
                || tag.starts_with("/h6");
            if is_break_tag {
                with_breaks.push('\n');
            }
            rest = &after_lt[gt + 1..];
        } else {
            with_breaks.push('<');
            rest = after_lt;
        }
    }
    with_breaks.push_str(rest);

    let decoded = decode_basic_entities(&with_breaks);
    let mut lines = Vec::new();
    let mut prev_blank = true;

    for raw_line in decoded.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            if !prev_blank {
                lines.push(String::new());
                prev_blank = true;
            }
        } else {
            lines.push(trimmed.to_string());
            prev_blank = false;
        }
    }

    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    lines.join("\n")
}

fn decode_basic_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[derive(Debug, Deserialize)]
struct ConfluenceSearchResponse {
    #[serde(default)]
    results: Vec<ConfluenceContentItem>,
}

#[derive(Debug, Deserialize)]
struct ConfluenceContentItem {
    #[serde(default, deserialize_with = "deserialize_id_string")]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: Option<ConfluenceBody>,
}

impl ConfluenceContentItem {
    fn into_doc_page(self) -> DocPageData {
        let body = self
            .body
            .and_then(|b| b.storage)
            .map(|s| s.value)
            .unwrap_or_default();
        DocPageData {
            id: self.id,
            title: self.title,
            body,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ConfluenceBody {
    #[serde(default)]
    storage: Option<ConfluenceStorage>,
}

#[derive(Debug, Deserialize)]
struct ConfluenceStorage {
    #[serde(default)]
    value: String,
}

fn deserialize_id_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val = serde_json::Value::deserialize(deserializer)?;
    match val {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Ok(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_base_url_handles_schemes_trailing_slashes_and_wiki_suffix() {
        assert_eq!(
            ConfluenceClient::normalize_base_url("example.atlassian.net"),
            "https://example.atlassian.net/wiki"
        );
        assert_eq!(
            ConfluenceClient::normalize_base_url("https://example.atlassian.net/wiki/"),
            "https://example.atlassian.net/wiki"
        );
        assert_eq!(
            ConfluenceClient::normalize_base_url("http://confluence.internal:8090/"),
            "http://confluence.internal:8090/wiki"
        );
    }

    #[test]
    fn auth_header_uses_basic_with_username_and_bearer_without() {
        let basic = ConfluenceClient::new("https://example.atlassian.net", "alice@x.com", "secret");
        let expected_basic = format!("Basic {}", STANDARD.encode("alice@x.com:secret"));
        assert_eq!(basic.auth_header(), expected_basic);

        let bearer = ConfluenceClient::new("https://example.atlassian.net", "", "pat123");
        assert_eq!(bearer.auth_header(), "Bearer pat123");
    }

    #[test]
    fn build_cql_query_escapes_quotes() {
        assert_eq!(
            build_cql_query(r#"Auth "V2" Spec"#),
            r#"title ~ "Auth \"V2\" Spec" and type = "page""#
        );
    }

    #[test]
    fn url_matching_and_page_id_extraction() {
        let client = ConfluenceClient::new("https://acme.atlassian.net/wiki", "u", "t");

        assert!(client.is_doc_page_url(
            "https://acme.atlassian.net/wiki/spaces/ENG/pages/987654/Architecture+Spec"
        ));
        assert!(client.is_doc_page_url(
            "https://acme.atlassian.net/wiki/pages/viewpage.action?pageId=424242"
        ));
        assert!(!client.is_doc_page_url(
            "https://other.atlassian.net/wiki/spaces/ENG/pages/987654/Spec"
        ));
        assert!(!client.is_doc_page_url("https://acme.atlassian.net/wiki/spaces/ENG/overview"));

        assert_eq!(
            client.extract_page_id("https://acme.atlassian.net/wiki/spaces/ENG/pages/12345/Title"),
            Some("12345".to_string())
        );
        assert_eq!(
            client.extract_page_id("https://acme.atlassian.net/wiki/viewpage.action?pageId=555"),
            Some("555".to_string())
        );
        assert_eq!(
            client.extract_page_id("https://acme.atlassian.net/wiki/spaces/ENG/pages/notanid"),
            None
        );
    }

    #[test]
    fn strip_html_for_preview_formats_readable_text() {
        let html = "<h1>Overview</h1><p>First &amp; second<br/>Line two</p><ul><li>Item &quot;A&quot;</li><li>Item &#39;B&#39;</li></ul>";
        let plain = strip_html_for_preview(html);
        assert_eq!(
            plain,
            "Overview\nFirst & second\nLine two\nItem \"A\"\nItem 'B'"
        );
    }

    #[test]
    fn deserialize_search_response() {
        let json = r#"{
            "results": [
                {
                    "id": "101",
                    "title": "Checkout Flow",
                    "body": { "storage": { "value": "<p>Details</p>" } }
                },
                {
                    "id": 202,
                    "title": "Empty Body Page"
                }
            ]
        }"#;
        let parsed: ConfluenceSearchResponse = serde_json::from_str(json).expect("parse response");
        assert_eq!(parsed.results.len(), 2);
        let first = parsed.results[0].id.clone();
        assert_eq!(first, "101");
        let second = parsed.results[1].id.clone();
        assert_eq!(second, "202");
    }
}
