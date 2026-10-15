//! Experimental typed Markdown content metadata.
//! This module does not register routes or render Markdown.

use serde::Deserialize;

/// One validated content entry. Slugs are stable, leading-slash route paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentPage {
    pub slug: String,
    pub title: String,
    pub draft: bool,
    pub tags: Vec<String>,
    /// Markdown is intentionally kept as source until a trusted renderer exists.
    pub body: String,
}

#[derive(Deserialize)]
struct FrontMatter {
    title: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    tags: Vec<String>,
}

/// Parse a Markdown file's TOML front matter and derive a safe route slug.
///
/// Only TOML +++ delimiters and title/draft/tags metadata are supported in
/// this experimental boundary. No filesystem walking, rendering, publishing,
/// or implicit route registration occurs here.
pub fn parse_page(relative_path: &str, source: &str) -> Result<ContentPage, String> {
    let slug = slug_from_relative_path(relative_path)?;
    let payload = source
        .strip_prefix("+++\n")
        .ok_or_else(|| format!("{relative_path}: expected TOML +++ front matter"))?;
    let (metadata, body) = payload
        .split_once("\n+++\n")
        .ok_or_else(|| format!("{relative_path}: missing closing +++ delimiter"))?;
    let front: FrontMatter = toml::from_str(metadata)
        .map_err(|error| format!("{relative_path}: invalid content metadata: {error}"))?;
    if front.title.trim().is_empty() {
        return Err(format!("{relative_path}: content title must not be empty"));
    }
    if front.tags.iter().any(|tag| tag.trim().is_empty()) {
        return Err(format!("{relative_path}: content tags must not be empty"));
    }
    Ok(ContentPage {
        slug,
        title: front.title,
        draft: front.draft,
        tags: front.tags,
        body: body.to_string(),
    })
}

fn slug_from_relative_path(path: &str) -> Result<String, String> {
    if path.starts_with('/') || path.contains('\\') {
        return Err(format!("unsafe content path: {path}"));
    }
    let stem = path
        .strip_suffix(".md")
        .ok_or_else(|| format!("content file must end in .md: {path}"))?;
    let mut components: Vec<&str> = stem.split('/').collect();
    if components.iter().any(|segment| {
        segment.is_empty()
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    }) {
        // Hugo-style section index is accepted only as the terminal filename.
        if !(components.last() == Some(&"_index")
            && components[..components.len() - 1].iter().all(|segment| {
                !segment.is_empty()
                    && segment.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            }))
        {
            return Err(format!("invalid content slug: {path}"));
        }
    }
    if matches!(components.last(), Some(&"index" | &"_index")) {
        components.pop();
    }
    if components.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(format!("/{}", components.join("/")))
    }
}

/// Discover typed Markdown pages under a collection root, without rendering or
/// route registration. Both traversal and final output are deterministic.
///
/// Symlinks are refused at every level, including non-Markdown assets, so a
/// future asset pipeline cannot accidentally follow a link outside the root.
/// All invalid Markdown and duplicate leaf/bundle slugs are build errors.
pub fn load_collection(root: &std::path::Path) -> Result<Vec<ContentPage>, String> {
    use std::collections::BTreeMap;

    let root_type = std::fs::symlink_metadata(root)
        .map_err(|error| format!("cannot inspect content root {}: {error}", root.display()))?
        .file_type();
    if root_type.is_symlink() || !root_type.is_dir() {
        return Err(format!(
            "content root must be a real directory: {}",
            root.display()
        ));
    }

    let mut pages = BTreeMap::new();
    collect_pages(root, root, &mut pages)?;
    Ok(pages.into_values().map(|(_, page)| page).collect())
}

