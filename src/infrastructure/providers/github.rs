use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::infrastructure::constants::get_attribution_statement;
use crate::infrastructure::providers::{
    IssueTrackerProvider, build_http_client, ensure_success_json,
};
use crate::types::TicketData;

const GITHUB_API_BASE: &str = "https://api.github.com";
const GITHUB_API_VERSION: &str = "2022-11-28";

/// REST client for GitHub Issues (`api.github.com`).
pub struct GitHubClient {
    client: reqwest::Client,
    token: String,
    default_owner: String,
}

impl GitHubClient {
    pub fn new(token: impl Into<String>, default_owner: impl Into<String>) -> Self {
        Self {
            client: build_http_client(),
            token: token.into().trim().to_string(),
            default_owner: default_owner.into().trim().to_string(),
        }
    }

    fn headers(&self) -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        let auth_val = HeaderValue::from_str(&format!("Bearer {}", self.token))
            .map_err(|err| anyhow::anyhow!("Invalid GitHub token header: {err}"))?;
        headers.insert(AUTHORIZATION, auth_val);
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static(GITHUB_API_VERSION),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static("Stitch-App"));
        Ok(headers)
    }

    /// Parses `"owner/repo/123"` or `"repo/123"` (falling back to `default_owner`).
    pub fn parse_ticket_id(&self, ticket_id: &str) -> anyhow::Result<(String, String, u64)> {
        let trimmed = ticket_id.trim();
        let parts: Vec<&str> = trimmed.split('/').map(str::trim).collect();
        match parts.as_slice() {
            [owner, repo, num_str] if !owner.is_empty() && !repo.is_empty() => {
                let number: u64 = num_str.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "Invalid GitHub issue number in '{trimmed}'. Expected 'owner/repo/123'."
                    )
                })?;
                Ok(((*owner).to_string(), (*repo).to_string(), number))
            }
            [repo, num_str] if !repo.is_empty() => {
                if self.default_owner.is_empty() {
                    anyhow::bail!(
                        "GitHub Default Owner/Org must be configured in Settings > Connectors to use 'repo/123' ticket IDs (or use 'owner/repo/123')."
                    );
                }
                let number: u64 = num_str.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "Invalid GitHub issue number in '{trimmed}'. Expected 'repo-name/123'."
                    )
                })?;
                Ok((self.default_owner.clone(), (*repo).to_string(), number))
            }
            _ => {
                if self.default_owner.is_empty() {
                    anyhow::bail!(
                        "GitHub Default Owner/Org must be configured in Settings > Connectors, or provide a full 'owner/repo/123' ticket ID."
                    );
                }
                anyhow::bail!(
                    "Invalid GitHub ticket ID '{trimmed}'. Include the repository name (e.g. 'repo-name/123' or 'owner/repo-name/123')."
                )
            }
        }
    }
}

#[async_trait]
impl IssueTrackerProvider for GitHubClient {
    async fn fetch_ticket(&self, ticket_id: &str) -> anyhow::Result<TicketData> {
        if self.token.is_empty() {
            anyhow::bail!("GitHub Personal Access Token is required.");
        }
        let (owner, repo, number) = self.parse_ticket_id(ticket_id)?;
        let number_str = number.to_string();
        let mut url = Url::parse(GITHUB_API_BASE)?;
        {
            let mut segs = url
                .path_segments_mut()
                .map_err(|()| anyhow::anyhow!("Invalid GitHub API base URL"))?;
            segs.push("repos")
                .push(&owner)
                .push(&repo)
                .push("issues")
                .push(&number_str);
        }

        let response = self
            .client
            .get(url)
            .headers(self.headers()?)
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("Failed to fetch GitHub issue: {err}"))?;

