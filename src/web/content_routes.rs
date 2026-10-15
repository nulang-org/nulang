//! Read-only content route reservations.
//!
//! This is a preflight for future Markdown publishing. It does not modify the
//! compiler-owned Web Contract IR or register executable routes.

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
            &[("POST", "/blog/post"), ("GET", "/about"), ("GET", "/blog/about")],
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
            error.iter().any(|message| message.contains("invalid route pattern")),
            "{error:?}"
        );
    }

    #[test]
    fn duplicate_content_slugs_are_rejected_even_for_separate_sources() {
        let input = vec![page("blog/post.md", false), page("blog/post/index.md", false)];
        let error = plan_content_routes(&input, &[]).unwrap_err();
        assert!(
            error.iter().any(|message| message.contains("duplicate content route")),
            "{error:?}"
        );
    }

    #[test]
    fn draft_pages_do_not_claim_explicit_routes() {
        let input = vec![page("blog/draft.md", true)];
        assert!(plan_content_routes(&input, &[("GET", "/blog/draft")])
            .unwrap()
            .is_empty());
    }
}
