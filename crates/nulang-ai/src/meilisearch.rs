//! Authenticated Meilisearch adapter. All operations use one configured tenant index.
//! Async client; the SearchBackend trait is the synchronous in-memory interface.

use std::collections::BTreeMap;
use std::time::Duration;
use reqwest::{Client, RequestBuilder, Url};
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use serde_json::json;
use crate::search::{OwnedSearchHit, SearchDocument, SearchQuery};

const MAX_REMOTE_RESULTS: usize = 200;

#[derive(Debug)]
pub enum MeilisearchError {
    InvalidEndpoint,
    InsecureEndpoint,
    MissingApiKey,
    InvalidNamespace,
    NamespaceMismatch,
    InvalidDocumentId,
    InvalidFilterKey,
    Transport(reqwest::Error),
    Api { status: u16, message: String },
}

impl std::fmt::Display for MeilisearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEndpoint => write!(f, "invalid Meilisearch endpoint"),
            Self::InsecureEndpoint => write!(f, "Meilisearch requires HTTPS outside localhost"),
            Self::MissingApiKey => write!(f, "Meilisearch API key is required"),
            Self::InvalidNamespace => write!(f, "invalid search namespace"),
            Self::NamespaceMismatch => write!(f, "search namespace does not match client scope"),
            Self::InvalidDocumentId => write!(f, "invalid Meilisearch document ID"),
            Self::InvalidFilterKey => write!(f, "invalid Meilisearch filter field"),
            Self::Transport(err) => write!(f, "Meilisearch transport failure: {err}"),
            Self::Api { status, message } => write!(f, "Meilisearch returned {status}: {message}"),
        }
    }
}

impl std::error::Error for MeilisearchError {}

impl From<reqwest::Error> for MeilisearchError {
    fn from(error: reqwest::Error) -> Self { Self::Transport(error) }
}

/// Enqueue acknowledgment, not a visibility guarantee.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeilisearchTask {
    pub task_uid: u64,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MeilisearchTaskStatus {
    pub uid: u64,
    pub status: String,
    #[serde(rename = "indexUid")]
    pub index_uid: Option<String>,
}

/// Configure exclusively on a trusted server, using a key restricted to
/// the generated index. Namespace checks alone are not authorization.
pub struct MeilisearchClient {
    client: Client,
    base: Url,
    api_key: String,
    namespace: String,
    index_uid: String,
}