        let issue: GitHubIssue = ensure_success_json(response, "GitHub").await?;
        Ok(TicketData {
            id: Some(format!("{repo}/{}", issue.number)),
            title: issue.title,
            description: issue.body.unwrap_or_default(),
            acceptance_criteria: None,
        })
    }

    async fn search_tickets(
        &self,
        query: &str,
        work_item_type: Option<&str>,
    ) -> anyhow::Result<Vec<TicketData>> {
        if self.token.is_empty() {
            anyhow::bail!("GitHub Personal Access Token is required.");
        }
        if self.default_owner.is_empty() {
            anyhow::bail!(
                "GitHub Default Owner/Org must be configured in Settings > Connectors to search issues."
            );
        }

        let clean = query.trim();
        if clean.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();
        if self.parse_ticket_id(clean).is_ok() {
            if let Ok(exact) = self.fetch_ticket(clean).await {
                results.push(exact);
            }
        }

        let q = build_search_query(&self.default_owner, clean, work_item_type);
        let mut url = Url::parse(GITHUB_API_BASE)?;
        url.set_path("/search/issues");
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("q", &q);
            pairs.append_pair("per_page", "20");
        }

        let response = self
            .client
            .get(url)
            .headers(self.headers()?)
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("Failed to search GitHub issues: {err}"))?;

        let payload: GitHubSearchResponse =
            ensure_success_json(response, "GitHub search").await?;

        for item in payload.items {
            let repo_name = extract_repo_name(&item.repository_url);
            let composite_id = if repo_name.is_empty() {
                item.number.to_string()
            } else {
                format!("{repo_name}/{}", item.number)
            };
            if results.iter().any(|r| r.id.as_deref() == Some(composite_id.as_str())) {
                continue;
            }
            results.push(TicketData {
                id: Some(composite_id),
                title: item.title,
                description: item.body.unwrap_or_default(),
                acceptance_criteria: None,
            });
            if results.len() >= 20 {
                break;
            }
        }

        Ok(results)
    }

    async fn create_ticket(
        &self,
        work_item_type: &str,
        parent_ticket_id: &str,
        data: &TicketData,
        edited: bool,
    ) -> anyhow::Result<Option<String>> {
        if self.token.is_empty() {
            anyhow::bail!("GitHub Personal Access Token is required.");
        }
        let (owner, repo, parent_number) = self.parse_ticket_id(parent_ticket_id)?;
        let body = build_create_issue_body(data, parent_number, edited);
        let clean_label = work_item_type.trim();
        let labels: Vec<&str> = if clean_label.is_empty() {
            Vec::new()
        } else {
            vec![clean_label]
        };

        let mut create_url = Url::parse(GITHUB_API_BASE)?;
        {
            let mut segs = create_url
                .path_segments_mut()
                .map_err(|()| anyhow::anyhow!("Invalid GitHub API base URL"))?;
            segs.push("repos")
                .push(&owner)
                .push(&repo)
                .push("issues");
        }

        let headers = self.headers()?;
        let response = self
            .client
            .post(create_url)
            .headers(headers.clone())
            .json(&GitHubCreateIssueRequest {
                title: &data.title,
                body: &body,
                labels: &labels,
            })
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("Failed to create GitHub issue: {err}"))?;

        let created: GitHubCreatedIssue = ensure_success_json(response, "GitHub").await?;

        if let Some(sub_issue_id) = created.id {
            let parent_str = parent_number.to_string();
            let mut sub_url = Url::parse(GITHUB_API_BASE)?;
            {
                let mut segs = sub_url
                    .path_segments_mut()
                    .map_err(|()| anyhow::anyhow!("Invalid GitHub API base URL"))?;
                segs.push("repos")
                    .push(&owner)
                    .push(&repo)
                    .push("issues")
                    .push(&parent_str)
                    .push("sub_issues");
            }
            let sub_res = self
                .client
                .post(sub_url)
                .headers(headers)
                .json(&GitHubSubIssueRequest { sub_issue_id })
                .send()
                .await;
            match sub_res {
                Ok(resp) if !resp.status().is_success() => {
                    let status = resp.status();
                    let err_body = resp.text().await.unwrap_or_default();
                    eprintln!(
                        "Warning: failed to link GitHub sub-issue #{0} under #{parent_number} ({status}): {err_body}",
                        created.number
                    );
                }
                Err(err) => {
                    eprintln!(
                        "Warning: failed to link GitHub sub-issue #{0} under #{parent_number}: {err}",
                        created.number
                    );
                }
                _ => {}
            }
        }

        Ok(Some(format!("{repo}/{}", created.number)))
    }
}

pub fn build_create_issue_body(data: &TicketData, parent_number: u64, edited: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !data.description.trim().is_empty() {
        parts.push(data.description.clone());
    }
    if let Some(ac) = data
        .acceptance_criteria
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("### Acceptance Criteria\n\n{ac}"));
    }
    parts.push(get_attribution_statement(edited));
    parts.push(format!("Parent Issue: #{parent_number}"));
    parts.join("\n\n")
}

