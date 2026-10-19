use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchRequest {
    pub query: String,
    #[serde(rename = "type", default = "default_search_type")]
    pub search_type: String,
    #[serde(default = "default_num_results")]
    pub num_results: usize,
    #[serde(default)]
    pub include_domains: Vec<String>,
    #[serde(default)]
    pub exclude_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_published_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_published_date: Option<String>,
    #[serde(default)]
    pub include_text: bool,
    #[serde(default)]
    pub include_highlights: bool,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            search_type: default_search_type(),
            num_results: default_num_results(),
            include_domains: Vec::new(),
            exclude_domains: Vec::new(),
            start_published_date: None,
            end_published_date: None,
            include_text: false,
            include_highlights: false,
        }
    }
}

fn default_search_type() -> String {
    "auto".to_owned()
}

fn default_num_results() -> usize {
    10
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContentsRequest {
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default = "default_include_text")]
    pub include_text: bool,
    #[serde(default)]
    pub include_highlights: bool,
}

impl Default for ContentsRequest {
    fn default() -> Self {
        Self {
            urls: Vec::new(),
            ids: Vec::new(),
            include_text: default_include_text(),
            include_highlights: false,
        }
    }
}

fn default_include_text() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub results: Vec<SearchResult>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub id: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub highlights: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    pub status: ItemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_origin: Option<ContentOrigin>,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Ok,
    Partial,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContentOrigin {
    ProviderSnippet,
    Live,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    pub fn timeout() -> Self {
        Self::new(ErrorCode::Timeout, "Request deadline exceeded")
    }

    pub fn cancelled() -> Self {
        Self::new(ErrorCode::Cancelled, "Request cancelled")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidInput,
    Unsupported,
    BlockedDestination,
    DomainFiltered,
    DnsFailed,
    UpstreamBlocked,
    RateLimited,
    UpstreamFailed,
    Timeout,
    Cancelled,
    TooLarge,
    UnsupportedContent,
    ExtractionFailed,
    Overloaded,
    Internal,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponse {
    pub error: Error,
}
