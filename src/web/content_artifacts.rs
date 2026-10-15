//! Deterministic, read-only artifact planning for typed Markdown content.
//!
//! Publication remains owned by the existing Web build pipeline. This module
//! neither renders Markdown nor writes any files.

use crate::web::content::{ContentKind, ContentPage};
use crate::web::content_routes::plan_content_routes_from_ir;
use crate::web::ir::{route_path_to_artifact, DeploymentIr};
use std::collections::BTreeSet;

/// A public Markdown artifact reservation, not a rendered or written file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentArtifact {
    pub slug: String,
    pub source_path: String,
    pub kind: ContentKind,
    pub artifact: String,
}

/// Validate static asset names and plan public content artifact paths.
///
/// The deployment IR must already be generated from compiler-validated Web
/// contracts. Static assets must be enumerated from the package's trusted
/// static-directory walk; this function validates their relative path syntax
/// and prevents a later copy from silently replacing generated Markdown HTML.
/// It does not determine route placement, render Markdown, or publish files.
pub fn plan_content_artifacts(
    pages: &[ContentPage],
    ir: &DeploymentIr,
    static_assets: &[&str],
) -> Result<Vec<ContentArtifact>, Vec<String>> {
    let mut diagnostics = Vec::new();
    let mut static_names = BTreeSet::new();
    for path in static_assets {
        if !valid_relative_asset_path(path) {
            diagnostics.push(format!("invalid static asset path: {path}"));
        } else {
            static_names.insert(*path);
        }
    }

    let routes = match plan_content_routes_from_ir(pages, ir) {
        Ok(routes) => routes,
        Err(errors) => {
            diagnostics.extend(errors);
            Vec::new()
        }
    };

    let artifacts = routes
        .into_iter()
        .map(|route| {
            let artifact = route_path_to_artifact(&route.slug);
            if static_names.contains(artifact.as_str()) {
                diagnostics.push(format!(
                    "content route {} from {} would overwrite static asset {}",
                    route.slug, route.source_path, artifact
                ));
            }
            ContentArtifact {
                slug: route.slug,
                source_path: route.source_path,
                kind: route.kind,
                artifact,
            }
        })
        .collect();

    if diagnostics.is_empty() {
        Ok(artifacts)
    } else {
        diagnostics.sort();
        diagnostics.dedup();
        Err(diagnostics)
    }
}

fn valid_relative_asset_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains('\0')
        && !path.split('/').next().is_some_and(|segment| segment.contains(':'))
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::content::parse_page;

    fn page(path: &str, draft: bool) -> ContentPage {
        let source = if draft {
            "+++\ntitle = \"Article\"\ndraft = true\n+++\n# Draft\n"
        } else {
            "+++\ntitle = \"Article\"\n+++\n# Published\n"
        };
        parse_page(path, source).unwrap()
    }

    #[test]
    fn builds_deterministic_artifact_names_for_pages_and_sections() {
        let pages = vec![
            page("docs/zeta.md", false),
            page("_index.md", false),
            page("docs/_index.md", false),
            page("docs/alpha/index.md", false),
            page("docs/draft.md", true),
        ];
        let output = plan_content_artifacts(&pages, &DeploymentIr::default(), &[]).unwrap();
        let actual: Vec<_> = output
            .iter()
            .map(|entry| {
                (
                    entry.slug.as_str(),
                    entry.source_path.as_str(),
                    entry.artifact.as_str(),
                    entry.kind,
                )
            })
            .collect();
        assert_eq!(
            actual,
            vec![
                ("/", "_index.md", "index.html", ContentKind::Section),
                (
                    "/docs",
                    "docs/_index.md",
                    "docs/index.html",
                    ContentKind::Section,
                ),
                (
                    "/docs/alpha",
                    "docs/alpha/index.md",
                    "docs/alpha/index.html",
                    ContentKind::Page,
                ),
                (
                    "/docs/zeta",
                    "docs/zeta.md",
                    "docs/zeta/index.html",
                    ContentKind::Page,
                ),
            ]
        );
    }

    #[test]
    fn fails_closed_on_user_managed_static_file_collisions() {
        let pages = vec![page("docs/post.md", false)];
        let err = plan_content_artifacts(
            &pages,
            &DeploymentIr::default(),
            &["images/cover.png", "docs/post/index.html"],
        )
        .unwrap_err();
        assert!(err.iter().any(|line| {
            line.contains("docs/post/index.html")
                && line.contains("docs/post.md")
                && line.contains("static asset")
        }), "{err:?}");
    }

    #[test]
    fn homepage_cannot_replace_explicit_static_index() {
        let pages = vec![page("_index.md", false)];
        let err = plan_content_artifacts(
            &pages,
            &DeploymentIr::default(),
            &["index.html"],
        )
        .unwrap_err();
        assert!(err.iter().any(|line| line.contains("index.html")), "{err:?}");
    }

    #[test]
    fn fails_closed_on_registered_server_get_route() {
        let pages = vec![page("blog/post.md", false)];
        let mut ir = DeploymentIr::default();
        ir.routes.push(serde_json::from_value(serde_json::json!({
            "method": "GET",
            "path": "/blog/{slug}",
            "placement": "server",
            "artifact": null
        })).unwrap());
        let err = plan_content_artifacts(&pages, &ir, &[]).unwrap_err();
        assert!(
            err.iter().any(|line| line.contains("GET /blog/{slug}")),
            "{err:?}"
        );
    }

    #[test]
    fn malformed_static_asset_paths_are_rejected_without_writing() {
        let pages = vec![page("docs/post.md", false)];
        for path in ["../index.html", "/index.html", "docs\\post.html", "docs//post.html"] {
            let err = plan_content_artifacts(&pages, &DeploymentIr::default(), &[path])
                .unwrap_err();
            assert!(
                err.iter().any(|line| line.contains("invalid static asset path")),
                "{path}: {err:?}"
            );
        }
    }

    #[test]
    fn unrelated_static_assets_and_post_routes_are_compatible() {
        let pages = vec![page("docs/post.md", false), page("docs/draft.md", true)];
        let mut ir = DeploymentIr::default();
        ir.routes.push(serde_json::from_value(serde_json::json!({
            "method": "POST",
            "path": "/docs/post",
            "placement": "server",
            "artifact": null
        })).unwrap());
        let output = plan_content_artifacts(&pages, &ir, &["assets/site.css"]).unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].artifact, "docs/post/index.html");
    }

    #[test]
    fn uses_exactly_the_compiler_artifact_naming_convention() {
        let pages = vec![page("docs/post.md", false), page("_index.md", false)];
        let output = plan_content_artifacts(&pages, &DeploymentIr::default(), &[]).unwrap();
        for entry in output {
            assert_eq!(
                entry.artifact,
                crate::web::ir::route_path_to_artifact(&entry.slug)
            );
        }
    }
}