pub fn build_search_query(default_owner: &str, query: &str, label_type: Option<&str>) -> String {
    let mut q = format!("user:{} is:issue {}", default_owner.trim(), query.trim());
    if let Some(label) = label_type.map(str::trim).filter(|s| !s.is_empty()) {
        q.push_str(&format!(" label:\"{label}\""));
    }
    q
}

pub fn extract_repo_name(repository_url: &str) -> &str {
    repository_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
}

#[derive(Serialize)]
struct GitHubCreateIssueRequest<'a> {
    title: &'a str,
    body: &'a str,
    labels: &'a [&'a str],
}

#[derive(Serialize)]
struct GitHubSubIssueRequest {
    sub_issue_id: u64,
}

#[derive(Debug, Deserialize)]
struct GitHubCreatedIssue {
    id: Option<u64>,
    number: u64,
}

#[derive(Debug, Deserialize)]
struct GitHubIssue {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubSearchResponse {
    #[serde(default)]
    items: Vec<GitHubSearchItem>,
}

#[derive(Debug, Deserialize)]
struct GitHubSearchItem {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    repository_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ticket_id_supports_two_and_three_segments() {
        let client = GitHubClient::new("tok", "octocat");
        assert_eq!(
            client.parse_ticket_id("stitch/42").unwrap(),
            ("octocat".to_string(), "stitch".to_string(), 42)
        );
        assert_eq!(
            client.parse_ticket_id("acme/web/99").unwrap(),
            ("acme".to_string(), "web".to_string(), 99)
        );
        assert!(client.parse_ticket_id("42").is_err());

        let no_owner = GitHubClient::new("tok", "");
        assert!(no_owner.parse_ticket_id("stitch/42").is_err());
        assert_eq!(
            no_owner.parse_ticket_id("acme/web/99").unwrap(),
            ("acme".to_string(), "web".to_string(), 99)
        );
    }

    #[test]
    fn build_search_query_and_extract_repo_name() {
        assert_eq!(
            build_search_query("octocat", "oauth login", Some("Feature")),
            "user:octocat is:issue oauth login label:\"Feature\""
        );
        assert_eq!(
            build_search_query("octocat", "oauth", None),
            "user:octocat is:issue oauth"
        );
        assert_eq!(
            extract_repo_name("https://api.github.com/repos/octocat/stitch-rs"),
            "stitch-rs"
        );
    }

    #[test]
    fn deserialize_github_issue_and_search() {
        let issue_json = r#"{"number": 12, "title": "Add SSO", "body": "Support SAML"}"#;
        let issue: GitHubIssue = serde_json::from_str(issue_json).expect("issue");
        assert_eq!(issue.number, 12);
        assert_eq!(issue.title, "Add SSO");
        assert_eq!(issue.body.as_deref(), Some("Support SAML"));

        let search_json = r#"{
            "items": [
                {
                    "number": 7,
                    "title": "SSO Epic",
                    "body": null,
                    "repository_url": "https://api.github.com/repos/octocat/platform"
                }
            ]
        }"#;
        let search: GitHubSearchResponse = serde_json::from_str(search_json).expect("search");
        assert_eq!(search.items.len(), 1);
        assert_eq!(
            extract_repo_name(&search.items[0].repository_url),
            "platform"
        );
    }

    #[test]
    fn build_create_issue_body_and_deserialize_created_issue() {
        let ticket = TicketData {
            id: None,
            title: "Add dark mode".to_string(),
            description: "As a user, I want dark mode.".to_string(),
            acceptance_criteria: Some("* Toggle persists".to_string()),
        };
        let body = build_create_issue_body(&ticket, 42, true);
        assert!(body.contains("As a user, I want dark mode."));
        assert!(body.contains("### Acceptance Criteria\n\n* Toggle persists"));
        assert!(body.contains("> Assisted by "));
        assert!(body.ends_with("Parent Issue: #42"));

        let created_json = r#"{"id": 998877, "number": 105}"#;
        let created: GitHubCreatedIssue =
            serde_json::from_str(created_json).expect("created issue");
        assert_eq!(created.id, Some(998877));
        assert_eq!(created.number, 105);
    }
}
