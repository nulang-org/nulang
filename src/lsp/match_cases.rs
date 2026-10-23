//! Compiler-AST-backed LSP quick fix for missing variant match arms.
//!
//! The initial scope deliberately requires a typed function parameter as the
//! scrutinee. Arbitrary expression inference and nested shadowing need deeper
//! typechecker support and should not be guessed by editor tooling.

use super::*;
use crate::ast::{AstModule, Decl, Expr, Param, Pattern};
use crate::lexer::{Token, TokenKind};
use crate::types::Type;

fn irrefutable(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Wild | Pattern::Var(_) => true,
        Pattern::Alias(_, nested) => irrefutable(nested),
        _ => false,
    }
}

fn covers_variant(pattern: &Pattern, name: &str, payload: bool) -> bool {
    match pattern {
        Pattern::Variant(n, sub) if n == name => {
            if payload {
                sub.as_deref().is_some_and(irrefutable)
            } else {
                sub.is_none()
            }
        }
        Pattern::Alias(_, nested) => covers_variant(nested, name, payload),
        _ => false,
    }
}

/// Match the brace belonging to the match expression, not an arm body.
fn closing_brace(tokens: &[Token], match_start: u32) -> Option<usize> {
    let start = tokens
        .iter()
        .position(|token| token.span.start == match_start && token.kind == TokenKind::Match)?;
    let open = tokens[start + 1..]
        .iter()
        .position(|token| token.kind == TokenKind::LBrace)?
        + start
        + 1;
    let mut depth = 0usize;
    for token in &tokens[open..] {
        match token.kind {
            TokenKind::LBrace => depth += 1,
            TokenKind::RBrace => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(token.span.start as usize);
                }
            }
            _ => {}
        }
    }
    None
}

