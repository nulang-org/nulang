//! Read-only content route reservations.
//!
//! Content routes share the compiler's existing route-pattern matcher; this
//! module neither registers routes nor emits static files. The build pipeline
//! can consume the resulting claims after validated contract compilation.

use crate::web::content::{ContentKind, ContentPage};
use crate::web::runtime_bindings::route_pattern_matches;
use std::collections::BTreeMap;

/// A publishable Markdown entry that reserves one canonical GET path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentRoute {
    pub slug: String,
    pub source_path: String,
    pub kind: ContentKind,
}

/// Reserve static content paths against explicit GET and HEAD routes.
///
/// The input route pairs must come from the already validated compiler/runtime
/// route representation; route parsing and matching stay owned by Web dispatch.
/// Drafts are retained by content discovery but never claim public paths.
/// On any collision or invalid route pattern, no partial claim set is returned.
pub fn plan_content_routes(
    pages: &[ContentPage],
    registered_routes: &[(&str, &str)],
) -> Result<Vec<ContentRoute>, Vec<String>> {
    let mut claims: BTreeMap<String, ContentRoute> = BTreeMap::new();
    let mut diagnostics = Vec::new();

    for page in pages.iter().filter(|page| !page.draft) {
        if !valid_slug(&page.slug) {
            diagnostics.push(format!(
                "invalid content route {} from {}",
                page.slug, page.source_path
            ));
            continue;
        }

        if let Some(previous) = claims.get(&page.slug) {
            diagnostics.push(format!(
                "duplicate content route {} from {} and {}",
                page.slug, previous.source_path, page.source_path
            ));
            continue;
        }
        claims.insert(
            page.slug.clone(),
            ContentRoute {
                slug: page.slug.clone(),
                source_path: page.source_path.clone(),
                kind: page.kind,
            },
        );
    }

    for (method, pattern) in registered_routes {
        if !method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD") {
            continue;
        }
        // A static host may serve /page and /page/ from the same index.html.
        // Be stricter than the live matcher on trailing slashes so the built
        // artifact cannot silently shadow either spelling.
        let normalized = canonical_path(pattern);
        for claim in claims.values() {
            match route_pattern_matches(normalized, &claim.slug) {
                Ok(true) => diagnostics.push(format!(
                    "content route {} from {} conflicts with {} {}",
                    claim.slug, claim.source_path, method, pattern
                )),
                Ok(false) => {}
                Err(error) => diagnostics.push(format!(
                    "invalid route pattern {} {}: {}",
                    method, pattern, error
                )),
            }
        }
    }

    if diagnostics.is_empty() {
        Ok(claims.into_values().collect())
    } else {
        diagnostics.sort();
        diagnostics.dedup();
        Err(diagnostics)
    }
}

fn canonical_path(path: &str) -> &str {
    match path.trim_end_matches('/') {
        "" => "/",
        canonical => canonical,
    }
}

