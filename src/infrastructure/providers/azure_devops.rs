use std::collections::HashMap;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

use crate::infrastructure::constants::get_attribution_statement;
use crate::infrastructure::providers::{
    CodeReviewProvider, IssueTrackerProvider, build_http_client, ensure_success,
    ensure_success_json, strip_git_suffix,
};
use crate::types::{ParsedPrUrl, ParsedRemoteUrl, PrHostType, PrMetadata, PrSearchType, TicketData};

const API_VERSION: &str = "7.1";

/// REST client for Azure DevOps Work Item Tracking (`_apis/wit`).
pub struct AzureDevOpsClient {
    client: reqwest::Client,
    org: String,
    pat: String,
}

impl AzureDevOpsClient {
    pub fn new(org: impl Into<String>, pat: impl Into<String>) -> Self {
        Self {
            client: build_http_client(),
            org: org.into().trim().to_string(),
            pat: pat.into().trim().to_string(),
        }
    }

    pub fn normalize_org_url(org: &str) -> String {
        let trimmed = org.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return String::new();
        }
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            trimmed.to_string()
        } else {
            format!("https://dev.azure.com/{trimmed}")
        }
    }

    fn auth_header(&self) -> String {
        let encoded = STANDARD.encode(format!(":{}", self.pat));
        format!("Basic {encoded}")
    }

    fn validate_credentials(&self) -> anyhow::Result<String> {
        if self.org.is_empty() || self.pat.is_empty() {
            anyhow::bail!("Azure DevOps Organization URL and PAT are required.");
        }
        let base = Self::normalize_org_url(&self.org);
        if base.is_empty() {
            anyhow::bail!("Azure DevOps Organization URL is invalid.");
        }
        Ok(base)
    }

    fn build_api_url(
        &self,
        segments: &[&str],
        query_pairs: &[(&str, &str)],
    ) -> anyhow::Result<Url> {
        let base = self.validate_credentials()?;
        let mut url = Url::parse(&base)
            .map_err(|err| anyhow::anyhow!("Invalid Azure DevOps Organization URL '{base}': {err}"))?;
        {
            let mut path_segments = url.path_segments_mut().map_err(|()| {
                anyhow::anyhow!("Azure DevOps Organization URL cannot be a base URL: '{base}'")
            })?;
            path_segments.pop_if_empty();
            for segment in segments {
                path_segments.push(segment);
            }
        }
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query_pairs {
                pairs.append_pair(key, value);
            }
            pairs.append_pair("api-version", API_VERSION);
        }
        Ok(url)
    }

    async fn fetch_work_item_raw(&self, id: u64) -> anyhow::Result<AdoWorkItem> {
        let id_str = id.to_string();
        let url = self.build_api_url(&["_apis", "wit", "workitems", &id_str], &[])?;
        let response = self
            .client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("Failed to connect to Azure DevOps: {err}"))?;
        ensure_success_json(response, "Azure DevOps").await
    }

    /// Lists all configured Work Item Type names for a given Azure DevOps project.
    pub async fn get_work_item_types(&self, project: &str) -> anyhow::Result<Vec<String>> {
        let clean_project = project.trim();
        if clean_project.is_empty() {
            anyhow::bail!("Azure DevOps Project Name is required to fetch work item types.");
        }
        let url = self.build_api_url(&[clean_project, "_apis", "wit", "workitemtypes"], &[])?;
        let response = self
            .client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|err| {
                anyhow::anyhow!("Failed to fetch Azure DevOps work item types: {err}")
            })?;
        let payload: AdoWorkItemTypesResponse =
            ensure_success_json(response, "Azure DevOps work item types").await?;
        Ok(payload
            .value
            .into_iter()
            .map(|t| t.name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect())
    }
}

#[async_trait]
impl IssueTrackerProvider for AzureDevOpsClient {
    async fn fetch_ticket(&self, ticket_id: &str) -> anyhow::Result<TicketData> {
        let clean_id = ticket_id.trim();
        let numeric_id: u64 = clean_id.parse().map_err(|_| {
            anyhow::anyhow!("Invalid Azure DevOps Work Item ID '{clean_id}'. Expected a number.")
        })?;
        let item = self.fetch_work_item_raw(numeric_id).await?;
        Ok(item.into_ticket_data())
    }

