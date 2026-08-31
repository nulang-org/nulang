//! LSP (Language Server Protocol) server for Nulang.
//!
//! Run with: `nulang --lsp` (starts stdin/stdout JSON-RPC server)
//!
//! # Supported LSP Features
//!
//! | Feature | Description |
//! |---------|-------------|
//! | `textDocument/publishDiagnostics` (push) | Parse/type/effect/capability diagnostics |
//! | `textDocument/hover` | Function signatures, effects, types, doc comments |
//! | `textDocument/definition` | Go to definition for all declaration types |
//! | `textDocument/references` | Find all usages of a symbol |
//! | `textDocument/documentSymbol` | Structured outline (functions, actors, etc.) |
//! | `textDocument/rename` | Rename symbol across document (with prepareRename) |
//! | `textDocument/signatureHelp` | Function parameter hints while typing |
//! | `textDocument/formatting` | Indentation-based code formatting |
//! | `textDocument/semanticTokens` | Syntax highlighting for editors |
//! | `textDocument/codeAction` | Quick fixes (add type annotations) |
//! | `textDocument/inlayHint` | Inferred types + effect rows + return types |
//! | `textDocument/completion` | Keyword/effect/capability/function completion |
//! | `textDocument/codeLens` | Reference counts on top-level declarations |
//! | `textDocument/documentLink` | Clickable import paths to stdlib/files |
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::repl::type_to_string;
use crate::typechecker::TypeChecker;
use crate::types::NuError;

/// Convert an LSP position column (UTF-16 code units) into a byte offset
/// within `line`, clamped to a char boundary.
///
/// LSP clients report columns as UTF-16 code units, but Rust strings are
/// sliced by byte offset. Using the raw column as a byte index on non-ASCII
/// text lands inside multibyte characters and panics; this helper walks the
/// line and snaps to the boundary covering the requested column.
fn utf16_col_to_byte(line: &str, col: usize) -> usize {
    let mut utf16 = 0usize;
    for (byte_idx, ch) in line.char_indices() {
        if utf16 >= col {
            return byte_idx;
        }
        utf16 += ch.len_utf16();
    }
    line.len()
}

// ---------------------------------------------------------------------------
// LSP Server
// ---------------------------------------------------------------------------

/// Nulang language server implementing the LSP protocol.
pub struct NulangLanguageServer {
    client: Client,
    documents: Mutex<HashMap<Url, DocumentState>>,
}

struct DocumentState {
    version: i32,
    source: String,
    type_map: Option<HashMap<usize, String>>,
    ast: Option<crate::ast::AstModule>,
}

impl NulangLanguageServer {
    pub fn new(client: Client) -> Self {
        NulangLanguageServer {
            client,
            documents: Mutex::new(HashMap::new()),
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for NulangLanguageServer {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                inlay_hint_provider: Some(OneOf::Right(InlayHintServerCapabilities::Options(
                    InlayHintOptions {
                        resolve_provider: Some(false),
                        work_done_progress_options: WorkDoneProgressOptions::default(),
                    },
                ))),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec![".".into(), ":".into()]),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                    all_commit_characters: None,
                    completion_item: None,
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: None,
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                document_formatting_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                            legend: SemanticTokensLegend {
                                token_types: vec![
                                    SemanticTokenType::KEYWORD,
                                    SemanticTokenType::FUNCTION,
                                    SemanticTokenType::VARIABLE,
                                    SemanticTokenType::TYPE,
                                    SemanticTokenType::CLASS,
                                    SemanticTokenType::STRING,
                                    SemanticTokenType::NUMBER,
                                    SemanticTokenType::OPERATOR,
                                    SemanticTokenType::COMMENT,
                                    SemanticTokenType::NAMESPACE,
                                ],
                                token_modifiers: vec![
                                    SemanticTokenModifier::DECLARATION,
                                    SemanticTokenModifier::READONLY,
                                ],
                            },
                            range: None,
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                        },
                    ),
                ),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: None,
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        will_save: None,
                        will_save_wait_until: None,
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                    },
                )),
                // No `diagnostic_provider`: pull diagnostics
                // (`textDocument/diagnostic`) are NOT implemented, and
                // advertising them makes clients send requests that fail
                // with MethodNotFound (and, worse, tower-lsp logs that
                // failure to stdout, corrupting the JSON-RPC framing).
                // Diagnostics are push-only via `publishDiagnostics`.
                ..ServerCapabilities::default()
            },
            ..InitializeResult::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "Nulang LSP server initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let version = params.text_document.version;
        let source = params.text_document.text.clone();

        let (diagnostics, type_map) = Self::compute_diagnostics(&source);

        {
            let mut docs = self.documents.lock().unwrap();
            let ast = Lexer::new(&source)
                .lex()
                .ok()
                .and_then(|tokens| Parser::new(tokens).parse_module().ok());
            docs.insert(
                uri.clone(),
                DocumentState {
                    version,
                    source: source.clone(),
                    type_map: Some(type_map),
                    ast,
                },
            );
        }

        self.client
            .publish_diagnostics(uri, diagnostics, Some(version))
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let version = params.text_document.version;
        let source = params
            .content_changes
            .into_iter()
            .next()
            .map(|c| c.text)
            .unwrap_or_default();

        let (diagnostics, type_map) = Self::compute_diagnostics(&source);

        {
            let mut docs = self.documents.lock().unwrap();
            if let Some(doc) = docs.get_mut(&uri) {
                doc.version = version;
                doc.source = source.clone();
                doc.type_map = Some(type_map);
                doc.ast = Lexer::new(&source)
                    .lex()
                    .ok()
                    .and_then(|tokens| Parser::new(tokens).parse_module().ok());
            }
        }

        self.client
            .publish_diagnostics(uri, diagnostics, Some(version))
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let mut docs = self.documents.lock().unwrap();
        docs.remove(&params.text_document.uri);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.clone();

        let (source, version) = {
            let docs = self.documents.lock().unwrap();
            match docs.get(&uri) {
                Some(doc) => (doc.source.clone(), doc.version),
                None => return,
            }
        };

        let (diagnostics, type_map) = Self::compute_diagnostics(&source);

        {
            let mut docs = self.documents.lock().unwrap();
            if let Some(doc) = docs.get_mut(&uri) {
                doc.type_map = Some(type_map);
            }
        }

        self.client
            .publish_diagnostics(uri, diagnostics, Some(version))
            .await;
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let docs = self.documents.lock().unwrap();
        let source = match docs.get(&params.text_document.uri) {
            Some(doc) => doc.source.clone(),
            None => return Ok(None),
        };
        drop(docs);

        let engine = InlayHintEngine::new(&source);
        let hints = engine.generate_inlay_hints();
        Ok(Some(hints))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let docs = self.documents.lock().unwrap();
        let doc = match docs.get(&params.text_document_position.text_document.uri) {
            Some(d) => d,
            None => return Ok(None),
        };
        let source = doc.source.clone();
        let ast = doc.ast.clone();
        drop(docs);

        let mut engine = CompletionEngine::new(&source);
        if let Some(ref a) = ast {
            engine.set_ast_info(a);
        }
        let doc_dir = params
            .text_document_position
            .text_document
            .uri
            .to_file_path()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()));
        let items = engine.complete(params.text_document_position.position, doc_dir.as_deref());
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let docs = self.documents.lock().unwrap();
        let doc = match docs.get(&params.text_document_position_params.text_document.uri) {
            Some(d) => d,
            None => return Ok(None),
        };
        let source = doc.source.clone();
        let type_map = doc.type_map.clone();
        drop(docs);

        // Check type_map first for inferred/explicit types
        if let Some(ref map) = type_map {
            if let Some(offset) = Self::position_to_byte_offset(
                &source,
                &params.text_document_position_params.position,
            ) {
                if let Some(type_str) = Self::find_type_at_offset(map, offset) {
                    return Ok(Some(Hover {
                        contents: HoverContents::Scalar(MarkedString::LanguageString(
                            LanguageString {
                                language: "nulang".to_string(),
                                value: type_str.clone(),
                            },
                        )),
                        range: None,
                    }));
                }
            }
        }

        Ok(Self::hover_at(
            &source,
            params.text_document_position_params.position,
        ))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let docs = self.documents.lock().unwrap();
        let doc = match docs.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };
        let source = doc.source.clone();
        let ast = doc.ast.clone();
        drop(docs);

        // Search local definitions first
        if let Some(loc) =
            self.goto_def_local(&source, params.text_document_position_params.position, &uri)
        {
            return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
        }

        // Try cross-file: extract word and search imported modules
        let line = params.text_document_position_params.position.line as usize;
        let col = params.text_document_position_params.position.character as usize;
        if let Some(target_line) = source.lines().nth(line) {
            let byte_col = utf16_col_to_byte(target_line, col);
            if let Some(word) = Self::word_at(target_line, byte_col) {
                if let Some(ref a) = ast {
                    if let Some(loc) = self.goto_def_cross_file(&uri, word, a) {
                        return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                    }
                }
            }
        }
        Ok(None)
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let docs = self.documents.lock().unwrap();
        let source = match docs.get(&params.text_document_position.text_document.uri) {
            Some(doc) => doc.source.clone(),
            None => return Ok(None),
        };
        drop(docs);
        let uri = params.text_document_position.text_document.uri.clone();
        Ok(Some(self.find_refs(
            &source,
            params.text_document_position.position,
            &uri,
        )))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let docs = self.documents.lock().unwrap();
        let source = match docs.get(&params.text_document.uri) {
            Some(doc) => doc.source.clone(),
            None => return Ok(None),
        };
        drop(docs);
        let locs = self.find_refs(&source, params.position, &params.text_document.uri);
        Ok(locs.first().map(|l| PrepareRenameResponse::Range(l.range)))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let docs = self.documents.lock().unwrap();
        let source = match docs.get(&params.text_document_position.text_document.uri) {
            Some(doc) => doc.source.clone(),
            None => return Ok(None),
        };
        drop(docs);
        let locs = self.find_refs(
            &source,
            params.text_document_position.position,
            &params.text_document_position.text_document.uri,
        );
        if locs.is_empty() {
            return Ok(None);
        }
        let uri = locs[0].uri.clone();
        let edits: Vec<TextEdit> = locs
            .iter()
            .map(|l| TextEdit {
                range: l.range,
                new_text: params.new_name.clone(),
            })
            .collect();
        let mut changes = std::collections::HashMap::new();
        changes.insert(uri, edits);
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        }))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let docs = self.documents.lock().unwrap();
        let source = match docs.get(&params.text_document.uri) {
            Some(doc) => doc.source.clone(),
            None => return Ok(None),
        };
        drop(docs);
        Ok(
            Self::doc_syms_uri(&source, &params.text_document.uri)
                .map(DocumentSymbolResponse::Flat),
        )
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let docs = self.documents.lock().unwrap();
        let query = params.query.to_lowercase();
        let mut all_syms = Vec::new();
        for (uri, doc) in docs.iter() {
            if let Some(syms) = Self::doc_syms_uri(&doc.source, uri) {
                if query.is_empty() {
                    all_syms.extend(syms);
                } else {
                    all_syms.extend(
                        syms.into_iter()
                            .filter(|s| s.name.to_lowercase().contains(&query)),
                    );
                }
            }
        }
        Ok(Some(all_syms))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let docs = self.documents.lock().unwrap();
        let source = match docs.get(&params.text_document_position_params.text_document.uri) {
            Some(doc) => doc.source.clone(),
            None => return Ok(None),
        };
        drop(docs);
        Ok(Self::sig_help(
            &source,
            params.text_document_position_params.position,
        ))
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let docs = self.documents.lock().unwrap();
        let source = match docs.get(&params.text_document.uri) {
            Some(doc) => doc.source.clone(),
            None => return Ok(None),
        };
        drop(docs);
        let formatted = self.fmt_source(&source);
        if formatted == source {
            return Ok(None);
        }
        let lines = source.lines().count() as u32;
        Ok(Some(vec![TextEdit {
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(lines, 0),
            },
            new_text: formatted,
        }]))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let docs = self.documents.lock().unwrap();
        let source = match docs.get(&params.text_document.uri) {
            Some(doc) => doc.source.clone(),
            None => return Ok(None),
        };
        drop(docs);
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: self.sem_tokens(&source),
        })))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri.clone();
        let docs = self.documents.lock().unwrap();
        let source = match docs.get(&uri) {
            Some(doc) => doc.source.clone(),
            None => return Ok(None),
        };
        drop(docs);
        let range = Some(params.range);
        Ok(Self::code_actions(&source, range, &uri))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let docs = self.documents.lock().unwrap();
        let source = match docs.get(&params.text_document.uri) {
            Some(doc) => doc.source.clone(),
            None => return Ok(None),
        };
        drop(docs);
        Ok(Some(Self::compute_folding_ranges(&source)))
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let docs = self.documents.lock().unwrap();
        let doc = match docs.get(&params.text_document.uri) {
            Some(d) => d,
            None => return Ok(None),
        };
        let source = doc.source.clone();
        let ast = doc.ast.clone();
        drop(docs);

        let mut lenses = Vec::new();

        if let Some(ref ast) = ast {
            // Build reference counts by scanning identifier tokens
            let refs = Self::count_all_refs(&source);
            for decl in &ast.decls {
                let (name, line) = match decl {
                    crate::ast::Decl::Function { name, span, .. } => {
                        (name.as_str(), span.line() as u32)
                    }
                    crate::ast::Decl::Actor { name, span, .. } => {
                        (name.as_str(), span.line() as u32)
                    }
                    crate::ast::Decl::Agent { name, span, .. } => {
                        (name.as_str(), span.line() as u32)
                    }
                    crate::ast::Decl::Workflow { name, span, .. } => {
                        (name.as_str(), span.line() as u32)
                    }
                    crate::ast::Decl::StateMachine { name, span, .. } => {
                        (name.as_str(), span.line() as u32)
                    }
                    crate::ast::Decl::TypeAlias { name, span, .. } => {
                        (name.as_str(), span.line() as u32)
                    }
                    crate::ast::Decl::VariantType { name, span, .. } => {
                        (name.as_str(), span.line() as u32)
                    }
                    crate::ast::Decl::RecordType { name, span, .. } => {
                        (name.as_str(), span.line() as u32)
                    }
                    crate::ast::Decl::Class { name, span, .. } => {
                        (name.as_str(), span.line() as u32)
                    }
                    crate::ast::Decl::NamedHandler { name, span, .. } => {
                        (name.as_str(), span.line() as u32)
                    }
                    _ => continue,
                };
                let count = refs.get(name).copied().unwrap_or(0);
                // Subtract 1 for the declaration itself
                let ref_count = count.saturating_sub(1);
                lenses.push(CodeLens {
                    range: Range {
                        start: Position::new(line.saturating_sub(1), 0),
                        end: Position::new(line.saturating_sub(1), 0),
                    },
                    command: Some(Command {
                        title: if ref_count == 0 {
                            "▶ references".to_string()
                        } else {
                            format!("▶ {} references", ref_count)
                        },
                        command: "nulang.showReferences".to_string(),
                        arguments: None,
                    }),
                    data: None,
                });
            }
        }

        Ok(Some(lenses))
    }

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        let docs = self.documents.lock().unwrap();
        let doc = match docs.get(&params.text_document.uri) {
            Some(d) => d,
            None => return Ok(None),
        };
        let source = doc.source.clone();
        drop(docs);

        let links = Self::extract_document_links(&source, &params.text_document.uri);
        Ok(Some(links))
    }
}

