//! Read-only site rendering: compose admitted content artifacts with HTML.
//! No filesystem access or publication is permitted in this module.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::content::parse_page;

    fn page(path: &str, markdown: &str, draft: bool) -> ContentPage {
        let metadata = if draft {
            "+++\ntitle = \"Example\"\ndraft = true\n+++\n"
        } else {
            "+++\ntitle = \"Example\"\n+++\n"
        };
        parse_page(path, &format!("{metadata}{markdown}")).unwrap()
    }

    #[test]
    fn renders_all_public_documents_in_canonical_artifact_order() {
        let pages = vec![
            page("blog/zeta.md", "## Last", false),
            page("blog/draft.md", "# Not public", true),
            page("_index.md", "# Welcome", false),
            page("blog/alpha/index.md", "## First", false),
        ];
        let output = render_content_site(
            &pages,
            &DeploymentIr::default(),
            &["assets/site.css"],
            &ContentLayout::default(),
        )
        .unwrap();
        let paths: Vec<_> = output.iter().map(|doc| doc.artifact.artifact.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "index.html",
                "blog/alpha/index.html",
                "blog/zeta/index.html",
            ]
        );
        assert!(output[0].html.contains("<h1>Welcome</h1>"));
        assert!(output[1].html.contains("<h2>First</h2>"));
        assert!(output[2].html.contains("<h2>Last</h2>"));
        assert!(output.iter().all(|doc| !doc.html.contains("Not public")));
    }

    #[test]
    fn returns_no_partial_site_when_one_page_has_unsupported_markdown() {
        let pages = vec![
            page("docs/ok.md", "# Okay", false),
            page("docs/bad.md", "[link](https://example.com)", false),
        ];
        let errors = render_content_site(
            &pages,
            &DeploymentIr::default(),
            &[],
            &ContentLayout::default(),
        )
        .unwrap_err();
        assert!(
            errors.iter().any(|error| error.contains("docs/bad.md")
                && error.contains("unsupported")),
            "{errors:?}"
        );
    }

    #[test]
    fn static_asset_conflict_stops_site_rendering() {
        let pages = vec![page("docs/post.md", "# Post", false)];
        let errors = render_content_site(
            &pages,
            &DeploymentIr::default(),
            &["docs/post/index.html"],
            &ContentLayout::default(),
        )
        .unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("docs/post/index.html")),
            "{errors:?}"
        );
    }

    #[test]
    fn server_get_collision_prevents_any_document_output() {
        let pages = vec![page("docs/post.md", "# Post", false)];
        let mut ir = DeploymentIr::default();
        ir.routes.push(
            serde_json::from_value(serde_json::json!({
                "method": "GET",
                "path": "/docs/{slug}",
                "placement": "server",
                "artifact": null
            }))
            .unwrap(),
        );
        let errors = render_content_site(
            &pages,
            &ir,
            &[],
            &ContentLayout::default(),
        )
        .unwrap_err();
        assert!(errors.iter().any(|error| error.contains("GET /docs/{slug}")));
    }

    #[test]
    fn failed_layout_validation_stops_site_rendering() {
        let pages = vec![page("docs/post.md", "# Post", false)];
        let invalid_layout = ContentLayout {
            site_title: " ",
            language: "en",
        };
        let errors = render_content_site(
            &pages,
            &DeploymentIr::default(),
            &[],
            &invalid_layout,
        )
        .unwrap_err();
        assert!(
            errors.iter().any(|error| error.contains("site title")),
            "{errors:?}"
        );
    }

    #[test]
    fn empty_public_collection_yields_no_artifacts() {
        let pages = vec![page("docs/draft.md", "# Draft", true)];
        let output = render_content_site(
            &pages,
            &DeploymentIr::default(),
            &[],
            &ContentLayout::default(),
        )
        .unwrap();
        assert!(output.is_empty());
    }
}