    async fn search_tickets(
        &self,
        query: &str,
        work_item_type: Option<&str>,
    ) -> anyhow::Result<Vec<TicketData>> {
        self.validate_credentials()?;
        let clean_query = query.trim();
        if clean_query.is_empty() {
            return Ok(Vec::new());
        }

        let type_filter = work_item_type.map(str::trim).filter(|s| !s.is_empty());

        let exact_match = if let Ok(numeric_id) = clean_query.parse::<u64>() {
            match self.fetch_work_item_raw(numeric_id).await {
                Ok(item) => {
                    let type_matches = match type_filter {
                        None => true,
                        Some(expected) => item
                            .fields
                            .work_item_type
                            .as_deref()
                            .is_some_and(|actual| actual.eq_ignore_ascii_case(expected)),
                    };
                    if type_matches {
                        Some((numeric_id, item.into_ticket_data()))
                    } else {
                        None
                    }
                }
                Err(_) => None,
            }
        } else {
            None
        };

        let wiql = build_wiql_query(clean_query, type_filter);
        let wiql_url = self.build_api_url(&["_apis", "wit", "wiql"], &[])?;
        let wiql_response = self
            .client
            .post(wiql_url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&WiqlRequest { query: &wiql })
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("Failed to run Azure DevOps WIQL search: {err}"))?;

        let wiql_data: WiqlResponse =
            ensure_success_json(wiql_response, "Azure DevOps WIQL").await?;

        let exact_id = exact_match.as_ref().map(|(id, _)| *id);
        let limit = if exact_id.is_some() { 19 } else { 20 };

        let ids: Vec<u64> = wiql_data
            .work_items
            .into_iter()
            .filter_map(|ref_item| ref_item.id)
            .filter(|id| exact_id != Some(*id))
            .take(limit)
            .collect();

        let mut results = Vec::new();
        if let Some((_, ticket)) = exact_match {
            results.push(ticket);
        }

        if !ids.is_empty() {
            let csv = ids
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let batch_url = self.build_api_url(
                &["_apis", "wit", "workitems"],
                &[
                    ("ids", &csv),
                    (
                        "fields",
                        "System.Id,System.Title,System.Description,Microsoft.VSTS.Common.AcceptanceCriteria",
                    ),
                ],
            )?;
            let batch_response = self
                .client
                .get(batch_url)
                .header(reqwest::header::AUTHORIZATION, self.auth_header())
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(|err| {
                    anyhow::anyhow!("Failed to batch-fetch Azure DevOps work items: {err}")
                })?;

            let batch: AdoBatchWorkItemsResponse =
                ensure_success_json(batch_response, "Azure DevOps work items").await?;

            let mut by_id: HashMap<u64, TicketData> = batch
                .value
                .into_iter()
                .filter_map(|item| {
                    let id = item.id?;
                    Some((id, item.into_ticket_data()))
                })
                .collect();

            for id in ids {
                if let Some(ticket) = by_id.remove(&id) {
                    results.push(ticket);
                }
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
        self.validate_credentials()?;
        let clean_type = work_item_type.trim();
        if clean_type.is_empty() {
            anyhow::bail!("Azure DevOps Work Item Type is required to create a ticket.");
        }
        let clean_parent = parent_ticket_id.trim();
        let parent_num: u64 = clean_parent.parse().map_err(|_| {
            anyhow::anyhow!(
                "Invalid parent Azure DevOps Work Item ID '{clean_parent}'. Expected a number."
            )
        })?;

        let parent = self.fetch_work_item_raw(parent_num).await?;
        let project = parent
            .fields
            .team_project
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("Parent work item #{parent_num} is missing System.TeamProject.")
            })?;
        let parent_url = parent
            .url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("Parent work item #{parent_num} is missing its relation URL.")
            })?;

        let patch = build_create_work_item_patch(data, parent_url, edited);
        let type_segment = format!("${clean_type}");
        let url = self.build_api_url(&[project, "_apis", "wit", "workitems", &type_segment], &[])?;
        let body_bytes = serde_json::to_vec(&patch)?;

        let response = self
            .client
            .post(url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json-patch+json")
            .body(body_bytes)
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("Failed to create Azure DevOps work item: {err}"))?;

        let created: AdoWorkItem = ensure_success_json(response, "Azure DevOps").await?;
        Ok(created.id.map(|id| id.to_string()))
    }

    async fn add_comment(&self, ticket_id: &str, text: &str, edited: bool) -> anyhow::Result<()> {
        self.validate_credentials()?;
        let clean_id = ticket_id.trim();
        let numeric_id: u64 = clean_id.parse().map_err(|_| {
            anyhow::anyhow!("Invalid Azure DevOps Work Item ID '{clean_id}'. Expected a number.")
        })?;
        let id_str = numeric_id.to_string();
        let url = self.build_api_url(&["_apis", "wit", "workitems", &id_str], &[])?;
        let patch = build_add_comment_patch(text, edited);
        let body_bytes = serde_json::to_vec(&patch)?;

        let response = self
            .client
            .patch(url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json-patch+json")
            .body(body_bytes)
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("Failed to post Azure DevOps comment: {err}"))?;

        ensure_success(response, "Azure DevOps").await
    }
}