fn valid_slug(slug: &str) -> bool {
    if slug == "/" {
        return true;
    }
    slug.starts_with('/')
        && slug.strip_prefix('/').is_some_and(|segments| {
            segments.split('/').all(|segment| {
                !segment.is_empty()
                    && segment.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::content::parse_page;

    fn page(path: &str, draft: bool) -> ContentPage {
        let front_matter = if draft {
            "+++\ntitle = \"Example\"\ndraft = true\n+++\n# Body\n"
        } else {
            "+++\ntitle = \"Example\"\n+++\n# Body\n"
        };
        parse_page(path, front_matter).unwrap()
    }

    #[test]
    fn only_published_pages_claim_routes_in_deterministic_order() {
        let input = vec![
            page("blog/zeta.md", false),
            page("blog/hidden.md", true),
            page("blog/_index.md", false),
            page("blog/alpha/index.md", false),
        ];

        let planned = plan_content_routes(&input, &[]).unwrap();
        let claims: Vec<_> = planned
            .iter()
            .map(|item| (item.slug.as_str(), item.source_path.as_str(), item.kind))
            .collect();

        assert_eq!(
            claims,
            vec![
                ("/blog", "blog/_index.md", ContentKind::Section),
                ("/blog/alpha", "blog/alpha/index.md", ContentKind::Page),
                ("/blog/zeta", "blog/zeta.md", ContentKind::Page),
            ]
        );
    }

    #[test]
    fn explicit_get_and_head_routes_cannot_shadow_published_content() {
        let input = vec![page("blog/post.md", false)];

        for (method, path) in [
            ("GET", "/blog/post"),
            ("HEAD", "/blog/post/"),
            ("GET", "/blog/:slug"),
            ("GET", "/blog/{slug}"),
            ("GET", "/blog/{slug: String}"),
        ] {
            let error = plan_content_routes(&input, &[(method, path)]).unwrap_err();
            assert!(
                error.iter().any(|message| message.contains("blog/post.md")
                    && message.contains(path)
                    && message.contains("/blog/post")),
                "{method} {path}: {error:?}"
            );
        }
    }

    #[test]
    fn post_routes_and_unrelated_get_routes_are_not_claim_conflicts() {
        let input = vec![page("blog/post.md", false)];
        let planned = plan_content_routes(
            &input,
            &[
                ("POST", "/blog/post"),
                ("GET", "/about"),
                ("GET", "/blog/about"),
            ],
        )
        .unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].slug, "/blog/post");
    }

    #[test]
    fn malformed_registered_get_patterns_fail_closed() {
        let input = vec![page("blog/post.md", false)];
        let error = plan_content_routes(&input, &[("GET", "/blog/{broken")]).unwrap_err();
        assert!(
            error
                .iter()
                .any(|message| message.contains("invalid route pattern")),
            "{error:?}"
        );
    }

    #[test]
    fn duplicate_content_slugs_are_rejected_even_for_separate_sources() {
        let input = vec![
            page("blog/post.md", false),
            page("blog/post/index.md", false),
        ];
        let error = plan_content_routes(&input, &[]).unwrap_err();
        assert!(
            error
                .iter()
                .any(|message| message.contains("duplicate content route")),
            "{error:?}"
        );
    }

    #[test]
    fn deployment_ir_blocks_content_shadowing_server_get_routes() {
        let input = vec![page("blog/post.md", false)];
        let mut ir = crate::web::ir::DeploymentIr::default();
        ir.routes = vec![
            serde_json::from_value(serde_json::json!({
                "method": "GET",
                "path": "/blog/:slug",
                "placement": "server",
                "artifact": null
            }))
            .unwrap(),
        ];
        let error = plan_content_routes_from_ir(&input, &ir).unwrap_err();
        assert!(
            error
                .iter()
                .any(|message| message.contains("blog/post.md")
                    && message.contains("GET /blog/:slug")),
            "{error:?}"
        );
    }

    #[test]
    fn deployment_ir_allows_nonconflicting_routes_and_excludes_drafts() {
        let input = vec![page("blog/post.md", false), page("blog/draft.md", true)];
        let mut ir = crate::web::ir::DeploymentIr::default();
        ir.routes = vec![
            serde_json::from_value(serde_json::json!({
                "method": "POST",
                "path": "/blog/post",
                "placement": "server",
                "artifact": null
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "method": "GET",
                "path": "/about",
                "placement": "static",
                "artifact": "about/index.html"
            }))
            .unwrap(),
        ];
        let routes = plan_content_routes_from_ir(&input, &ir).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].slug, "/blog/post");
    }

    #[test]
    fn draft_pages_do_not_claim_explicit_routes() {
        let input = vec![page("blog/draft.md", true)];
        assert!(plan_content_routes(&input, &[("GET", "/blog/draft")])
            .unwrap()
            .is_empty());
    }
}