fn collect_pages(
    root: &std::path::Path,
    directory: &std::path::Path,
    pages: &mut std::collections::BTreeMap<String, (String, ContentPage)>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(directory).map_err(|error| {
        format!(
            "cannot read content directory {}: {error}",
            directory.display()
        )
    })?;
    let mut entries = entries
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| {
            format!(
                "cannot read content entry in {}: {error}",
                directory.display()
            )
        })?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect content path {}: {error}", path.display()))?;
        if file_type.is_symlink() {
            return Err(format!(
                "symlinks are not permitted in content: {}",
                path.display()
            ));
        }
        if file_type.is_dir() {
            collect_pages(root, &path, pages)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(format!("unsupported content entry: {}", path.display()));
        }
        if path.extension().is_none_or(|extension| extension != "md") {
            // Ordinary image/asset files are not pages. Asset processing is
            // intentionally deferred to the future typed resource graph.
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .map_err(|error| format!("content path escaped root: {error}"))?;
        let segments = relative
            .components()
            .map(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .ok_or_else(|| format!("content path is not UTF-8: {}", path.display()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let relative_name = segments.join("/");
        let source = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read content file {}: {error}", path.display()))?;
        let page = parse_page(&relative_name, &source)?;
        if let Some((existing, _)) = pages.get(&page.slug) {
            return Err(format!(
                "duplicate content slug {} from {} and {}",
                page.slug, existing, relative_name
            ));
        }
        pages.insert(page.slug.clone(), (relative_name, page));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_page;

    #[test]
    fn parses_typed_front_matter_and_preserves_markdown() {
        let page = parse_page(
            "blog/hello.md",
            "+++\ntitle = \"Hello\"\ndraft = true\ntags = [\"nulang\", \"web\"]\n+++\n# Hello\n",
        )
        .unwrap();
        assert_eq!(page.slug, "/blog/hello");
        assert_eq!(page.title, "Hello");
        assert!(page.draft);
        assert_eq!(page.tags, vec!["nulang", "web"]);
        assert_eq!(page.body, "# Hello\n");
    }

    #[test]
    fn page_bundle_index_has_same_slug_as_leaf_file() {
        let source = "+++\ntitle = \"Post\"\n+++\nBody";
        let leaf = parse_page("blog/post.md", source).unwrap();
        let bundle = parse_page("blog/post/index.md", source).unwrap();
        assert_eq!(leaf.slug, bundle.slug);
        assert!(!bundle.draft);
        assert!(bundle.tags.is_empty());
    }

    #[test]
    fn rejects_invalid_typed_front_matter() {
        assert!(parse_page("post.md", "# Missing metadata").is_err());
        assert!(parse_page("post.md", "+++\ndraft = true\n+++\nBody").is_err());
        assert!(parse_page("post.md", "+++\ntitle = 2\n+++\nBody").is_err());
        assert!(parse_page(
            "post.md",
            "+++\ntitle = \"Ok\"\ntags = \"not-array\"\n+++\nBody"
        )
        .is_err());
        assert!(parse_page("post.md", "+++\ntitle = \"Incomplete\"\nBody").is_err());
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_paths() {
        let source = "+++\ntitle = \"Safe\"\n+++\nBody";
        for path in [
            "../post.md",
            "/absolute.md",
            "blog/Bad Name.md",
            "blog\\post.md",
            "blog/./post.md",
            "blog//post.md",
        ] {
            assert!(parse_page(path, source).is_err(), "{path}");
        }
        assert!(parse_page("blog/post.txt", source).is_err());
    }

    struct CollectionFixture(std::path::PathBuf);

    impl CollectionFixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "nulang-web-content-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, text: &str) {
            let file = self.0.join(name);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
    }

    impl Drop for CollectionFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn collection_is_sorted_and_preserves_draft_pages_without_publishing() {
        let fixture = CollectionFixture::new();
        fixture.write(
            "blog/zeta.md",
            "+++\ntitle = \"Zeta\"\ndraft = true\n+++\n# Draft\n",
        );
        fixture.write(
            "blog/alpha/index.md",
            "+++\ntitle = \"Alpha\"\n+++\n# Article\n",
        );
        fixture.write("blog/alpha/photo.png", "ignored asset");
        fixture.write("index.md", "+++\ntitle = \"Home\"\n+++\n# Home\n");

        let pages = super::load_collection(&fixture.0).unwrap();
        let slugs: Vec<&str> = pages.iter().map(|page| page.slug.as_str()).collect();
        assert_eq!(slugs, vec!["/", "/blog/alpha", "/blog/zeta"]);
        assert!(pages[2].draft);
        assert_eq!(pages[1].body, "# Article\n");
    }

    #[test]
    fn collection_rejects_leaf_bundle_slug_collisions() {
        let fixture = CollectionFixture::new();
        fixture.write("news/story.md", "+++\ntitle = \"Leaf\"\n+++\nLeaf");
        fixture.write(
            "news/story/index.md",
            "+++\ntitle = \"Bundle\"\n+++\nBundle",
        );

        let err = super::load_collection(&fixture.0).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
        assert!(err.contains("/news/story"), "{err}");
    }

    #[test]
    fn collection_rejects_invalid_markdown_and_invalid_subpaths() {
        let fixture = CollectionFixture::new();
        fixture.write("valid.md", "+++\ntitle = \"Valid\"\n+++\nOkay");
        fixture.write(
            "blog/Bad Name.md",
            "+++\ntitle = \"Invalid Path\"\n+++\nBody",
        );
        assert!(super::load_collection(&fixture.0).is_err());
        std::fs::remove_file(fixture.0.join("blog/Bad Name.md")).unwrap();
        fixture.write("blog/invalid.md", "No front matter");
        assert!(super::load_collection(&fixture.0).is_err());
    }

    #[test]
    fn section_indexes_are_distinct_from_leaf_and_bundle_pages() {
        use super::ContentKind;

        let source = "+++\ntitle = \"Docs\"\n+++\nContent";
        let section = parse_page("docs/_index.md", source).unwrap();
        let bundle = parse_page("docs/index.md", source).unwrap();
        let leaf = parse_page("docs.md", source).unwrap();
        let root_section = parse_page("_index.md", source).unwrap();

        assert_eq!(section.slug, "/docs");
        assert_eq!(section.kind, ContentKind::Section);
        assert_eq!(section.source_path, "docs/_index.md");
        assert_eq!(bundle.slug, "/docs");
        assert_eq!(bundle.kind, ContentKind::Page);
        assert_eq!(bundle.source_path, "docs/index.md");
        assert_eq!(leaf.kind, ContentKind::Page);
        assert_eq!(leaf.source_path, "docs.md");
        assert_eq!(root_section.slug, "/");
        assert_eq!(root_section.kind, ContentKind::Section);
    }

    #[test]
    fn collection_preserves_page_and_section_origins_deterministically() {
        use super::ContentKind;
        let fixture = CollectionFixture::new();
        fixture.write("docs/_index.md", "+++\ntitle = \"Docs\"\n+++\nIntro");
        fixture.write("docs/guides/getting-started/index.md", "+++\ntitle = \"Start\"\n+++\nStart");
        fixture.write("docs/guides/_index.md", "+++\ntitle = \"Guides\"\n+++\nGuides");
        let entries = super::load_collection(&fixture.0).unwrap();
        let actual: Vec<_> = entries.iter().map(|entry| {
            (entry.slug.as_str(), entry.source_path.as_str(), entry.kind)
        }).collect();
        assert_eq!(
            actual,
            vec![
                ("/docs", "docs/_index.md", ContentKind::Section),
                ("/docs/guides", "docs/guides/_index.md", ContentKind::Section),
                ("/docs/guides/getting-started", "docs/guides/getting-started/index.md", ContentKind::Page),
            ]
        );
    }

    #[test]
    fn collection_rejects_page_and_section_claiming_same_slug() {
        let fixture = CollectionFixture::new();
        fixture.write("docs.md", "+++\ntitle = \"Page\"\n+++\nPage");
        fixture.write("docs/_index.md", "+++\ntitle = \"Section\"\n+++\nSection");
        let error = super::load_collection(&fixture.0).unwrap_err();
        assert!(error.contains("duplicate content slug /docs"), "{error}");
        assert!(error.contains("docs.md"), "{error}");
        assert!(error.contains("docs/_index.md"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn collection_rejects_symlinked_pages_and_directories() {
        use std::os::unix::fs::symlink;
        let fixture = CollectionFixture::new();
        let outside = CollectionFixture::new();
        outside.write("secret.md", "+++\ntitle = \"Secret\"\n+++\nSecret");
        symlink(outside.0.join("secret.md"), fixture.0.join("linked.md")).unwrap();
        assert!(super::load_collection(&fixture.0).is_err());
        std::fs::remove_file(fixture.0.join("linked.md")).unwrap();
        symlink(&outside.0, fixture.0.join("directory")).unwrap();
        assert!(super::load_collection(&fixture.0).is_err());
        assert!(super::load_collection(&fixture.0.join("directory")).is_err());
    }
}
