//! Standalone Nulang lexer.
//!
//! The lexer implementation is intentionally kept independent of the root
//! `nulang` crate. The small `types` compatibility module lets the extracted
//! implementation retain its existing code shape while depending only on
//! `nulang-source`.

pub mod types {
    pub use nulang_source::Span;

    pub type NuResult<T> = Result<T, NuError>;

    /// Error emitted by the lexer. The name remains `NuError` inside the
    /// compatibility module so the extracted implementation can stay
    /// byte-for-byte identical to the former root module.
    #[derive(Debug, Clone)]
    pub enum NuError {
        LexError { msg: String, span: Span },
        #[doc(hidden)]
        __NonExhaustive,
    }

    impl std::fmt::Display for NuError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::LexError { msg, span } => {
                    write!(f, "Lex error at {}:{}: {}", span.line(), span.column(), msg)
                }
                Self::__NonExhaustive => f.write_str("unknown lexer error"),
            }
        }
    }

    impl std::error::Error for NuError {}

    #[inline]
    pub fn set_source_map(source: &str) {
        nulang_source::set_source_map(source);
    }
}

#[path = "lexer_impl.rs"]
mod lexer_impl;

pub use lexer_impl::{Lexer, Token, TokenKind};
pub use types::{NuError as LexError, NuResult as LexResult};
pub use nulang_source::Span;
