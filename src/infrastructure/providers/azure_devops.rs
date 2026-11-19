use std::collections::HashMap;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::infrastructure::providers::{
    IssueTrackerProvider, build_http_client, ensure_success_json,
};
use crate::types::TicketData;

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
}
