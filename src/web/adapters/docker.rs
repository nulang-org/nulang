//! Docker adapter for self-hosting a Nulang Web app.
//!
//! Produces a minimal `Dockerfile` and `.dockerignore` in `dist/` suitable for
//! serving the static output with nginx. Server routes require a separate
//! server container; this adapter documents the static tier.

use std::path::Path;

pub const NAME: &str = "docker";

pub fn generate_files(output_dir: &Path) -> std::io::Result<()> {
    let dockerfile = r#"# Nulang Web static tier
FROM nginx:alpine
COPY . /usr/share/nginx/html
EXPOSE 80
"#;
    std::fs::write(output_dir.join("Dockerfile"), dockerfile)?;

    let dockerignore = r#".nula
"#;
    std::fs::write(output_dir.join(".dockerignore"), dockerignore)?;
    Ok(())
}