impl MeilisearchClient {
    pub fn new(
        endpoint: &str,
        api_key: impl Into<String>,
        namespace: &str,
    ) -> Result<Self, MeilisearchError> {
        let base = Url::parse(endpoint).map_err(|_| MeilisearchError::InvalidEndpoint)?;
        if base.cannot_be_a_base()
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(MeilisearchError::InvalidEndpoint);
        }
        let loopback = matches!(base.host_str(), Some("127.0.0.1" | "localhost" | "::1" | "[::1]"));
        if base.scheme() != "https" && !(base.scheme() == "http" && loopback) {
            return Err(MeilisearchError::InsecureEndpoint);
        }
        if !valid_identifier(namespace) || namespace.len() > 64 {
            return Err(MeilisearchError::InvalidNamespace);
        }
        let api_key = api_key.into();
        if api_key.trim().is_empty() { return Err(MeilisearchError::MissingApiKey); }
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client, base, api_key, namespace: namespace.to_owned(),
            index_uid: format!("nulang_{namespace}"),
        })
    }

    pub fn index_uid(&self) -> &str { &self.index_uid }

    fn endpoint(&self, components: &[&str]) -> Url {
        let mut url = self.base.clone();
        {
            let mut segments = url.path_segments_mut().expect("HTTP URL has path segments");
            segments.pop_if_empty();
            for segment in components { segments.push(segment); }
        }
        url
    }

    fn validate_namespace(&self, namespace: &str) -> Result<(), MeilisearchError> {
        if namespace != self.namespace { Err(MeilisearchError::NamespaceMismatch) }
        else { Ok(()) }
    }

    fn is_empty_query(&self, query: &SearchQuery) -> Result<bool, MeilisearchError> {
        self.validate_namespace(&query.namespace)?;
        Ok(query.text.trim().is_empty() || query.limit == 0)
    }

    async fn response<T: DeserializeOwned>(
        &self,
        request: RequestBuilder,
    ) -> Result<T, MeilisearchError> {
        let response = request.bearer_auth(&self.api_key).send().await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response.text().await?;
            return Err(MeilisearchError::Api {
                status, message: message.chars().take(1024).collect(),
            });
        }
        Ok(response.json::<T>().await?)
    }

    /// Queue an upsert. Poll task_status to observe its completion.
    pub async fn upsert(&self, document: &SearchDocument)
        -> Result<MeilisearchTask, MeilisearchError>
    {
        self.validate_namespace(&document.namespace)?;
        validate_document_id(&document.id)?;
        let url = self.endpoint(&["indexes", &self.index_uid, "documents"]);
        self.response(self.client.post(url)
            .query(&[("primaryKey", "id")]).json(&[document])).await
    }

    /// Queue a deletion; completion is asynchronous.
    pub async fn delete(&self, namespace: &str, id: &str)
        -> Result<MeilisearchTask, MeilisearchError>
    {
        self.validate_namespace(namespace)?;
        validate_document_id(id)?;
        let url = self.endpoint(&["indexes", &self.index_uid, "documents", id]);
        self.response(self.client.delete(url)).await
    }

    /// Returns hits from the configured namespace only.
    pub async fn search(&self, query: &SearchQuery)
        -> Result<Vec<OwnedSearchHit>, MeilisearchError>
    {
        if self.is_empty_query(query)? { return Ok(Vec::new()); }
        let filters = filter_predicates(&query.filters)?;
        let mut body = json!({
            "q": query.text,
            "limit": query.limit.min(MAX_REMOTE_RESULTS),
            "showRankingScore": true
        });
        if !filters.is_empty() { body["filter"] = json!(filters); }
        let url = self.endpoint(&["indexes", &self.index_uid, "search"]);
        let response: MeilisearchSearchResponse =
            self.response(self.client.post(url).json(&body)).await?;
        if response.hits.iter().any(|hit| hit.document.namespace != self.namespace) {
            return Err(MeilisearchError::NamespaceMismatch);
        }
        Ok(response.hits.into_iter().map(|hit| OwnedSearchHit {
            document: hit.document,
            score: hit.ranking_score.unwrap_or(0.0),
        }).collect())
    }

    pub async fn task_status(&self, uid: u64)
        -> Result<MeilisearchTaskStatus, MeilisearchError>
    {
        let uid = uid.to_string();
        let url = self.endpoint(&["tasks", &uid]);
        let status: MeilisearchTaskStatus = self.response(self.client.get(url)).await?;
        if status.index_uid.as_deref() != Some(self.index_uid.as_str()) {
            return Err(MeilisearchError::NamespaceMismatch);
        }
        Ok(status)
    }
}

#[derive(Deserialize)]
struct MeilisearchSearchResponse {
    hits: Vec<MeilisearchRankedDocument>,
}

#[derive(Deserialize)]
struct MeilisearchRankedDocument {
    #[serde(flatten)]
    document: SearchDocument,
    #[serde(rename = "_rankingScore")]
    ranking_score: Option<f64>,
}

fn valid_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn validate_document_id(id: &str) -> Result<(), MeilisearchError> {
    if valid_identifier(id) { Ok(()) }
    else { Err(MeilisearchError::InvalidDocumentId) }
}