impl NulangLanguageServer {
    /// Run the compiler frontend on `source` and return LSP diagnostics.
    ///
    /// This is intentionally tolerant: each stage is tried in order, and the
    /// first fatal error in a stage is reported. Effect and capability checks
    /// also report accumulated warnings from their internal diagnostic lists.
    fn hover_at(source: &str, position: Position) -> Option<Hover> {
        let line = position.line as usize;
        let col = position.character as usize;
        let target_line = source.lines().nth(line)?;
        let word = Self::word_at(target_line, utf16_col_to_byte(target_line, col))?;
        let tokens = Lexer::new(source).lex().ok()?;
        let ast = Parser::new(tokens).parse_module().ok()?;
        let mut tc = TypeChecker::new();
        let mt = tc.check_module(&ast).ok()?;
        for decl in &ast.decls {
            if let crate::ast::Decl::Function {
                name,
                params,
                ret_type,
                effect,
                span,
                ..
            } = decl
            {
                if name == word {
                    let p = params
                        .iter()
                        .map(|p| {
                            format!(
                                "{}: {}",
                                p.name,
                                p.ty.as_ref()
                                    .map(|ty| format!("{:?}", ty))
                                    .unwrap_or_else(|| "?".into())
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let r = ret_type
                        .as_ref()
                        .map(|ty| format!("{:?}", ty))
                        .unwrap_or_else(|| "?".into());
                    let mut hover_text = format!("fn {}({}) -> {}", name, p, r);
                    // Append effects if present
                    if let Some(ref eff) = effect {
                        use std::fmt::Write;
                        let _ = write!(hover_text, "\n\neffects: {}", eff);
                    }
                    // Append doc comment if present
                    let decl_line = span.line().saturating_sub(1);
                    if let Some(doc) = Self::extract_doc_comment(source, decl_line) {
                        hover_text.push_str("\n\n---\n");
                        hover_text.push_str(&doc);
                    }
                    return Some(Hover {
                        contents: HoverContents::Scalar(MarkedString::String(hover_text)),
                        range: None,
                    });
                }
            }
            if let crate::ast::Decl::Actor { name, span, .. } = decl {
                if name == word {
                    let mut hover_text = format!("actor {}", name);
                    let decl_line = span.line().saturating_sub(1);
                    if let Some(doc) = Self::extract_doc_comment(source, decl_line) {
                        hover_text.push_str("\n\n---\n");
                        hover_text.push_str(&doc);
                    }
                    return Some(Hover {
                        contents: HoverContents::Scalar(MarkedString::String(hover_text)),
                        range: None,
                    });
                }
            }
            if let crate::ast::Decl::StateMachine {
                name, states, span, ..
            } = decl
            {
                if name == word {
                    let mut hover_text =
                        format!("state_machine {} (states: {})", name, states.join(", "));
                    let decl_line = span.line().saturating_sub(1);
                    if let Some(doc) = Self::extract_doc_comment(source, decl_line) {
                        hover_text.push_str("\n\n---\n");
                        hover_text.push_str(&doc);
                    }
                    return Some(Hover {
                        contents: HoverContents::Scalar(MarkedString::String(hover_text)),
                        range: None,
                    });
                }
            }
        }
        let kw = [
            "let", "fn", "fun", "actor", "agent", "if", "else", "match", "case", "for", "in",
            "spawn", "send", "ask", "perform", "handle", "emit", "return", "unit", "nil", "true",
            "false",
        ];
        if kw.contains(&word) {
            return Some(Hover {
                contents: HoverContents::Scalar(MarkedString::String(format!(
                    "keyword `{}`",
                    word
                ))),
                range: None,
            });
        }
        Some(Hover {
            contents: HoverContents::Scalar(MarkedString::String(format!("module type: {:?}", mt))),
            range: None,
        })
    }

    fn word_at(line: &str, col: usize) -> Option<&str> {
        if col >= line.len() {
            return None;
        }
        let b = line.as_bytes();
        if !b[col].is_ascii_alphanumeric() && b[col] != b'_' {
            return None;
        }
        let mut s = col;
        while s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'_') {
            s -= 1;
        }
        let mut e = col;
        while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'_') {
            e += 1;
        }
        Some(&line[s..e])
    }

    fn goto_def_local(&self, source: &str, position: Position, uri: &Url) -> Option<Location> {
        let line = position.line as usize;
        let col = position.character as usize;
        let target_line = source.lines().nth(line)?;
        let word = Self::word_at(target_line, utf16_col_to_byte(target_line, col))?;
        let tokens = Lexer::new(source).lex().ok()?;
        let ast = Parser::new(tokens).parse_module().ok()?;
        let _ = TypeChecker::new().check_module(&ast).ok()?;
        for decl in &ast.decls {
            if let Some(loc) = self.find_decl(decl, word, uri) {
                return Some(loc);
            }
        }
        None
    }

    /// Follow import statements to find definitions in other files.
    fn goto_def_cross_file(
        &self,
        current_uri: &Url,
        word: &str,
        ast: &crate::ast::AstModule,
    ) -> Option<Location> {
        for decl in &ast.decls {
            if let crate::ast::Decl::Import { path, .. } = decl {
                let docs = self.documents.lock().unwrap();
                // Resolve relative to current file's directory
                if let Ok(mut resolved) = current_uri.to_file_path() {
                    resolved.pop();
                    resolved.push(path);
                    resolved.set_extension("nula");
                    if let Ok(imported_uri) = Url::from_file_path(&resolved) {
                        if let Some(doc) = docs.get(&imported_uri) {
                            if let Some(ref imported_ast) = doc.ast {
                                for d in &imported_ast.decls {
                                    if let Some(loc) = self.find_decl(d, word, &imported_uri) {
                                        return Some(loc);
                                    }
                                }
                            }
                        }
                    }
                }
                drop(docs);
            }
        }
        None
    }
    fn find_decl(&self, decl: &crate::ast::Decl, word: &str, uri: &Url) -> Option<Location> {
        use crate::ast::Decl;
        let loc = |s: &crate::types::Span| Location {
            uri: uri.clone(),
            range: Range {
                start: Position::new(
                    s.line().saturating_sub(1) as u32,
                    s.column().saturating_sub(1) as u32,
                ),
                end: Position::new(
                    s.line().saturating_sub(1) as u32,
                    s.column().saturating_sub(1) as u32,
                ),
            },
        };
        match decl {
            Decl::Function { name, span, .. }
            | Decl::Actor { name, span, .. }
            | Decl::Agent { name, span, .. }
            | Decl::Workflow { name, span, .. }
            | Decl::StateMachine { name, span, .. }
            | Decl::TypeAlias { name, span, .. }
                if name == word =>
            {
                Some(loc(span))
            }
            Decl::Module {
                name, decls, span, ..
            } => {
                if name == word {
                    return Some(loc(span));
                }
                for d in decls {
                    if let Some(l) = self.find_decl(d, word, uri) {
                        return Some(l);
                    }
                }
                None
            }
            _ => None,
        }
    }
    fn find_refs(&self, source: &str, position: Position, uri: &Url) -> Vec<Location> {
        let line = position.line as usize;
        let col = position.character as usize;
        let target_line = match source.lines().nth(line) {
            Some(l) => l,
            None => return vec![],
        };
        let word = match Self::word_at(target_line, utf16_col_to_byte(target_line, col)) {
            Some(w) => w.to_string(),
            None => return vec![],
        };
        let tokens = match Lexer::new(source).lex() {
            Ok(t) => t,
            Err(_) => return vec![],
        };
        let ast = match Parser::new(tokens).parse_module() {
            Ok(a) => a,
            Err(_) => return vec![],
        };
        let _ = TypeChecker::new().check_module(&ast);
        let mut locs = Vec::new();
        for decl in &ast.decls {
            self.collect_refs(decl, &word, &mut locs, uri);
        }
        locs
    }
    fn collect_refs(
        &self,
        decl: &crate::ast::Decl,
        word: &str,
        locs: &mut Vec<Location>,
        uri: &Url,
    ) {
        use crate::ast::Decl;
        let loc = |s: &crate::types::Span| Location {
            uri: uri.clone(),
            range: Range {
                start: Position::new(
                    s.line().saturating_sub(1) as u32,
                    s.column().saturating_sub(1) as u32,
                ),
                end: Position::new(
                    s.line().saturating_sub(1) as u32,
                    s.column().saturating_sub(1) as u32,
                ),
            },
        };
        match decl {
            Decl::Function {
                name, body, span, ..
            } => {
                if name == word {
                    locs.push(loc(span));
                }
                self.refs_expr(body, word, locs);
            }
            Decl::Actor {
                name,
                behaviors,
                span,
                ..
            } => {
                if name == word {
                    locs.push(loc(span));
                }
                for b in behaviors {
                    self.refs_expr(&b.body, word, locs);
                }
            }
            Decl::StateMachine {
                name,
                entry_hooks,
                exit_hooks,
                span,
                ..
            } => {
                if name == word {
                    locs.push(loc(span));
                }
                for (_, body) in entry_hooks.iter().chain(exit_hooks.iter()) {
                    self.refs_expr(body, word, locs);
                }
            }
            Decl::Agent { name, span, .. }
            | Decl::Workflow { name, span, .. }
            | Decl::TypeAlias { name, span, .. }
                if name == word =>
            {
                locs.push(loc(span));
            }
            Decl::Module {
                name, decls, span, ..
            } => {
                if name == word {
                    locs.push(loc(span));
                }
                for d in decls {
                    self.collect_refs(d, word, locs, uri);
                }
            }
            _ => {}
        }
    }
    fn refs_expr(&self, expr: &crate::ast::Expr, word: &str, locs: &mut Vec<Location>) {
        use crate::ast::Expr;
        let loc = |s: &crate::types::Span| Location {
            uri: Url::parse("file:///current.nula").unwrap(),
            range: Range {
                start: Position::new(
                    s.line().saturating_sub(1) as u32,
                    s.column().saturating_sub(1) as u32,
                ),
                end: Position::new(
                    s.line().saturating_sub(1) as u32,
                    s.column().saturating_sub(1) as u32,
                ),
            },
        };
        match expr {
            Expr::Var(name, span) => {
                if name == word {
                    locs.push(loc(span));
                }
            }
            Expr::Binary { left, right, .. } => {
                self.refs_expr(left, word, locs);
                self.refs_expr(right, word, locs);
            }
            Expr::Let {
                name,
                value,
                body,
                span,
                ..
            } => {
                if name == word {
                    locs.push(loc(span));
                }
                self.refs_expr(value, word, locs);
                self.refs_expr(body, word, locs);
            }
            Expr::Block { exprs, .. } => {
                for e in exprs {
                    self.refs_expr(e, word, locs);
                }
            }
            Expr::Par { exprs, .. } => {
                for e in exprs {
                    self.refs_expr(e, word, locs);
                }
            }
            Expr::App { func, args, .. } => {
                self.refs_expr(func, word, locs);
                for a in args {
                    self.refs_expr(a, word, locs);
                }
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                self.refs_expr(cond, word, locs);
                self.refs_expr(then_branch, word, locs);
                if let Some(ref eb) = else_branch {
                    self.refs_expr(eb, word, locs);
                }
            }
            Expr::Pipe { left, right, .. } => {
                self.refs_expr(left, word, locs);
                self.refs_expr(right, word, locs);
            }
            _ => {}
        }
    }

    fn doc_syms_uri(source: &str, uri: &Url) -> Option<Vec<SymbolInformation>> {
        let tokens = Lexer::new(source).lex().ok()?;
        let ast = Parser::new(tokens).parse_module().ok()?;
        let mut syms = Vec::new();
        for decl in &ast.decls {
            Self::collect_syms_uri(decl, &mut syms, uri);
        }
        Some(syms)
    }
    // `deprecated` is a required-but-deprecated field in lsp-types; we always
    // pass `None` (no symbols are marked deprecated), so silence the lint here.
    #[allow(deprecated)]
    fn collect_syms_uri(decl: &crate::ast::Decl, syms: &mut Vec<SymbolInformation>, uri: &Url) {
        use crate::ast::Decl;
        let si = |name: &str, kind: SymbolKind, span: &crate::types::Span| SymbolInformation {
            name: name.to_string(),
            kind,
            tags: None,
            deprecated: None,
            location: Location {
                uri: uri.clone(),
                range: Range {
                    start: Position::new(
                        span.line().saturating_sub(1) as u32,
                        span.column().saturating_sub(1) as u32,
                    ),
                    end: Position::new(
                        span.line().saturating_sub(1) as u32,
                        span.column().saturating_sub(1) as u32,
                    ),
                },
            },
            container_name: None,
        };
        match decl {
            Decl::Function { name, span, .. } => syms.push(si(name, SymbolKind::FUNCTION, span)),
            Decl::Actor {
                name,
                behaviors,
                span,
                ..
            } => {
                syms.push(si(name, SymbolKind::CLASS, span));
                for b in behaviors {
                    syms.push(si(
                        &format!("{}.{}", name, b.name),
                        SymbolKind::METHOD,
                        &b.span,
                    ));
                }
            }
            Decl::Agent { name, span, .. } => syms.push(si(name, SymbolKind::CLASS, span)),
            Decl::Workflow { name, span, .. } => syms.push(si(name, SymbolKind::CLASS, span)),
            Decl::StateMachine {
                name, events, span, ..
            } => {
                syms.push(si(name, SymbolKind::CLASS, span));
                for e in events {
                    syms.push(si(
                        &format!("{}.{}", name, e.name),
                        SymbolKind::EVENT,
                        &e.span,
                    ));
                }
            }
            Decl::TypeAlias { name, span, .. } => syms.push(si(name, SymbolKind::STRUCT, span)),
            Decl::RecordType { name, span, .. } => syms.push(si(name, SymbolKind::STRUCT, span)),
            Decl::VariantType { name, span, .. } => syms.push(si(name, SymbolKind::ENUM, span)),
            Decl::Module {
                name, decls, span, ..
            } => {
                syms.push(si(name, SymbolKind::NAMESPACE, span));
                for d in decls {
                    Self::collect_syms_uri(d, syms, uri);
                }
            }
            Decl::EffectDecl { name, span, .. } => syms.push(si(name, SymbolKind::INTERFACE, span)),
            _ => {}
        }
    }

    fn sig_help(source: &str, position: Position) -> Option<SignatureHelp> {
        let line = position.line as usize;
        let col = position.character as usize;
        let target_line = source.lines().nth(line)?;
        // LSP columns are UTF-16 code units; slice at a char boundary so
        // non-ASCII source cannot panic here.
        let prefix = &target_line[..utf16_col_to_byte(target_line, col)];
        let func_name = prefix
            .trim_end_matches(|c: char| c.is_whitespace() || c == '(' || c == ',')
            .rsplit(|c: char| c.is_whitespace() || c == '(' || c == ',')
            .next()?;
        if func_name.is_empty() || func_name == "let" || func_name == "if" {
            return None;
        }
        let comma_count = prefix.chars().filter(|&c| c == ',').count();
        let tokens = Lexer::new(source).lex().ok()?;
        let ast = Parser::new(tokens).parse_module().ok()?;
        let _ = TypeChecker::new().check_module(&ast).ok()?;
        for decl in &ast.decls {
            if let crate::ast::Decl::Function { name, params, .. } = decl {
                if name == func_name {
                    let label = format!(
                        "fn {}({})",
                        name,
                        params
                            .iter()
                            .map(|p| format!(
                                "{}: {}",
                                p.name,
                                p.ty.as_ref()
                                    .map(|ty| format!("{:?}", ty))
                                    .unwrap_or_else(|| "?".into())
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    return Some(SignatureHelp {
                        signatures: vec![SignatureInformation {
                            label,
                            documentation: None,
                            parameters: None,
                            active_parameter: None,
                        }],
                        active_signature: Some(0),
                        active_parameter: Some(comma_count as u32),
                    });
                }
            }
        }
        None
    }

    fn fmt_source(&self, source: &str) -> String {
        let mut out = String::with_capacity(source.len());
        let mut indent: usize = 0;
        let w: usize = 4;
        let mut prev = false;
        for line in source.lines() {
            let t = line.trim();
            if t.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                prev = true;
                continue;
            }
            let close = t
                .chars()
                .filter(|&c| c == '}' || c == ']' || c == ')')
                .count();
            let open = t
                .chars()
                .filter(|&c| c == '{' || c == '[' || c == '(')
                .count();
            let net = open as isize - close as isize;
            if close > 0 && close >= open {
                indent = indent.saturating_sub((close - open) * w);
            }
            if t.starts_with("in ") || t == "in" {
                indent = indent.saturating_sub(w);
            }
            if !prev && !out.is_empty() {
                out.push('\n');
            }
            for _ in 0..indent / w {
                out.push_str("    ");
            }
            out.push_str(t);
            if net > 0 {
                indent += net as usize * w;
            }
            if (t.ends_with("in") || t.ends_with("then")) && !t.contains('{') {
                indent += w;
            }
            prev = false;
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    fn sem_tokens(&self, source: &str) -> Vec<SemanticToken> {
        // Run capability analysis to find consumed linear variable spans.
        let consumed = self.find_consumed_spans(source);

        let mut tokens = Vec::new();
        let mut pl = 0u32;
        let mut pc = 0u32;
        let mut line = 0u32;
        let mut col = 0u32;
        let bytes = source.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let ch = bytes[i] as char;
            if ch == '\n' {
                line += 1;
                col = 0;
                i += 1;
                continue;
            }
            if ch.is_whitespace() {
                col += 1;
                i += 1;
                continue;
            }
            if ch == '-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                let end = source[i..].find('\n').unwrap_or(source.len() - i);
                tokens.push(SemanticToken {
                    delta_line: line - pl,
                    delta_start: if line == pl { col - pc } else { col },
                    length: end as u32,
                    token_type: 8,
                    token_modifiers_bitset: 0,
                });
                pl = line;
                pc = col;
                col += end as u32;
                i += end;
                continue;
            }
            if ch == '"' {
                if let Some(end) = source[i + 1..].find('"') {
                    let len = (end + 2) as u32;
                    tokens.push(SemanticToken {
                        delta_line: line - pl,
                        delta_start: if line == pl { col - pc } else { col },
                        length: len,
                        token_type: 5,
                        token_modifiers_bitset: 0,
                    });
                    pl = line;
                    pc = col;
                    col += len;
                    i += end + 2;
                    continue;
                }
            }
            if ch.is_ascii_alphabetic() || ch == '_' {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let word = &source[start..i];
                let len = (i - start) as u32;
                let kw = [
                    "let",
                    "fn",
                    "fun",
                    "agent",
                    "workflow",
                    "if",
                    "else",
                    "match",
                    "case",
                    "for",
                    "in",
                    "spawn",
                    "send",
                    "ask",
                    "perform",
                    "handle",
                    "emit",
                    "return",
                    "break",
                    "unit",
                    "nil",
                    "true",
                    "false",
                    "iso",
                    "trn",
                    "ref",
                    "val",
                    "box",
                    "tag",
                    "lineariso",
                    "linear",
                    "type",
                    "effect",
                    "module",
                    "import",
                    "extern",
                    "self",
                    "and",
                    "or",
                    "not",
                    "consume",
                    "recover",
                ];
                let tt: u32 = if kw.contains(&word) { 0 } else { 2 };
                // Apply READONLY modifier if this variable was consumed (linear).
                let mut modifiers: u32 = 0;
                if tt == 2 {
                    if consumed.iter().any(|&(cs, ce)| cs <= start && start < ce) {
                        modifiers = 2; // READONLY bit
                    }
                }
                tokens.push(SemanticToken {
                    delta_line: line - pl,
                    delta_start: if line == pl { col - pc } else { col },
                    length: len,
                    token_type: tt,
                    token_modifiers_bitset: modifiers,
                });
                pl = line;
                pc = col;
                col += len;
                continue;
            }
            if ch.is_ascii_digit() {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'.' {
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let len = (i - start) as u32;
                tokens.push(SemanticToken {
                    delta_line: line - pl,
                    delta_start: if line == pl { col - pc } else { col },
                    length: len,
                    token_type: 6,
                    token_modifiers_bitset: 0,
                });
                pl = line;
                pc = col;
                col += len;
                continue;
            }
            if "=+-*/<>!|&^%.:;{},()[]".contains(ch) {
                tokens.push(SemanticToken {
                    delta_line: line - pl,
                    delta_start: if line == pl { col - pc } else { col },
                    length: 1,
                    token_type: 7,
                    token_modifiers_bitset: 0,
                });
                pl = line;
                pc = col;
                col += 1;
                i += 1;
                continue;
            }
            col += 1;
            i += 1;
        }
        tokens
    }

    /// Run the capability analysis pass and return the byte ranges of
    /// consumed linear/lineariso variable references.
    fn find_consumed_spans(&self, source: &str) -> Vec<(usize, usize)> {
        use crate::ast::Decl;
        use crate::effect_checker::{flatten_decls, CapContext, CapabilityAnalyzer};

        let mut lexer = crate::lexer::Lexer::new(source);
        let tokens = match lexer.lex() {
            Ok(t) => t,
            Err(_) => return vec![],
        };
        let mut parser = crate::parser::Parser::new(tokens);
        let ast = match parser.parse_module() {
            Ok(a) => a,
            Err(_) => return vec![],
        };

        let mut type_checker = crate::typechecker::TypeChecker::new();
        if type_checker.check_module(&ast).is_err() {
            return vec![];
        }

        let flat_decls = flatten_decls(&ast.decls);
        let mut cap_analyzer = CapabilityAnalyzer::new();
        let cap_ctx = CapContext::new();
        for decl in &flat_decls {
            if let Decl::Function { body, params, .. } = decl {
                let mut ctx = cap_ctx.clone();
                for p in params {
                    if let Some(c) = p.cap {
                        ctx = ctx.with_binding(&p.name, c);
                    }
                }
                let _ = cap_analyzer.infer_cap(&ctx, body);
            } else if let Decl::Actor { behaviors, .. } = decl {
                for b in behaviors {
                    let mut ctx = cap_ctx.clone();
                    for p in &b.params {
                        if let Some(c) = p.cap {
                            ctx = ctx.with_binding(&p.name, c);
                        }
                    }
                    let _ = cap_analyzer.infer_cap(&ctx, &b.body);
                }
            }
        }

        cap_analyzer
            .consumed_spans
            .iter()
            .map(|s| (s.start as usize, s.end as usize))
            .collect()
    }

    fn code_actions(source: &str, range: Option<Range>, uri: &Url) -> Option<CodeActionResponse> {
        let mut actions = Vec::new();

        // Extract variable — only when a non-empty selection range is given.
        if let Some(ref sel) = range {
            if sel.start != sel.end {
                if let Some(action) = Self::extract_variable_action(source, sel, uri) {
                    actions.push(action);
                }
            }
        }

        // Existing: add type annotations for `let` bindings without explicit types.
        for (li, line) in source.lines().enumerate() {
            let t = line.trim();
            let ln = li as u32;
            if let Some(pos) = t.find("let ") {
                if !t[pos..].contains(':') {
                    let after = pos + 4;
                    let rest = &t[after..];
                    if let Some(end) = rest.find(|c: char| c == ' ' || c == '=') {
                        let vname = &rest[..end];
                        let Some(eq) = t.find('=') else { continue };
                        let rhs = t[eq + 1..].trim();
                        let ty = if rhs.parse::<i64>().is_ok() {
                            "Int"
                        } else if rhs.starts_with('"') {
                            "String"
                        } else if rhs == "true" || rhs == "false" {
                            "Bool"
                        } else {
                            "a"
                        };
                        let edit = TextEdit {
                            range: Range {
                                start: Position::new(ln, (pos + eq) as u32),
                                end: Position::new(ln, (pos + eq) as u32),
                            },
                            new_text: format!(" : {}", ty),
                        };
                        let mut changes = std::collections::HashMap::new();
                        changes.insert(uri.clone(), vec![edit]);
                        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                            title: format!("Add type annotation ': {}' for '{}'", ty, vname),
                            kind: Some(CodeActionKind::QUICKFIX),
                            diagnostics: None,
                            edit: Some(WorkspaceEdit {
                                changes: Some(changes),
                                ..WorkspaceEdit::default()
                            }),
                            command: None,
                            is_preferred: Some(false),
                            disabled: None,
                            data: None,
                        }));
                    }
                }
            }
        }
        if actions.is_empty() {
            None
        } else {
            Some(actions)
        }
    }

    /// Offer "Extract to variable" for a selected expression.
    ///
    /// Simple version: if the selection is on a line with `= expr`, replace
    /// from `=` to end of line with `let temp = expr in temp`.
    fn extract_variable_action(
        source: &str,
        range: &Range,
        uri: &Url,
    ) -> Option<CodeActionOrCommand> {
        let selected = Self::extract_selected_text(source, range)?;
        let sel = selected.trim();
        if sel.is_empty() || sel.len() == 1 {
            return None;
        }

        let line_num = range.start.line;
        let line = source.lines().nth(line_num as usize)?;

        // Look for `= expr` pattern on this line.
        if let Some(eq_pos) = line.find('=') {
            let rhs = line[eq_pos + 1..].trim();
            if rhs.is_empty() {
                return None;
            }
            let edit = TextEdit {
                range: Range {
                    start: Position::new(line_num, eq_pos as u32 + 1),
                    end: Position::new(line_num, line.len() as u32),
                },
                new_text: format!(" let temp = {} in temp", rhs),
            };
            let mut changes = std::collections::HashMap::new();
            changes.insert(uri.clone(), vec![edit]);
            return Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Extract '{}' to variable", sel),
                kind: Some(CodeActionKind::REFACTOR_EXTRACT),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..WorkspaceEdit::default()
                }),
                is_preferred: Some(true),
                ..CodeAction::default()
            }));
        }
        None
    }

    /// Extract the text covered by an LSP range from the source.
    fn extract_selected_text(source: &str, range: &Range) -> Option<String> {
        let start = Self::position_to_byte_offset(source, &range.start)?;
        let end = Self::position_to_byte_offset(source, &range.end)?;
        if start >= end {
            return None;
        }
        Some(source[start..end].to_string())
    }

    /// Compute folding ranges by tracking brace depth line-by-line.
    fn compute_folding_ranges(source: &str) -> Vec<FoldingRange> {
        let mut ranges = Vec::new();
        let mut stack: Vec<(u32, u32)> = Vec::new();
        let lines: Vec<&str> = source.lines().collect();

        for (li, line) in lines.iter().enumerate() {
            let line_num = li as u32;
            let mut opens = 0i32;
            let mut closes = 0i32;
            let mut in_string = false;
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                let c = bytes[i];
                if in_string {
                    if c == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
                        in_string = false;
                    }
                } else if c == b'"' {
                    in_string = true;
                } else if c == b'{' {
                    opens += 1;
                } else if c == b'}' {
                    closes += 1;
                }
                i += 1;
            }

            for _ in 0..closes {
                if let Some((start, _)) = stack.pop() {
                    if start < line_num {
                        ranges.push(FoldingRange {
                            start_line: start,
                            end_line: line_num,
                            start_character: None,
                            end_character: None,
                            kind: Some(FoldingRangeKind::Region),
                            collapsed_text: None,
                        });
                    }
                }
            }
            for _ in 0..opens {
                stack.push((line_num, stack.len() as u32));
            }
        }

        let eof_line = lines.len().saturating_sub(1) as u32;
        while let Some((start, _)) = stack.pop() {
            if start < eof_line {
                ranges.push(FoldingRange {
                    start_line: start,
                    end_line: eof_line,
                    start_character: None,
                    end_character: None,
                    kind: Some(FoldingRangeKind::Region),
                    collapsed_text: None,
                });
            }
        }

        ranges
    }
    fn compute_diagnostics(source: &str) -> (Vec<Diagnostic>, HashMap<usize, String>) {
        let mut diagnostics = Vec::new();

        // Lex
        let tokens = match Lexer::new(source).lex() {
            Ok(t) => t,
            Err(e) => {
                diagnostics.extend(nu_error_to_diagnostic(e));
                return (diagnostics, HashMap::new());
            }
        };

        // Parse
        let ast = match Parser::new(tokens).parse_module() {
            Ok(a) => a,
            Err(e) => {
                diagnostics.extend(nu_error_to_diagnostic(e));
                return (diagnostics, HashMap::new());
            }
        };

        // Type check — collect all errors so we surface every problem,
        // not just the first. Downstream effect/capability checks still
        // run on the successfully-typed subset.
        let mut tc = TypeChecker::new();
        tc.collect_errors = true;
        let _ = tc.check_module(&ast);
        for err in tc.collected_errors {
            diagnostics.extend(nu_error_to_diagnostic(err));
        }

        // Effect check: same two-pass driver as the CLI frontend
        // (`run_frontend` in main.rs) — `check_module` flattens nested
        // `module {}` decls, registers function rows so callee effects
        // propagate to call sites (pass 1), then enforces declared rows
        // (pass 2). Stops at the first fatal error.
        let mut effect_checker = EffectChecker::new();
        if let Err(e) = effect_checker.check_module(&ast.decls) {
            diagnostics.extend(nu_error_to_diagnostic(e));
        }
        for msg in &effect_checker.diagnostics {
            diagnostics.push(Diagnostic {
                range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                severity: Some(DiagnosticSeverity::WARNING),
                code: None,
                code_description: None,
                source: Some("nulang-effect".to_string()),
                message: msg.clone(),
                related_information: None,
                tags: None,
                data: None,
            });
        }

        // Capability analysis over the flattened declaration list, so
        // functions nested in `module {}` blocks are checked like top-level
        // ones (mirroring the CLI frontend).
        let mut cap_analyzer = CapabilityAnalyzer::new();
        let cap_ctx = CapContext::new();
        for decl in crate::effect_checker::flatten_decls(&ast.decls) {
            match decl {
                crate::ast::Decl::Function { body, params, .. } => {
                    let ctx = cap_ctx.with_params(params);
                    if let Err(e) = cap_analyzer.infer_cap(&ctx, body) {
                        diagnostics.extend(nu_error_to_diagnostic(e));
                    }
                }
                crate::ast::Decl::Actor { behaviors, .. } => {
                    for behavior in behaviors {
                        let ctx = cap_ctx.with_params(&behavior.params);
                        if let Err(e) = cap_analyzer.infer_cap(&ctx, &behavior.body) {
                            diagnostics.extend(nu_error_to_diagnostic(e));
                        }
                    }
                }
                _ => {}
            }
        }
        for msg in &cap_analyzer.diagnostics {
            diagnostics.push(Diagnostic {
                range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                severity: Some(DiagnosticSeverity::WARNING),
                code: None,
                code_description: None,
                source: Some("nulang-capability".to_string()),
                message: msg.clone(),
                related_information: None,
                tags: None,
                data: None,
            });
        }

        let type_map = Self::extract_type_map(source, &ast);
        (diagnostics, type_map)
    }

    /// Walk the AST and extract type information for `let` bindings.
    fn extract_type_map(source: &str, ast: &crate::ast::AstModule) -> HashMap<usize, String> {
        let mut map = HashMap::new();
        for decl in crate::effect_checker::flatten_decls(&ast.decls) {
            Self::extract_decl_types(decl, source, &mut map);
        }
        map
    }

    fn extract_decl_types(decl: &crate::ast::Decl, source: &str, map: &mut HashMap<usize, String>) {
        use crate::ast::Decl;
        match decl {
            Decl::Function { body, .. } => {
                Self::extract_expr_types(body, source, map);
            }
            Decl::Actor {
                behaviors,
                init,
                initializer,
                ..
            } => {
                for b in behaviors {
                    Self::extract_expr_types(&b.body, source, map);
                }
                for (_, e) in init {
                    Self::extract_expr_types(e, source, map);
                }
                if let Some((_, _, body)) = initializer {
                    Self::extract_expr_types(body, source, map);
                }
            }
            Decl::StateMachine {
                entry_hooks,
                exit_hooks,
                ..
            } => {
                for (_, body) in entry_hooks {
                    Self::extract_expr_types(body, source, map);
                }
                for (_, body) in exit_hooks {
                    Self::extract_expr_types(body, source, map);
                }
            }
            Decl::Workflow {
                items, compensate, ..
            } => {
                for item in items {
                    match item {
                        crate::ast::WorkflowItem::Step(s) => {
                            Self::extract_expr_types(&s.body, source, map);
                            if let Some(ref c) = s.compensate {
                                Self::extract_expr_types(c, source, map);
                            }
                        }
                        crate::ast::WorkflowItem::Parallel(steps) => {
                            for s in steps {
                                Self::extract_expr_types(&s.body, source, map);
                                if let Some(ref c) = s.compensate {
                                    Self::extract_expr_types(c, source, map);
                                }
                            }
                        }
                    }
                }
                if let Some(ref c) = compensate {
                    Self::extract_expr_types(c, source, map);
                }
            }
            _ => {}
        }
    }

    fn extract_expr_types(expr: &crate::ast::Expr, source: &str, map: &mut HashMap<usize, String>) {
        use crate::ast::Expr;
        match expr {
            Expr::FString(parts, _) => {
                for part in parts {
                    Self::extract_expr_types(part, source, map);
                }
            }
            Expr::Let {
                name,
                ty,
                value,
                body,
                mutable: _,
                span,
                let_in: _,
            } => {
                if let Some(ref t) = ty {
                    if let Some(pos) = Self::find_ident_position(source, span.start as usize, name)
                    {
                        map.insert(pos, t.to_string());
                    }
                }
                Self::extract_expr_types(value, source, map);
                Self::extract_expr_types(body, source, map);
            }
            Expr::LetRec { value, body, .. } => {
                Self::extract_expr_types(value, source, map);
                Self::extract_expr_types(body, source, map);
            }
            Expr::Block { exprs, .. } => {
                for e in exprs {
                    Self::extract_expr_types(e, source, map);
                }
            }
            Expr::Par { exprs, .. } => {
                for e in exprs {
                    Self::extract_expr_types(e, source, map);
                }
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                Self::extract_expr_types(cond, source, map);
                Self::extract_expr_types(then_branch, source, map);
                if let Some(e) = else_branch {
                    Self::extract_expr_types(e, source, map);
                }
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                Self::extract_expr_types(scrutinee, source, map);
                for (_, guard, body) in arms {
                    if let Some(g) = guard {
                        Self::extract_expr_types(&g, source, map);
                    }
                    Self::extract_expr_types(body, source, map);
                }
            }
            Expr::Lambda { body, .. } => {
                Self::extract_expr_types(body, source, map);
            }
            Expr::App { func, args, .. } => {
                Self::extract_expr_types(func, source, map);
                for a in args {
                    Self::extract_expr_types(a, source, map);
                }
            }
            Expr::Binary { left, right, .. } => {
                Self::extract_expr_types(left, source, map);
                Self::extract_expr_types(right, source, map);
            }
            Expr::Unary { expr, .. } => {
                Self::extract_expr_types(expr, source, map);
            }
            Expr::Tuple(exprs, _) | Expr::Array(exprs, _) => {
                for e in exprs {
                    Self::extract_expr_types(e, source, map);
                }
            }
            Expr::Record(fields, _) => {
                for (_, e) in fields {
                    Self::extract_expr_types(e, source, map);
                }
            }
            Expr::RecordUpdate { base, fields, .. } => {
                Self::extract_expr_types(base, source, map);
                for (_, e) in fields {
                    Self::extract_expr_types(e, source, map);
                }
            }
            Expr::FieldAccess { expr, .. } => {
                Self::extract_expr_types(expr, source, map);
            }
            Expr::Index { arr, idx, .. } => {
                Self::extract_expr_types(arr, source, map);
                Self::extract_expr_types(idx, source, map);
            }
            Expr::Assign { target, value, .. } => {
                Self::extract_expr_types(target, source, map);
                Self::extract_expr_types(value, source, map);
            }
            Expr::Spawn {
                actor_type,
                init,
                positional_args,
                target_node,
                ..
            } => {
                Self::extract_expr_types(actor_type, source, map);
                for (_, e) in init {
                    Self::extract_expr_types(e, source, map);
                }
                if let Some(ref args) = positional_args {
                    for a in args {
                        Self::extract_expr_types(a, source, map);
                    }
                }
                if let Some(ref node) = target_node {
                    Self::extract_expr_types(node, source, map);
                }
            }
            Expr::Send { actor, args, .. } | Expr::Ask { actor, args, .. } => {
                Self::extract_expr_types(actor, source, map);
                for a in args {
                    Self::extract_expr_types(a, source, map);
                }
            }
            Expr::Receive { arms, after, .. } => {
                for (_, _, _, body) in arms {
                    Self::extract_expr_types(body, source, map);
                }
                if let Some((ref timeout, ref body)) = after {
                    Self::extract_expr_types(timeout, source, map);
                    Self::extract_expr_types(body, source, map);
                }
            }
            Expr::Emit { args, .. } | Expr::Perform { args, .. } => {
                for a in args {
                    Self::extract_expr_types(a, source, map);
                }
            }
            Expr::GrainRef { key, .. } => {
                Self::extract_expr_types(key, source, map);
            }
            Expr::Handle { body, handlers, .. } => {
                Self::extract_expr_types(body, source, map);
                for h in handlers {
                    Self::extract_expr_types(&h.body, source, map);
                }
            }
            Expr::Migrate { actor, node, .. } => {
                Self::extract_expr_types(actor, source, map);
                Self::extract_expr_types(node, source, map);
            }
            Expr::CapAnnotate { expr, .. } | Expr::TypeAnnotate { expr, .. } => {
                Self::extract_expr_types(expr, source, map);
            }
            Expr::For { iterable, body, .. }
            | Expr::While {
                cond: iterable,
                body,
                ..
            } => {
                Self::extract_expr_types(iterable, source, map);
                Self::extract_expr_types(body, source, map);
            }
            Expr::Return(opt_expr, _) | Expr::Break(opt_expr, _) => {
                if let Some(ref e) = opt_expr {
                    Self::extract_expr_types(e, source, map);
                }
            }
            Expr::Pipe { left, right, .. } => {
                Self::extract_expr_types(left, source, map);
                Self::extract_expr_types(right, source, map);
            }
            // Leaf nodes: no sub-expressions to walk
            Expr::Literal(..) | Expr::Var(..) | Expr::SelfRef(..) => {}
            // Consume/recover: walk inner expression
            Expr::Consume { expr, .. } => {
                Self::extract_expr_types(expr, source, map);
            }
            Expr::Recover { body, .. } => {
                Self::extract_expr_types(body, source, map);
            }
            Expr::Defer { expr, .. } => {
                Self::extract_expr_types(expr, source, map);
            }
            Expr::Hide { body, .. } | Expr::Seal { body, .. } => {
                Self::extract_expr_types(body, source, map);
            }
            Expr::Panic(..) => {}
            Expr::Resume { value, .. } => {
                Self::extract_expr_types(value, source, map);
            }
        }
    }
    /// Find the byte offset of an identifier within the source, searching
    /// backwards from `near_offset`. Used to locate the position of a
    /// `let`-bound variable name (which appears before the `:` or `=` token
    /// whose span marks `near_offset`).
    fn find_ident_position(source: &str, near_offset: usize, name: &str) -> Option<usize> {
        let search_start = near_offset.saturating_sub(50);
        let search_end = source.len().min(near_offset + name.len());
        let search_region = &source[search_start..search_end];
        // Find the name in the search region, ensuring it's a whole identifier.
        // Collect into a Vec so we can pick the last (nearest to near_offset).
        let matches: Vec<usize> = search_region
            .match_indices(name)
            .filter(|&(pos, _)| {
                let abs_pos = search_start + pos;
                let before = if abs_pos > 0 {
                    source
                        .as_bytes()
                        .get(abs_pos - 1)
                        .map_or(false, |&b| !b.is_ascii_alphanumeric() && b != b'_')
                } else {
                    true
                };
                let after = source
                    .as_bytes()
                    .get(abs_pos + name.len())
                    .map_or(true, |&b| !b.is_ascii_alphanumeric() && b != b'_');
                before && after
            })
            .map(|(pos, _)| search_start + pos)
            .collect();
        matches.into_iter().next_back()
    }

    /// Convert an LSP Position to a byte offset within the source.
    fn position_to_byte_offset(source: &str, position: &Position) -> Option<usize> {
        let line = position.line as usize;
        let lines: Vec<&str> = source.lines().collect();
        let target_line = lines.get(line)?;
        let col_byte = utf16_col_to_byte(target_line, position.character as usize);
        let prev_bytes: usize = lines.iter().take(line).map(|l| l.len() + 1).sum();
        Some(prev_bytes + col_byte)
    }

    /// Find the nearest type entry at or before the given byte offset.
    fn find_type_at_offset(map: &HashMap<usize, String>, offset: usize) -> Option<&String> {
        map.iter()
            .filter(|(&k, _)| k <= offset)
            .max_by_key(|(&k, _)| k)
            .map(|(_, v)| v)
    }

    /// Scan all identifier tokens in the source and return a map from
    /// identifier name to occurrence count. Used by code-lens for
    /// reference counting.
    fn count_all_refs(source: &str) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        let tokens = match Lexer::new(source).lex() {
            Ok(t) => t,
            Err(_) => return counts,
        };
        for tok in &tokens {
            let name = match &tok.kind {
                crate::lexer::TokenKind::Ident(s) | crate::lexer::TokenKind::UpperIdent(s) => s,
                _ => continue,
            };
            *counts.entry(name.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// Extract document links from import declarations in the source.
    /// Handles `import stdlib::<mod>` (→ file://<repo>/src/stdlib/<mod>.nula)
    /// and `import "<path>"` (→ resolved relative to the document URI).
    fn extract_document_links(source: &str, doc_uri: &Url) -> Vec<DocumentLink> {
        let mut links = Vec::new();

        for (line_idx, line) in source.lines().enumerate() {
            let trimmed = line.trim();
            let path_str = if let Some(rest) = trimmed.strip_prefix("import stdlib::") {
                let mod_name = rest.split_whitespace().next().unwrap_or(rest);
                let mod_name = mod_name.trim_end_matches(';');
                let repo_root = env!("CARGO_MANIFEST_DIR");
                format!("file://{}/src/stdlib/{}.nula", repo_root, mod_name)
            } else if let Some(rest) = trimmed.strip_prefix("import \"") {
                // Extract the path up to the closing quote
                let path = rest.split('"').next().unwrap_or(rest);
                let path = path.trim_end_matches(';');
                // Resolve relative to document URI
                if let Ok(mut base) = doc_uri.to_file_path() {
                    base.pop(); // remove filename
                    base.push(path);
                    if let Ok(resolved) = Url::from_file_path(&base) {
                        resolved.to_string()
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            } else {
                continue;
            };

            let target = match Url::parse(&path_str) {
                Ok(u) => u,
                Err(_) => continue,
            };

            // Compute range for the import keyword
            let start_col = line.find("import").unwrap_or(0) as u32;
            let end_col = line.len() as u32;
            links.push(DocumentLink {
                range: Range {
                    start: Position::new(line_idx as u32, start_col),
                    end: Position::new(line_idx as u32, end_col),
                },
                target: Some(target),
                tooltip: None,
                data: None,
            });
        }

        links
    }

    /// Extract a `///` doc comment immediately preceding a given source line.
    /// Returns the concatenated comment text with `///` prefixes stripped,
    /// or None if there is no doc comment.
    fn extract_doc_comment(source: &str, decl_line_0: usize) -> Option<String> {
        let lines: Vec<&str> = source.lines().collect();
        if decl_line_0 == 0 {
            return None;
        }
        let mut doc_lines = Vec::new();
        let mut idx = decl_line_0.saturating_sub(1);
        loop {
            let line = lines.get(idx)?;
            let trimmed = line.trim();
            if let Some(doc) = trimmed.strip_prefix("///") {
                doc_lines.push(if doc.starts_with(' ') { &doc[1..] } else { doc });
            } else {
                break;
            }
            if idx == 0 {
                break;
            }
            idx -= 1;
        }
        if doc_lines.is_empty() {
            return None;
        }
        doc_lines.reverse();
        Some(doc_lines.join("\n"))
    }
}

/// Convert a `NuError` into zero or more LSP `Diagnostic`s.
/// For `NuError::Multiple`, each sub-error becomes its own diagnostic.
fn nu_error_to_diagnostic(err: NuError) -> Vec<Diagnostic> {
    match err {
        NuError::Multiple(errors) => {
            let mut diags = Vec::new();
            for err in errors {
                diags.extend(nu_error_to_diagnostic(err));
            }
            diags
        }
        other => vec![single_diagnostic(other)],
    }
}

/// Build a single LSP Diagnostic from a non-Multiple NuError.
fn single_diagnostic(err: NuError) -> Diagnostic {
    let (message, start_line, start_col, end_line, end_col) = match err {
        NuError::LexError { msg, span }
        | NuError::ParseError { msg, span, .. }
        | NuError::TypeError { msg, span, .. }
        | NuError::EffectError { msg, span, .. }
        | NuError::CapError { msg, span, .. }
        | NuError::FFIError { msg, span }
        | NuError::NotYetImplemented { feature: msg, span } => (
            msg,
            span.line(),
            span.column(),
            span.end_line(),
            span.end_column(),
        ),
        NuError::RuntimeError { msg, span }
        | NuError::VMError { msg, span }
        | NuError::PythonError { msg, span }
        | NuError::PackageError { msg, span } => (
            msg,
            span.line(),
            span.column(),
            span.end_line(),
            span.end_column(),
        ),
        NuError::Suspended(kind) => (format!("VM suspended: {}", kind), 1, 1, 1, 1),
        NuError::Multiple(_) => unreachable!("Multiple handled by caller"),
    };

    let start = Position::new(
        start_line.saturating_sub(1) as u32,
        start_col.saturating_sub(1) as u32,
    );
    let end = Position::new(
        end_line.saturating_sub(1) as u32,
        end_col.saturating_sub(1) as u32,
    );

    Diagnostic {
        range: Range::new(start, end),
        severity: Some(DiagnosticSeverity::ERROR),
        code: None,
        code_description: None,
        source: Some("nulang".to_string()),
        message,
        related_information: None,
        tags: None,
        data: None,
    }
}

// ---------------------------------------------------------------------------
// Inlay Hint Engine
// ---------------------------------------------------------------------------

/// Generates inlay hints for Nulang source code.
///
/// Parses the source, runs type inference, and produces inlay hints
/// showing inferred types, capabilities, and effect annotations.
pub struct InlayHintEngine<'a> {
    source: &'a str,
}

/// A type annotation to display as an inlay hint.
#[derive(Debug, Clone)]
pub struct TypeAnnotation {
    pub line: u32,
    pub character: u32,
    pub label: String,
    pub kind: AnnotationKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationKind {
    Type,       // : Int, : Float, etc.
    Capability, // : iso, : val, etc.
    Effect,     // [IO], [FileSystem], etc.
}

impl<'a> InlayHintEngine<'a> {
    pub fn new(source: &'a str) -> Self {
        InlayHintEngine { source }
    }

    /// Generate inlay hints for the source file.
    pub fn generate_inlay_hints(&self) -> Vec<InlayHint> {
        let annotations = self.collect_annotations();
        annotations
            .into_iter()
            .map(|a| self.annotation_to_inlay(a))
            .collect()
    }

    fn collect_annotations(&self) -> Vec<TypeAnnotation> {
        // Parse and typecheck the source.
        let mut lexer = crate::lexer::Lexer::new(self.source);
        let tokens = match lexer.lex() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let ast = match Parser::new(tokens).parse_module() {
            Ok(a) => a,
            Err(_) => return Vec::new(),
        };
        let mut tc = TypeChecker::new();
        if tc.check_module(&ast).is_err() {
            return Vec::new();
        }
        let mut ec = EffectChecker::new();
        let decl_refs: Vec<&crate::ast::Decl> = ast.decls.iter().collect();
        if ec.register_function_rows(&decl_refs).is_err() {
            return Vec::new();
        }

        let mut annotations = Vec::new();

        for decl in &ast.decls {
            match decl {
                crate::ast::Decl::Function {
                    name,
                    params,
                    ret_type,
                    effect,
                    span,
                    ..
                } => {
                    let func_ty = match tc.inferred_decl_types.get(name) {
                        Some(t) => t,
                        None => continue,
                    };
                    let param_types: Vec<&crate::types::Type> = match func_ty {
                        crate::types::Type::Function { param, .. } => match param.as_ref() {
                            crate::types::Type::Tuple(types) => types.iter().collect(),
                            t => vec![t],
                        },
                        _ => continue,
                    };
                    let line = span.line().saturating_sub(1) as u32;
                    let source_line = self.source.lines().nth(line as usize).unwrap_or("");
                    if let Some(lparen) = source_line.find('(') {
                        let mut col = (lparen + 1) as u32;
                        for (i, p) in params.iter().enumerate() {
                            let pname = &p.name;
                            let ptype_ann = &p.ty;
                            let pname_len = pname.len() as u32;
                            if ptype_ann.is_none() && i < param_types.len() {
                                annotations.push(TypeAnnotation {
                                    line,
                                    character: col + pname_len,
                                    label: format!(": {}", type_to_string(param_types[i])),
                                    kind: AnnotationKind::Type,
                                });
                            }
                            col += pname_len + 2;
                        }
                    }
                    if ret_type.is_none() {
                        if let crate::types::Type::Function { ret, .. } = func_ty {
                            if let Some(rparen) = source_line.find(')') {
                                annotations.push(TypeAnnotation {
                                    line,
                                    character: rparen as u32 + 1,
                                    label: format!(" -> {}", type_to_string(ret)),
                                    kind: AnnotationKind::Type,
                                });
                            }
                        }
                    }
                    // Show effect row hints on functions.
                    if effect.is_none() {
                        if let Some(row) = ec.function_row(name) {
                            let is_empty = match row {
                                crate::types::EffectRow::Closed(effects) => effects.is_empty(),
                                crate::types::EffectRow::Open(effects, _) => effects.is_empty(),
                            };
                            if !is_empty {
                                let effect_str = row.to_string();
                                let pos = source_line
                                    .rfind(|c: char| c == ')' || c == '!')
                                    .map_or(0, |p| p as u32 + 1);
                                annotations.push(TypeAnnotation {
                                    line,
                                    character: pos,
                                    label: format!(" ! {}", effect_str),
                                    kind: AnnotationKind::Effect,
                                });
                            }
                        }
                    }
                }
                crate::ast::Decl::Signal { name, span, .. } => {
                    if let Some(ty) = tc.inferred_decl_types.get(name) {
                        let line = span.line().saturating_sub(1) as u32;
                        let col = (span.column() + name.len()) as u32;
                        annotations.push(TypeAnnotation {
                            line,
                            character: col,
                            label: format!(": {}", type_to_string(ty)),
                            kind: AnnotationKind::Type,
                        });
                    }
                }
                crate::ast::Decl::LetBinding {
                    name,
                    type_ann,
                    span,
                    ..
                } => {
                    if type_ann.is_none() {
                        if let Some(ty) = tc.inferred_decl_types.get(name) {
                            let line = span.line().saturating_sub(1) as u32;
                            let col = (span.column() + name.len()) as u32;
                            annotations.push(TypeAnnotation {
                                line,
                                character: col,
                                label: format!(": {}", type_to_string(ty)),
                                kind: AnnotationKind::Type,
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        annotations
    }

    /// Convert a TypeAnnotation to an LSP InlayHint.
    fn annotation_to_inlay(&self, ann: TypeAnnotation) -> InlayHint {
        InlayHint {
            position: Position {
                line: ann.line,
                character: ann.character,
            },
            label: InlayHintLabel::String(ann.label),
            kind: Some(match ann.kind {
                AnnotationKind::Type => InlayHintKind::TYPE,
                AnnotationKind::Capability => InlayHintKind::PARAMETER,
                AnnotationKind::Effect => InlayHintKind::TYPE,
            }),
            text_edits: None,
            tooltip: Some(InlayHintTooltip::String(match ann.kind {
                AnnotationKind::Type => "Inferred type".to_string(),
                AnnotationKind::Capability => "Reference capability".to_string(),
                AnnotationKind::Effect => "Effect row".to_string(),
            })),
            padding_left: Some(false),
            padding_right: Some(false),
            data: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Completion Engine
// ---------------------------------------------------------------------------

/// Generates completion items for Nulang source code.
///
/// The engine is intentionally lightweight: it offers keywords, built-in
/// effect names, and top-level function names extracted from the current
/// document. It does not require a full parse or typecheck.
pub struct CompletionEngine<'a> {
    source: &'a str,
    function_names: Vec<String>,
    /// (name, [param_names]) for snippet generation.
    function_params: Vec<(String, Vec<String>)>,
    variant_names: Vec<String>,
    type_names: Vec<String>,
    /// (actor_name, [field_names]).
    actor_state_fields: Vec<(String, Vec<String>)>,
    /// (type_name, [field_names]).
    record_type_fields: Vec<(String, Vec<String>)>,
    /// (behavior_name, [param_names]) for snippet generation.
    behavior_params: Vec<(String, Vec<String>)>,
}

impl<'a> CompletionEngine<'a> {
    /// Nulang language keywords with markdown documentation.
    const KEYWORDS: &'static [(&'static str, &'static str)] = &[
        (
            "fn",
            "Declare a function.\n\n```nulang\nfn add(x: Int, y: Int) -> Int { x + y }\n```",
        ),
        (
            "let",
            "Bind a value to a name.\n\n```nulang\nlet x = 42\nlet y: Int = x + 1\n```",
        ),
        (
            "if",
            "Conditional expression.\n\n```nulang\nif x > 0 { \"positive\" } else { \"non-positive\" }\n```",
        ),
        (
            "else",
            "Else branch of a conditional.\n\n```nulang\nif x > 0 { \"positive\" } else { \"non-positive\" }\n```",
        ),
        (
            "match",
            "Pattern match expression.\n\n```nulang\nmatch x {\n  | Some(v) => v\n  | None => 0\n}\n```",
        ),
        (
            "effect",
            "Declare an effect.\n\n```nulang\neffect MyEffect {\n  op1: Int -> String\n}\n```",
        ),
        (
            "actor",
            "Declare an actor.\n\n```nulang\nactor Counter {\n  state count = 0\n  behavior inc() { self.count = self.count + 1 }\n}\n```",
        ),
        (
            "state_machine",
            "Declare a state machine.\n\n```nulang\nstate_machine Door {\n  state Closed\n  event open(): Open\n}\n```",
        ),
        (
            "type",
            "Declare a type alias, record, or variant.\n\n```nulang\ntype Point = { x: Int, y: Int }\ntype Option[T] = Some(T) | None\n```",
        ),
        (
            "module",
            "Declare a module.\n\n```nulang\nmodule MyModule {\n  fn foo() { 42 }\n}\n```",
        ),
        (
            "import",
            "Import a module.\n\n```nulang\nimport \"path/to/file.nula\"\nimport stdlib::json\n```",
        ),
        (
            "handle",
            "Handle effects.\n\n```nulang\nhandle perform IO.print(\"x\") with {\n  | IO.print(s) => { /* custom logic */ }\n  | return(x) => x\n}\n```",
        ),
        (
            "perform",
            "Perform an effect operation.\n\n```nulang\nperform IO.print(\"Hello\")\nperform Http.get(\"https://example.com\")\n```",
        ),
        (
            "resume",
            "Resume a handled effect continuation.\n\n```nulang\nhandle perform MyEffect.op() with {\n  | MyEffect.op() => resume(\"result\")\n}\n```",
        ),
        (
            "return",
            "Return a value from a function.\n\n```nulang\nfn foo() -> Int { return 42 }\n```",
        ),
        ("true", "Boolean `true` literal."),
        ("false", "Boolean `false` literal."),
        ("nil", "Nil / null value."),
        ("unit", "Unit value (void)."),
        ("behavior", "Define a behavior inside an actor.\n\n```nulang\nactor Foo {\n  behavior bar() { 42 }\n}\n```"),
        ("state", "Define state fields inside an actor.\n\n```nulang\nactor Foo {\n  state count = 0\n}\n```"),
        ("spawn", "Spawn a new actor.\n\n```nulang\nspawn Counter\nspawn@node Counter\n```"),
        ("send", "Send a fire-and-forget message to an actor (Phase 1.1).\n\n```nulang\nworker <- Process(data)\n```"),
        ("receive", "Block the current actor to receive a message.\n\n```nulang\nreceive { | Msg(payload) => payload }\n```"),
        ("workflow", "Declare a durable workflow.\n\n```nulang\nworkflow Name {\n  step one { ... }\n}\n```"),
        ("self", "Reference to the current actor or context.\n\n```nulang\nself.count\nself.field_name\n```"),
        ("with", "Provide a handler for an effect block.\n\n```nulang\nhandle expr with handler_var\nhandle expr with { | op(x) => body }\n```"),
        ("crdt", "Declare a CRDT-typed state field.\n\n```nulang\nactor Foo {\n  state crdt gcounter count = 0\n}\n```"),
    ];

    /// Built-in effect names with markdown documentation.
    const EFFECTS: &'static [(&'static str, &'static str)] = &[
        ("IO", "Input/Output operations.\n\n```nulang\neffect IO {\n  print(s: String) -> Unit\n  read() -> String\n}\n```"),
        ("Http", "Network operations.\n\n```nulang\neffect Http {\n  get(url: String) -> String\n  post(url: String, body: String) -> String\n}\n```"),
        ("FS", "File system operations.\n\n```nulang\neffect FS {\n  read(path: String) -> String\n  write(path: String, content: String) -> Unit\n}\n```"),
        ("Inference", "AI inference operations.\n\n```nulang\neffect Inference {\n  ask(prompt: String) -> String\n}\n```"),
        ("Random", "Random value generation.\n\n```nulang\neffect Random {\n  int(lo: Int, hi: Int) -> Int\n}\n```"),
        ("Time", "Wall-clock read.\n\n```nulang\neffect Time {\n  now() -> Int\n}\n```\n\nNote: `Timer.sleep`/`Timer.after` are a separate effect and are a silent no-op outside a workflow/actor context."),
        ("Actor", "Actor system operations.\n\n```nulang\neffect Actor {\n  self() -> ActorId\n  link(target: ActorId) -> Unit\n  monitor(target: ActorId) -> Unit\n  exit(reason: Int) -> Unit\n}\n```"),
        ("Provider", "Service provider discovery.\n\n```nulang\neffect Provider {\n  resolve(service: String) -> Url\n}\n```"),
        ("Env", "Environment variable access.\n\n```nulang\neffect Env {\n  get(name: String) -> String\n}\n```"),
        ("System", "System-level operations.\n\n```nulang\neffect System {\n  arg(n: Int) -> String\n}\n```"),
    ];

    /// Reference capability keywords with markdown documentation.
    const CAPABILITIES: &'static [(&'static str, &'static str)] = &[
        ("iso", "Isolated — unique, mutable reference. Cannot be shared or copied.\n\n```nulang\nlet iso x: iso X = X()\n```"),
        ("ref", "Reference — shared, mutable reference. Multiple readers, one writer.\n\n```nulang\nlet ref x: ref X = X()\n```"),
        ("val", "Value — immutable, shareable reference.\n\n```nulang\nlet val x: val X = X()\n```"),
        ("box", "Box — opaque, read-only reference.\n\n```nulang\nlet box x: box X = X()\n```"),
        ("tag", "Tag — opaque, shareable, usable only for identity comparison.\n\n```nulang\nlet tag x: tag X = X()\n```"),
        ("trn", "Transition — write-unique reference that can become `val`.\n\n```nulang\nlet trn x: trn X = X()\n```"),
        ("linear", "Linear — must be used exactly once.\n\n```nulang\nlet linear x: linear X = X()  // must consume x\n```"),
        ("lineariso", "Linear + Isolated — combination of linear and iso.\n\n```nulang\nlet lineariso x: lineariso X = X()  // must consume, can't share\n```"),
    ];

    /// Known stdlib modules for import path completion.
    const STDLIB_MODULES: &'static [&'static str] = &[
        "core", "set", "string", "list", "map", "math", "json", "http", "test",
    ];

    pub fn new(source: &'a str) -> Self {
        CompletionEngine {
            source,
            function_names: Vec::new(),
            function_params: Vec::new(),
            variant_names: Vec::new(),
            type_names: Vec::new(),
            actor_state_fields: Vec::new(),
            record_type_fields: Vec::new(),
            behavior_params: Vec::new(),
        }
    }

    /// Populate cached names and parameter info from a parsed AST.
    pub fn set_ast_info(&mut self, ast: &crate::ast::AstModule) {
        for decl in &ast.decls {
            match decl {
                crate::ast::Decl::Function { name, params, .. } => {
                    self.function_names.push(name.clone());
                    let param_names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                    self.function_params.push((name.clone(), param_names));
                }
                crate::ast::Decl::Actor {
                    name,
                    behaviors,
                    state_fields,
                    ..
                } => {
                    self.type_names.push(name.clone());
                    let field_names: Vec<String> =
                        state_fields.iter().map(|(n, _, _, _)| n.clone()).collect();
                    self.actor_state_fields.push((name.clone(), field_names));
                    for b in behaviors {
                        self.function_names.push(b.name.clone());
                        let bparams: Vec<String> =
                            b.params.iter().map(|p| p.name.clone()).collect();
                        self.behavior_params.push((b.name.clone(), bparams));
                    }
                }
                crate::ast::Decl::VariantType { name, variants, .. } => {
                    self.type_names.push(name.clone());
                    for (vname, _) in variants {
                        self.variant_names.push(vname.clone());
                    }
                }
                crate::ast::Decl::TypeAlias { name, .. } => {
                    self.type_names.push(name.clone());
                }
                crate::ast::Decl::RecordType { name, fields, .. } => {
                    self.type_names.push(name.clone());
                    let rfields: Vec<String> = fields.iter().map(|(n, _)| n.clone()).collect();
                    self.record_type_fields.push((name.clone(), rfields));
                }
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------------
    // Public entry point
    // ------------------------------------------------------------------

    /// Return completion items at the given LSP position.
    pub fn complete(&self, position: Position, document_dir: Option<&Path>) -> Vec<CompletionItem> {
        let offset = self.position_to_offset(position);

        // 1. Field access: `self.` or `expr.`
        if let Some(ident) = self.field_access_before(offset) {
            let prefix = self.prefix_at(offset);
            return self.complete_field_access(&ident, &prefix);
        }

        // 2. Import path: `import "` or `import stdlib::`
        if let Some(path_prefix) = self.import_context(offset) {
            return self.complete_import_path(&path_prefix, document_dir);
        }

        // 3. Regular identifier completion
        self.complete_ident(position)
    }

    // ------------------------------------------------------------------
    // Regular identifier completions
    // ------------------------------------------------------------------

    fn complete_ident(&self, position: Position) -> Vec<CompletionItem> {
        let offset = self.position_to_offset(position);
        let prefix = self.prefix_at(offset);
        let prefix_lower = prefix.to_lowercase();

        let mut items = Vec::new();

        // Local let bindings — highest priority (sort "0").
        for name in self.local_bindings() {
            if name.to_lowercase().starts_with(&prefix_lower) {
                items.push(CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::VARIABLE),
                    detail: Some("local binding".to_string()),
                    sort_text: Some(format!("0_{}", name)),
                    ..CompletionItem::default()
                });
            }
        }

        // Cached function names from AST — with snippet for params (sort "1").
        for name in &self.function_names {
            if name.to_lowercase().starts_with(&prefix_lower) {
                items.push(self.make_function_item(name));
            }
        }

        // Regex-based function names (fallback) — lower priority (sort "1b").
        for name in self.top_level_functions() {
            let nl = name.to_lowercase();
            if nl.starts_with(&prefix_lower)
                && !self.function_names.iter().any(|n| n.to_lowercase() == nl)
            {
                items.push(CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::FUNCTION),
                    detail: Some("function".to_string()),
                    sort_text: Some(format!("1b_{}", name)),
                    ..CompletionItem::default()
                });
            }
        }

        // Cached type names (sort "2").
        for name in &self.type_names {
            if name.to_lowercase().starts_with(&prefix_lower) {
                items.push(CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::CLASS),
                    detail: Some("type".to_string()),
                    sort_text: Some(format!("2_{}", name)),
                    ..CompletionItem::default()
                });
            }
        }

        // Cached variant constructors (sort "3").
        for name in &self.variant_names {
            if name.to_lowercase().starts_with(&prefix_lower) {
                items.push(CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::CONSTRUCTOR),
                    detail: Some("variant".to_string()),
                    sort_text: Some(format!("3_{}", name)),
                    ..CompletionItem::default()
                });
            }
        }

        // Keywords — with markdown documentation (sort "4").
        for &(kw, doc) in Self::KEYWORDS {
            if kw.to_lowercase().starts_with(&prefix_lower) {
                items.push(CompletionItem {
                    label: kw.to_string(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    documentation: Some(Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: doc.to_string(),
                    })),
                    sort_text: Some(format!("4_{}", kw)),
                    ..CompletionItem::default()
                });
            }
        }

        // Built-in effects — with markdown documentation (sort "5").
        for &(eff, doc) in Self::EFFECTS {
            let eff_lower = eff.to_lowercase();
            if eff_lower.starts_with(prefix_lower.as_str()) {
                items.push(CompletionItem {
                    label: eff.to_string(),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    detail: Some("built-in effect".to_string()),
                    documentation: Some(Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: doc.to_string(),
                    })),
                    sort_text: Some(format!("5_{}", eff)),
                    ..CompletionItem::default()
                });
            }
        }

        // Capability keywords — with markdown documentation (sort "5b").
        for &(cap, doc) in Self::CAPABILITIES {
            if cap.to_lowercase().starts_with(&prefix_lower) {
                items.push(CompletionItem {
                    label: cap.to_string(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    detail: Some("capability annotation".to_string()),
                    documentation: Some(Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: doc.to_string(),
                    })),
                    sort_text: Some(format!("5b_{}", cap)),
                    ..CompletionItem::default()
                });
            }
        }

        items
    }

    /// Build a function/behavior completion item with a snippet when params
    /// are known.
    fn make_function_item(&self, name: &str) -> CompletionItem {
        // Check behavior params first.
        if let Some((_, bparams)) = self.behavior_params.iter().find(|(n, _)| n == name) {
            let snippet = if bparams.is_empty() {
                name.to_string()
            } else {
                let placeholders: Vec<String> = bparams
                    .iter()
                    .enumerate()
                    .map(|(i, p)| format!("${{{}:{}}}", i + 1, p))
                    .collect();
                format!("{} {}", name, placeholders.join(" "))
            };
            return CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some("behavior".to_string()),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                sort_text: Some(format!("0b_{}", name)),
                ..CompletionItem::default()
            };
        }

        // Check function params.
        if let Some((_, fparams)) = self.function_params.iter().find(|(n, _)| n == name) {
            let snippet = if fparams.is_empty() {
                format!("{}()", name)
            } else {
                let placeholders: Vec<String> = fparams
                    .iter()
                    .enumerate()
                    .map(|(i, p)| format!("${{{}:{}}}", i + 1, p))
                    .collect();
                format!("{}({})", name, placeholders.join(", "))
            };
            return CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some("function".to_string()),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                sort_text: Some(format!("1_{}", name)),
                ..CompletionItem::default()
            };
        }

        // No params known — plain completion.
        CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some("function".to_string()),
            sort_text: Some(format!("1_{}", name)),
            ..CompletionItem::default()
        }
    }

    // ------------------------------------------------------------------
    // Field access completions (`self.` or `expr.`)
    // ------------------------------------------------------------------

    /// Check whether the cursor is positioned after `ident.` — return the
    /// identifier before the dot.
    fn field_access_before(&self, offset: usize) -> Option<String> {
        let bytes = self.source.as_bytes();
        // Walk back from offset past any identifier characters (the partial
        // field name).
        let mut pos = offset;
        while pos > 0 {
            let b = bytes[pos - 1];
            if b.is_ascii_alphanumeric() || b == b'_' {
                pos -= 1;
            } else {
                break;
            }
        }
        // Expect a dot right before the identifier.
        if pos == 0 || bytes[pos - 1] != b'.' {
            return None;
        }
        // Walk back from before the dot to collect the object identifier.
        let dot_pos = pos - 1;
        let mut ident_end = dot_pos;
        while ident_end > 0 && bytes[ident_end - 1].is_ascii_whitespace() {
            ident_end -= 1;
        }
        let mut ident_start = ident_end;
        while ident_start > 0 {
            let b = bytes[ident_start - 1];
            if b.is_ascii_alphanumeric() || b == b'_' {
                ident_start -= 1;
            } else {
                break;
            }
        }
        if ident_start < ident_end {
            Some(self.source[ident_start..ident_end].to_string())
        } else {
            None
        }
    }

    /// Offer field completions for the given object identifier.
    fn complete_field_access(&self, ident: &str, prefix: &str) -> Vec<CompletionItem> {
        let prefix_lower = prefix.to_lowercase();
        let mut items = Vec::new();

        // `self.` → actor state fields.
        if ident == "self" {
            for (actor_name, fields) in &self.actor_state_fields {
                for f in fields {
                    if f.to_lowercase().starts_with(&prefix_lower) {
                        items.push(CompletionItem {
                            label: f.clone(),
                            kind: Some(CompletionItemKind::FIELD),
                            detail: Some(format!("state field of {}", actor_name)),
                            sort_text: Some(format!("0_{}", f)),
                            ..CompletionItem::default()
                        });
                    }
                }
            }
            return items;
        }

        // Type-name-based record field access.
        for (type_name, fields) in &self.record_type_fields {
            if type_name.to_lowercase() == ident.to_lowercase() {
                for f in fields {
                    if f.to_lowercase().starts_with(&prefix_lower) {
                        items.push(CompletionItem {
                            label: f.clone(),
                            kind: Some(CompletionItemKind::FIELD),
                            detail: Some(format!("field of {}", type_name)),
                            sort_text: Some(format!("0_{}", f)),
                            ..CompletionItem::default()
                        });
                    }
                }
            }
        }

        items
    }

    // ------------------------------------------------------------------
    // Import path completions
    // ------------------------------------------------------------------

    /// Check whether the cursor is inside an import path string or after
    /// `stdlib::`, returning the prefix typed so far.
    fn import_context(&self, offset: usize) -> Option<String> {
        let line = self.current_line(offset)?;
        let line_trimmed = line.trim();

        // `import "path"`
        if let Some(rest) = line_trimmed.strip_prefix("import \"") {
            // Compute how far into the path the cursor is.
            let col_in_trimmed = offset.saturating_sub(
                self.source[..offset]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0)
                    + (line.len() - line_trimmed.len()),
            );
            // `import "` is 8 chars.
            if col_in_trimmed >= 8 {
                let path_so_far = &rest[..(col_in_trimmed - 8).min(rest.len())];
                return Some(path_so_far.to_string());
            }
            return Some(String::new());
        }

        // `import stdlib::mod`
        if let Some(rest) = line_trimmed.strip_prefix("import stdlib::") {
            let col_in_trimmed = offset.saturating_sub(
                self.source[..offset]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0)
                    + (line.len() - line_trimmed.len()),
            );
            let prefix_len = "import stdlib::".len();
            if col_in_trimmed >= prefix_len {
                let mod_so_far = &rest[..(col_in_trimmed - prefix_len).min(rest.len())];
                return Some(format!("stdlib::{}", mod_so_far));
            }
            return Some("stdlib::".to_string());
        }

        None
    }

    /// Offer import path completions.
    fn complete_import_path(
        &self,
        prefix: &str,
        document_dir: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let mut items = Vec::new();

        // `stdlib::` prefix → suggest known stdlib modules.
        if let Some(mod_prefix) = prefix.strip_prefix("stdlib::") {
            let mod_prefix_lower = mod_prefix.to_lowercase();
            for &mod_name in Self::STDLIB_MODULES {
                if mod_name.to_lowercase().starts_with(&mod_prefix_lower) {
                    let full = format!("stdlib::{}", mod_name);
                    items.push(CompletionItem {
                        label: full.clone(),
                        kind: Some(CompletionItemKind::MODULE),
                        detail: Some("stdlib module".to_string()),
                        insert_text: Some(full.clone()),
                        sort_text: Some(format!("0_{}", full)),
                        ..CompletionItem::default()
                    });
                }
            }
            return items;
        }

        // For bare `import "` — suggest stdlib:: paths and local .nula files.
        for &mod_name in Self::STDLIB_MODULES {
            let full = format!("stdlib::{}", mod_name);
            if full.to_lowercase().starts_with(&prefix.to_lowercase()) {
                items.push(CompletionItem {
                    label: full.clone(),
                    kind: Some(CompletionItemKind::MODULE),
                    detail: Some("stdlib module".to_string()),
                    insert_text: Some(full.clone()),
                    sort_text: Some(format!("0_{}", full)),
                    ..CompletionItem::default()
                });
            }
        }

        // Scan for local .nula files when a document dir is known.
        if let Some(dir) = document_dir {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map_or(false, |e| e == "nula") {
                        if let Some(fname) = path.file_stem().and_then(|s| s.to_str()) {
                            let rel = format!("./{}", fname);
                            if rel.to_lowercase().starts_with(&prefix.to_lowercase()) {
                                items.push(CompletionItem {
                                    label: rel.clone(),
                                    kind: Some(CompletionItemKind::FILE),
                                    detail: Some("local module".to_string()),
                                    insert_text: Some(format!("./{}.nula", fname)),
                                    sort_text: Some(format!("1_{}", rel)),
                                    ..CompletionItem::default()
                                });
                            }
                        }
                    }
                }
            }
        }

        items
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Get the text of the line containing `offset`.
    fn current_line(&self, offset: usize) -> Option<&str> {
        let start = self.source[..offset]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let end = self.source[offset..]
            .find('\n')
            .map(|i| offset + i)
            .unwrap_or(self.source.len());
        Some(&self.source[start..end])
    }

    /// Extract all `let` binding names from the source (simple heuristic
    /// for local variable completions).
    fn local_bindings(&self) -> Vec<String> {
        let mut names = Vec::new();
        for line in self.source.lines() {
            let trimmed = line.trim_start();
            if let Some(after_let) = trimmed.strip_prefix("let ") {
                if let Some(name) = after_let
                    .split(|c: char| c.is_whitespace() || c == ':' || c == '=')
                    .next()
                {
                    let name = name.trim();
                    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        names.push(name.to_string());
                    }
                }
            }
        }
        names
    }

    /// Convert an LSP position to a byte offset in the source.
    fn position_to_offset(&self, position: Position) -> usize {
        let mut offset = 0usize;
        for (line_idx, line) in self.source.lines().enumerate() {
            if line_idx as u32 == position.line {
                return offset + utf16_col_to_byte(line, position.character as usize);
            }
            offset += line.len() + 1; // +1 for newline
        }
        self.source.len()
    }

    /// Extract the identifier fragment the user has typed so far.
    fn prefix_at(&self, offset: usize) -> String {
        let bytes = self.source.as_bytes();
        let mut offset = offset.min(bytes.len());
        while offset > 0 && !self.source.is_char_boundary(offset) {
            offset -= 1;
        }
        let mut start = offset;
        while start > 0 {
            let prev = bytes[start - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                start -= 1;
            } else {
                break;
            }
        }
        self.source[start..offset].to_string()
    }

    /// Extract top-level function names from the document.
    fn top_level_functions(&self) -> Vec<String> {
        let mut names = Vec::new();
        for line in self.source.lines() {
            let trimmed = line.trim_start();
            if let Some(after_fun) = trimmed.strip_prefix("fun ") {
                let name = after_fun.split_whitespace().next().unwrap_or("").trim();
                let name = name.split('(').next().unwrap_or("").trim();
                if !name.is_empty() && !name.contains(':') {
                    names.push(name.to_string());
                }
            }
        }
        names
    }
}

