//! Read-only site rendering: compose admitted content artifacts with HTML.
//! No filesystem access or publication is permitted in this module.

use crate::web::content::ContentPage;
use crate::web::content_artifacts::{plan_content_artifacts, ContentArtifact};
use crate::web::content_render::{render_content_document, ContentLayout};
use crate::web::ir::DeploymentIr;

/// Fully rendered output owned by a previously validated content artifact claim.
///
/// No bytes are written by this type or its constructor. Publication remains
/// the responsibility of the staged Web build, after all checks have passed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedContentDocument {
    pub artifact: ContentArtifact,
    pub html: String,
}

/// Plan and render the entire content collection, or return all diagnostics.
///
/// Explicit Web routes and static asset names are checked before any document
/// is rendered. An invalid page or layout discards the entire candidate site:
/// callers must not publish a partial result. Stable order comes from the
/// compiler-aligned artifact planner.
pub fn render_content_site(
    pages: &[ContentPage],
    ir: &DeploymentIr,
    static_assets: &[&str],
    layout: &ContentLayout<'_>,
) -> Result<Vec<RenderedContentDocument>, Vec<String>> {
    let planned = plan_content_artifacts(pages, ir, static_assets)?;
    let mut rendered = Vec::with_capacity(planned.len());
    let mut diagnostics = Vec::new();

    for artifact in planned {
        let source = pages.iter().find(|page| {
            !page.draft
                && page.source_path == artifact.source_path
                && page.slug == artifact.slug
        });

        match source {
            Some(page) => match render_content_document(page, layout) {
                Ok(html) => rendered.push(RenderedContentDocument { artifact, html }),
                Err(error) => diagnostics.push(error),
            },
            None => diagnostics.push(format!(
                "planned content source missing: {} for {}",
                artifact.source_path, artifact.slug
            )),
        }
    }

    if diagnostics.is_empty() {
        Ok(rendered)
    } else {
        diagnostics.sort();
        diagnostics.dedup();
        Err(diagnostics)
    }
}

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
