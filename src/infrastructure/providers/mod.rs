pub mod azure_devops;
pub mod confluence;
pub mod github;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

pub use azure_devops::AzureDevOpsClient;
pub use confluence::ConfluenceClient;
pub use github::GitHubClient;

use crate::types::{AppSettings, DocPageData, TicketData};

/// External documentation provider (e.g., Atlassian Confluence).
#[async_trait]
pub trait DocumentationProvider: Send + Sync {
    async fn fetch_page(&self, page_id: &str) -> anyhow::Result<DocPageData>;
    async fn search_pages(&self, query: &str) -> anyhow::Result<Vec<DocPageData>>;
    #[allow(dead_code)]
    fn is_doc_page_url(&self, url: &str) -> bool;
    #[allow(dead_code)]
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

pub(crate) async fn ensure_success_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    service_label: &str,
) -> anyhow::Result<T> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let summary = summarize_error_body(&body);
        if summary.is_empty() {
            anyhow::bail!("{service_label} request failed ({status})");
        }
        anyhow::bail!("{service_label} request failed ({status}): {summary}");
    }
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
