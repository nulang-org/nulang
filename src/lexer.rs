//! Compatibility facade for the extracted Nulang lexer crate.
//!
//! The implementation now compiles in `crates/nulang-lexer`; this module
//! preserves the historical `crate::lexer::{Lexer, Token, TokenKind}` API and
//! maps the standalone lexer's lightweight source spans/errors back into the
//! root compiler's existing `Span` and `NuError` types.

use crate::types::{NuError, NuResult, Span};

pub use nulang_lexer::TokenKind;

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

pub struct Lexer<'a> {
    inner: nulang_lexer::Lexer<'a>,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        // Preserve the root compiler's existing SourceMap behavior. The
        // standalone lexer also installs its own dependency-light SourceMap
        // for callers that use `nulang-lexer` directly.
        crate::types::set_source_map(source);
        Self {
            inner: nulang_lexer::Lexer::new(source),
        }
    }

    pub fn lex(&mut self) -> NuResult<Vec<Token>> {
        self.inner
            .lex()
            .map(|tokens| tokens.into_iter().map(Token::from_standalone).collect())
            .map_err(map_lex_error)
    }
}

impl Token {
    #[inline]
    fn from_standalone(token: nulang_lexer::Token) -> Self {
        Self {
            kind: token.kind,
            span: Span::new(token.span.start, token.span.end),
        }
    }
}

#[inline]
fn map_lex_error(err: nulang_lexer::LexError) -> NuError {
    match err {
        nulang_lexer::LexError::LexError { msg, span } => NuError::LexError {
            msg,
            span: Span::new(span.start, span.end),
        },
        nulang_lexer::LexError::__NonExhaustive => {
            unreachable!("nulang-lexer emitted its non-exhaustive sentinel")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_root_error_shape() {
        let mut lexer = Lexer::new("\"unterminated");
        let err = lexer.lex().unwrap_err();
        assert!(matches!(err, NuError::LexError { .. }));
    }

    #[test]
    fn preserves_token_spans() {
        let mut lexer = Lexer::new("let x = 1");
        let tokens = lexer.lex().unwrap();
        assert!(matches!(tokens[0].kind, TokenKind::Let));
        assert_eq!(tokens[0].span, Span::new(0, 3));
    }
}
