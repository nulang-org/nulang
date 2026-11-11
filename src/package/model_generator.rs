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
    migration: bool,
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
                "usage: nula generate model <PascalCase> --field name:String [--field rate:Float ...]",
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
        let mut migration = false;
        let mut index = 2;
        while index < args.len() {
            if args[index] == "--migration" {
                if migration {
                    return Err(invalid("--migration may only appear once"));
                }
                migration = true;
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
            migration,
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
        code
    }

    /// Emit application-owned SQL without activating a database connection or
    /// granting privileges. A source model and its migration come from the
    /// same validated field specification.
    fn render_migration(&self) -> NuResult<String> {
        let table = self.file_stem();
        // PostgreSQL truncates identifiers to 63 bytes by default. Truncation
        // would silently change the table being created.
        if table.len() > 63 {
            return Err(invalid(format!(
                "migration table name '{table}' exceeds PostgreSQL's 63-byte identifier limit"
            )));
        }

        let qualified = format!(r#""public"."{table}""#);
        let mut sql = format!(
            r#"-- Generated SQL scaffold; review before applying. Not automatically executed.
-- Use a separate migration role; runtime access remains ungranted.
-- app.tenant_id must come from trusted authentication inside a transaction.
CREATE TABLE {qualified} (
    "tenant_id" UUID NOT NULL,
    "id" UUID NOT NULL"#
        );
        for field in &self.fields {
            let database_type = match field.ty.as_str() {
                "String" => "TEXT",
                "Int" => "BIGINT",
                "Float" => "DOUBLE PRECISION",
                "Bool" => "BOOLEAN",
                _ => unreachable!("Resource::parse validates model types"),
            };
            sql.push_str(&format!(
                r#",
    "{}" {database_type} NOT NULL"#,
                field.name
            ));
        }
        sql.push_str(&format!(
            r#",
    PRIMARY KEY ("tenant_id", "id")
);

ALTER TABLE {qualified} ENABLE ROW LEVEL SECURITY;
ALTER TABLE {qualified} FORCE ROW LEVEL SECURITY;
CREATE POLICY "tenant_isolation" ON {qualified} FOR ALL
  USING ("tenant_id" = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
  WITH CHECK ("tenant_id" = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
"#
        ));
        Ok(sql)
    }
}

/// Generate one resource module without overwriting existing source.
///
/// Safe by default: this creates type declarations and deny-all policy hooks,
/// but never installs routes or persistence operations implicitly.
pub fn run(args: &[String], root: &Path) -> NuResult<PathBuf> {
    // Validate the entire declaration before performing filesystem writes.
    let resource = Resource::parse(args)?;
    let migration_sql = if resource.migration {
        Some(resource.render_migration()?)
    } else {
        None
    };
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
    let migration_dir = root.join("migrations");
    let migration_path = migration_sql
        .as_ref()
        .map(|_| migration_dir.join(format!("create_{}.sql", resource.file_stem())));

    // Do not traverse pre-existing symlink directories, including migrations/.
    // This is not a sandbox against concurrent hostile filesystem mutation.
    let mut directories = vec![&source_dir, &dest_dir];
    if migration_path.is_some() {
        directories.push(&migration_dir);
    }
    for dir in directories {
        match fs::symlink_metadata(dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "resource generation refuses a symlinked directory {}",
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

    // Refuse both collisions before creating either output; use create_new
    // below as a second check against races and preserve all existing files.
    if let Some(path) = &migration_path {
        for candidate in [&output, path] {
            match fs::symlink_metadata(candidate) {
                Ok(_) => {
                    return Err(invalid(format!(
                        "cannot overwrite existing resource output {}",
                        candidate.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(invalid(format!(
                        "cannot inspect resource output {}: {error}",
                        candidate.display()
                    )));
                }
            }
        }
    }

    fs::create_dir_all(&dest_dir)
        .map_err(|error| invalid(format!("cannot create {}: {error}", dest_dir.display())))?;
    if migration_path.is_some() {
        fs::create_dir_all(&migration_dir).map_err(|error| {
            invalid(format!(
                "cannot create {}: {error}",
                migration_dir.display()
            ))
        })?;
    }
    // Refuse output directories that resolve outside the package root.
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| invalid(format!("cannot resolve {}: {error}", root.display())))?;
    for dir in [&dest_dir, &migration_dir] {
        if dir == &migration_dir && migration_path.is_none() {
            continue;
        }
        let canonical = fs::canonicalize(dir)
            .map_err(|error| invalid(format!("cannot resolve {}: {error}", dir.display())))?;
        if !canonical.starts_with(&canonical_root) {
            return Err(invalid(format!(
                "resource output directory {} resolves outside the package",
                dir.display()
            )));
        }
    }

    let mut model_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)
        .map_err(|error| {
            invalid(format!(
                "cannot create {} without overwriting: {error}",
                output.display()
            ))
        })?;

    if let Err(error) = model_file
        .write_all(resource.render().as_bytes())
        .and_then(|_| model_file.sync_all())
    {
        drop(model_file);
        let _ = fs::remove_file(&output);
        return Err(invalid(format!(
            "cannot write {}: {error}",
            output.display()
        )));
    }
    drop(model_file);

    if let (Some(sql), Some(path)) = (migration_sql, migration_path) {
        let mut migration_file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_file(&output);
                return Err(invalid(format!(
                    "cannot create {} without overwriting: {error}",
                    path.display()
                )));
            }
        };
        if let Err(error) = migration_file
            .write_all(sql.as_bytes())
            .and_then(|_| migration_file.sync_all())
        {
            drop(migration_file);
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(&output);
            return Err(invalid(format!("cannot write {}: {error}", path.display())));
        }
        println!("Created SQL migration scaffold at {}", path.display());
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
    fn migration_scaffold_generates_tenant_scoped_sql_without_grants() {
        let root = test_package("migration_sql");
        let args = argv(&[
            "model",
            "BillboardFace",
            "--field",
            "name:String",
            "--field",
            "active:Bool",
            "--field",
            "daily_rate:Float",
            "--field",
            "impressions:Int",
            "--migration",
        ]);

        let model = run(&args, &root).expect("model and migration should generate");
        assert!(model.ends_with("src/resources/billboard_face.nula"));
        let sql = fs::read_to_string(root.join("migrations/create_billboard_face.sql"))
            .expect("reviewable migration should exist");
        assert!(sql.contains("CREATE TABLE \"public\".\"billboard_face\" ("));
        assert!(sql.contains("\"tenant_id\" UUID NOT NULL"));
        assert!(sql.contains("\"id\" UUID NOT NULL"));
        assert!(sql.contains("\"active\" BOOLEAN NOT NULL"));
        assert!(sql.contains("\"daily_rate\" DOUBLE PRECISION NOT NULL"));
        assert!(sql.contains("\"impressions\" BIGINT NOT NULL"));
        assert!(sql.contains("\"name\" TEXT NOT NULL"));
        assert!(sql.contains("PRIMARY KEY (\"tenant_id\", \"id\")"));
        assert!(sql.contains("ENABLE ROW LEVEL SECURITY"));
        assert!(sql.contains("FORCE ROW LEVEL SECURITY"));
        assert!(sql.contains("WITH CHECK"));
        assert!(sql.contains("current_setting('app.tenant_id', true)"));
        assert!(!sql.contains("GRANT "));
        assert!(!sql.contains("BYPASSRLS"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn migration_collision_preserves_existing_sql_and_does_not_write_model() {
        let root = test_package("migration_collision");
        let migration_dir = root.join("migrations");
        fs::create_dir_all(&migration_dir).unwrap();
        let migration = migration_dir.join("create_billboard.sql");
        fs::write(&migration, "-- application-owned migration\n").unwrap();
        let args = argv(&[
            "model",
            "Billboard",
            "--field",
            "name:String",
            "--migration",
        ]);
        assert!(run(&args, &root).is_err());
        assert_eq!(
            fs::read_to_string(migration).unwrap(),
            "-- application-owned migration\n"
        );
        assert!(!root.join("src/resources/billboard.nula").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn migration_directory_symlink_is_rejected_before_model_write() {
        use std::os::unix::fs::symlink;
        let root = test_package("migration_symlink");
        let outside = test_package("migration_symlink_outside");
        symlink(&outside, root.join("migrations")).unwrap();
        let args = argv(&[
            "model",
            "Billboard",
            "--field",
            "name:String",
            "--migration",
        ]);
        assert!(run(&args, &root).is_err());
        assert!(!root.join("src/resources/billboard.nula").exists());
        assert!(!outside.join("create_billboard.sql").exists());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn migration_rejects_postgres_identifier_truncation_before_writing() {
        let root = test_package("migration_long_name");
        let name = format!("A{}", "b".repeat(63));
        let args = vec![
            "model".to_string(),
            name,
            "--field".to_string(),
            "name:String".to_string(),
            "--migration".to_string(),
        ];
        assert!(run(&args, &root).is_err());
        assert!(!root.join("src").exists());
        assert!(!root.join("migrations").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn migration_quotes_reserved_postgres_identifiers() {
        let resource = Resource::parse(&argv(&[
            "model",
            "Order",
            "--field",
            "user:String",
            "--migration",
        ]))
        .unwrap();
        let sql = resource.render_migration().unwrap();
        assert!(sql.contains(r#"CREATE TABLE "public"."order" ("#));
        assert!(sql.contains(r#""user" TEXT NOT NULL"#));
    }

    #[test]
    fn migration_flag_must_be_unique() {
        assert!(Resource::parse(&argv(&[
            "model",
            "Billboard",
            "--field",
            "name:String",
            "--migration",
            "--migration",
        ]))
        .is_err());
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