// ---------------------------------------------------------------------------
// Server Entry Point
// ---------------------------------------------------------------------------

/// Run the LSP server over stdin/stdout.
pub async fn run_lsp_server() {
    let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());
    let (service, socket) = LspService::new(|client| NulangLanguageServer::new(client));
    Server::new(stdin, stdout, socket).serve(service).await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod lsp_tests {
    use super::*;

    /// Function parameter without type annotation gets an inlay hint.
    #[test]
    fn test_type_inlay_for_fn_param() {
        let source = "fn add(x: Int, y) { x + y }";
        let engine = InlayHintEngine::new(source);
        let hints = engine.generate_inlay_hints();
        // The unannotated param `y` should get a hint
        assert!(!hints.is_empty(), "expected hints for fn params, got 0");
    }

    /// Function return type without annotation gets an inlay hint.
    #[test]
    fn test_type_inlay_for_fn_return() {
        let source = "fn answer() -> Int { 42 }";
        let engine = InlayHintEngine::new(source);
        let hints = engine.generate_inlay_hints();
        // Return type is explicit, so no hint expected
        assert!(
            hints.is_empty(),
            "no hints expected when types are explicit"
        );
    }

    /// Cross-decl inference: main calls helper, both get hints.
    #[test]
    fn test_inlay_cross_decl() {
        let source = "fn helper(x) { x + 1 }\nfn main() { helper(5) }";
        let engine = InlayHintEngine::new(source);
        let hints = engine.generate_inlay_hints();
        assert!(!hints.is_empty(), "expected hints for cross-decl functions");
    }

    /// No hint when type is already explicit.
    #[test]
    fn test_no_inlay_when_explicit_type() {
        let source = "fn add(x: Int, y: Int) -> Int { x + y }";
        let engine = InlayHintEngine::new(source);
        let hints = engine.generate_inlay_hints();
        assert!(
            hints.is_empty(),
            "should not add hints when all types are explicit"
        );
    }

    #[test]
    fn test_inlay_position_calculation() {
        let source = "fn f(x) { x }";
        let engine = InlayHintEngine::new(source);
        let hints = engine.generate_inlay_hints();
        assert!(!hints.is_empty(), "expected hint for fn param");
        assert_eq!(hints[0].position.line, 0);
    }

    #[test]
    fn test_effect_row_inlay_hint() {
        // A function that performs an effect should get an effect-row hint.
        let source = "fn greet(s: String) -> String {\n    perform IO.print(s)\n    s\n}";
        let engine = InlayHintEngine::new(source);
        let hints = engine.generate_inlay_hints();
        assert!(
            hints.iter().any(|h| match &h.label {
                InlayHintLabel::String(l) => l.contains("!"),
                _ => false,
            }),
            "expected an effect-row inlay hint on the effectful function, got {:?}",
            hints
                .iter()
                .map(|h| match &h.label {
                    InlayHintLabel::String(l) => l.clone(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_effect_hint_for_pure_fn() {
        let source = "fn add(x: Int, y: Int) -> Int { x + y }";
        let engine = InlayHintEngine::new(source);
        let hints = engine.generate_inlay_hints();
        assert!(
            !hints.iter().any(|h| match &h.label {
                InlayHintLabel::String(l) => l.contains("!"),
                _ => false,
            }),
            "pure function should not get an effect-row hint"
        );
    }

    // -- Completion engine tests --

    fn labels(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn test_completion_keywords() {
        let source = "let x = 42";
        let engine = CompletionEngine::new(source);
        let items = engine.complete(
            Position {
                line: 0,
                character: 0,
            },
            None,
        );
        let labels = labels(&items);
        assert!(labels.contains(&"let"));
        assert!(labels.contains(&"fn"));
        assert!(labels.contains(&"match"));
    }

    #[test]
    fn test_completion_capabilities() {
        let source = "let x = 42";
        let engine = CompletionEngine::new(source);
        let items = engine.complete(
            Position {
                line: 0,
                character: 0,
            },
            None,
        );
        let labels = labels(&items);
        for cap in [
            "iso",
            "ref",
            "val",
            "box",
            "tag",
            "trn",
            "linear",
            "lineariso",
        ] {
            assert!(
                labels.contains(&cap),
                "missing capability completion: {}",
                cap
            );
        }
    }

    #[test]
    fn test_completion_behavior_keywords() {
        let source = "actor A { }";
        let engine = CompletionEngine::new(source);
        let items = engine.complete(
            Position {
                line: 0,
                character: 0,
            },
            None,
        );
        let labels = labels(&items);
        assert!(labels.contains(&"behavior"));
        assert!(labels.contains(&"state"));
        assert!(labels.contains(&"spawn"));
        assert!(labels.contains(&"receive"));
        assert!(labels.contains(&"workflow"));
    }

    #[test]
    fn test_completion_capability_prefix() {
        let source = "li";
        let engine = CompletionEngine::new(source);
        let items = engine.complete(
            Position {
                line: 0,
                character: 2,
            },
            None,
        );
        let labels = labels(&items);
        assert!(labels.contains(&"linear"));
        assert!(labels.contains(&"lineariso"));
    }

    #[test]
    fn test_completion_prefix_filtering() {
        let source = "ret";
        let engine = CompletionEngine::new(source);
        // Cursor at end of "ret".
        let items = engine.complete(
            Position {
                line: 0,
                character: 3,
            },
            None,
        );
        let labels = labels(&items);
        assert!(
            labels.contains(&"return"),
            "should offer 'return' for prefix 'ret'"
        );
        assert!(
            !labels.contains(&"let"),
            "'let' should not match prefix 'ret'"
        );
    }

    #[test]
    fn test_completion_top_level_functions() {
        let source = "fun foo()\nfun bar(x: Int)\nlet x = 1";
        let engine = CompletionEngine::new(source);
        let items = engine.complete(
            Position {
                line: 2,
                character: 0,
            },
            None,
        );
        let labels = labels(&items);
        assert!(labels.contains(&"foo"));
        assert!(labels.contains(&"bar"));
    }

    #[test]
    fn test_completion_effects() {
        let source = "";
        let engine = CompletionEngine::new(source);
        let items = engine.complete(
            Position {
                line: 0,
                character: 0,
            },
            None,
        );
        let labels = labels(&items);
        assert!(labels.contains(&"IO"));
        assert!(labels.contains(&"Http"));
        assert!(labels.contains(&"Inference"));
    }

    #[test]
    fn test_completion_case_insensitive() {
        // Effect names are matched case-insensitively by prefix.
        let source = "en";
        let engine = CompletionEngine::new(source);
        let items = engine.complete(
            Position {
                line: 0,
                character: 2,
            },
            None,
        );
        let labels = labels(&items);
        assert!(
            labels.contains(&"Env"),
            "should match 'Env' for prefix 'en'"
        );
    }

    // -- Crash-safety regression tests --

    #[test]
    fn test_code_action_let_binding_without_rhs() {
        // A half-typed `let` line with no `=` (e.g. `let x y`) must not
        // panic the code action provider; no quick fix can be offered
        // without a right-hand side.
        assert!(NulangLanguageServer::code_actions(
            "let x y",
            None,
            &Url::parse("file:///test.nula").unwrap(),
        )
        .is_none());
        // A well-formed binding still produces a quick fix.
        assert!(NulangLanguageServer::code_actions(
            "let x = 42",
            None,
            &Url::parse("file:///test.nula").unwrap(),
        )
        .is_some());
    }

    #[test]
    fn test_sig_help_non_ascii_line() {
        // UTF-16 columns must map to byte offsets on char boundaries: a
        // column inside the multibyte é must not panic the prefix slice.
        let source = "fun add(a, b) = a + b\nlet résumé = add(1, 2)";
        // UTF-16 column 6 on line 1 sits right after the é; as a raw byte
        // index it would land mid-character.
        let result = NulangLanguageServer::sig_help(source, Position::new(1, 6));
        assert!(result.is_none());
        // A column past the end of the line clamps to the line end.
        let result = NulangLanguageServer::sig_help(source, Position::new(1, 999));
        assert!(result.is_none());
    }

    #[test]
    fn test_position_to_offset_non_ascii_line() {
        // "café": é is one UTF-16 code unit but two bytes.
        let engine = CompletionEngine::new("let café = 1");
        // UTF-16 column 8 (right after "café") maps to byte offset 9.
        assert_eq!(engine.position_to_offset(Position::new(0, 8)), 9);
        // Completion at that offset must not panic: pre-fix the raw column
        // landed mid-character and prefix_at sliced inside é.
        let items = engine.complete(Position::new(0, 8), None);
        assert!(!items.is_empty(), "empty prefix should offer completions");
    }

    #[test]
    fn test_prefix_at_stops_on_char_boundary() {
        let engine = CompletionEngine::new("let caféx = 1");
        // Byte offset 10 is right after the x (é occupies bytes 7-8).
        assert_eq!(engine.prefix_at(10), "x");
        // A mid-character offset snaps back to a char boundary instead of
        // panicking: offset 8 (inside é) behaves like the start of é.
        assert_eq!(engine.prefix_at(8), "caf");
    }

    #[test]
    fn test_inlay_hints_rparen_before_lparen() {
        // A malformed `fun` line with `)` before `(` must not panic the
        // parameter slicer in collect_annotations.
        let engine = InlayHintEngine::new("fun foo) bar(");
        let hints = engine.generate_inlay_hints();
        assert!(hints.is_empty());
    }

    /// Regression: the LSP effect check is interprocedural, matching the CLI
    /// frontend — a function declared `! {}` that calls an IO function must
    /// produce an effect diagnostic in the editor, not just in `nulang run`.
    #[test]
    fn test_diagnostics_pure_fn_calling_io_fn() {
        let source = "fn do_io() -> Unit ! {IO} { perform IO.print(\"x\") }\n\
                      fn pure() -> Unit ! {} { do_io() }";
        let (diagnostics, _) = NulangLanguageServer::compute_diagnostics(source);
        assert!(
            diagnostics.iter().any(|d| {
                d.severity == Some(DiagnosticSeverity::ERROR) && d.message.contains("IO")
            }),
            "expected an effect error diagnostic mentioning IO, got: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    /// Regression: declarations nested in `module {}` blocks must be
    /// effect-checked just like top-level ones (the diagnostics pass
    /// flattens them, mirroring the CLI frontend).
    #[test]
    fn test_diagnostics_module_nested_effect_violation() {
        let source = "module M { fn pure() -> Unit ! {} { perform IO.print(\"x\") } }";
        let (diagnostics, _) = NulangLanguageServer::compute_diagnostics(source);
        assert!(
            diagnostics.iter().any(|d| {
                d.severity == Some(DiagnosticSeverity::ERROR) && d.message.contains("IO")
            }),
            "expected an effect error diagnostic for module-nested IO, got: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    /// Positive control: functions staying within their declared effect rows
    /// must produce no diagnostics at all.
    #[test]
    fn test_diagnostics_pure_functions_clean() {
        let source = "fn pure() -> Unit ! {} { unit }\n\
                      fn also_pure() -> Unit ! {} { pure() }\n\
                      fn do_io() -> Unit ! {IO} { perform IO.print(\"x\") }\n\
                      fn caller() -> Unit ! {IO} { do_io() }";
        let (diagnostics, _) = NulangLanguageServer::compute_diagnostics(source);
        assert!(
            diagnostics.is_empty(),
            "well-formed effectful/pure functions must be diagnostic-free, got: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_hover_shows_type_for_let_binding() {
        // Create a minimal NulangLanguageServer to test hover_at with type info
        let source = "fn main() { let x: Int = 42; x }";
        // compute_diagnostics now returns (diagnostics, type_map)
        let (_diagnostics, type_map) = NulangLanguageServer::compute_diagnostics(source);
        assert!(
            !type_map.is_empty(),
            "type_map should contain an entry for 'x'"
        );
        // The entry for x should show "Int"
        let has_int = type_map.values().any(|v| v == "Int");
        assert!(
            has_int,
            "type_map should contain 'Int' for x: {:?}",
            type_map
        );
    }

    #[test]
    fn test_hover_type_map_for_multiple_bindings() {
        let source = "fn main() { let x: Int = 42; let y: String = \"hi\"; x }";
        let (_diagnostics, type_map) = NulangLanguageServer::compute_diagnostics(source);
        assert!(
            type_map.values().any(|v| v == "Int"),
            "should have Int type"
        );
        assert!(
            type_map.values().any(|v| v == "String"),
            "should have String type"
        );
    }

    // -- Workspace symbol tests --

    #[test]
    fn test_workspace_symbol_extracts_top_level_decls() {
        let source = "fn add(x: Int, y: Int) -> Int { x + y }\n\
                      actor Counter { state count = 0\n  behavior inc() { self.count = self.count + 1 } }\n\
                      type Point = { x: Int, y: Int }";
        let uri = Url::parse("file:///test.nula").unwrap();
        let syms = NulangLanguageServer::doc_syms_uri(source, &uri).unwrap();

        // Find each expected symbol by name.
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"add"),
            "should find function 'add', got: {:?}",
            names
        );
        assert!(names.contains(&"Counter"), "should find actor 'Counter'");
        assert!(
            names.contains(&"Counter.inc"),
            "should find behavior 'Counter.inc'"
        );
        assert!(names.contains(&"Point"), "should find type 'Point'");

        // Verify URIs are correct.
        for s in &syms {
            assert_eq!(
                s.location.uri, uri,
                "all symbols should reference the document URI"
            );
        }
    }

    #[test]
    fn test_workspace_symbol_filtering_empty_query_returns_all() {
        let source = "fn foo() { unit }\nfn bar() { unit }\nfn baz() { unit }";
        let uri = Url::parse("file:///test.nula").unwrap();
        let syms = NulangLanguageServer::doc_syms_uri(source, &uri).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        // Empty query returns all — we verify all three functions present.
        assert_eq!(syms.len(), 3, "empty query should return all symbols");
        assert!(names.contains(&"foo") && names.contains(&"bar") && names.contains(&"baz"));
    }

    #[test]
    fn test_workspace_symbol_case_insensitive_filtering() {
        let source = "fn myFunc() { unit }\nfn myOTHER() { unit }\nfn unrelated() { unit }";
        let uri = Url::parse("file:///test.nula").unwrap();
        let syms = NulangLanguageServer::doc_syms_uri(source, &uri).unwrap();

        // Case-insensitive contains match on "my".
        let matched: Vec<&str> = syms
            .iter()
            .filter(|s| s.name.to_lowercase().contains("my"))
            .map(|s| s.name.as_str())
            .collect();
        assert!(matched.contains(&"myFunc"));
        assert!(matched.contains(&"myOTHER"));
        assert!(!matched.contains(&"unrelated"));
        assert_eq!(matched.len(), 2);
    }

    #[test]
    fn test_workspace_symbol_nested_module_decls() {
        let source = "module M { fn inner() { unit } }";
        let uri = Url::parse("file:///test.nula").unwrap();
        let syms = NulangLanguageServer::doc_syms_uri(source, &uri).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"M"), "should find module 'M'");
        assert!(
            names.contains(&"inner"),
            "should find nested function 'inner'"
        );
    }

    #[test]
    fn test_workspace_symbol_variant_and_record_types() {
        let source = "type Option[T] = Some(T) | None\n\
                      type Person = { name: String, age: Int }";
        let uri = Url::parse("file:///test.nula").unwrap();
        let syms = NulangLanguageServer::doc_syms_uri(source, &uri).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"Option"),
            "should find variant type 'Option'"
        );
        assert!(
            names.contains(&"Person"),
            "should find record type 'Person'"
        );
    }

    // -----------------------------------------------------------------
    // count_all_refs tests
    // -----------------------------------------------------------------

    #[test]
    fn test_count_all_refs_basic() {
        let source = "fn foo(x: Int) -> Int { foo(1) }\nfn bar() { foo(2); bar() }";
        let counts = NulangLanguageServer::count_all_refs(source);
        assert_eq!(counts.get("foo").copied().unwrap_or(0), 3); // decl + 2 calls
        assert_eq!(counts.get("bar").copied().unwrap_or(0), 2); // decl + 1 call
    }

    #[test]
    fn test_count_all_refs_empty() {
        let source = "";
        let counts = NulangLanguageServer::count_all_refs(source);
        assert!(counts.is_empty());
    }

    #[test]
    fn test_count_all_refs_ignores_keywords() {
        let source = "fn foo() { let x = 1; if true { foo() } }";
        let counts = NulangLanguageServer::count_all_refs(source);
        // 'let', 'if' are keywords, not identifiers
        assert!(!counts.contains_key("let"));
        assert!(!counts.contains_key("if"));
        assert!(counts.contains_key("foo"));
        assert!(counts.contains_key("x"));
    }

    // -----------------------------------------------------------------
    // extract_doc_comment tests
    // -----------------------------------------------------------------

    #[test]
    fn test_extract_doc_comment_single_line() {
        let source = "/// This is a doc comment\nfn foo() { }";
        let doc = NulangLanguageServer::extract_doc_comment(source, 1);
        assert_eq!(doc, Some("This is a doc comment".to_string()));
    }

    #[test]
    fn test_extract_doc_comment_multi_line() {
        let source = "/// First line\n/// Second line\nfn foo() { }";
        let doc = NulangLanguageServer::extract_doc_comment(source, 2);
        assert_eq!(doc, Some("First line\nSecond line".to_string()));
    }

    #[test]
    fn test_extract_doc_comment_none() {
        let source = "fn foo() { }";
        let doc = NulangLanguageServer::extract_doc_comment(source, 0);
        assert_eq!(doc, None);
    }

    #[test]
    fn test_extract_doc_comment_with_leading_space() {
        let source = "/// Hello world\nfn foo() { }";
        let doc = NulangLanguageServer::extract_doc_comment(source, 1);
        assert_eq!(doc, Some("Hello world".to_string()));
    }

    #[test]
    fn test_extract_doc_comment_not_at_top_of_file() {
        // Only extracts when there's a blank/non-doc line before
        let source = "import stdlib::core\n\n/// Doc comment\nfn foo() { }";
        let doc = NulangLanguageServer::extract_doc_comment(source, 3);
        assert_eq!(doc, Some("Doc comment".to_string()));
    }

    // -----------------------------------------------------------------
    // extract_document_links tests
    // -----------------------------------------------------------------

    #[test]
    fn test_document_links_stdlib_import() {
        let source = "import stdlib::json\nimport stdlib::math";
        let uri = Url::parse("file:///test.nula").unwrap();
        let links = NulangLanguageServer::extract_document_links(source, &uri);
        assert_eq!(links.len(), 2);
        assert!(links[0].target.is_some());
        assert!(links[1].target.is_some());
    }

    #[test]
    fn test_document_links_relative_import() {
        let source = "import \"./other.nula\"";
        let uri = Url::parse("file:///project/main.nula").unwrap();
        let links = NulangLanguageServer::extract_document_links(source, &uri);
        assert_eq!(links.len(), 1);
        assert!(links[0].target.is_some());
    }

    // -----------------------------------------------------------------
    // enriched hover tests
    // -----------------------------------------------------------------

    #[test]
    fn test_hover_includes_effects() {
        let source = "fn io_func() -> Unit ! {IO} { perform IO.print(\"x\") }";
        let hover = NulangLanguageServer::hover_at(source, Position::new(0, 3));
        assert!(hover.is_some());
        let text = match &hover.unwrap().contents {
            HoverContents::Scalar(MarkedString::String(s)) => s.clone(),
            _ => String::new(),
        };
        assert!(
            text.contains("effects:"),
            "hover should include effects: {}",
            text
        );
        assert!(text.contains("IO"), "hover should mention IO: {}", text);
    }

    #[test]
    fn test_hover_includes_doc_comment() {
        let source = "/// Squares a number\n/// Returns x * x\nfn square(x: Int) -> Int { x * x }";
        let hover = NulangLanguageServer::hover_at(source, Position::new(2, 3));
        assert!(hover.is_some());
        let text = match &hover.unwrap().contents {
            HoverContents::Scalar(MarkedString::String(s)) => s.clone(),
            _ => String::new(),
        };
        assert!(
            text.contains("Squares a number"),
            "hover should include doc: {}",
            text
        );
        assert!(
            text.contains("Returns x * x"),
            "hover should include second doc line: {}",
            text
        );
        assert!(
            text.contains("---"),
            "hover should have separator: {}",
            text
        );
    }

    #[test]
    fn test_hover_function_without_effects() {
        let source = "fn pure(x: Int) -> Int { x }";
        let hover = NulangLanguageServer::hover_at(source, Position::new(0, 3));
        assert!(hover.is_some());
        let text = match &hover.unwrap().contents {
            HoverContents::Scalar(MarkedString::String(s)) => s.clone(),
            _ => String::new(),
        };
        assert!(text.starts_with("fn pure"), "basic sig: {}", text);
        assert!(!text.contains("effects:"), "no effects section: {}", text);
    }

    // -----------------------------------------------------------------------
    // Protocol-level integration tests (PLAN Phase 2, bullet 7: "No
    // protocol-level (tower-lsp test-harness) integration tests").
    //
    // These drive the FULL JSON-RPC dispatch path — `tower_lsp::LspService`
    // with real `Request` objects, the same service the stdio server runs —
    // asserting request/response round-trips (initialize, hover, completion,
    // documentSymbol, shutdown) AND the server->client notification stream
    // (publishDiagnostics on didOpen), which the engine-level unit tests
    // above never reach. No subprocess, no wall-clock: `#[tokio::test]`.
    // -----------------------------------------------------------------------

    use futures::StreamExt;
    use tower_lsp::jsonrpc::{Id, Request};
    // `LspService` implements `tower::Service`; tower-lsp does not
    // re-export the trait, so the Service methods come from the
    // `tower_service` crate (a transitive dep of tower-lsp, already in
    // the lockfile; declared directly below for the `lsp` feature).
    use tower_service::Service;

    const DOC_URL: &str = "file:///protocol_test.nula";

    fn initialize_req() -> Request {
        Request::build("initialize")
            .params(serde_json::json!({
                "processId": null,
                "rootUri": null,
                "capabilities": {}
            }))
            .id(Id::Number(1.into()))
            .finish()
    }

    fn did_open_req(url: &str, version: i32, text: &str) -> Request {
        Request::build("textDocument/didOpen")
            .params(serde_json::json!({
                "textDocument": {
                    "uri": url,
                    "languageId": "nulang",
                    "version": version,
                    "text": text
                }
            }))
            .finish()
    }

    fn hover_req(url: &str, line: u32, character: u32) -> Request {
        Request::build("textDocument/hover")
            .params(serde_json::json!({
                "textDocument": { "uri": url },
                "position": { "line": line, "character": character }
            }))
            .id(Id::Number(2.into()))
            .finish()
    }

    fn completion_req(url: &str, line: u32, character: u32) -> Request {
        Request::build("textDocument/completion")
            .params(serde_json::json!({
                "textDocument": { "uri": url },
                "position": { "line": line, "character": character }
            }))
            .id(Id::Number(3.into()))
            .finish()
    }

    fn document_symbol_req(url: &str) -> Request {
        Request::build("textDocument/documentSymbol")
            .params(serde_json::json!({
                "textDocument": { "uri": url }
            }))
            .id(Id::Number(4.into()))
            .finish()
    }

    /// Drive one JSON-RPC exchange through the real service: poll-ready,
    /// call, unwrap the transport result, return the response.
    async fn call(
        service: &mut LspService<NulangLanguageServer>,
        req: Request,
    ) -> Option<tower_lsp::jsonrpc::Response> {
        std::future::poll_fn(|cx| service.poll_ready(cx))
            .await
            .expect("service ready");
        service.call(req).await.expect("call succeeds")
    }

    async fn init(service: &mut LspService<NulangLanguageServer>) {
        let resp = call(service, initialize_req())
            .await
            .expect("initialize response");
        let (_id, result) = resp.into_parts();
        let caps = result.expect("initialize must succeed");
        // The full capability table survives the wire round-trip.
        assert_eq!(
            caps["capabilities"]["hoverProvider"],
            serde_json::json!(true)
        );
        assert_eq!(
            caps["capabilities"]["definitionProvider"],
            serde_json::json!(true)
        );
        assert_eq!(
            caps["capabilities"]["completionProvider"]["triggerCharacters"],
            serde_json::json!([".", ":"])
        );
        assert!(caps["capabilities"].get("inlayHintProvider").is_some());
    }

    /// initialize -> didOpen(good doc) -> didOpen(broken doc): the server
    /// pushes publishDiagnostics over the server->client stream for BOTH,
    /// empty for the good doc and non-empty (parse error) for the broken one.
    #[tokio::test]
    async fn test_protocol_diagnostics_pushed_on_did_open() {
        let (mut service, mut socket) = LspService::new(|client| NulangLanguageServer::new(client));
        init(&mut service).await;

        call(
            &mut service,
            did_open_req(DOC_URL, 1, "fn add(x: Int, y: Int) { x + y }"),
        )
        .await;
        let msg = socket.next().await.expect("diagnostics notification");
        assert_eq!(msg.method(), "textDocument/publishDiagnostics");
        let params = msg.params().cloned().expect("params");
        assert_eq!(
            params["diagnostics"].as_array().map(|a| a.len()),
            Some(0),
            "well-formed doc must publish zero diagnostics, got {params}"
        );
        assert_eq!(params["uri"], serde_json::json!(DOC_URL));

        call(&mut service, did_open_req(DOC_URL, 2, "fn broken( ")).await;
        let msg = socket.next().await.expect("diagnostics notification");
        let params = msg.params().cloned().expect("params");
        let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
        assert!(
            !diagnostics.is_empty(),
            "broken doc must publish a diagnostic, got {params}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d["severity"] == serde_json::json!(1)),
            "parse error must be severity 1 (Error), got {params}"
        );
    }

    /// didChange pushes fresh diagnostics for the new text.
    #[tokio::test]
    async fn test_protocol_diagnostics_pushed_on_did_change() {
        let (mut service, mut socket) = LspService::new(|client| NulangLanguageServer::new(client));
        init(&mut service).await;
        call(
            &mut service,
            did_open_req(DOC_URL, 1, "fn add(x: Int, y: Int) { x + y }"),
        )
        .await;
        socket.next().await.expect("open diagnostics");

        call(
            &mut service,
            Request::build("textDocument/didChange")
                .params(serde_json::json!({
                    "textDocument": { "uri": DOC_URL, "version": 2 },
                    "contentChanges": [ { "text": "fn broken( " } ]
                }))
                .finish(),
        )
        .await;
        let msg = socket.next().await.expect("change diagnostics");
        assert_eq!(msg.method(), "textDocument/publishDiagnostics");
        let params = msg.params().cloned().expect("params");
        assert!(
            !params["diagnostics"].as_array().unwrap().is_empty(),
            "change to a broken doc must publish diagnostics, got {params}"
        );
    }

    /// hover over a function declaration returns its signature text through
    /// the full JSON-RPC round-trip.
    #[tokio::test]
    async fn test_protocol_hover_returns_signature() {
        let (mut service, _socket) = LspService::new(|client| NulangLanguageServer::new(client));
        init(&mut service).await;
        call(
            &mut service,
            did_open_req(DOC_URL, 1, "fn add(x: Int, y: Int) { x + y }"),
        )
        .await;

        let resp = call(&mut service, hover_req(DOC_URL, 0, 4))
            .await
            .expect("hover response");
        let (id, result) = resp.into_parts();
        assert_eq!(id, Id::Number(2.into()));
        let v = result.expect("hover must succeed");
        // HoverContents serializes as an object ({language,value} or
        // {kind,value}) for language/markup strings, or a bare string for
        // plain MarkedString::String — accept both.
        let value = match v["contents"].as_str() {
            Some(s) => s.to_string(),
            None => v["contents"]
                .get("value")
                .or_else(|| v["contents"].get("language"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        };
        assert!(
            !value.is_empty(),
            "hover must return signature text, got {v}"
        );
    }

    /// completion at an empty line returns language keywords through the
    /// full JSON-RPC round-trip.
    #[tokio::test]
    async fn test_protocol_completion_returns_keywords() {
        let (mut service, _socket) = LspService::new(|client| NulangLanguageServer::new(client));
        init(&mut service).await;
        call(&mut service, did_open_req(DOC_URL, 1, "let x = 42\n")).await;

        let resp = call(&mut service, completion_req(DOC_URL, 1, 0))
            .await
            .expect("completion response");
        let (_id, result) = resp.into_parts();
        let v = result.expect("completion must succeed");
        let items = v
            .as_array()
            .expect("CompletionResponse::Array serializes as array");
        let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
        assert!(
            labels.contains(&"fn"),
            "completion must include fn, got {labels:?}"
        );
        assert!(
            labels.contains(&"match"),
            "completion must include match, got {labels:?}"
        );
    }

    /// documentSymbol returns the top-level declaration outline through the
    /// full JSON-RPC round-trip.
    #[tokio::test]
    async fn test_protocol_document_symbol_returns_outline() {
        let (mut service, _socket) = LspService::new(|client| NulangLanguageServer::new(client));
        init(&mut service).await;
        call(
            &mut service,
            did_open_req(
                DOC_URL,
                1,
                "fn add(x: Int, y: Int) { x + y }\nfn main() { add(1, 2) }\n",
            ),
        )
        .await;

        let resp = call(&mut service, document_symbol_req(DOC_URL))
            .await
            .expect("documentSymbol response");
        let (_id, result) = resp.into_parts();
        let v = result.expect("documentSymbol must succeed");
        let symbols = v.as_array().expect("documentSymbol response is an array");
        let names: Vec<&str> = symbols.iter().filter_map(|s| s["name"].as_str()).collect();
        assert!(
            names.contains(&"add") && names.contains(&"main"),
            "outline must contain add and main, got {names:?}"
        );
    }

    /// shutdown then exit: shutdown returns Ok; a request after exit fails
    /// with the service's Exited error (the LSP lifecycle contract).
    #[tokio::test]
    async fn test_protocol_shutdown_then_exit() {
        let (mut service, _socket) = LspService::new(|client| NulangLanguageServer::new(client));
        init(&mut service).await;

        let resp = call(
            &mut service,
            Request::build("shutdown").id(Id::Number(9.into())).finish(),
        )
        .await
        .expect("shutdown response");
        let (id, result) = resp.into_parts();
        assert_eq!(id, Id::Number(9.into()));
        assert_eq!(
            result.expect("shutdown must succeed"),
            serde_json::Value::Null
        );

        call(&mut service, Request::build("exit").finish()).await;

        // Any request after exit is rejected at the service boundary.
        let err = service
            .call(
                Request::build("textDocument/hover")
                    .id(Id::Number(10.into()))
                    .finish(),
            )
            .await;
        assert!(
            err.is_err(),
            "request after exit must fail with ExitedError"
        );
    }
}
