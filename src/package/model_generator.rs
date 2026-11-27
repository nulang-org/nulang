//! Deterministic, fail-closed application resource scaffolding.
//!
//! This is deliberately a source generator, not a second router, ORM, or
//! authorization engine. Generated policy hooks deny access until an
//! application explicitly connects its own authentication/authorization.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::package::manifest::{Manifest, MANIFEST_FILE};
use crate::types::{NuError, NuResult, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    name: String,
    ty: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Resource {
    name: String,
    fields: Vec<Field>,
    read_only_views: bool,
}

fn invalid(msg: impl Into<String>) -> NuError {
    NuError::PackageError {
        msg: msg.into(),
        span: Span::default(),
    }
}

fn valid_resource_name(name: &str) -> bool {
    name.len() <= 64
        && name.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
        && name.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn valid_field_name(name: &str) -> bool {
    let valid_shape = !name.is_empty()
        && name.len() <= 64
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !valid_shape || matches!(name, "id" | "tenant_id") {
        return false;
    }

    // Use the language's authoritative lexer rather than a hand-maintained
    // keyword list. New syntax keywords must not silently become bad scaffolds.
    let Ok(tokens) = crate::lexer::Lexer::new(name).lex() else {
        return false;
    };
    tokens.len() == 2
        && matches!(&tokens[0].kind, crate::lexer::TokenKind::Ident(identifier) if identifier == name)
        && matches!(&tokens[1].kind, crate::lexer::TokenKind::Eof)
}

impl Resource {
    fn parse(args: &[String]) -> NuResult<Self> {
        if args.first().map(String::as_str) != Some("model") {
            return Err(invalid(
                "usage: nula generate model <PascalCase> --field name:String [--field rate:Float ...] [--read-only-views]",
            ));
        }
        let name = args
            .get(1)
            .ok_or_else(|| invalid("resource name is required"))?;
        if !valid_resource_name(name) {
            return Err(invalid(format!(
                "invalid resource name '{name}' (expected PascalCase ASCII identifier, up to 64 characters)"
            )));
        }

        let mut fields = Vec::new();
        let mut seen = HashSet::new();
        let mut read_only_views = false;
        let mut index = 2;
        while index < args.len() {
            if args[index] == "--read-only-views" {
                if read_only_views {
                    return Err(invalid("--read-only-views may only appear once"));
                }
                read_only_views = true;
                index += 1;
                continue;
            }
            if args[index] != "--field" {
                return Err(invalid(format!(
                    "unexpected argument '{}' (expected --field name:Type)",
                    args[index]
                )));
            }
            let spec = args
                .get(index + 1)
                .ok_or_else(|| invalid("--field requires name:Type"))?;
            let (field, ty) = spec
                .split_once(':')
                .ok_or_else(|| invalid(format!("invalid field '{spec}' (expected name:Type)")))?;
            if !valid_field_name(field) {
                return Err(invalid(format!(
                    "invalid or reserved field name '{field}' (id and tenant_id are generated automatically)"
                )));
            }
            if !matches!(ty, "String" | "Int" | "Float" | "Bool") {
                return Err(invalid(format!(
                    "unsupported field type '{ty}' (supported: String, Int, Float, Bool)"
                )));
            }
            if !seen.insert(field.to_string()) {
                return Err(invalid(format!("duplicate field '{field}'")));
            }
            fields.push(Field {
                name: field.to_string(),
                ty: ty.to_string(),
            });
            index += 2;
        }
        if fields.is_empty() {
            return Err(invalid("at least one --field name:Type is required"));
        }

        // Order is canonical, independent of CLI flag ordering.
        fields.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(Self {
            name: name.clone(),
            fields,
            read_only_views,
        })
    }

    fn file_stem(&self) -> String {
        let mut stem = String::new();
        let mut previous_lower_or_digit = false;
        for letter in self.name.chars() {
            if letter.is_ascii_uppercase() && previous_lower_or_digit {
                stem.push('_');
            }
            stem.push(letter.to_ascii_lowercase());
            previous_lower_or_digit = letter.is_ascii_lowercase() || letter.is_ascii_digit();
        }
        stem
    }

    fn render(&self) -> String {
        let stem = self.file_stem();
        let mut code = format!(
            "// Generated by nula generate model. Edit to add domain behavior.\n\
             // Route registration, persistence, and authorization integration are NOT generated.\n\
             // Every policy hook denies access until explicitly implemented.\n\n\
             pub type {} = {{\n    id: String,\n    tenant_id: String",
            self.name
        );
        for field in &self.fields {
            code.push_str(&format!(",\n    {}: {}", field.name, field.ty));
        }
        code.push_str("\n}\n\n");

        code.push_str(&format!("pub type {}Input = {{\n", self.name));
        for (index, field) in self.fields.iter().enumerate() {
            if index > 0 {
                code.push_str(",\n");
            }
            code.push_str(&format!("    {}: {}", field.name, field.ty));
        }
        code.push_str("\n}\n\n");

        code.push_str(&format!(
            "// Connect these hooks to authenticated tenant and permission checks.\n\
             // A matching tenant alone is not sufficient for authorization.\n\
             pub fn can_read_{stem}(_actor_tenant_id: String, _record: {}) -> Bool {{ false }}\n\
             pub fn can_write_{stem}(_actor_tenant_id: String, _record: {}) -> Bool {{ false }}\n",
            self.name, self.name
        ));
        if self.read_only_views {
            code = format!("import stdlib::web::types\nimport stdlib::web::admin\nimport stdlib::web::host\n\n{code}");
            code.push_str(&self.render_read_only_views());
        }
        code
    }

    /// Render opt-in views from the same validated fields as the domain type.
    /// Identity is obtained only from a trusted request-scoped host effect;
    /// authorization hooks still deny every record until implemented.
    fn render_read_only_views(&self) -> String {
        let stem = self.file_stem();
        let mut column_names = vec!["ID".to_string()];
        let mut values = vec!["record.id".to_string()];
        for field in &self.fields {
            column_names.push(field.name.clone());
            let value = match field.ty.as_str() {
                "String" => format!("record.{}", field.name),
                "Int" => format!("perform Int.to_string(record.{})", field.name),
                "Float" => format!("perform Float.to_string(record.{})", field.name),
                "Bool" => format!("if record.{} then \"true\" else \"false\"", field.name),
                _ => unreachable!("validated model field type"),
            };
            values.push(value);
        }
        let columns = column_names
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let row = values.join(", ");
        format!(
            "\n// Presentation helpers only. Do not derive actor_tenant_id from request input.\n\
             // Nonempty tenant and record tenant equality are mandatory before policy.
\
             // The only identity source is Web.verified_tenant_id in an authenticated request.\n\
             // A matching tenant alone is not authorization: also apply custom policy.\n\
             pub fn {stem}_readonly_table(records: [{}]) -> Html {{\n\
                 let actor_tenant_id = verified_tenant_id();\n\
                 var rows = [];\n\
                 for record in records {{\n\
                     if actor_tenant_id != \"\" && record.tenant_id == actor_tenant_id && can_read_{stem}(actor_tenant_id, record) then {{\n\
                         rows = perform Array.push(rows, [{row}])\n\
                     }} else {{}}\n\
                 }};\n\
                 readonly_table([{columns}], rows)\n\
             }}\n\n\
             pub fn {stem}_readonly_detail(record: {}) -> Html {{\n\
                 let actor_tenant_id = verified_tenant_id();\n\
                 if actor_tenant_id != \"\" && record.tenant_id == actor_tenant_id && can_read_{stem}(actor_tenant_id, record) then {{\n\
                     readonly_detail([{columns}], [{row}])\n\
                 }} else {{\n\
                     denied_view()\n\
                 }}\n\
             }}\n",
            self.name, self.name
        )
    }
}

/// Generate one resource module without overwriting existing source.
///
/// Safe by default: this creates type declarations and deny-all policy hooks,
/// but never installs routes or persistence operations implicitly.
pub fn run(args: &[String], root: &Path) -> NuResult<PathBuf> {
    // Validate every argument before performing filesystem writes.
    let resource = Resource::parse(args)?;
    if !root.join(MANIFEST_FILE).is_file() {
        return Err(invalid(format!(
            "no {MANIFEST_FILE} found in {} — initialize a package first",
            root.display()
        )));
    }
    Manifest::load(root)?;

    let source_dir = root.join("src");
    let dest_dir = source_dir.join("resources");
    let output = dest_dir.join(format!("{}.nula", resource.file_stem()));
    // A pre-existing src/ or src/resources/ symlink can redirect generation
    // outside the package. Reject it instead of following it implicitly.
    for dir in [&source_dir, &dest_dir] {
        match fs::symlink_metadata(dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "cannot generate resources through symlinked directory {}",
                    dir.display()
                )));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(invalid(format!(
                    "resource output parent {} is not a directory",
                    dir.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(invalid(format!(
                    "cannot inspect resource output directory {}: {error}",
                    dir.display()
                )));
            }
        }
    }
    fs::create_dir_all(&dest_dir)
        .map_err(|error| invalid(format!("cannot create {}: {error}", dest_dir.display())))?;
    // Defense in depth: verify the effective directory is still inside the
    // package root after it has been created. This is not a concurrent-write
    // sandbox, but it prevents ordinary pre-existing redirect mistakes.
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| invalid(format!("cannot resolve {}: {error}", root.display())))?;
    let canonical_dest = fs::canonicalize(&dest_dir)
        .map_err(|error| invalid(format!("cannot resolve {}: {error}", dest_dir.display())))?;
    if !canonical_dest.starts_with(&canonical_root) {
        return Err(invalid(format!(
            "resource output directory {} resolves outside the package",
            dest_dir.display()
        )));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)
        .map_err(|error| {
            invalid(format!(
                "cannot create {} without overwriting: {error}",
                output.display()
            ))
        })?;

    if let Err(error) = file.write_all(resource.render().as_bytes()) {
        drop(file);
        let _ = fs::remove_file(&output);
        return Err(invalid(format!(
            "cannot write {}: {error}",
            output.display()
        )));
    }
    println!("Created typed model scaffold at {}", output.display());
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter()
            .map(|argument| (*argument).to_string())
            .collect()
    }

    fn test_package(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nulang_resource_scaffold_{label}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(MANIFEST_FILE),
            "[package]\nname = \"resource-test\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn generated_types_and_fail_closed_policies_typecheck() {
        let resource = Resource::parse(&argv(&[
            "model",
            "BillboardFace",
            "--field",
            "daily_rate:Float",
            "--field",
            "name:String",
            "--field",
            "active:Bool",
        ]))
        .unwrap();
        assert_eq!(resource.file_stem(), "billboard_face");
        let source = resource.render();
        assert!(source.contains("pub type BillboardFace = {"));
        assert!(source.contains("tenant_id: String"));
        assert!(source.contains("pub type BillboardFaceInput = {"));
        assert!(source.contains("can_read_billboard_face"));
        assert!(source.contains("-> Bool { false }"));
        let tokens = Lexer::new(&source).lex().expect("generated source lexes");
        let module = Parser::new(tokens)
            .parse_module()
            .expect("generated source parses");
        TypeChecker::new()
            .check_module(&module)
            .expect("generated source typechecks");
    }

    #[test]
    fn opt_in_read_only_views_are_generated_from_model_fields_with_denied_policy() {
        let resource = Resource::parse(&argv(&[
            "model",
            "BillboardFace",
            "--field",
            "name:String",
            "--field",
            "daily_rate:Float",
            "--field",
            "active:Bool",
            "--field",
            "impressions:Int",
            "--read-only-views",
        ]))
        .expect("parse opt-in views");
        let output = resource.render();
        assert!(output.contains("import stdlib::web::admin"));
        assert!(output.contains("pub fn billboard_face_readonly_table("));
        assert!(output.contains("pub fn billboard_face_readonly_detail("));
        assert!(output.contains("can_read_billboard_face(actor_tenant_id, record)"));
        assert!(output.contains("perform Float.to_string(record.daily_rate)"));
        assert!(output.contains("perform Int.to_string(record.impressions)"));
        assert!(output.contains(r#"if record.active then "true" else "false""#));
        assert!(!output.contains("Web.route"));
        assert!(!output.contains("can_write_billboard_face(actor_tenant_id, record)"));
    }

    #[test]
    fn generated_view_identity_is_server_owned_not_a_caller_argument() {
        let resource = Resource::parse(&argv(&[
            "model",
            "BillboardFace",
            "--field",
            "name:String",
            "--read-only-views",
        ]))
        .unwrap();
        let code = resource.render();
        assert!(code.contains("import stdlib::web::host"));
        assert!(code.contains("pub fn billboard_face_readonly_table(records: [BillboardFace])"));
        assert!(code.contains("pub fn billboard_face_readonly_detail(record: BillboardFace)"));
        assert!(code.contains("let actor_tenant_id = verified_tenant_id()"));
        assert!(!code.contains("pub fn billboard_face_readonly_table(actor_tenant_id: String"));
        assert!(!code.contains("pub fn billboard_face_readonly_detail(actor_tenant_id: String"));
    }

    #[test]
    fn generated_read_only_views_resolve_imports_and_typecheck() {
        let resource = Resource::parse(&argv(&[
            "model",
            "BillboardFace",
            "--field",
            "name:String",
            "--field",
            "daily_rate:Float",
            "--field",
            "active:Bool",
            "--field",
            "impressions:Int",
            "--read-only-views",
        ]))
        .unwrap();
        let source = resource.render();
        let mut module = Parser::new(Lexer::new(&source).lex().unwrap())
            .parse_module()
            .expect("generated resource views parse");
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("conformance/behavior/web_21_admin_views.nula");
        let mut visited = HashSet::new();
        crate::resolver::resolve_imports(&mut module, &path, &mut visited)
            .expect("generated views import admin stdlib");
        TypeChecker::new()
            .check_module(&module)
            .expect("generated views typecheck against the model");
    }

    // Exercise the generated code over the real experimental Web HTTP host,
    // not just parser/typechecker or string assertions.
    fn generated_http_view(source: &str) -> String {
        generated_http_view_authenticated(source, None)
    }

    fn generated_http_view_authenticated(source: &str, verified_tenant: Option<&str>) -> String {
        use crate::runtime::{HttpMethod, WebDevServer, WebRoute};
        use std::io::{Read, Write};
        use std::net::{Shutdown, TcpStream};
        use std::time::Duration;

        let mut module = Parser::new(Lexer::new(source).lex().expect("lex model view"))
            .parse_module()
            .expect("parse generated view source");
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("conformance/behavior/web_21_admin_views.nula");
        let mut visited = HashSet::new();
        crate::resolver::resolve_imports(&mut module, &path, &mut visited)
            .expect("resolve generated view dependencies");
        let mut checker = TypeChecker::new();
        checker
            .check_module(&module)
            .expect("typecheck generated view");
        let hir = crate::hir_lower::lower_module(&module, &checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).expect("lower view MIR");
        let compiled = crate::mir_codegen::compile_mir(&mut mir, "model-view-policy")
            .expect("compile generated resource view");
        let offset = compiled
            .debug_functions
            .iter()
            .find(|f| f.name == "show")
            .expect("show handler")
            .code_offset;
        let idx = compiled
            .function_table
            .iter()
            .position(|code_offset| *code_offset == offset)
            .expect("compiled show handler slot");
        let route = WebRoute {
            method: HttpMethod::Get,
            path: "/resource-view".into(),
            handler_module: compiled,
            handler_func_idx: idx,
        };
        let server = if let Some(tenant_id) = verified_tenant {
            use crate::web::tenant_auth::{TenantAuthGuard, VerifiedTenant};
            let tenant_id = tenant_id.to_owned();
            let resolver = std::sync::Arc::new(move |token: &str| {
                if token == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" {
                    VerifiedTenant::new("test-user", &tenant_id)
                } else {
                    None
                }
            });
            let guard = TenantAuthGuard::new(
                "__Host-nulang_tenant",
                vec![(HttpMethod::Get, "/resource-view".into())],
                resolver,
            )
            .unwrap();
            WebDevServer::bind_with_tenant_guard(0, None, None, vec![route], guard)
                .expect("bind guarded model-view HTTP server")
        } else {
            WebDevServer::bind(0, None, None, vec![route])
                .expect("bind unguarded model-view HTTP server")
        };
        let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request =
            "GET /resource-view HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n".to_owned();
        if verified_tenant.is_some() {
            request.push_str("Cookie: __Host-nulang_tenant=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).expect("read HTTP response");
        String::from_utf8(bytes).expect("UTF-8 response")
    }

    fn demo_model_with_read_policy_permitting_everything() -> String {
        let resource = Resource::parse(&argv(&[
            "model",
            "BillboardFace",
            "--field",
            "name:String",
            "--field",
            "active:Bool",
            "--read-only-views",
        ]))
        .expect("generate model with table/detail views");
        let default = resource.render();
        let original = "pub fn can_read_billboard_face(_actor_tenant_id: String, _record: BillboardFace) -> Bool { false }";
        assert!(default.contains(original), "model must default deny");
        default.replacen(original,
            "pub fn can_read_billboard_face(_actor_tenant_id: String, _record: BillboardFace) -> Bool { true }", 1)
    }

    #[test]
    fn generated_table_blocks_cross_tenant_data_even_if_app_policy_is_too_permissive() {
        let mut source = demo_model_with_read_policy_permitting_everything();
        source.push_str(
            r#"
fn show() {
    let own = {id: "1", tenant_id: "tenant-a", name: "VISIBLE-OWN", active: true} in
    let foreign = {id: "2", tenant_id: "tenant-b", name: "SECRET-FOREIGN", active: true} in
    billboard_face_readonly_table([own, foreign])
}
"#,
        );
        let response = generated_http_view_authenticated(&source, Some("tenant-a"));
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("VISIBLE-OWN"), "{response}");
        assert!(!response.contains("SECRET-FOREIGN"), "{response}");
    }

    #[test]
    fn generated_detail_blocks_cross_tenant_record_even_if_app_policy_is_too_permissive() {
        let mut source = demo_model_with_read_policy_permitting_everything();
        source.push_str(
            r#"
fn show() {
    let foreign = {id: "2", tenant_id: "tenant-b", name: "SECRET-FOREIGN", active: true} in
    billboard_face_readonly_detail(foreign)
}
"#,
        );
        let response = generated_http_view_authenticated(&source, Some("tenant-a"));
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("Access denied"), "{response}");
        assert!(!response.contains("SECRET-FOREIGN"), "{response}");
    }

    #[test]
    fn generated_detail_rejects_empty_tenant_identity_even_with_permissive_policy() {
        let mut source = demo_model_with_read_policy_permitting_everything();
        source.push_str(
            r#"
fn show() {
    let unscoped = {id: "1", tenant_id: "", name: "SECRET-NO-IDENTITY", active: true} in
    billboard_face_readonly_detail(unscoped)
}
"#,
        );
        let response = generated_http_view(&source);
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("Access denied"), "{response}");
        assert!(!response.contains("SECRET-NO-IDENTITY"), "{response}");
    }

    #[test]
    fn generated_table_rejects_empty_tenant_identity_even_with_permissive_policy() {
        let mut source = demo_model_with_read_policy_permitting_everything();
        source.push_str(
            r#"
fn show() {
    let unscoped = {id: "1", tenant_id: "", name: "SECRET-NO-IDENTITY", active: true} in
    billboard_face_readonly_table([unscoped])
}
"#,
        );
        let response = generated_http_view(&source);
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(!response.contains("SECRET-NO-IDENTITY"), "{response}");
    }

    #[test]
    fn generated_detail_allows_same_tenant_record_after_explicit_permissive_policy() {
        let mut source = demo_model_with_read_policy_permitting_everything();
        source.push_str(
            r#"
fn show() {
    let own = {id: "1", tenant_id: "tenant-a", name: "VISIBLE-OWN", active: true} in
    billboard_face_readonly_detail(own)
}
"#,
        );
        let response = generated_http_view_authenticated(&source, Some("tenant-a"));
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("VISIBLE-OWN"), "{response}");
    }

    #[test]
    fn generated_detail_defaults_to_denied_even_for_own_tenant_record() {
        let source = Resource::parse(&argv(&[
            "model",
            "BillboardFace",
            "--field",
            "name:String",
            "--field",
            "active:Bool",
            "--read-only-views",
        ]))
        .unwrap()
        .render();
        let source = format!(
            "{source}\n{}",
            r#"
fn show() {
    let own = {id: "1", tenant_id: "tenant-a", name: "SECRET-OWN", active: true} in
    billboard_face_readonly_detail(own)
}
"#
        );
        let response = generated_http_view_authenticated(&source, Some("tenant-a"));
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("Access denied"), "{response}");
        assert!(!response.contains("SECRET-OWN"), "{response}");
    }

    #[test]
    fn view_generation_is_explicit_and_rejects_duplicate_options() {
        let default = Resource::parse(&argv(&["model", "BillboardFace", "--field", "name:String"]))
            .unwrap()
            .render();
        assert!(!default.contains("readonly_table"));
        assert!(!default.contains("import stdlib::web::admin"));
        assert!(Resource::parse(&argv(&[
            "model",
            "BillboardFace",
            "--field",
            "name:String",
            "--read-only-views",
            "--read-only-views",
        ]))
        .is_err());
    }

    #[test]
    fn canonical_field_order_is_independent_of_flag_order() {
        let a = Resource::parse(&argv(&[
            "model",
            "Billboard",
            "--field",
            "name:String",
            "--field",
            "rate:Float",
        ]))
        .unwrap();
        let b = Resource::parse(&argv(&[
            "model",
            "Billboard",
            "--field",
            "rate:Float",
            "--field",
            "name:String",
        ]))
        .unwrap();
        assert_eq!(a.render(), b.render());
    }

    #[test]
    fn lexer_keywords_cannot_be_generated_as_resource_fields() {
        for name in [
            "in",
            "with",
            "as",
            "module",
            "entity",
            "behavior",
            "await",
            "using",
            "organization",
            "defer",
        ] {
            let specification = format!("{name}:String");
            assert!(
                Resource::parse(&argv(&["model", "Billboard", "--field", &specification])).is_err(),
                "reserved lexer keyword {name:?} must not become a field",
            );
        }
    }

    #[test]
    fn malicious_and_duplicate_fields_are_rejected() {
        for field in [
            "id:String",
            "tenant_id:String",
            "name:String}",
            "name:Unknown",
            "name:String\n",
            "type:String",
        ] {
            assert!(
                Resource::parse(&argv(&["model", "Billboard", "--field", field])).is_err(),
                "should reject {field:?}"
            );
        }
        assert!(Resource::parse(&argv(&[
            "model",
            "Billboard",
            "--field",
            "name:String",
            "--field",
            "name:Int",
        ]))
        .is_err());
        assert!(
            Resource::parse(&argv(&["model", "../../Escape", "--field", "name:String",])).is_err()
        );
        assert!(Resource::parse(&argv(&["model", "Billboard", "--unknown"])).is_err());
    }

    #[test]
    fn generator_does_not_replace_existing_source() {
        let root = test_package("no_overwrite");
        let args = argv(&["model", "Billboard", "--field", "name:String"]);
        let path = run(&args, &root).expect("first scaffold succeeds");
        let original = fs::read_to_string(&path).unwrap();
        assert!(run(&args, &root).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn validation_failure_does_not_create_any_resource_files() {
        let root = test_package("no_partial");
        let result = run(
            &argv(&[
                "model",
                "Billboard",
                "--field",
                "name:String",
                "--field",
                "id:Int",
            ]),
            &root,
        );
        assert!(result.is_err());
        assert!(!root.join("src").join("resources").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn generator_requires_existing_package_manifest() {
        let dir = std::env::temp_dir().join(format!(
            "nulang_resource_missing_manifest_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let result = run(
            &argv(&["model", "Billboard", "--field", "name:String"]),
            &dir,
        );
        assert!(result.is_err());
        assert!(!dir.join("src").exists());
    }

    #[cfg(unix)]
    #[test]
    fn generator_rejects_symlinked_resource_directory() {
        use std::os::unix::fs::symlink;

        let root = test_package("resource_symlink");
        let outside = test_package("resource_symlink_outside");
        fs::create_dir_all(root.join("src")).unwrap();
        symlink(&outside, root.join("src").join("resources")).unwrap();

        let result = run(
            &argv(&["model", "Billboard", "--field", "name:String"]),
            &root,
        );
        assert!(
            result.is_err(),
            "generator must reject a symlinked output directory"
        );
        assert!(!outside.join("billboard.nula").exists());

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn generator_rejects_symlinked_source_directory() {
        use std::os::unix::fs::symlink;

        let root = test_package("src_symlink");
        let outside = test_package("src_symlink_outside");
        symlink(&outside, root.join("src")).unwrap();

        let result = run(
            &argv(&["model", "Billboard", "--field", "name:String"]),
            &root,
        );
        assert!(
            result.is_err(),
            "generator must reject a symlinked source directory"
        );
        assert!(!outside.join("resources").exists());

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }
}