/// Restrict matching to parameter references that have not been shadowed by a
/// let in the path to the expression. Nested arm bodies are outside this
/// initial conservative scope.
fn find_selected_match<'a>(
    expr: &'a Expr,
    param_name: &str,
    start: usize,
    end: usize,
    tokens: &[Token],
) -> Option<(&'a [(Pattern, Option<Expr>, Expr)], usize, usize)> {
    match expr {
        Expr::Match {
            scrutinee,
            arms,
            span,
        } => {
            let Expr::Var(name, _) = scrutinee.as_ref() else {
                return None;
            };
            if name != param_name {
                return None;
            }
            let close = closing_brace(tokens, span.start)?;
            if start >= span.start as usize && end <= close {
                Some((arms, span.start as usize, close))
            } else {
                None
            }
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => exprs
            .iter()
            .find_map(|part| find_selected_match(part, param_name, start, end, tokens)),
        Expr::Let {
            name, value, body, ..
        } => find_selected_match(value, param_name, start, end, tokens).or_else(|| {
            (name != param_name)
                .then(|| find_selected_match(body, param_name, start, end, tokens))
                .flatten()
        }),
        Expr::LetRec {
            name,
            params,
            value,
            body,
            ..
        } => {
            // Recursive binding and its parameters are scoped over the value.
            // Only the recursive name remains bound in the body.
            let value_match = (name != param_name
                && params.iter().all(|param| param.name != param_name))
            .then(|| find_selected_match(value, param_name, start, end, tokens))
            .flatten();
            value_match.or_else(|| {
                (name != param_name)
                    .then(|| find_selected_match(body, param_name, start, end, tokens))
                    .flatten()
            })
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => find_selected_match(cond, param_name, start, end, tokens)
            .or_else(|| find_selected_match(then_branch, param_name, start, end, tokens))
            .or_else(|| {
                else_branch
                    .as_ref()
                    .and_then(|branch| find_selected_match(branch, param_name, start, end, tokens))
            }),
        _ => None,
    }
}

fn lsp_position(source: &str, offset: usize) -> Option<Position> {
    let prefix = source.get(..offset)?;
    let line = prefix.bytes().filter(|&b| b == b'\n').count() as u32;
    let col = prefix.rsplit('\n').next()?.encode_utf16().count() as u32;
    Some(Position::new(line, col))
}

pub(super) fn actions(
    source: &str,
    selection: &Range,
    uri: &Url,
    ast: &AstModule,
) -> Vec<CodeActionOrCommand> {
    let (Some(start), Some(end)) = (
        NulangLanguageServer::position_to_byte_offset(source, &selection.start),
        NulangLanguageServer::position_to_byte_offset(source, &selection.end),
    ) else {
        return Vec::new();
    };
    if start > end {
        return Vec::new();
    }
    let Ok(tokens) = Lexer::new(source).lex() else {
        return Vec::new();
    };
    for decl in &ast.decls {
        let Decl::Function {
            params,
            using_params,
            body,
            ..
        } = decl
        else {
            continue;
        };
        for Param { name, ty, .. } in params {
            // The typechecker binds using parameters after ordinary
            // parameters. A matching name therefore changes the effective
            // type of the scrutinee. Until we resolve that binding, withhold
            // the edit instead of generating arms for the wrong variant.
            if using_params.iter().any(|param| param.name == *name) {
                continue;
            }
            let Some(Type::Variant(variants)) = ty else {
                continue;
            };
            let Some((arms, match_start, close)) =
                find_selected_match(body, name, start, end, &tokens)
            else {
                continue;
            };

            // Unguarded catch-alls cover every variant. Guarded arms and
            // refutable payload patterns do not prove full coverage.
            if arms
                .iter()
                .any(|(pattern, guard, _)| guard.is_none() && irrefutable(pattern))
            {
                return Vec::new();
            }
            let missing: Vec<_> = variants
                .iter()
                .filter(|(variant, ty)| {
                    !arms.iter().any(|(pattern, guard, _)| {
                        guard.is_none() && covers_variant(pattern, variant, ty.is_some())
                    })
                })
                .collect();
            if missing.is_empty() {
                return Vec::new();
            }

            let match_line_start = source[..match_start].rfind('\n').map_or(0, |idx| idx + 1);
            let match_indent: String = source[match_line_start..match_start]
                .chars()
                .take_while(|ch| *ch == ' ' || *ch == '\t')
                .collect();
            let closing_line_start = source[..close].rfind('\n').map_or(0, |idx| idx + 1);
            let closing_prefix = &source[closing_line_start..close];
            let multiline_close = closing_line_start > match_line_start
                && closing_prefix.chars().all(|ch| ch == ' ' || ch == '\t');
            let (insert_at, arm_indent, suffix) = if multiline_close {
                (
                    closing_line_start,
                    format!("{}    ", closing_prefix),
                    String::new(),
                )
            } else {
                (
                    close,
                    format!("{}    ", match_indent),
                    format!("\n{match_indent}"),
                )
            };
            let Some(position) = lsp_position(source, insert_at) else {
                return Vec::new();
            };
            let lines: Option<Vec<String>> = missing
                .iter()
                .map(|(name, payload)| {
                    let pat = if payload.is_some() {
                        format!("{name}(_)")
                    } else {
                        name.to_string()
                    };
                    // Avoid every user identifier in the source. Fail closed
                    // if no fresh placeholder can be found.
                    let todo = (0..1024)
                        .map(|i| {
                            if i == 0 {
                                format!("todo_{name}")
                            } else {
                                format!("todo_{name}_{i}")
                            }
                        })
                        .find(|candidate| {
                            !tokens.iter().any(|token| {
                            matches!(&token.kind, TokenKind::Ident(s) if s == candidate)
                        })
                        })?;
                    Some(format!("{arm_indent}| {pat} => {todo}"))
                })
                .collect();
            let Some(lines) = lines else {
                return Vec::new();
            };
            let lines = lines.join("\n");
            let insertion = if multiline_close {
                format!("{lines}\n")
            } else {
                format!("\n{lines}{suffix}")
            };
            let mut changes = HashMap::new();
            changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: Range::new(position, position),
                    new_text: insertion,
                }],
            );
            return vec![CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Generate {} missing variant case(s) (TODO)", missing.len()),
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..WorkspaceEdit::default()
                }),
                is_preferred: Some(false),
                ..CodeAction::default()
            })];
        }
    }
    Vec::new()
}
