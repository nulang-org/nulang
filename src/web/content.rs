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

#[cfg(test)]
mod tests {
    use super::parse_page;

    #[test]
    fn parses_typed_front_matter_and_preserves_markdown() {
        let page = parse_page(
            "blog/hello.md",
            "+++\ntitle = \"Hello\"\ndraft = true\ntags = [\"nulang\", \"web\"]\n+++\n# Hello\n",
        ).unwrap();
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
        assert!(parse_page("post.md", "+++\ntitle = \"Ok\"\ntags = \"not-array\"\n+++\nBody").is_err());
        assert!(parse_page("post.md", "+++\ntitle = \"Incomplete\"\nBody").is_err());
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_paths() {
        let source = "+++\ntitle = \"Safe\"\n+++\nBody";
        for path in ["../post.md", "/absolute.md", "blog/Bad Name.md", "blog\\post.md", "blog/./post.md", "blog//post.md"] {
            assert!(parse_page(path, source).is_err(), "{path}");
        }
        assert!(parse_page("blog/post.txt", source).is_err());
    }
}