fn append_attribution(text: &str, edited: bool) -> String {
    let attribution = get_attribution_statement(edited);
    if text.trim().is_empty() {
        attribution
    } else {
        format!("{text}\n\n{attribution}")
    }
}

pub fn build_add_comment_patch(text: &str, edited: bool) -> Vec<serde_json::Value> {
    let comment_with_attribution = append_attribution(text, edited);
    vec![
        json!({
            "op": "add",
            "path": "/fields/System.History",
            "value": comment_with_attribution,
        }),
        json!({
            "op": "add",
            "path": "/multilineFieldsFormat/System.History",
            "value": "Markdown",
        }),
    ]
}

pub fn build_create_work_item_patch(
    data: &TicketData,
    parent_url: &str,
    edited: bool,
) -> Vec<serde_json::Value> {
    let description_with_attribution = append_attribution(&data.description, edited);

    let mut ops = vec![
        json!({
            "op": "add",
            "path": "/fields/System.Title",
            "value": data.title,
        }),
        json!({
            "op": "add",
            "path": "/fields/System.Description",
            "value": description_with_attribution,
        }),
        json!({
            "op": "add",
            "path": "/multilineFieldsFormat/System.Description",
            "value": "Markdown",
        }),
        json!({
            "op": "add",
            "path": "/relations/-",
            "value": {
                "rel": "System.LinkTypes.Hierarchy-Reverse",
                "url": parent_url,
                "attributes": { "comment": "Created via Stitch" }
            },
        }),
    ];

    if let Some(ac) = data
        .acceptance_criteria
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        ops.push(json!({
            "op": "add",
            "path": "/fields/Microsoft.VSTS.Common.AcceptanceCriteria",
            "value": ac,
        }));
        ops.push(json!({
            "op": "add",
            "path": "/multilineFieldsFormat/Microsoft.VSTS.Common.AcceptanceCriteria",
            "value": "Markdown",
        }));
    }

    ops
}

pub fn build_wiql_query(query: &str, work_item_type: Option<&str>) -> String {
    let escaped_query = query.trim().replace('\'', "''");
    let mut wiql = format!(
        "Select [System.Id], [System.Title] From WorkItems Where [System.Title] Contains '{escaped_query}'"
    );
    if let Some(wi_type) = work_item_type.map(str::trim).filter(|s| !s.is_empty()) {
        let escaped_type = wi_type.replace('\'', "''");
        wiql.push_str(&format!(" And [System.WorkItemType] = '{escaped_type}'"));
    }
    wiql.push_str(" Order By [System.Id] Desc");
    wiql
}

#[derive(Serialize)]
struct WiqlRequest<'a> {
    query: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WiqlResponse {
    #[serde(default)]
    work_items: Vec<WiqlWorkItemRef>,
}

