//! Lexical analyzer for Nulang.
//!
//! Hand-written state machine. Single-pass over input source.

use crate::types::{NuError, NuResult, Span};

// ---------------------------------------------------------------------------
// Token Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Literals
    IntLit(i64),
    FloatLit(f64),
    StringLit(String),
    FStringLit(String),
    BoolLit(bool),
    NilLit,
    UnitLit,

    // Keywords
    Fn,
    Let,
    Var,
    Rec,
    In,
    If,
    Then,
    Else,
    Match,
    With,
    Case,
    Actor,
    Entity,
    Organization,
    Virtual,
    Behavior,
    State,
    StateMachine,
    SelfKw,
    Spawn,
    Send,
    Remote,
    Ask,
    Persistent,
    Local,
    Durable,
    EventSourced,
    Crdt,
    Until,
    Emit,
    Workflow,
    Step,
    Parallel,
    Compensate,
    Agent,
    Database,
    Receive,
    Effect,
    Perform,
    Handle,
    Par,
    Resume,
    Extern,
    Module,
    Import,
    Pub,
    App,
    Signal,
    Migrate,
    Monitor,
    Link,
    Exit,
    For,
    While,
    Break,
    Return,
    Type,
    Alias,
    Iso,
    Trn,
    Ref,
    Val,
    Box,
    Tag,
    LinearIso,
    Linear,
    True,
    False,
    Unit,
    Tool,
    Handler,
    Consume,
    Recover,
    Initial,
    Catch,
    Fail,
    Throws,
    Class,
    Impl,
    As,
    Defer,
    Opaque,
    Given,
    Using,
    Errdefer,
    Await,
    Hide,
    Seal,
    Except,
    Requires,
    Ensures,

    // Identifiers
    Ident(String),
    UpperIdent(String), // Type/actor names (convention)

    // Operators
    Plus,
    Minus,
    Star,
    Star2,
    Slash,
    Percent, // + - * ** / %
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge, // == != < <= > >=
    And,
    Or,
    Not, // && || !
    Ampersand,
    Pipe,
    PipeOp,
    Pipe3,
    Caret,
    Tilde, // & | |> ||| ^ ~
    Shl,
    Shr, // << >>
    Assign,
    PlusAssign,
    MinusAssign, // = += -=
    Arrow,
    FatArrow,
    ThinArrow,         // -> => <-
    ThinArrowQuestion, // <-? (async ask)
    Dot,
    DotDot,
    Colon,
    DoubleColon, // . .. : ::
    At,          // @
    Bang,
    Question, // ! ?

    // Delimiters
    LParen,
    RParen, // ( )
    LBrace,
    RBrace, // { }
    LBracket,
    RBracket, // [ ]
    Comma,
    Semicolon, // , ;

    // Special
    Newline,
    Comment(String),
    DocComment(String),
    Eof,
}

