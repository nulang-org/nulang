//! Opt-in, experimental offside-rule lowering for Nulang.
//!
//! This module deliberately does not modify the default lexer or parser.
//! Supported layout headers: `fn`, `actor`, `entity`, `organization`,
//! and `behavior`, with an ending colon and an indented body.
//! The lowering inserts ordinary brace tokens with original-source spans;
//! the existing parser and downstream typechecker remain authoritative.

use crate::ast::AstModule;
use crate::lexer::{Lexer, Token, TokenKind};
use crate::parser::Parser;
use crate::types::{NuError, NuResult, Span};

/// Parse an opt-in layout source using the canonical Nulang AST.
pub fn parse_layout_module(source: &str) -> NuResult<AstModule> {
    Parser::new(lex_layout(source)?).parse_module()
}

/// Lower experimental layout headers to brace tokens.
///
/// Normal brace-delimited programs remain unchanged. This is intentionally
/// independent of the CLI, formatter, and LSP until the RFC is ratified.
pub fn lex_layout(source: &str) -> NuResult<Vec<Token>> {
    let raw = Lexer::new(source).lex()?;
    let mut out = Vec::with_capacity(raw.len() + 16);
    let mut line = Vec::new();
    let mut body_indents = Vec::<usize>::new();
    let mut awaiting_body: Option<(usize, Span)> = None;
    let mut explicit_depth = 0usize;
    let mut layout_seen = false;

    for token in raw {
        match token.kind {
            TokenKind::Newline => {
                lower_line(
                    source,
                    &mut line,
                    &mut out,
                    &mut body_indents,
                    &mut awaiting_body,
                    &mut explicit_depth,
                    &mut layout_seen,
                )?;
                out.push(token);
            }
            TokenKind::Eof => {
                lower_line(
                    source,
                    &mut line,
                    &mut out,
                    &mut body_indents,
                    &mut awaiting_body,
                    &mut explicit_depth,
                    &mut layout_seen,
                )?;
                if let Some((_, span)) = awaiting_body {
                    return Err(NuError::parse_error(
                        "Expected an indented body after ':'".into(),
                        span,
                    ));
                }
                let eof_span = token.span;
                for _ in body_indents {
                    out.push(Token {
                        kind: TokenKind::RBrace,
                        span: eof_span,
                    });
                }
                out.push(token);
            }
            _ => line.push(token),
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn lower_line(
    source: &str,
    line: &mut Vec<Token>,
    out: &mut Vec<Token>,
    body_indents: &mut Vec<usize>,
    awaiting_body: &mut Option<(usize, Span)>,
    explicit_depth: &mut usize,
    layout_seen: &mut bool,
) -> NuResult<()> {
    if line.is_empty() {
        return Ok(());
    }

    // Only recognize a declaration header at top-level delimiter depth.
    // Parenthesized multiline expressions and explicit blocks retain their
    // existing newline and indentation behavior.
    let is_header = *explicit_depth == 0
        && matches!(line.last().map(|t| &t.kind), Some(TokenKind::Colon))
        && is_supported_header(line);

    let needs_indent = *explicit_depth == 0
        && (*layout_seen || is_header || awaiting_body.is_some() || !body_indents.is_empty());

    let indent = if needs_indent {
        let column = indentation(source, line[0].span)?;
        if let Some((header_indent, _)) = awaiting_body.take() {
            if column <= header_indent {
                return Err(NuError::parse_error(
                    "Expected an indented body after ':'".into(),
                    line[0].span,
                ));
            }
            body_indents.push(column);
        } else {
            while body_indents.last().is_some_and(|&level| column < level) {
                body_indents.pop();
                out.push(Token {
                    kind: TokenKind::RBrace,
                    span: Span::new(line[0].span.start, line[0].span.start),
                });
            }
            let expected = body_indents.last().copied().unwrap_or(0);
            if column != expected {
                return Err(NuError::parse_error(
                    format!("Inconsistent indentation: expected {expected} spaces, found {column}"),
                    line[0].span,
                ));
            }
        }
        column
    } else {
        0
    };

    // Header colon is purely syntactic sugar for the normal opening brace.
    // Keep the byte position for reliable diagnostic highlighting.
    if is_header {
        let colon = line.pop().expect("checked trailing colon");
        out.extend(line.drain(..));
        out.push(Token {
            kind: TokenKind::LBrace,
            span: colon.span,
        });
        *awaiting_body = Some((indent, colon.span));
        *layout_seen = true;
    } else {
        for token in line.drain(..) {
            match token.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                    *explicit_depth += 1;
                }
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    *explicit_depth = explicit_depth.saturating_sub(1);
                }
                _ => {}
            }
            out.push(token);
        }
    }
    Ok(())
}

fn is_supported_header(line: &[Token]) -> bool {
    let head = if matches!(line.first().map(|t| &t.kind), Some(TokenKind::Pub)) {
        line.get(1).map(|t| &t.kind)
    } else {
        line.first().map(|t| &t.kind)
    };
    matches!(
        head,
        Some(
            TokenKind::Fn
                | TokenKind::Actor
                | TokenKind::Entity
                | TokenKind::Organization
                | TokenKind::Behavior
        )
    )
}

/// Count leading ASCII spaces from the original source, rejecting tab-based
/// layout rather than silently assigning an arbitrary tab width.
fn indentation(source: &str, span: Span) -> NuResult<usize> {
    let end = span.start as usize;
    let bytes = source.as_bytes();
    let start = bytes[..end]
        .iter()
        .rposition(|b| *b == b'\n' || *b == b'\r')
        .map_or(0, |pos| pos + 1);
    let prefix = &bytes[start..end];
    if prefix.iter().any(|b| *b != b' ') {
        return Err(NuError::parse_error(
            "Layout indentation must use spaces, not tabs or mixed whitespace".into(),
            span,
        ));
    }
    Ok(prefix.len())
}
