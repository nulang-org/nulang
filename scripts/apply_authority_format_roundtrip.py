#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one anchor, found {count}: {old[:100]!r}")
    p.write_text(text.replace(old, new, 1))


replace_once(
    "src/authority.rs",
    r'''        token.parse()
    }
}

/// Deterministically ordered authority set.''',
    r'''        token.parse()
    }

    /// Render this grant as valid source syntax. Unlike the canonical token
    /// representation used for artifact identity, source syntax quotes and
    /// escapes authority arguments so formatter output always reparses.
    pub fn to_source_syntax(&self) -> String {
        match self {
            AuthorityGrant::NetTcpOut { host, port } => {
                format!("Net::TcpOut({})", quote_source_string(&format!("{host}:{port}")))
            }
            AuthorityGrant::FsRead { path } => {
                format!("Fs::Read({})", quote_source_string(path))
            }
            AuthorityGrant::FsWrite { path } => {
                format!("Fs::Write({})", quote_source_string(path))
            }
            AuthorityGrant::EnvRead { name } => {
                format!("Env::Read({})", quote_source_string(name))
            }
            AuthorityGrant::SecretRead { name } => {
                format!("Secret::Read({})", quote_source_string(name))
            }
            AuthorityGrant::Other {
                namespace,
                operation,
                argument,
            } => match argument {
                Some(argument) => format!(
                    "{namespace}::{operation}({})",
                    quote_source_string(argument)
                ),
                None => format!("{namespace}::{operation}"),
            },
        }
    }
}

fn quote_source_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Deterministically ordered authority set.''',
)

replace_once(
    "src/fmt.rs",
    "use crate::ast::{BinOp, Decl, Expr, Literal, Pattern};\n",
    "use crate::ast::{BinOp, Decl, Expr, Literal, Pattern};\nuse crate::authority::AuthorityGrant;\n",
)

replace_once(
    "src/fmt.rs",
    r'''                    // Canonical token `Net::TcpOut(host:port)` → source form.
                    if let Some(dest) = cap
                        .strip_prefix("Net::TcpOut(")
                        .and_then(|r| r.strip_suffix(')'))
                    {
                        out.push_str(&format!("Net::TcpOut(\"{}\")", dest));
                    } else {
                        out.push_str(cap);
                    }''',
    r'''                    match cap.parse::<AuthorityGrant>() {
                        Ok(grant) => out.push_str(&grant.to_source_syntax()),
                        Err(_) => {
                            // Parsed source should never carry malformed authority
                            // metadata. Refuse to format rather than emit source that
                            // silently changes or drops authority.
                            *had_unhandled = true;
                        }
                    }''',
)

roundtrip_test = r'''    #[test]
    fn test_fmt_spawn_authority_roundtrips_all_grant_kinds() {
        let src = r#"
actor Worker { behavior ping() { 1 } }
fn main() {
    spawn Worker() with [
        Net::TcpOut("api.example.com:443"),
        Fs::Read("/tmp/in put"),
        Fs::Write("C:\\tmp\\\"out\""),
        Env::Read("HOME"),
        Secret::Read("stripe\nkey"),
        Vendor::Use("scope:alpha")
    ]
}"#;
        let out = format_source(src).expect("authority grants format");
        assert!(out.contains("Net::TcpOut(\"api.example.com:443\")"), "got: {out}");
        assert!(out.contains("Fs::Read(\"/tmp/in put\")"), "got: {out}");
        assert!(out.contains("Env::Read(\"HOME\")"), "got: {out}");
        assert!(out.contains("Vendor::Use(\"scope:alpha\")"), "got: {out}");
        let tokens = crate::lexer::Lexer::new(&out)
            .lex()
            .expect("formatted source lexes");
        crate::parser::Parser::new(tokens)
            .parse_module()
            .expect("formatted authority source reparses");
        assert_eq!(format_source(&out).expect("second format succeeds"), out);
    }

'''
replace_once(
    "src/fmt.rs",
    r'''    #[test]
    fn test_fmt_spawn_handle_receive() {''',
    roundtrip_test + r'''    #[test]
    fn test_fmt_spawn_handle_receive() {''',
)
