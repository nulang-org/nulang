//! Canonical compiler/runtime URL composition for statically scoped routes.
//!
//! Prefixes must not be interpreted as URLs or filesystem paths: an encoded
//! delimiter, double slash or dot segment changes route identity and can
//! invalidate middleware/authorization boundaries.

/// Join a static group prefix and a route path with one separating slash.
/// Both compiler contract extraction and the standalone VM host use this
/// exact function, preventing discrepancies between declared and registered
/// route identities.
pub fn join_scoped_route_path(prefix: &str, path: &str) -> Result<String, String> {
    fn validate_part(part: &str, label: &str) -> Result<(), String> {
        if part.is_empty() || !part.starts_with('/') {
            return Err(format!("{label} must start with '/'"));
        }
        if part.len() > 1024 || !part.is_ascii() {
            return Err(format!(
                "{label} must be an ASCII path of at most 1024 bytes"
            ));
        }
        // Percent-encoded paths may represent slash/dot tokens and must not
        // change route identity after proxy/backend URL decoding.
        if part
            .bytes()
            .any(|b| b.is_ascii_control() || matches!(b, b'\\' | b'?' | b'#' | b'%'))
        {
            return Err(format!("{label} contains an ambiguous path character"));
        }
        if part.split('/').skip(1).any(|segment| {
            segment == "."
                || segment == ".."
                || segment.is_empty()
                || (segment.bytes().any(|b| b.is_ascii_whitespace())
                    && !(segment.starts_with('{')
                        && segment.ends_with('}')
                        && segment.contains(':')))
        }) && part != "/"
        {
            return Err(format!("{label} contains an empty or dot segment"));
        }
        Ok(())
    }
    validate_part(prefix, "route group prefix")?;
    validate_part(path, "scoped route path")?;
    if path == "/" {
        return Ok(prefix.to_string());
    }
    let result = if prefix == "/" {
        path.to_string()
    } else {
        format!("{prefix}{path}")
    };
    if result.len() > 1024 {
        return Err("combined route path exceeds 1024 bytes".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composes_scoped_paths_without_double_separators() {
        assert_eq!(
            join_scoped_route_path("/api/v1", "/users/:id").unwrap(),
            "/api/v1/users/:id"
        );
        assert_eq!(
            join_scoped_route_path("/api", "/users/{id: String}").unwrap(),
            "/api/users/{id: String}"
        );
        assert_eq!(join_scoped_route_path("/", "/users").unwrap(), "/users");
        assert_eq!(join_scoped_route_path("/api", "/").unwrap(), "/api");
    }

    #[test]
    fn rejects_ambiguous_or_unsafe_scope_paths() {
        for (prefix, path) in [
            ("api", "/users"),
            ("/api/../admin", "/users"),
            ("/api%2fprivate", "/users"),
            ("/api", "//users"),
            ("/api", "/./users"),
            ("/api", "/a/../b"),
            ("/api", "/users?admin=1"),
            ("/api\\admin", "/users"),
            ("/api", "users"),
            ("/api", "/bad name"),
            ("/api/", "/users"),
        ] {
            assert!(
                join_scoped_route_path(prefix, path).is_err(),
                "{prefix:?} + {path:?}"
            );
        }
    }
}