impl std::fmt::Display for TokenKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Literals
            TokenKind::IntLit(n) => write!(f, "integer {}", n),
            TokenKind::FloatLit(n) => write!(f, "float {}", n),
            TokenKind::StringLit(s) => write!(f, "\"{}\"", s),
            TokenKind::FStringLit(s) => write!(f, "f\"{}\"", s),
            TokenKind::BoolLit(b) => write!(f, "{}", b),
            TokenKind::NilLit => write!(f, "nil"),
            TokenKind::UnitLit => write!(f, "unit"),
            // Keywords
            TokenKind::Fn => write!(f, "fn"),
            TokenKind::Let => write!(f, "let"),
            TokenKind::Var => write!(f, "var"),
            TokenKind::Rec => write!(f, "rec"),
            TokenKind::In => write!(f, "in"),
            TokenKind::If => write!(f, "if"),
            TokenKind::Then => write!(f, "then"),
            TokenKind::Else => write!(f, "else"),
            TokenKind::Match => write!(f, "match"),
            TokenKind::With => write!(f, "with"),
            TokenKind::Case => write!(f, "case"),
            TokenKind::Actor => write!(f, "actor"),
            TokenKind::Entity => write!(f, "entity"),
            TokenKind::Organization => write!(f, "organization"),
            TokenKind::Virtual => write!(f, "virtual"),
            TokenKind::Behavior => write!(f, "behavior"),
            TokenKind::State => write!(f, "state"),
            TokenKind::StateMachine => write!(f, "statemachine"),
            TokenKind::SelfKw => write!(f, "self"),
            TokenKind::Spawn => write!(f, "spawn"),
            TokenKind::Send => write!(f, "send"),
            TokenKind::Remote => write!(f, "remote"),
            TokenKind::Ask => write!(f, "ask"),
            TokenKind::Persistent => write!(f, "persistent"),
            TokenKind::Local => write!(f, "local"),
            TokenKind::Durable => write!(f, "durable"),
            TokenKind::EventSourced => write!(f, "eventsourced"),
            TokenKind::Until => write!(f, "until"),
            TokenKind::Crdt => write!(f, "crdt"),
            TokenKind::Emit => write!(f, "emit"),
            TokenKind::Workflow => write!(f, "workflow"),
            TokenKind::Step => write!(f, "step"),
            TokenKind::Parallel => write!(f, "parallel"),
            TokenKind::Compensate => write!(f, "compensate"),
            TokenKind::Agent => write!(f, "agent"),
            TokenKind::Database => write!(f, "database"),
            TokenKind::Receive => write!(f, "receive"),
            TokenKind::Effect => write!(f, "effect"),
            TokenKind::Perform => write!(f, "perform"),
            TokenKind::Handle => write!(f, "handle"),
            TokenKind::Par => write!(f, "par"),
            TokenKind::Resume => write!(f, "resume"),
            TokenKind::Extern => write!(f, "extern"),
            TokenKind::Module => write!(f, "module"),
            TokenKind::Import => write!(f, "import"),
            TokenKind::App => write!(f, "app"),
            TokenKind::Signal => write!(f, "signal"),
            TokenKind::Pub => write!(f, "pub"),
            TokenKind::Migrate => write!(f, "migrate"),
            TokenKind::Monitor => write!(f, "monitor"),
            TokenKind::Link => write!(f, "link"),
            TokenKind::Exit => write!(f, "exit"),
            TokenKind::For => write!(f, "for"),
            TokenKind::While => write!(f, "while"),
            TokenKind::Break => write!(f, "break"),
            TokenKind::Return => write!(f, "return"),
            TokenKind::Type => write!(f, "type"),
            TokenKind::Alias => write!(f, "alias"),
            TokenKind::Iso => write!(f, "iso"),
            TokenKind::Trn => write!(f, "trn"),
            TokenKind::Ref => write!(f, "ref"),
            TokenKind::Val => write!(f, "val"),
            TokenKind::Box => write!(f, "box"),
            TokenKind::Tag => write!(f, "tag"),
            TokenKind::LinearIso => write!(f, "lineariso"),
            TokenKind::Linear => write!(f, "linear"),
            TokenKind::True => write!(f, "true"),
            TokenKind::False => write!(f, "false"),
            TokenKind::Unit => write!(f, "unit"),
            TokenKind::Tool => write!(f, "tool"),
            TokenKind::Given => write!(f, "given"),
            TokenKind::Using => write!(f, "using"),
            TokenKind::Opaque => write!(f, "opaque"),
            TokenKind::Handler => write!(f, "handler"),
            TokenKind::Defer => write!(f, "defer"),
            TokenKind::Errdefer => write!(f, "errdefer"),
            TokenKind::Await => write!(f, "await"),
            TokenKind::Consume => write!(f, "consume"),
            TokenKind::Recover => write!(f, "recover"),
            TokenKind::Catch => write!(f, "catch"),
            TokenKind::Fail => write!(f, "fail"),
            TokenKind::Initial => write!(f, "initial"),
            TokenKind::Class => write!(f, "class"),
            TokenKind::Impl => write!(f, "impl"),
            TokenKind::As => write!(f, "as"),
            TokenKind::Hide => write!(f, "hide"),
            TokenKind::Seal => write!(f, "seal"),
            TokenKind::Except => write!(f, "except"),
            TokenKind::Requires => write!(f, "requires"),
            TokenKind::Ensures => write!(f, "ensures"),
            TokenKind::Throws => write!(f, "throws"),
            // Identifiers
            TokenKind::Ident(s) => write!(f, "identifier `{}`", s),
            TokenKind::UpperIdent(s) => write!(f, "type name `{}`", s),
            // Operators
            TokenKind::Plus => write!(f, "+"),
            TokenKind::Minus => write!(f, "-"),
            TokenKind::Star => write!(f, "*"),
            TokenKind::Star2 => write!(f, "**"),
            TokenKind::Slash => write!(f, "/"),
            TokenKind::Percent => write!(f, "%"),
            TokenKind::Eq => write!(f, "=="),
            TokenKind::Ne => write!(f, "!="),
            TokenKind::Lt => write!(f, "<"),
            TokenKind::Le => write!(f, "<="),
            TokenKind::Gt => write!(f, ">"),
            TokenKind::Ge => write!(f, ">="),
            TokenKind::And => write!(f, "&&"),
            TokenKind::Or => write!(f, "||"),
            TokenKind::Not => write!(f, "!"),
            TokenKind::Ampersand => write!(f, "&"),
            TokenKind::Pipe => write!(f, "|"),
            TokenKind::PipeOp => write!(f, "|>"),
            TokenKind::Pipe3 => write!(f, "|||"),
            TokenKind::Caret => write!(f, "^"),
            TokenKind::Tilde => write!(f, "~"),
            TokenKind::Shl => write!(f, "<<"),
            TokenKind::Shr => write!(f, ">>"),
            TokenKind::Assign => write!(f, "="),
            TokenKind::PlusAssign => write!(f, "+="),
            TokenKind::MinusAssign => write!(f, "-="),
            TokenKind::Arrow => write!(f, "->"),
            TokenKind::FatArrow => write!(f, "=>"),
            TokenKind::ThinArrow => write!(f, "<-"),
            TokenKind::ThinArrowQuestion => write!(f, "<-?"),
            TokenKind::Dot => write!(f, "."),
            TokenKind::DotDot => write!(f, ".."),
            TokenKind::Colon => write!(f, ":"),
            TokenKind::DoubleColon => write!(f, "::"),
            TokenKind::At => write!(f, "@"),
            TokenKind::Bang => write!(f, "!"),
            TokenKind::Question => write!(f, "?"),
            // Delimiters
            TokenKind::LParen => write!(f, "("),
            TokenKind::RParen => write!(f, ")"),
            TokenKind::LBrace => write!(f, "{{"),
            TokenKind::RBrace => write!(f, "}}"),
            TokenKind::LBracket => write!(f, "["),
            TokenKind::RBracket => write!(f, "]"),
            TokenKind::Comma => write!(f, ","),
            TokenKind::Semicolon => write!(f, ";"),
            // Special
            TokenKind::Newline => write!(f, "newline"),
            TokenKind::Comment(_) => write!(f, "comment"),
            TokenKind::DocComment(_) => write!(f, "doc comment"),
            TokenKind::Eof => write!(f, "end of file"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

pub struct Lexer<'a> {
    source: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        // Install a SourceMap so Span::line()/column() can resolve byte
        // offsets to human-readable positions throughout the compiler
        // pipeline (error display, LSP, etc.).
        crate::types::set_source_map(source);
        Lexer {
            source,
            bytes: source.as_bytes(),
            pos: 0,
        }
    }

    /// Run the lexer, producing a vector of tokens. Newlines are preserved
    /// for semicolon insertion logic but comments are filtered out.
    pub fn lex(&mut self) -> NuResult<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token()?;
            match tok.kind {
                TokenKind::Eof => {
                    tokens.push(tok);
                    break;
                }
                // Filter out regular comments, keep doc comments as tokens
                TokenKind::Comment(_) => {
                    // skip regular comments
                }
                _ => tokens.push(tok),
            }
        }
        Ok(tokens)
    }

    // --- Core tokenizer ---

    fn next_token(&mut self) -> NuResult<Token> {
        // Shebang line: skip `#!/path/to/interpreter ...` at start of file
        if self.pos == 0 && self.bytes.len() >= 2 && self.bytes[0] == b'#' && self.bytes[1] == b'!'
        {
            while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                self.pos += 1;
            }
            if self.pos < self.bytes.len() && self.bytes[self.pos] == b'\n' {
                self.pos += 1;
            }
        }
        self.skip_whitespace();

        let start = self.pos;

        let ch = match self.peek() {
            Some(c) => c,
            None => {
                return Ok(Token {
                    kind: TokenKind::Eof,
                    span: Span::new(start as u32, start as u32),
                })
            }
        };

        let tok = match ch {
            b'\n' | b'\r' => {
                self.advance();
                if ch == b'\r' && self.peek() == Some(b'\n') {
                    self.advance();
                }
                Token {
                    kind: TokenKind::Newline,
                    span: Span::new(start as u32, self.pos as u32),
                }
            }
            b'/' => {
                // Could be comment or division operator
                let next = self.bytes.get(self.pos + 1);
                if next == Some(&b'/') || next == Some(&b'*') {
                    self.read_comment()
                } else {
                    self.read_operator()?
                }
            }
            b'a'..=b'e' | b'g'..=b'z' | b'_' => self.read_identifier(),
            b'f' => {
                if self.bytes.get(self.pos + 1) == Some(&b'"') {
                    let f_start = self.pos;
                    self.advance(); // consume 'f'
                    let mut tok = if self.bytes.get(self.pos + 1) == Some(&b'"')
                        && self.bytes.get(self.pos + 2) == Some(&b'"')
                    {
                        self.read_triple_string()?
                    } else {
                        self.read_string()?
                    };
                    if let TokenKind::StringLit(s) = tok.kind {
                        tok.kind = TokenKind::FStringLit(s);
                        tok.span = crate::types::Span::new(f_start as u32, self.pos as u32);
                    }
                    tok
                } else {
                    self.read_identifier()
                }
            }
            b'A'..=b'Z' => self.read_identifier(),
            b'0'..=b'9' => self.read_number()?,
            b'"' => {
                // Triple-quoted string?
                if self.bytes.get(self.pos + 1) == Some(&b'"')
                    && self.bytes.get(self.pos + 2) == Some(&b'"')
                {
                    self.read_triple_string()?
                } else {
                    self.read_string()?
                }
            }
            b'+' | b'-' | b'*' | b'%' | b'=' | b'!' | b'<' | b'>' | b'&' | b'|' | b'^' | b'~'
            | b'.' | b':' | b'#' | b'?' => self.read_operator()?,
            b'(' => {
                self.advance();
                Token {
                    kind: TokenKind::LParen,
                    span: Span::new(start as u32, self.pos as u32),
                }
            }
            b')' => {
                self.advance();
                Token {
                    kind: TokenKind::RParen,
                    span: Span::new(start as u32, self.pos as u32),
                }
            }
            b'{' => {
                self.advance();
                Token {
                    kind: TokenKind::LBrace,
                    span: Span::new(start as u32, self.pos as u32),
                }
            }
            b'}' => {
                self.advance();
                Token {
                    kind: TokenKind::RBrace,
                    span: Span::new(start as u32, self.pos as u32),
                }
            }
            b'[' => {
                self.advance();
                Token {
                    kind: TokenKind::LBracket,
                    span: Span::new(start as u32, self.pos as u32),
                }
            }
            b']' => {
                self.advance();
                Token {
                    kind: TokenKind::RBracket,
                    span: Span::new(start as u32, self.pos as u32),
                }
            }
            b',' => {
                self.advance();
                Token {
                    kind: TokenKind::Comma,
                    span: Span::new(start as u32, self.pos as u32),
                }
            }
            b'@' => {
                self.advance();
                Token {
                    kind: TokenKind::At,
                    span: Span::new(start as u32, self.pos as u32),
                }
            }
            b';' => {
                self.advance();
                Token {
                    kind: TokenKind::Semicolon,
                    span: Span::new(start as u32, self.pos as u32),
                }
            }
            _ => {
                return Err(NuError::LexError {
                    msg: format!("Unexpected character: '{}' (byte {})", ch as char, ch),
                    span: Span::new(start as u32, start as u32 + 1),
                })
            }
        };
        Ok(tok)
    }

    // --- Helper methods ---

    fn skip_whitespace(&mut self) {
        loop {
            match self.peek() {
                Some(b' ') | Some(b'\t') => {
                    self.advance();
                }
                _ => break,
            }
        }
    }

    fn read_identifier(&mut self) -> Token {
        let start = self.pos;

        // First char: lowercase, uppercase, or underscore
        self.advance(); // consume first char

        // Rest: alphanumeric or underscore
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == b'_' {
                self.advance();
            } else {
                break;
            }
        }

        let text = &self.source[start..self.pos];

        // Check if it's a keyword
        if let Some(kw) = keyword(text) {
            Token {
                kind: kw,
                span: Span::new(start as u32, self.pos as u32),
            }
        } else if text.starts_with(|c: char| c.is_ascii_uppercase()) {
            Token {
                kind: TokenKind::UpperIdent(text.to_string()),
                span: Span::new(start as u32, self.pos as u32),
            }
        } else {
            Token {
                kind: TokenKind::Ident(text.to_string()),
                span: Span::new(start as u32, self.pos as u32),
            }
        }
    }

    fn read_number(&mut self) -> NuResult<Token> {
        let start = self.pos;

        // Check for hex prefix
        if self.peek() == Some(b'0') {
            let next = self.bytes.get(self.pos + 1);
            if next == Some(&b'x') || next == Some(&b'X') {
                self.advance(); // '0'
                self.advance(); // 'x' or 'X'
                let hex_start = self.pos;
                while let Some(ch) = self.peek() {
                    if ch.is_ascii_hexdigit() {
                        self.advance();
                    } else {
                        break;
                    }
                }
                if self.pos == hex_start {
                    return Err(NuError::LexError {
                        msg: "Expected hex digits after 0x".to_string(),
                        span: Span::new(start as u32, self.pos as u32),
                    });
                }
                let hex_str = &self.source[hex_start..self.pos];
                let val = i64::from_str_radix(hex_str, 16).map_err(|_| NuError::LexError {
                    msg: format!("Invalid hex number: {}", hex_str),
                    span: Span::new(start as u32, self.pos as u32),
                })?;
                return Ok(Token {
                    kind: TokenKind::IntLit(val),
                    span: Span::new(start as u32, self.pos as u32),
                });
            }
        }

        // Read integer part
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }

        // Check for float: either .fraction or exponent
        let mut is_float = false;
        let _int_end = self.pos;

        // Fractional part
        if self.peek() == Some(b'.') {
            // Make sure it's not followed by another dot (range operator)
            let after_dot = self.bytes.get(self.pos + 1);
            if after_dot.map_or(false, |c| c.is_ascii_digit()) {
                is_float = true;
                self.advance(); // '.'
                while let Some(ch) = self.peek() {
                    if ch.is_ascii_digit() {
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
        }

        // Exponent part
        if self.peek() == Some(b'e') || self.peek() == Some(b'E') {
            is_float = true;
            self.advance(); // e or E
            if self.peek() == Some(b'+') || self.peek() == Some(b'-') {
                self.advance();
            }
            let exp_start = self.pos;
            while let Some(ch) = self.peek() {
                if ch.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
            if self.pos == exp_start {
                return Err(NuError::LexError {
                    msg: "Expected digits in exponent".to_string(),
                    span: Span::new(start as u32, self.pos as u32),
                });
            }
        }

        let num_str = &self.source[start..self.pos];

        if is_float {
            let val = num_str.parse::<f64>().map_err(|_| NuError::LexError {
                msg: format!("Invalid float literal: {}", num_str),
                span: Span::new(start as u32, self.pos as u32),
            })?;
            Ok(Token {
                kind: TokenKind::FloatLit(val),
                span: Span::new(start as u32, self.pos as u32),
            })
        } else {
            let val = num_str.parse::<i64>().map_err(|_| NuError::LexError {
                msg: format!("Invalid integer literal: {}", num_str),
                span: Span::new(start as u32, self.pos as u32),
            })?;
            Ok(Token {
                kind: TokenKind::IntLit(val),
                span: Span::new(start as u32, self.pos as u32),
            })
        }
    }

    fn read_string(&mut self) -> NuResult<Token> {
        let start = self.pos;

        self.advance(); // opening "

        let mut result = String::new();
        loop {
            match self.peek() {
                Some(b'"') => {
                    self.advance(); // closing "
                    break;
                }
                Some(b'\\') => {
                    self.advance(); // backslash
                    match self.advance() {
                        Some(b'n') => result.push('\n'),
                        Some(b't') => result.push('\t'),
                        Some(b'\\') => result.push('\\'),
                        Some(b'"') => result.push('"'),
                        Some(b'r') => result.push('\r'),
                        Some(b'0') => result.push('\0'),
                        Some(b'u') => {
                            let c = self.read_unicode_escape(start)?;
                            result.push(c);
                        }
                        Some(other) => {
                            return Err(NuError::LexError {
                                msg: format!("Unknown escape sequence: \\\\{}", other as char),
                                span: Span::new((self.pos - 1) as u32, self.pos as u32),
                            })
                        }
                        None => {
                            return Err(NuError::LexError {
                                msg: "Unterminated string escape".to_string(),
                                span: Span::new(start as u32, self.pos as u32),
                            })
                        }
                    }
                }
                Some(ch) => {
                    if ch < 0x80 {
                        result.push(ch as char);
                        self.advance();
                    } else {
                        // Multi-byte UTF-8 sequence: decode the whole char and
                        // advance over all of its bytes. `self.pos` is on a
                        // char boundary here because the source is valid UTF-8
                        // and every prior step consumed whole chars.
                        match self.source[self.pos..].chars().next() {
                            Some(c) => {
                                result.push(c);
                                for _ in 0..c.len_utf8() {
                                    self.advance();
                                }
                            }
                            None => {
                                return Err(NuError::LexError {
                                    msg: "Unterminated string literal".to_string(),
                                    span: Span::new(start as u32, self.pos as u32),
                                })
                            }
                        }
                    }
                }
                None => {
                    return Err(NuError::LexError {
                        msg: "Unterminated string literal".to_string(),
                        span: Span::new(start as u32, self.pos as u32),
                    })
                }
            }
        }

        Ok(Token {
            kind: TokenKind::StringLit(result),
            span: Span::new(start as u32, self.pos as u32),
        })
    }

    /// Scan a triple-quoted multi-line string literal `"""..."""`.
    /// Escape sequences (\\n, \\t, \\r, \\0, \\", \\\\, \\u{...}) are processed
    /// identically to single-quoted strings.  Interpolation is not supported
    /// inside triple-quoted strings.
    fn read_triple_string(&mut self) -> NuResult<Token> {
        let start = self.pos;

        // Consume the three opening quotes.
        self.advance(); // "
        self.advance(); // "
        self.advance(); // "

        let mut result = String::new();
        loop {
            // Closing delimiter: three consecutive double-quotes.
            if self.peek() == Some(b'"')
                && self.bytes.get(self.pos + 1) == Some(&b'"')
                && self.bytes.get(self.pos + 2) == Some(&b'"')
            {
                self.advance(); // "
                self.advance(); // "
                self.advance(); // "
                break;
            }

            match self.peek() {
                Some(b'\\') => {
                    self.advance(); // backslash
                    match self.advance() {
                        Some(b'n') => result.push('\n'),
                        Some(b't') => result.push('\t'),
                        Some(b'\\') => result.push('\\'),
                        Some(b'"') => result.push('"'),
                        Some(b'r') => result.push('\r'),
                        Some(b'0') => result.push('\0'),
                        Some(b'u') => {
                            let c = self.read_unicode_escape(start)?;
                            result.push(c);
                        }
                        Some(other) => {
                            return Err(NuError::LexError {
                                msg: format!("Unknown escape sequence: \\\\{}", other as char),
                                span: Span::new((self.pos - 1) as u32, self.pos as u32),
                            })
                        }
                        None => {
                            return Err(NuError::LexError {
                                msg: "Unterminated string escape".to_string(),
                                span: Span::new(start as u32, self.pos as u32),
                            })
                        }
                    }
                }
                Some(_) => {
                    // Any character — including quotes (single or double) and
                    // newlines — is literal inside a triple-quoted string.
                    match self.source[self.pos..].chars().next() {
                        Some(c) => {
                            result.push(c);
                            for _ in 0..c.len_utf8() {
                                self.advance();
                            }
                        }
                        None => {
                            return Err(NuError::LexError {
                                msg: "Unterminated triple-quoted string literal".to_string(),
                                span: Span::new(start as u32, self.pos as u32),
                            })
                        }
                    }
                }
                None => {
                    return Err(NuError::LexError {
                        msg: "Unterminated triple-quoted string literal".to_string(),
                        span: Span::new(start as u32, self.pos as u32),
                    })
                }
            }
        }

        Ok(Token {
            kind: TokenKind::StringLit(result),
            span: Span::new(start as u32, self.pos as u32),
        })
    }

    /// Called after consuming '\\' and then 'u' (the first char of a \\u{...}
    /// escape).  The next byte MUST be '{', followed by 1–6 hex digits and a
    /// closing '}'.  Validates that the scalar value is in range (≤ 0x10FFFF)
    /// and not a surrogate (0xD800–0xDFFF).  Returns the decoded `char`.
    fn read_unicode_escape(&mut self, string_start: usize) -> NuResult<char> {
        let esc_start = self.pos - 2; // backslash position for error spans

        // Expect '{' after 'u'.
        match self.advance() {
            Some(b'{') => {}
            Some(other) => {
                return Err(NuError::LexError {
                    msg: format!("Expected '{{' after \\\\u, found '{}'", other as char),
                    span: Span::new(esc_start as u32, self.pos as u32),
                })
            }
            None => {
                return Err(NuError::LexError {
                    msg: "Unterminated \\\\u escape".to_string(),
                    span: Span::new(esc_start as u32, self.pos as u32),
                })
            }
        }

        let hex_start = self.pos;
        let mut hex_count = 0u32;
        let mut scalar: u32 = 0;

        loop {
            let digit = match self.peek() {
                Some(b'0'..=b'9') => {
                    self.advance();
                    self.bytes[self.pos - 1] - b'0'
                }
                Some(b'a'..=b'f') => {
                    self.advance();
                    self.bytes[self.pos - 1] - b'a' + 10
                }
                Some(b'A'..=b'F') => {
                    self.advance();
                    self.bytes[self.pos - 1] - b'A' + 10
                }
                _ => break,
            };
            hex_count += 1;
            if hex_count > 6 {
                return Err(NuError::LexError {
                    msg: "Too many hex digits in \\\\u{{...}} escape (max 6)".to_string(),
                    span: Span::new(string_start as u32, self.pos as u32),
                });
            }
            scalar = scalar
                .checked_mul(16)
                .and_then(|v| v.checked_add(digit as u32))
                .unwrap_or(0xFFFF_FFFF); // saturate; range check below catches
        }

        if hex_count == 0 {
            return Err(NuError::LexError {
                msg: "Expected hex digits in \\\\u{{...}} escape".to_string(),
                span: Span::new(esc_start as u32, self.pos as u32),
            });
        }

        // Expect closing '}'.
        match self.advance() {
            Some(b'}') => {}
            Some(other) => {
                return Err(NuError::LexError {
                    msg: format!(
                        "Expected '}}' after \\\\u{{...}} escape, found '{}'",
                        other as char
                    ),
                    span: Span::new(esc_start as u32, self.pos as u32),
                })
            }
            None => {
                return Err(NuError::LexError {
                    msg: "Unterminated \\\\u escape".to_string(),
                    span: Span::new(esc_start as u32, self.pos as u32),
                })
            }
        }

        // Validate range: valid Unicode scalar (≤ 0x10FFFF, not surrogate).
        if scalar > 0x10FFFF {
            return Err(NuError::LexError {
                msg: format!(
                    "Unicode scalar value U+{:X} is out of range (max U+10FFFF)",
                    scalar
                ),
                span: Span::new(hex_start as u32, self.pos as u32),
            });
        }
        if (0xD800..=0xDFFF).contains(&scalar) {
            return Err(NuError::LexError {
                msg: format!(
                    "Unicode scalar value U+{:X} is a surrogate (not a valid character)",
                    scalar
                ),
                span: Span::new(hex_start as u32, self.pos as u32),
            });
        }

        // Safety: we validated the range, so unwrap is fine.
        Ok(char::from_u32(scalar).unwrap())
    }

    fn read_comment(&mut self) -> Token {
        let start = self.pos;

        self.advance(); // first '/'
        let kind = self.advance().unwrap_or(b' ');

        match kind {
            b'/' => {
                // Line comment: // or ///
                let is_doc = self.peek() == Some(b'/');
                if is_doc {
                    self.advance(); // third '/'
                }

                let content_start = self.pos;
                while let Some(ch) = self.peek() {
                    if ch == b'\n' || ch == b'\r' {
                        break;
                    }
                    self.advance();
                }
                let content = self.source[content_start..self.pos].to_string();
                if is_doc {
                    Token {
                        kind: TokenKind::DocComment(content),
                        span: Span::new(start as u32, self.pos as u32),
                    }
                } else {
                    Token {
                        kind: TokenKind::Comment(content),
                        span: Span::new(start as u32, self.pos as u32),
                    }
                }
            }
            b'*' => {
                // Block comment: /* ... */
                let content_start = self.pos;
                let mut depth = 1;
                while depth > 0 {
                    match self.peek() {
                        Some(b'*') => {
                            self.advance();
                            if self.peek() == Some(b'/') {
                                self.advance();
                                depth -= 1;
                            }
                        }
                        Some(b'/') => {
                            self.advance();
                            if self.peek() == Some(b'*') {
                                self.advance();
                                depth += 1;
                            }
                        }
                        Some(_) => {
                            self.advance();
                        }
                        None => break,
                    }
                }
                let end = self.pos.saturating_sub(2);
                let end = if end < content_start {
                    content_start
                } else {
                    end
                };
                let content = self.source[content_start..end].to_string();
                Token {
                    kind: TokenKind::Comment(content),
                    span: Span::new(start as u32, self.pos as u32),
                }
            }
            _ => unreachable!(),
        }
    }

    fn read_operator(&mut self) -> NuResult<Token> {
        let start = self.pos;

        let ch = self.advance().unwrap_or(b'\0');

        let kind = match ch {
            b'+' => {
                if self.match_char(b'=') {
                    TokenKind::PlusAssign
                } else {
                    TokenKind::Plus
                }
            }
            b'-' => {
                if self.match_char(b'>') {
                    TokenKind::Arrow
                } else if self.match_char(b'=') {
                    TokenKind::MinusAssign
                } else {
                    TokenKind::Minus
                }
            }
            b'*' => {
                if self.match_char(b'*') {
                    TokenKind::Star2
                } else {
                    TokenKind::Star
                }
            }
            b'/' => TokenKind::Slash,
            b'%' => TokenKind::Percent,
            b'=' => {
                if self.match_char(b'=') {
                    TokenKind::Eq
                } else if self.match_char(b'>') {
                    TokenKind::FatArrow
                } else {
                    TokenKind::Assign
                }
            }
            b'!' => {
                if self.match_char(b'=') {
                    TokenKind::Ne
                } else {
                    TokenKind::Bang
                }
            }
            b'<' => {
                if self.match_char(b'=') {
                    TokenKind::Le
                } else if self.match_char(b'<') {
                    TokenKind::Shl
                } else if self.match_char(b'-') {
                    if self.match_char(b'?') {
                        TokenKind::ThinArrowQuestion
                    } else {
                        TokenKind::ThinArrow
                    }
                } else {
                    TokenKind::Lt
                }
            }
            b'>' => {
                if self.match_char(b'=') {
                    TokenKind::Ge
                } else if self.match_char(b'>') {
                    TokenKind::Shr
                } else {
                    TokenKind::Gt
                }
            }
            b'&' => {
                if self.match_char(b'&') {
                    TokenKind::And
                } else {
                    TokenKind::Ampersand
                }
            }
            b'|' => {
                if self.match_char(b'|') {
                    if self.match_char(b'|') {
                        TokenKind::Pipe3
                    } else {
                        TokenKind::Or
                    }
                } else if self.match_char(b'>') {
                    TokenKind::PipeOp
                } else {
                    TokenKind::Pipe
                }
            }
            b'^' => TokenKind::Caret,
            b'~' => TokenKind::Tilde,
            b'.' => {
                if self.match_char(b'.') {
                    TokenKind::DotDot
                } else {
                    TokenKind::Dot
                }
            }
            b':' => {
                if self.match_char(b':') {
                    TokenKind::DoubleColon
                } else {
                    TokenKind::Colon
                }
            }
            b'?' => TokenKind::Question,
            _ => {
                return Err(NuError::LexError {
                    msg: format!("Unexpected operator character: {}", ch as char),
                    span: Span::new(start as u32, self.pos as u32),
                })
            }
        };

        Ok(Token {
            kind,
            span: Span::new(start as u32, self.pos as u32),
        })
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let ch = self.bytes.get(self.pos).copied();
        if ch.is_some() {
            self.pos += 1;
        }
        ch
    }

    fn match_char(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Keywords lookup
// ---------------------------------------------------------------------------

fn keyword(s: &str) -> Option<TokenKind> {
    match s {
        "var" => Some(TokenKind::Var),
        "fn" => Some(TokenKind::Fn),
        "let" => Some(TokenKind::Let),
        "rec" => Some(TokenKind::Rec),
        "in" => Some(TokenKind::In),
        "if" => Some(TokenKind::If),
        "then" => Some(TokenKind::Then),
        "else" => Some(TokenKind::Else),
        "match" => Some(TokenKind::Match),
        "with" => Some(TokenKind::With),
        "hide" => Some(TokenKind::Hide),
        "seal" => Some(TokenKind::Seal),
        "except" => Some(TokenKind::Except),
        "requires" => Some(TokenKind::Requires),
        "ensures" => Some(TokenKind::Ensures),
        "case" => Some(TokenKind::Case),
        "await" => Some(TokenKind::Await),
        "as" => Some(TokenKind::As),
        "actor" => Some(TokenKind::Actor),
        "entity" => Some(TokenKind::Entity),
        "organization" => Some(TokenKind::Organization),
        "virtual" => Some(TokenKind::Virtual),
        "behavior" => Some(TokenKind::Behavior),
        "state" => Some(TokenKind::State),
        "state_machine" => Some(TokenKind::StateMachine),
        "persistent" => Some(TokenKind::Persistent),
        "local" => Some(TokenKind::Local),
        "durable" => Some(TokenKind::Durable),
        "event_sourced" => Some(TokenKind::EventSourced),
        "crdt" => Some(TokenKind::Crdt),
        "until" => Some(TokenKind::Until),
        "emit" => Some(TokenKind::Emit),
        "workflow" => Some(TokenKind::Workflow),
        "step" => Some(TokenKind::Step),
        "initial" => Some(TokenKind::Initial),
        "parallel" => Some(TokenKind::Parallel),
        "compensate" => Some(TokenKind::Compensate),
        "self" => Some(TokenKind::SelfKw),
        "spawn" => Some(TokenKind::Spawn),
        "send" => Some(TokenKind::Send),
        "remote" => Some(TokenKind::Remote),
        "ask" => Some(TokenKind::Ask),
        "effect" => Some(TokenKind::Effect),
        "perform" => Some(TokenKind::Perform),
        "handle" => Some(TokenKind::Handle),
        "resume" => Some(TokenKind::Resume),
        "extern" => Some(TokenKind::Extern),
        "module" => Some(TokenKind::Module),
        "import" => Some(TokenKind::Import),
        "app" => Some(TokenKind::App),
        "signal" => Some(TokenKind::Signal),
        "pub" => Some(TokenKind::Pub),
        "while" => Some(TokenKind::While),
        "migrate" => Some(TokenKind::Migrate),
        "monitor" => Some(TokenKind::Monitor),
        "link" => Some(TokenKind::Link),
        "exit" => Some(TokenKind::Exit),
        "for" => Some(TokenKind::For),
        "break" => Some(TokenKind::Break),
        "return" => Some(TokenKind::Return),
        "type" => Some(TokenKind::Type),
        "alias" => Some(TokenKind::Alias),
        "iso" => Some(TokenKind::Iso),
        "trn" => Some(TokenKind::Trn),
        "ref" => Some(TokenKind::Ref),
        "val" => Some(TokenKind::Val),
        "box" => Some(TokenKind::Box),
        "tag" => Some(TokenKind::Tag),
        "lineariso" => Some(TokenKind::LinearIso),
        "linear" => Some(TokenKind::Linear),
        "true" => Some(TokenKind::BoolLit(true)),
        "given" => Some(TokenKind::Given),
        "using" => Some(TokenKind::Using),
        "false" => Some(TokenKind::BoolLit(false)),
        "nil" => Some(TokenKind::NilLit),
        "opaque" => Some(TokenKind::Opaque),
        "and" => Some(TokenKind::And),
        "or" => Some(TokenKind::Or),
        "not" => Some(TokenKind::Not),
        "defer" => Some(TokenKind::Defer),
        "errdefer" => Some(TokenKind::Errdefer),
        "catch" => Some(TokenKind::Catch),
        "fail" => Some(TokenKind::Fail),
        "class" => Some(TokenKind::Class),
        "impl" => Some(TokenKind::Impl),
        "throws" => Some(TokenKind::Throws),
        "unit" => Some(TokenKind::UnitLit),
        "tool" => Some(TokenKind::Tool),
        "handler" => Some(TokenKind::Handler),
        "consume" => Some(TokenKind::Consume),
        "recover" => Some(TokenKind::Recover),
        "database" => Some(TokenKind::Database),
        "agent" => Some(TokenKind::Agent),
        "receive" => Some(TokenKind::Receive),
        "par" => Some(TokenKind::Par),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(source: &str) -> Vec<TokenKind> {
        let mut lexer = Lexer::new(source);
        lexer.lex().unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn test_simple_tokens() {
        let kinds = lex("let x = 42 in x + 1");
        assert_eq!(
            kinds,
            vec![
                TokenKind::Let,
                TokenKind::Ident("x".to_string()),
                TokenKind::Assign,
                TokenKind::IntLit(42),
                TokenKind::In,
                TokenKind::Ident("x".to_string()),
                TokenKind::Plus,
                TokenKind::IntLit(1),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_nested_block_comments() {
        // Nested comments are supported by the lexer.
        let source = "/* outer /* inner */ still outer */ 42";
        let kinds = lex(source);
        assert_eq!(kinds, vec![TokenKind::IntLit(42), TokenKind::Eof]);
    }

    #[test]
    fn test_doc_comment_preserved() {
        let kinds = lex("/// hello\n42");
        assert_eq!(
            kinds,
            vec![
                TokenKind::DocComment(" hello".to_string()),
                TokenKind::Newline,
                TokenKind::IntLit(42),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_line_comment_skipped() {
        let kinds = lex("// ignored\n42");
        assert_eq!(
            kinds,
            vec![TokenKind::Newline, TokenKind::IntLit(42), TokenKind::Eof]
        );
    }

    #[test]
    fn test_string_escapes() {
        let kinds = lex(r#""a\nb\tc\"d""#);
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringLit("a\nb\tc\"d".to_string()),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn test_string_utf8_multibyte() {
        // Multi-byte UTF-8 chars (2-byte é, 3-byte 你, 4-byte 🎉) must be
        // decoded as chars, not pushed as raw bytes (`é` must not become `Ã©`).
        let kinds = lex("\"héllo 你好 🎉\"");
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringLit("héllo 你好 🎉".to_string()),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn test_float_variants() {
        let kinds = lex("3.5 1e3 2.5e-2");
        assert!(matches!(kinds[0], TokenKind::FloatLit(v) if (v - 3.5).abs() < 1e-9));
        assert!(matches!(kinds[1], TokenKind::FloatLit(v) if (v - 1000.0).abs() < 1e-9));
        assert!(matches!(kinds[2], TokenKind::FloatLit(v) if (v - 0.025).abs() < 1e-9));
    }

    #[test]
    fn test_all_operators() {
        // Exercise every operator that the lexer currently recognizes.
        let source = "+ - * / % == != < <= > >= && || & | |> ^ ~ << >> = += -= -> => <- . .. : ::";
        let kinds = lex(source);
        assert_eq!(
            kinds,
            vec![
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
                TokenKind::Percent,
                TokenKind::Eq,
                TokenKind::Ne,
                TokenKind::Lt,
                TokenKind::Le,
                TokenKind::Gt,
                TokenKind::Ge,
                TokenKind::And,
                TokenKind::Or,
                TokenKind::Ampersand,
                TokenKind::Pipe,
                TokenKind::PipeOp,
                TokenKind::Caret,
                TokenKind::Tilde,
                TokenKind::Shl,
                TokenKind::Shr,
                TokenKind::Assign,
                TokenKind::PlusAssign,
                TokenKind::MinusAssign,
                TokenKind::Arrow,
                TokenKind::FatArrow,
                TokenKind::ThinArrow,
                TokenKind::Dot,
                TokenKind::DotDot,
                TokenKind::Colon,
                TokenKind::DoubleColon,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_entity_keyword() {
        let kinds = lex("entity actor persistent");
        assert_eq!(
            kinds,
            vec![
                TokenKind::Entity,
                TokenKind::Actor,
                TokenKind::Persistent,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_keywords_and_identifiers() {
        let kinds = lex("fn let rec if true False MyType _under");
        assert_eq!(
            kinds,
            vec![
                TokenKind::Fn,
                TokenKind::Let,
                TokenKind::Rec,
                TokenKind::If,
                TokenKind::BoolLit(true),
                TokenKind::UpperIdent("False".to_string()),
                TokenKind::UpperIdent("MyType".to_string()),
                TokenKind::Ident("_under".to_string()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_unterminated_block_comment() {
        // Unterminated block comments are currently accepted and consume to EOF.
        let kinds = lex("/* never closed");
        assert_eq!(kinds, vec![TokenKind::Eof]);
    }

    #[test]
    fn test_unterminated_string_errors() {
        let mut lexer = Lexer::new("\"hello");
        let err = lexer.lex().unwrap_err();
        match err {
            NuError::LexError { msg, .. } => {
                assert!(msg.contains("Unterminated string literal"));
            }
            _ => panic!("Expected LexError"),
        }
    }

    #[test]
    fn test_unknown_escape_errors() {
        let mut lexer = Lexer::new("\"\\q\"");
        let err = lexer.lex().unwrap_err();
        match err {
            NuError::LexError { msg, .. } => {
                assert!(msg.contains("Unknown escape sequence"));
            }
            _ => panic!("Expected LexError"),
        }
    }

    #[test]
    fn test_invalid_hex_errors() {
        let mut lexer = Lexer::new("0x");
        let err = lexer.lex().unwrap_err();
        match err {
            NuError::LexError { msg, .. } => {
                assert!(msg.contains("Expected hex digits"));
            }
            _ => panic!("Expected LexError"),
        }
    }

    #[test]
    fn test_invalid_float_exponent_errors() {
        let mut lexer = Lexer::new("1e");
        let err = lexer.lex().unwrap_err();
        match err {
            NuError::LexError { msg, .. } => {
                assert!(msg.contains("Expected digits in exponent"));
            }
            _ => panic!("Expected LexError"),
        }
    }

    #[test]
    fn test_unexpected_character_errors() {
        let mut lexer = Lexer::new("$");
        let err = lexer.lex().unwrap_err();
        match err {
            NuError::LexError { msg, .. } => {
                assert!(msg.contains("Unexpected character"));
            }
            _ => panic!("Expected LexError"),
        }
    }

    #[test]
    fn test_unicode_identifier_rejected() {
        // The current lexer only accepts ASCII identifiers.
        let mut lexer = Lexer::new("α");
        let err = lexer.lex().unwrap_err();
        assert!(matches!(err, NuError::LexError { .. }));
    }

    #[test]
    fn test_workflow_keywords() {
        let mut lexer = Lexer::new("workflow step parallel compensate");
        let tokens = lexer.lex().unwrap();
        let kinds: Vec<_> = tokens.iter().map(|t| t.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Workflow,
                TokenKind::Step,
                TokenKind::Parallel,
                TokenKind::Compensate,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_former_keywords_now_identifiers() {
        // subworkflow, priv, node, loop, where were reserved but unwired;
        // they now lex as plain identifiers.
        for word in &["subworkflow", "priv", "node", "loop", "where"] {
            let mut lexer = Lexer::new(word);
            let tokens = lexer.lex().unwrap();
            assert_eq!(tokens.len(), 2); // ident + eof
            assert!(matches!(tokens[0].kind, TokenKind::Ident(_)));
        }
    }

    #[test]
    fn test_await_is_reserved_keyword() {
        let mut lexer = Lexer::new("await");
        let tokens = lexer.lex().unwrap();
        assert_eq!(tokens.len(), 2); // keyword + eof
        assert!(matches!(tokens[0].kind, TokenKind::Await));
    }

    #[test]
    fn test_tool_annotation_tokens() {
        let source = r#"@tool(description: "Adds two integers.")"#;
        let kinds = lex(source);
        assert_eq!(
            kinds,
            vec![
                TokenKind::At,
                TokenKind::Tool,
                TokenKind::LParen,
                TokenKind::Ident("description".to_string()),
                TokenKind::Colon,
                TokenKind::StringLit("Adds two integers.".to_string()),
                TokenKind::RParen,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_after_is_a_plain_identifier() {
        // `after` is a CONTEXTUAL keyword: it is only special immediately
        // after a receive block (`receive { ... } after ms => body`, see
        // parse_receive) and lexes as an ordinary identifier everywhere
        // else — e.g. workflow steps may be named `after`. This mirrors the
        // `to` identifier in `migrate actor to node` (parse_migrate).
        let kinds = lex("after 100");
        assert_eq!(
            kinds,
            vec![
                TokenKind::Ident("after".to_string()),
                TokenKind::IntLit(100),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_agent_keyword() {
        let kinds = lex("agent MyAgent = { model: \"gpt-4o\" }");
        assert_eq!(
            kinds,
            vec![
                TokenKind::Agent,
                TokenKind::UpperIdent("MyAgent".to_string()),
                TokenKind::Assign,
                TokenKind::LBrace,
                TokenKind::Ident("model".to_string()),
                TokenKind::Colon,
                TokenKind::StringLit("gpt-4o".to_string()),
                TokenKind::RBrace,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_state_machine_keyword() {
        // `state_machine` is a reserved keyword; `event`/`on_entry`/`on_exit`
        // are CONTEXTUAL keywords — only special inside a state_machine body
        // (see parse_state_machine), plain identifiers everywhere else. This
        // mirrors `after` in `receive { } after ms => body`.
        let kinds = lex("state_machine event on_entry on_exit");
        assert_eq!(
            kinds,
            vec![
                TokenKind::StateMachine,
                TokenKind::Ident("event".to_string()),
                TokenKind::Ident("on_entry".to_string()),
                TokenKind::Ident("on_exit".to_string()),
                TokenKind::Eof,
            ]
        );
    }

    // -------------------------------------------------------------------
    // Triple-quoted string tests
    // -------------------------------------------------------------------

    #[test]
    fn test_triple_quoted_basic_multiline() {
        let source = "\"\"\"Hello\nWorld\n\"\"\"";
        let kinds = lex(source);
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringLit("Hello\nWorld\n".to_string()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_triple_quoted_empty() {
        let kinds = lex("\"\"\"\"\"\"");
        assert_eq!(
            kinds,
            vec![TokenKind::StringLit("".to_string()), TokenKind::Eof,]
        );
    }

    #[test]
    fn test_triple_quoted_with_inner_quotes() {
        // Single and double quotes inside triple-quoted string should be literal.
        let source = "\"\"\"a\"b\"\"c\"\"\"";
        let kinds = lex(source);
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringLit("a\"b\"\"c".to_string()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_triple_quoted_escapes() {
        let source = "\"\"\"a\\nb\\tc\"\"\"";
        let kinds = lex(source);
        assert_eq!(
            kinds,
            vec![TokenKind::StringLit("a\nb\tc".to_string()), TokenKind::Eof,]
        );
    }

    #[test]
    fn test_triple_quoted_unicode_escape() {
        let kinds = lex("\"\"\"\\u{41}\\u{1F600}\"\"\"");
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringLit("A\u{1F600}".to_string()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_triple_quoted_single_line() {
        // Triple-quoted doesn't require multiple lines — it still works.
        let kinds = lex("\"\"\"hello\"\"\"");
        assert_eq!(
            kinds,
            vec![TokenKind::StringLit("hello".to_string()), TokenKind::Eof,]
        );
    }

    // -------------------------------------------------------------------
    // Unicode \\u{...} escape tests
    // -------------------------------------------------------------------

    #[test]
    fn test_unicode_escape_basic() {
        let kinds = lex("\"\\u{41}\"");
        assert_eq!(
            kinds,
            vec![TokenKind::StringLit("A".to_string()), TokenKind::Eof,]
        );
    }

    #[test]
    fn test_unicode_escape_multiple() {
        let kinds = lex("\"\\u{48}\\u{49}\"");
        assert_eq!(
            kinds,
            vec![TokenKind::StringLit("HI".to_string()), TokenKind::Eof,]
        );
    }

    #[test]
    fn test_unicode_escape_emoji() {
        let kinds = lex("\"\\u{1F600}\"");
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringLit("\u{1F600}".to_string()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_unicode_escape_lowercase() {
        let kinds = lex("\"\\u{7a}\"");
        assert_eq!(
            kinds,
            vec![TokenKind::StringLit("z".to_string()), TokenKind::Eof,]
        );
    }

    #[test]
    fn test_unicode_escape_max() {
        // U+10FFFF is the maximum valid scalar.
        let kinds = lex("\"\\u{10FFFF}\"");
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringLit("\u{10FFFF}".to_string()),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn test_unicode_escape_surrogate_errors() {
        let mut lexer = Lexer::new("\"\\u{D800}\"");
        let err = lexer.lex().unwrap_err();
        match err {
            NuError::LexError { msg, .. } => {
                assert!(msg.contains("surrogate"));
            }
            _ => panic!("Expected LexError for surrogate"),
        }
    }

    #[test]
    fn test_unicode_escape_out_of_range_errors() {
        let mut lexer = Lexer::new("\"\\u{110000}\"");
        let err = lexer.lex().unwrap_err();
        match err {
            NuError::LexError { msg, .. } => {
                assert!(msg.contains("out of range"));
            }
            _ => panic!("Expected LexError for out-of-range"),
        }
    }

    #[test]
    fn test_unicode_escape_no_hex_errors() {
        let mut lexer = Lexer::new("\"\\u{}\"");
        let err = lexer.lex().unwrap_err();
        match err {
            NuError::LexError { msg, .. } => {
                assert!(msg.contains("Expected hex digits"));
            }
            _ => panic!("Expected LexError for empty braces"),
        }
    }

    #[test]
    fn test_unicode_escape_no_close_brace_errors() {
        let mut lexer = Lexer::new("\"\\u{41\"");
        let err = lexer.lex().unwrap_err();
        match err {
            NuError::LexError { msg, .. } => {
                assert!(msg.contains("Expected '}'"));
            }
            _ => panic!("Expected LexError for missing close brace"),
        }
    }

    #[test]
    fn test_unicode_escape_too_many_digits_errors() {
        let mut lexer = Lexer::new("\"\\u{1234567}\"");
        let err = lexer.lex().unwrap_err();
        match err {
            NuError::LexError { msg, .. } => {
                assert!(msg.contains("Too many hex digits"));
            }
            _ => panic!("Expected LexError for too many digits"),
        }
    }

    #[test]
    fn test_unicode_escape_in_triple_quoted_errors() {
        // Surrogate inside triple-quoted strings also errors.
        let mut lexer = Lexer::new("\"\"\"\\u{D800}\"\"\"");
        let err = lexer.lex().unwrap_err();
        match err {
            NuError::LexError { msg, .. } => {
                assert!(msg.contains("surrogate"));
            }
            _ => panic!("Expected LexError for surrogate in triple-quoted"),
        }
    }
}