#[derive(Debug, Deserialize)]
struct WiqlWorkItemRef {
    id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AdoBatchWorkItemsResponse {
    #[serde(default)]
    value: Vec<AdoWorkItem>,
}

#[derive(Debug, Deserialize)]
struct AdoWorkItem {
    id: Option<u64>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    fields: AdoWorkItemFields,
}

impl AdoWorkItem {
    fn into_ticket_data(self) -> TicketData {
        let ac = self
            .fields
            .acceptance_criteria
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        TicketData {
            id: self.id.map(|id| id.to_string()),
            title: self.fields.title.unwrap_or_default(),
            description: self.fields.description.unwrap_or_default(),
            acceptance_criteria: ac,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct AdoWorkItemFields {
    #[serde(rename = "System.Title")]
    title: Option<String>,
    #[serde(rename = "System.Description")]
    description: Option<String>,
    #[serde(rename = "Microsoft.VSTS.Common.AcceptanceCriteria")]
    acceptance_criteria: Option<String>,
    #[serde(rename = "System.WorkItemType")]
    work_item_type: Option<String>,
    #[serde(rename = "System.TeamProject")]
    team_project: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdoWorkItemTypesResponse {
    #[serde(default)]
    value: Vec<AdoWorkItemType>,
}

#[derive(Debug, Deserialize)]
struct AdoWorkItemType {
    #[serde(default)]
    name: String,
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn build_ado_api_url(
    base_org_url: &str,
    segments: &[&str],
    query_pairs: &[(&str, &str)],
    include_api_version: bool,
) -> anyhow::Result<Url> {
    let mut url = Url::parse(base_org_url).map_err(|err| {
        anyhow::anyhow!("Invalid Azure DevOps Organization URL '{base_org_url}': {err}")
    })?;
    {
        let mut path_segments = url.path_segments_mut().map_err(|()| {
            anyhow::anyhow!("Azure DevOps Organization URL cannot be a base URL: '{base_org_url}'")
        })?;
        path_segments.pop_if_empty();
        for segment in segments {
            path_segments.push(segment);
        }
    }
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query_pairs {
            pairs.append_pair(key, value);
        }
        if include_api_version {
            pairs.append_pair("api-version", API_VERSION);
        }
    }
    Ok(url)
}

/// REST client for Azure DevOps Git Pull Requests (`_apis/git`).
pub struct AzureDevOpsCodeReviewClient {
    client: reqwest::Client,
    org: String,
    pat: String,
    default_project: String,
}

impl AzureDevOpsCodeReviewClient {
    pub fn new(
        org: impl Into<String>,
        pat: impl Into<String>,
        default_project: impl Into<String>,
    ) -> Self {
        Self {
            client: build_http_client(),
            org: org.into().trim().to_string(),
            pat: pat.into().trim().to_string(),
            default_project: default_project.into().trim().to_string(),
        }
    }

    fn auth_header(&self) -> String {
        let encoded = STANDARD.encode(format!(":{}", self.pat));
        format!("Basic {encoded}")
    }

    async fn resolve_current_user_id(&self, base_org_url: &str) -> anyhow::Result<String> {
        let url = build_ado_api_url(base_org_url, &["_apis", "connectionData"], &[], false)?;
        let response = self
            .client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("Failed to connect to Azure DevOps: {err}"))?;
        let data: AdoConnectionData =
            ensure_success_json(response, "Azure DevOps connectionData").await?;
        data.authorized_user
            .and_then(|u| u.id)
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Could not resolve current authenticated Azure DevOps user ID."))
    }

    fn map_pr_to_metadata(org_url: &str, project: &str, pr: AdoPullRequest) -> PrMetadata {
        let clean_ref = |r: String| {
            r.trim()
                .strip_prefix("refs/heads/")
                .map(str::to_string)
                .unwrap_or_else(|| r.trim().to_string())
        };
        let pr_id = pr
            .pull_request_id
            .map(|id| id.to_string())
            .unwrap_or_default();
        let repo_name = pr
            .repository
            .as_ref()
            .and_then(|r| r.name.clone())
            .unwrap_or_default();
        let repo_id = pr.repository.as_ref().and_then(|r| r.id.clone());
        let base_trimmed = org_url.trim_end_matches('/');
        let web_url = (!pr_id.is_empty() && !project.is_empty() && !repo_name.is_empty())
            .then(|| format!("{base_trimmed}/{project}/_git/{repo_name}/pullrequest/{pr_id}"));

        let author = pr
            .created_by
            .as_ref()
            .and_then(|c| c.display_name.clone())
            .unwrap_or_default();
        let author_unique_name = pr
            .created_by
            .as_ref()
            .and_then(|c| c.unique_name.clone())
            .unwrap_or_default();

        PrMetadata {
            id: pr_id,
            title: pr.title.unwrap_or_default(),
            description: pr.description.unwrap_or_default(),
            source_branch: clean_ref(pr.source_ref_name.unwrap_or_default()),
            target_branch: clean_ref(pr.target_ref_name.unwrap_or_default()),
            author,
            author_unique_name,
            repository_name: repo_name,
            repository_id: repo_id,
            host_type: PrHostType::Azure,
            url: web_url,
        }
    }
}

#[async_trait]
impl CodeReviewProvider for AzureDevOpsCodeReviewClient {
    fn parse_pr_url(&self, url: &str) -> Option<ParsedPrUrl> {
        let parsed = Url::parse(url.trim()).ok()?;
        let host = parsed.host_str()?.to_ascii_lowercase();
        let raw_segments: Vec<String> = parsed
            .path_segments()?
            .filter(|s| !s.is_empty())
            .map(percent_decode)
            .collect();

        if host == "dev.azure.com" {
            // /{org}/{project}/_git/{repo}/pullrequest/{id}
            if raw_segments.len() >= 6
                && raw_segments[2].eq_ignore_ascii_case("_git")
                && raw_segments[4].eq_ignore_ascii_case("pullrequest")
            {
                let pr_number = raw_segments[5].parse::<u64>().ok()?;
                return Some(ParsedPrUrl {
                    org: raw_segments[0].clone(),
                    project: raw_segments[1].clone(),
                    repo_name: raw_segments[3].clone(),
                    pr_number,
                });
            }
        } else if let Some(org_prefix) = host.strip_suffix(".visualstudio.com") {
            if !org_prefix.is_empty() {
                let segs = if raw_segments
                    .first()
                    .is_some_and(|s| s.eq_ignore_ascii_case("defaultcollection"))
                {
                    &raw_segments[1..]
                } else {
                    &raw_segments[..]
                };
                if segs.len() >= 5
                    && segs[1].eq_ignore_ascii_case("_git")
                    && segs[3].eq_ignore_ascii_case("pullrequest")
                {
                    let pr_number = segs[4].parse::<u64>().ok()?;
                    return Some(ParsedPrUrl {
                        org: org_prefix.to_string(),
                        project: segs[0].clone(),
                        repo_name: segs[2].clone(),
                        pr_number,
                    });
                }
            }
        }

        None
    }

    fn parse_remote_url(&self, url: &str) -> Option<ParsedRemoteUrl> {
        let trimmed = url.trim();
        // SSH: git@ssh.dev.azure.com:v3/{org}/{project}/{repo}
        if let Some(rest) = trimmed
            .strip_prefix("git@ssh.dev.azure.com:v3/")
            .or_else(|| trimmed.strip_prefix("ssh://git@ssh.dev.azure.com/v3/"))
        {
            let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
            if parts.len() >= 3 {
                return Some(ParsedRemoteUrl {
                    org: percent_decode(parts[0]),
                    project: percent_decode(parts[1]),
                    repo_name: strip_git_suffix(&percent_decode(parts[2])),
                });
            }
            return None;
        }

        let parsed = Url::parse(trimmed).ok()?;
        let host = parsed.host_str()?.to_ascii_lowercase();
        let raw_segments: Vec<String> = parsed
            .path_segments()?
            .filter(|s| !s.is_empty())
            .map(percent_decode)
            .collect();

        if host == "dev.azure.com" {
            // /{org}/{project}/_git/{repo}
            if raw_segments.len() >= 4 && raw_segments[2].eq_ignore_ascii_case("_git") {
                return Some(ParsedRemoteUrl {
                    org: raw_segments[0].clone(),
                    project: raw_segments[1].clone(),
                    repo_name: strip_git_suffix(&raw_segments[3]),
                });
            }
        } else if let Some(org_prefix) = host.strip_suffix(".visualstudio.com") {
            if !org_prefix.is_empty() {
                let segs = if raw_segments
                    .first()
                    .is_some_and(|s| s.eq_ignore_ascii_case("defaultcollection"))
                {
                    &raw_segments[1..]
                } else {
                    &raw_segments[..]
                };
                if segs.len() >= 3 && segs[1].eq_ignore_ascii_case("_git") {
                    return Some(ParsedRemoteUrl {
                        org: org_prefix.to_string(),
                        project: segs[0].clone(),
                        repo_name: strip_git_suffix(&segs[2]),
                    });
                }
            }
        }

        None
    }

    async fn get_pr_details(
        &self,
        pr_url_or_id: &str,
        remote_url: Option<&str>,
    ) -> anyhow::Result<PrMetadata> {
        if self.pat.is_empty() {
            anyhow::bail!("Azure DevOps Personal Access Token is required.");
        }

        let trimmed = pr_url_or_id.trim();
        let mut org = self.org.clone();
        let mut project = self.default_project.clone();

        let pr_number = if let Some(parsed) = self.parse_pr_url(trimmed) {
            org = parsed.org;
            project = parsed.project;
            parsed.pr_number
        } else {
            let num: u64 = trimmed.parse().map_err(|_| {
                anyhow::anyhow!("Invalid Pull Request URL or ID format: \"{trimmed}\"")
            })?;
            if let Some(remote) = remote_url.and_then(|u| self.parse_remote_url(u)) {
                if org.is_empty() {
                    org = remote.org;
                }
                if project.is_empty() {
                    project = remote.project;
                }
            }
            num
        };

        if org.trim().is_empty() {
            anyhow::bail!(
                "Azure DevOps Organization is not configured in settings and could not be detected from git remote."
            );
        }
        if project.trim().is_empty() {
            anyhow::bail!(
                "Azure DevOps Project is not configured in settings and could not be detected from git remote."
            );
        }

        let org_url = AzureDevOpsClient::normalize_org_url(&org);
        let pr_str = pr_number.to_string();
        let url = build_ado_api_url(
            &org_url,
            &[project.trim(), "_apis", "git", "pullrequests", &pr_str],
            &[],
            true,
        )?;

        let response = self
            .client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("Failed to fetch PR from Azure DevOps API: {err}"))?;

        let pr: AdoPullRequest = ensure_success_json(response, "Azure DevOps PR").await?;
        if pr
            .target_ref_name
            .as_deref()
            .map(str::trim)
            .is_none_or(|s| s.is_empty())
        {
            anyhow::bail!("Pull Request #{pr_number} is missing target branch ref.");
        }

        Ok(Self::map_pr_to_metadata(&org_url, project.trim(), pr))
    }

    async fn get_project_prs(&self, search_type: PrSearchType) -> anyhow::Result<Vec<PrMetadata>> {
        if self.org.trim().is_empty() || self.pat.is_empty() {
            anyhow::bail!("Azure DevOps Organization URL and PAT are required.");
        }
        if self.default_project.trim().is_empty() {
            anyhow::bail!(
                "Azure DevOps Project Name must be configured in Settings > Connectors to list PRs."
            );
        }

        let org_url = AzureDevOpsClient::normalize_org_url(&self.org);
        let mut query_pairs: Vec<(&str, String)> =
            vec![("searchCriteria.status", "active".to_string())];

        match search_type {
            PrSearchType::Assigned => {
                let user_id = self.resolve_current_user_id(&org_url).await?;
                query_pairs.push(("searchCriteria.reviewerId", user_id));
            }
            PrSearchType::Created => {
                let user_id = self.resolve_current_user_id(&org_url).await?;
                query_pairs.push(("searchCriteria.creatorId", user_id));
            }
            PrSearchType::All => {}
        }

        let query_refs: Vec<(&str, &str)> = query_pairs
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect();
        let url = build_ado_api_url(
            &org_url,
            &[
                self.default_project.trim(),
                "_apis",
                "git",
                "pullrequests",
            ],
            &query_refs,
            true,
        )?;

        let response = self
            .client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("Failed to query PRs from Azure DevOps: {err}"))?;

        let list: AdoPullRequestListResponse =
            ensure_success_json(response, "Azure DevOps PR list").await?;

        Ok(list
            .value
            .into_iter()
            .map(|pr| Self::map_pr_to_metadata(&org_url, self.default_project.trim(), pr))
            .collect())
    }

    async fn get_linked_tickets(
        &self,
        pr_id: &str,
        repository_id: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        if self.org.trim().is_empty() || self.pat.is_empty() {
            anyhow::bail!("Azure DevOps Organization URL and PAT are required.");
        }
        let repo_id = repository_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Repository ID is required to fetch linked tickets in Azure DevOps."
                )
            })?;
        let pr_num: u64 = pr_id.trim().parse().map_err(|_| {
            anyhow::anyhow!("Invalid Azure DevOps PR ID '{pr_id}'. Expected a number.")
        })?;