fn filter_predicates(filters: &BTreeMap<String, String>)
    -> Result<Vec<String>, MeilisearchError>
{
    let mut output = Vec::with_capacity(filters.len());
    for (key, value) in filters {
        if key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(MeilisearchError::InvalidFilterKey);
        }
        let value = serde_json::to_string(value)
            .expect("Rust String serialization cannot fail");
        output.push(format!("attributes.{key} = {value}"));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbids_insecure_remote_endpoints_and_invalid_namespace() {
        assert!(matches!(
            MeilisearchClient::new("http://search.example.com", "test-key", "tenant-a"),
            Err(MeilisearchError::InsecureEndpoint)
        ));
        assert!(matches!(
            MeilisearchClient::new("https://search.example.com", "test-key", "../admin"),
            Err(MeilisearchError::InvalidNamespace)
        ));
        assert!(matches!(
            MeilisearchClient::new("https://search.example.com", "", "tenant-a"),
            Err(MeilisearchError::MissingApiKey)
        ));
    }

    #[test]
    fn constructs_an_isolated_index_url() {
        let client = MeilisearchClient::new(
            "https://search.example.com/proxy",
            "test-key",
            "tenant-a",
        ).unwrap();
        assert_eq!(
            client.endpoint(&["indexes", &client.index_uid, "search"]).as_str(),
            "https://search.example.com/proxy/indexes/nulang_tenant-a/search"
        );
    }

    #[test]
    fn exact_filters_escape_quotes_and_reject_injection_in_field_names() {
        let filters = BTreeMap::from([
            ("category".to_owned(), "quote \" and slash \\".to_owned())
        ]);
        assert_eq!(
            filter_predicates(&filters).unwrap(),
            vec![r#"attributes.category = "quote \" and slash \\""#.to_owned()]
        );
        let bad = BTreeMap::from([("field OR id".to_owned(), "1".to_owned())]);
        assert!(matches!(filter_predicates(&bad), Err(MeilisearchError::InvalidFilterKey)));
    }

    #[test]
    fn namespace_mismatch_and_unsafe_document_id_are_rejected_before_network() {
        let client = MeilisearchClient::new(
            "https://search.example.com", "test-key", "tenant-a"
        ).unwrap();
        assert!(matches!(client.validate_namespace("tenant-b"), Err(MeilisearchError::NamespaceMismatch)));
        assert!(matches!(validate_document_id("../escaping"), Err(MeilisearchError::InvalidDocumentId)));
    }

    #[test]
    fn task_status_requires_its_index_to_match_the_tenant() {
        let client = MeilisearchClient::new(
            "https://search.example.com", "test-key", "tenant-a"
        ).unwrap();
        let foreign: MeilisearchTaskStatus = serde_json::from_str(
            r#"{"uid":7,"status":"succeeded","indexUid":"nulang_tenant-b"}"#
        ).unwrap();
        assert_ne!(foreign.index_uid.as_deref(), Some(client.index_uid()));
    }

    #[test]
    fn queued_document_tasks_must_belong_to_requested_index() {
        let client = MeilisearchClient::new(
            "https://search.example.com", "test-key", "tenant-a"
        ).unwrap();
        let valid: MeilisearchTask = serde_json::from_str(
            r#"{"taskUid":41,"status":"enqueued","indexUid":"nulang_tenant-a"}"#
        ).unwrap();
        assert!(client.validate_task_index(valid.index_uid.as_deref()).is_ok());

        let foreign: MeilisearchTask = serde_json::from_str(
            r#"{"taskUid":42,"status":"enqueued","indexUid":"nulang_tenant-b"}"#
        ).unwrap();
        assert!(matches!(
            client.validate_task_index(foreign.index_uid.as_deref()),
            Err(MeilisearchError::NamespaceMismatch)
        ));

        let global: MeilisearchTask = serde_json::from_str(
            r#"{"taskUid":43,"status":"enqueued","indexUid":null}"#
        ).unwrap();
        assert!(matches!(
            client.validate_task_index(global.index_uid.as_deref()),
            Err(MeilisearchError::NamespaceMismatch)
        ));
    }

    #[test]
    fn blank_queries_are_not_sent_as_match_everything_searches() {
        let client = MeilisearchClient::new(
            "https://search.example.com", "test-key", "tenant-a"
        ).unwrap();
        assert!(client.is_empty_query(&SearchQuery::new("tenant-a", "   ")).unwrap());
    }
}
