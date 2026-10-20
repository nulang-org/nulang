//! Authenticated Meilisearch client adapter for Nulang's experimental search API.
//! Queries and writes are scoped to exactly one configured namespace.
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
    fn blank_queries_are_not_sent_as_match_everything_searches() {
        let client = MeilisearchClient::new(
            "https://search.example.com", "test-key", "tenant-a"
        ).unwrap();
        assert!(client.is_empty_query(&SearchQuery::new("tenant-a", "   ")).unwrap());
    }
}
