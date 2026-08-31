//! Static host adapter (S3, R2, Netlify, Vercel, …).
//!
//! Generates a `_redirects` file in `dist/` so that static routes serve their
//! emitted HTML files and server routes fall back to a function or edge path
//! that the host operator wires separately. The IR itself is also copied as
//! `nulang-app.ir.json`.

use crate::web::ir::DeploymentIr;
use std::path::Path;

pub const NAME: &str = "static_host";

/// Generate static-host specific files in `output_dir`.
///
/// For every static route, add a redirect rule mapping the route path to the
/// generated HTML artifact. Server routes are listed as comments so the host
/// operator can wire them to an edge/serverless function.
pub fn generate_files(output_dir: &Path, ir: &DeploymentIr) -> std::io::Result<()> {
    let mut lines = Vec::new();
    lines.push("# Nulang static host redirects".to_string());
    for route in &ir.routes {
        if route.placement == "static" {
            if let Some(artifact) = &route.artifact {
                lines.push(format!("{} {} 200", route.path, artifact));
            }
        } else {
            lines.push(format!(
                "# server route: {} {} -> (host function)",
                route.method, route.path
            ));
        }
    }
    std::fs::write(output_dir.join("_redirects"), lines.join("\n"))?;
    Ok(())
}
