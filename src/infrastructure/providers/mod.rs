pub mod azure_devops;
pub mod confluence;
pub mod github;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

pub use azure_devops::AzureDevOpsClient;
pub use confluence::ConfluenceClient;
pub use github::GitHubClient;

use crate::types::{AppSettings, DocPageData, KnownDoc, TicketData};

/// External documentation provider (e.g., Atlassian Confluence).
#[async_trait]
pub trait DocumentationProvider: Send + Sync {
    async fn fetch_page(&self, page_id: &str) -> anyhow::Result<DocPageData>;
    async fn search_pages(&self, query: &str) -> anyhow::Result<Vec<DocPageData>>;
    fn is_doc_page_url(&self, url: &str) -> bool;
    fn extract_page_id(&self, url: &str) -> Option<String>;
}

/// External issue/work-item tracker provider (e.g., Azure DevOps, GitHub Issues).
#[async_trait]
pub trait IssueTrackerProvider: Send + Sync {
    async fn fetch_ticket(&self, ticket_id: &str) -> anyhow::Result<TicketData>;
    async fn search_tickets(
        &self,
        query: &str,
        work_item_type: Option<&str>,
    ) -> anyhow::Result<Vec<TicketData>>;
    async fn create_ticket(
        &self,
        work_item_type: &str,
        parent_ticket_id: &str,
        data: &TicketData,
        edited: bool,
    ) -> anyhow::Result<Option<String>>;
    async fn add_comment(&self, ticket_id: &str, text: &str, edited: bool) -> anyhow::Result<()>;
}

/// Builds the active `DocumentationProvider` based on `settings.sources.docs`.
pub fn build_documentation_provider(
    settings: &AppSettings,
) -> anyhow::Result<Arc<dyn DocumentationProvider>> {
    match settings.sources.docs.trim() {
        "atlassian" => {
            let url = settings.connectors.atlassian.url.trim();
            let token = settings
                .connectors
                .atlassian
                .token
                .as_deref()
                .map(str::trim)
                .unwrap_or("");
            if url.is_empty() || token.is_empty() {
                anyhow::bail!(
                    "Atlassian Confluence URL and API Token are required. Configure them in Settings > Connectors."
                );
            }
            Ok(Arc::new(ConfluenceClient::new(
                url,
                settings.connectors.atlassian.username.trim(),
                token,
            )))
        }
        "" => anyhow::bail!(
            "No Documentation source is configured. Open Settings > Connectors to connect Atlassian Confluence and route Documentation Source."
        ),
        other => anyhow::bail!("Unsupported documentation source: {other}"),
    }
}

/// Builds the active `IssueTrackerProvider` based on `settings.sources.issues`.
pub fn build_issue_tracker_provider(
    settings: &AppSettings,
) -> anyhow::Result<Arc<dyn IssueTrackerProvider>> {
    match settings.sources.issues.trim() {
        "azureDevOps" => {
            let org = settings.connectors.azure_dev_ops.org.trim();
            let pat = settings
                .connectors
                .azure_dev_ops
                .pat
                .as_deref()
                .map(str::trim)
                .unwrap_or("");
            if org.is_empty() || pat.is_empty() {
                anyhow::bail!(
                    "Azure DevOps Organization URL and PAT are required. Configure them in Settings > Connectors."
                );
            }
            Ok(Arc::new(AzureDevOpsClient::new(org, pat)))
        }
        "github" => {
            let token = settings
                .connectors
                .github
                .token
                .as_deref()
                .map(str::trim)
                .unwrap_or("");
            if token.is_empty() {
                anyhow::bail!(
                    "GitHub Personal Access Token is required. Configure it in Settings > Connectors."
                );
            }
            Ok(Arc::new(GitHubClient::new(
                token,
                settings.connectors.github.owner.trim(),
            )))
        }
        "" => anyhow::bail!(
            "No Issues Tracker source is configured. Open Settings > Connectors to connect Azure DevOps or GitHub and route Issues Tracker Source."
        ),
        other => anyhow::bail!("Unsupported issues tracker source: {other}"),
    }
}

pub(crate) fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn check_response_status(
    response: reqwest::Response,
    service_label: &str,
) -> anyhow::Result<reqwest::Response> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let summary = summarize_error_body(&body);
        if summary.is_empty() {
            anyhow::bail!("{service_label} request failed ({status})");
        }
        anyhow::bail!("{service_label} request failed ({status}): {summary}");
    }
    Ok(response)
}

pub(crate) async fn ensure_success(
    response: reqwest::Response,
    service_label: &str,
) -> anyhow::Result<()> {
    check_response_status(response, service_label).await?;
    Ok(())
}

pub(crate) async fn ensure_success_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    service_label: &str,
) -> anyhow::Result<T> {
    let response = check_response_status(response, service_label).await?;
    response
        .json::<T>()
        .await
        .map_err(|err| anyhow::anyhow!("Failed to parse {service_label} JSON response: {err}"))
}

fn summarize_error_body(raw: &str) -> String {
    let compact: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_LEN: usize = 400;
    if compact.chars().count() <= MAX_LEN {
        compact
    } else {
        let prefix: String = compact.chars().take(MAX_LEN).collect();
        format!("{prefix}...")
    }
}