        let org_url = AzureDevOpsClient::normalize_org_url(&self.org);
        let pr_str = pr_num.to_string();
        let url = build_ado_api_url(
            &org_url,
            &[
                "_apis",
                "git",
                "repositories",
                repo_id,
                "pullRequests",
                &pr_str,
                "workitems",
            ],
            &[],
            true,
        )?;

        let response = self
            .client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|err| {
                anyhow::anyhow!("Failed to fetch linked work items from Azure DevOps: {err}")
            })?;

        let payload: AdoPrWorkItemsResponse =
            ensure_success_json(response, "Azure DevOps PR work items").await?;

        Ok(payload
            .value
            .into_iter()
            .filter_map(|item| item.id)
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdoConnectionData {
    authorized_user: Option<AdoAuthorizedUser>,
}

#[derive(Debug, Deserialize)]
struct AdoAuthorizedUser {
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdoPullRequestListResponse {
    #[serde(default)]
    value: Vec<AdoPullRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdoPullRequest {
    pull_request_id: Option<u64>,
    title: Option<String>,
    description: Option<String>,
    source_ref_name: Option<String>,
    target_ref_name: Option<String>,
    created_by: Option<AdoIdentityRef>,
    repository: Option<AdoGitRepository>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdoIdentityRef {
    display_name: Option<String>,
    unique_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdoGitRepository {
    id: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdoPrWorkItemsResponse {
    #[serde(default)]
    value: Vec<AdoWorkItemRefId>,
}

#[derive(Debug, Deserialize)]
struct AdoWorkItemRefId {
    id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_org_url_prepends_dev_azure_com_when_slug_given() {
        assert_eq!(
            AzureDevOpsClient::normalize_org_url("my-org"),
            "https://dev.azure.com/my-org"
        );
        assert_eq!(
            AzureDevOpsClient::normalize_org_url("https://dev.azure.com/my-org/"),
            "https://dev.azure.com/my-org"
        );
        assert_eq!(
            AzureDevOpsClient::normalize_org_url("https://my-org.visualstudio.com/"),
            "https://my-org.visualstudio.com"
        );
    }

    #[test]
    fn auth_header_encodes_empty_user_and_pat() {
        let client = AzureDevOpsClient::new("my-org", "secret-pat");
        let expected = format!("Basic {}", STANDARD.encode(":secret-pat"));
        assert_eq!(client.auth_header(), expected);
    }

    #[test]
    fn build_wiql_query_escapes_single_quotes_and_optional_type() {
        assert_eq!(
            build_wiql_query("O'Reilly auth", None),
            "Select [System.Id], [System.Title] From WorkItems Where [System.Title] Contains 'O''Reilly auth' Order By [System.Id] Desc"
        );
        assert_eq!(
            build_wiql_query("Checkout", Some("User's Feature")),
            "Select [System.Id], [System.Title] From WorkItems Where [System.Title] Contains 'Checkout' And [System.WorkItemType] = 'User''s Feature' Order By [System.Id] Desc"
        );
    }

    #[test]
    fn deserialize_ado_responses() {
        let work_item_json = r#"{
            "id": 421,
            "fields": {
                "System.Title": "Payment Retry",
                "System.Description": "<p>Retry failed charges</p>",
                "Microsoft.VSTS.Common.AcceptanceCriteria": "<ul><li>Max 3 retries</li></ul>",
                "System.WorkItemType": "Feature"
            }
        }"#;
        let item: AdoWorkItem = serde_json::from_str(work_item_json).expect("work item");
        let ticket = item.into_ticket_data();
        assert_eq!(ticket.id.as_deref(), Some("421"));
        assert_eq!(ticket.title, "Payment Retry");
        assert_eq!(ticket.description, "<p>Retry failed charges</p>");
        assert_eq!(
            ticket.acceptance_criteria.as_deref(),
            Some("<ul><li>Max 3 retries</li></ul>")
        );

        let wiql_json = r#"{"workItems": [{"id": 10}, {"id": 20}]}"#;
        let wiql: WiqlResponse = serde_json::from_str(wiql_json).expect("wiql");
        assert_eq!(wiql.work_items.len(), 2);
        assert_eq!(wiql.work_items[0].id, Some(10));

        let types_json = r#"{"value": [{"name": "Feature"}, {"name": "Product Backlog Item"}, {"name": "Task"}]}"#;
        let types: AdoWorkItemTypesResponse = serde_json::from_str(types_json).expect("types");
        let names: Vec<String> = types.value.into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["Feature", "Product Backlog Item", "Task"]);
    }

    #[test]
    fn build_create_work_item_patch_includes_markdown_fields_and_parent_relation() {
        let ticket = TicketData {
            id: None,
            title: "Add OAuth".to_string(),
            description: "As a user...\n\nMore details.".to_string(),
            acceptance_criteria: Some("* AC 1\n* AC 2".to_string()),
        };
        let parent_url = "https://dev.azure.com/myorg/_apis/wit/workItems/12";
        let patch = build_create_work_item_patch(&ticket, parent_url, false);

        assert_eq!(patch.len(), 6);
        assert_eq!(patch[0]["path"], "/fields/System.Title");
        assert_eq!(patch[0]["value"], "Add OAuth");
        let desc = patch[1]["value"].as_str().unwrap();
        assert!(desc.starts_with("As a user..."));
        assert!(desc.contains("> Generated with "));
        assert_eq!(patch[2]["path"], "/multilineFieldsFormat/System.Description");
        assert_eq!(patch[2]["value"], "Markdown");
        assert_eq!(patch[3]["path"], "/relations/-");
        assert_eq!(
            patch[3]["value"]["rel"],
            "System.LinkTypes.Hierarchy-Reverse"
        );
        assert_eq!(patch[3]["value"]["url"], parent_url);
        assert_eq!(
            patch[4]["path"],
            "/fields/Microsoft.VSTS.Common.AcceptanceCriteria"
        );
        assert_eq!(
            patch[5]["path"],
            "/multilineFieldsFormat/Microsoft.VSTS.Common.AcceptanceCriteria"
        );

        let no_ac_ticket = TicketData {
            id: None,
            title: "Simple task".to_string(),
            description: String::new(),
            acceptance_criteria: Some("   ".to_string()),
        };
        let patch_no_ac = build_create_work_item_patch(&no_ac_ticket, parent_url, true);
        assert_eq!(patch_no_ac.len(), 4);
        assert!(
            patch_no_ac[1]["value"]
                .as_str()
                .unwrap()
                .starts_with("> Assisted by ")
        );
    }

    #[test]
    fn create_ticket_url_encodes_spaces_in_project_and_type() {
        let client = AzureDevOpsClient::new("my-org", "pat");
        let url = client
            .build_api_url(
                &["My Project", "_apis", "wit", "workitems", "$Product Backlog Item"],
                &[],
            )
            .expect("url");
        assert_eq!(
            url.as_str(),
            "https://dev.azure.com/my-org/My%20Project/_apis/wit/workitems/$Product%20Backlog%20Item?api-version=7.1"
        );
    }

    #[test]
    fn build_add_comment_patch_includes_markdown_format_and_attribution() {
        let patch = build_add_comment_patch("Test Cases:\n\n| TC-1 | Works |", false);
        assert_eq!(patch.len(), 2);
        assert_eq!(patch[0]["op"], "add");
        assert_eq!(patch[0]["path"], "/fields/System.History");
        let value = patch[0]["value"].as_str().unwrap();
        assert!(value.starts_with("Test Cases:\n\n| TC-1 | Works |"));
        assert!(value.contains("> Generated with "));
        assert_eq!(patch[1]["op"], "add");
        assert_eq!(patch[1]["path"], "/multilineFieldsFormat/System.History");
        assert_eq!(patch[1]["value"], "Markdown");

        let edited_patch = build_add_comment_patch("Edited comment", true);
        let edited_val = edited_patch[0]["value"].as_str().unwrap();
        assert!(edited_val.contains("> Assisted by "));
    }

    #[test]
    fn parse_ado_pr_and_remote_urls() {
        let client = AzureDevOpsCodeReviewClient::new("myorg", "pat", "CoreProj");

        let parsed_dev = client
            .parse_pr_url("https://dev.azure.com/acme/My%20Project/_git/stitch-rs/pullrequest/42")
            .expect("dev.azure.com PR URL");
        assert_eq!(parsed_dev.org, "acme");
        assert_eq!(parsed_dev.project, "My Project");
        assert_eq!(parsed_dev.repo_name, "stitch-rs");
        assert_eq!(parsed_dev.pr_number, 42);

        let parsed_vs = client
            .parse_pr_url("https://acme.visualstudio.com/DefaultCollection/Platform/_git/api/pullrequest/99")
            .expect("visualstudio.com PR URL");
        assert_eq!(parsed_vs.org, "acme");
        assert_eq!(parsed_vs.project, "Platform");
        assert_eq!(parsed_vs.repo_name, "api");
        assert_eq!(parsed_vs.pr_number, 99);

        assert!(client.parse_pr_url("https://example.com/not-a-pr").is_none());

        let remote_https = client
            .parse_remote_url("https://user@dev.azure.com/acme/Platform/_git/service.git")
            .expect("HTTPS remote");
        assert_eq!(remote_https.org, "acme");
        assert_eq!(remote_https.project, "Platform");
        assert_eq!(remote_https.repo_name, "service");

        let remote_ssh = client
            .parse_remote_url("git@ssh.dev.azure.com:v3/acme/Platform/service.git")
            .expect("SSH remote");
        assert_eq!(remote_ssh.org, "acme");
        assert_eq!(remote_ssh.project, "Platform");
        assert_eq!(remote_ssh.repo_name, "service");

        let remote_vs = client
            .parse_remote_url("https://acme.visualstudio.com/Platform/_git/service")
            .expect("visualstudio remote");
        assert_eq!(remote_vs.org, "acme");
        assert_eq!(remote_vs.project, "Platform");
        assert_eq!(remote_vs.repo_name, "service");
    }

    #[test]
    fn deserialize_ado_pr_responses_and_map_metadata() {
        let json = r#"{
            "pullRequestId": 512,
            "title": "Add PR reviewer",
            "description": "Implements checkout",
            "sourceRefName": "refs/heads/feat/pr-review",
            "targetRefName": "refs/heads/main",
            "createdBy": { "displayName": "Jane Dev", "uniqueName": "jane@acme.com" },
            "repository": { "id": "repo-uuid-1", "name": "stitch-rs" }
        }"#;
        let pr: AdoPullRequest = serde_json::from_str(json).expect("pr json");
        let meta = AzureDevOpsCodeReviewClient::map_pr_to_metadata(
            "https://dev.azure.com/acme",
            "CoreProj",
            pr,
        );
        assert_eq!(meta.id, "512");
        assert_eq!(meta.source_branch, "feat/pr-review");
        assert_eq!(meta.target_branch, "main");
        assert_eq!(meta.author, "Jane Dev");
        assert_eq!(meta.author_unique_name, "jane@acme.com");
        assert_eq!(meta.repository_name, "stitch-rs");
        assert_eq!(meta.repository_id.as_deref(), Some("repo-uuid-1"));
        assert_eq!(
            meta.url.as_deref(),
            Some("https://dev.azure.com/acme/CoreProj/_git/stitch-rs/pullrequest/512")
        );

        let conn_json = r#"{"authorizedUser": {"id": "user-123"}}"#;
        let conn: AdoConnectionData = serde_json::from_str(conn_json).expect("connectionData");
        assert_eq!(
            conn.authorized_user.and_then(|u| u.id).as_deref(),
            Some("user-123")
        );

        let wi_json = r#"{"value": [{"id": "101"}, {"id": "205"}]}"#;
        let wi: AdoPrWorkItemsResponse = serde_json::from_str(wi_json).expect("workitems");
        let ids: Vec<_> = wi.value.into_iter().filter_map(|w| w.id).collect();
        assert_eq!(ids, vec!["101", "205"]);
    }
}