/// Extracts unique HTTP(S) URLs from HTML `href` attributes and plain-text links, preserving order.
pub fn extract_urls(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut urls = Vec::new();
    let mut seen = HashSet::new();

    // Pass 1: Extract href="https://..." or href='https://...' attributes cleanly.
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("href") {
        let after_kw = pos + rel + 4;
        pos = after_kw;

        let mut i = after_kw;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || (bytes[i] != b'"' && bytes[i] != b'\'') {
            continue;
        }
        let quote = bytes[i] as char;
        let val_start = i + 1;
        let Some(end_rel) = text[val_start..].find(quote) else {
            continue;
        };
        let val_end = val_start + end_rel;
        let raw_val = text[val_start..val_end].trim();
        let raw_val_lower = lower[val_start..val_end].trim_start();
        if raw_val_lower.starts_with("http://") || raw_val_lower.starts_with("https://") {
            let cleaned = raw_val.replace("&amp;", "&");
            if !cleaned.is_empty() && seen.insert(cleaned.clone()) {
                urls.push(cleaned);
            }
        }
        pos = val_end + 1;
    }

    // Pass 2: Extract raw HTTP(S) URLs outside/alongside text, stripping trailing punctuation.
    let mut scan = 0;
    while scan < lower.len() {
        let next_https = lower[scan..].find("https://");
        let next_http = lower[scan..].find("http://");
        let rel = match (next_https, next_http) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => break,
        };
        let url_start = scan + rel;
        let mut end_rel = text[url_start..].len();
        for (idx, ch) in text[url_start..].char_indices() {
            if ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>') {
                end_rel = idx;
                break;
            }
        }
        let url_end = url_start + end_rel;
        let trimmed = text[url_start..url_end]
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')'])
            .replace("&amp;", "&");
        if trimmed.len() > "http://".len() && seen.insert(trimmed.clone()) {
            urls.push(trimmed);
        }
        scan = url_end.max(url_start + 1);
    }

    urls
}

/// Scans text fields for Confluence documentation URLs, deduplicates IDs, and fetches titles.
pub async fn discover_known_docs(
    doc_provider: &dyn DocumentationProvider,
    texts: &[&str],
) -> Vec<KnownDoc> {
    let mut page_ids = Vec::new();
    let mut seen_ids = HashSet::new();

    for text in texts {
        for url in extract_urls(text) {
            if !doc_provider.is_doc_page_url(&url) {
                continue;
            }
            if let Some(page_id) = doc_provider.extract_page_id(&url) {
                if seen_ids.insert(page_id.clone()) {
                    page_ids.push(page_id);
                }
            }
        }
    }

    let mut known_docs = Vec::with_capacity(page_ids.len());
    for page_id in page_ids {
        match doc_provider.fetch_page(&page_id).await {
            Ok(page) => known_docs.push(KnownDoc {
                id: page.id,
                title: page.title,
            }),
            Err(err) => {
                eprintln!(
                    "Warning: failed to fetch metadata for Confluence page {page_id}: {err}"
                );
                known_docs.push(KnownDoc {
                    id: page_id.clone(),
                    title: format!("Document ID: {page_id}"),
                });
            }
        }
    }

    known_docs
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockDocProvider;

    #[async_trait]
    impl DocumentationProvider for MockDocProvider {
        async fn fetch_page(&self, page_id: &str) -> anyhow::Result<DocPageData> {
            match page_id {
                "12345" => Ok(DocPageData {
                    id: "12345".to_string(),
                    title: "API Auth Spec".to_string(),
                    body: "<p>OAuth flow</p>".to_string(),
                }),
                _ => anyhow::bail!("404 Not Found"),
            }
        }

        async fn search_pages(&self, _query: &str) -> anyhow::Result<Vec<DocPageData>> {
            Ok(Vec::new())
        }

        fn is_doc_page_url(&self, url: &str) -> bool {
            url.contains("example.atlassian.net")
        }

        fn extract_page_id(&self, url: &str) -> Option<String> {
            ConfluenceClient::new("https://example.atlassian.net", "", "tok").extract_page_id(url)
        }
    }

    #[test]
    fn extract_urls_handles_html_href_and_raw_links_with_trailing_punctuation() {
        let html = r#"
            See <A HREF="https://example.atlassian.net/wiki/spaces/ENG/pages/12345/Spec?a=1&amp;b=2">Spec</A>
            and raw link (https://example.atlassian.net/wiki/pages/viewpage.action?pageId=99999),
            plus duplicate https://example.atlassian.net/wiki/spaces/ENG/pages/12345/Spec?a=1&b=2.
            External link: http://other.example.com/path!
        "#;
        let urls = extract_urls(html);
        assert_eq!(
            urls,
            vec![
                "https://example.atlassian.net/wiki/spaces/ENG/pages/12345/Spec?a=1&b=2",
                "https://example.atlassian.net/wiki/pages/viewpage.action?pageId=99999",
                "http://other.example.com/path",
            ]
        );
    }

    #[tokio::test]
    async fn discover_known_docs_filters_deduplicates_and_falls_back_on_fetch_error() {
        let provider = MockDocProvider;
        let desc = r#"
            <p>Read <a href="https://example.atlassian.net/wiki/spaces/ENG/pages/12345/Spec">Auth</a>
            and https://unrelated.site/wiki/pages/55555/Ignore.</p>
        "#;
        let ac = "Also verify https://example.atlassian.net/wiki/pages/viewpage.action?pageId=12345 and https://example.atlassian.net/wiki/spaces/ENG/pages/67890/Missing.";

        let docs = discover_known_docs(&provider, &[desc, ac]).await;
        assert_eq!(
            docs,
            vec![
                KnownDoc {
                    id: "12345".to_string(),
                    title: "API Auth Spec".to_string(),
                },
                KnownDoc {
                    id: "67890".to_string(),
                    title: "Document ID: 67890".to_string(),
                },
            ]
        );
    }
}
