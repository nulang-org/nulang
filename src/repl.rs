//! Read-Eval-Print Loop for Nulang.
//!
//! Full-featured REPL with:
//! - Persistent type context across evaluations
//! - Multi-line input support
//! - `:commands` for introspection
//! - Graceful error handling

use crate::ast::{AstModule, Decl, Expr};
use crate::effect_checker::flatten_decls;
use crate::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::typechecker::TypeChecker;
use crate::types::{Capability, NuError, NuResult, Span, Type, TypeContext};
use crate::vm::{Value, VM};
use rustyline::error::ReadlineError;
use rustyline::{history::DefaultHistory, Editor, Result as RlResult};
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::rc::Rc;
/// REPL state that persists across evaluations.
pub struct Repl {
    vm: VM,
    /// Accumulated declarations from previous inputs (functions, actors, etc.)
    accumulated_decls: Vec<Decl>,
    /// Source text for session persistence.
    session_source: String,
    /// Persistent type context across evaluations
    type_ctx: TypeContext,
    /// Fresh type checker (can be reused)
    type_checker: TypeChecker,
    /// Last compiled bytecode module (for :bytecode command)
    last_bytecode: Option<String>,
    /// Last AST (for :ast command display)
    last_ast: Option<AstModule>,
    /// Set of user-defined identifiers for tab completion (shared with ReplHelper).
    user_names: Rc<RefCell<HashSet<String>>>,
}
struct ReplHelper {
    keywords: Vec<String>,
    user_names: Rc<RefCell<HashSet<String>>>,
}

impl ReplHelper {
    fn new(user_names: Rc<RefCell<HashSet<String>>>) -> Self {
        ReplHelper {
            keywords: vec![
                // Keywords
                "fn",
                "let",
                "rec",
                "in",
                "if",
                "then",
                "else",
                "match",
                "with",
                "actor",
                "behavior",
                "state",
                "spawn",
                "send",
                "ask",
                "receive",
                "perform",
                "handle",
                "resume",
                "effect",
                "workflow",
                "step",
                "parallel",
                "compensate",
                "statemachine",
                "event",
                "on_entry",
                "on_exit",
                "persistent",
                "local",
                "durable",
                "eventsourced",
                "crdt",
                "module",
                "import",
                "pub",
                "type",
                "alias",
                "extern",
                "iso",
                "trn",
                "ref",
                "val",
                "box",
                "tag",
                "lineariso",
                "linear",
                "true",
                "false",
                "nil",
                "unit",
                "for",
                "loop",
                "break",
                "return",
                "node",
                "link",
                "monitor",
                "exit",
                "agent",
                "database",
                // Types
                "Int",
                "Float",
                "String",
                "Bool",
                "Unit",
                "Nil",
                // Built-in effects
                "IO.print",
                "IO.read",
                "IO.flush",
                "Timer.sleep",
                "Timer.now",
                "Signal.wait",
                "Signal.notify",
                "Inference.ask",
                "Net.connect",
                "Net.listen",
                "Actor.spawn",
                "Actor.send",
                "Actor.link",
                "Actor.monitor",
                "Actor.trap_exit",
                "Actor.exit",
                "Actor.register",
                "Actor.unregister",
                "Actor.whereis",
                "Actor.set_priority",
                "Actor.stats",
                "Workflow.query",
                "Workflow.respond",
                // REPL commands
                ":help",
                ":quit",
                ":type",
                ":ast",
                ":bytecode",
                ":clear",
                ":reset",
                ":version",
                ":stats",
                ":load",
            ]
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
            user_names,
        }
    }
}

impl rustyline::Helper for ReplHelper {}

impl rustyline::highlight::Highlighter for ReplHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> std::borrow::Cow<'l, str> {
        // Respect NO_COLOR
        if std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty()) {
            return std::borrow::Cow::Borrowed(line);
        }

        let mut lexer = crate::lexer::Lexer::new(line);
        let tokens = match lexer.lex() {
            Ok(ts) => ts,
            Err(_) => return std::borrow::Cow::Borrowed(line),
        };

        let mut result = String::with_capacity(line.len() + 64);
        let mut last_end = 0usize;

        for token in &tokens {
            let start = token.span.start as usize;
            let end = token.span.end as usize;

            // Copy unhighlighted text before this token
            if start > last_end {
                result.push_str(&line[last_end..start]);
            }

            let color = color_for_token(&token.kind);
            let text = &line[start..end];
            if color.is_empty() {
                result.push_str(text);
            } else {
                result.push_str(color);
                result.push_str(text);
                result.push_str("\x1b[0m");
            }
            last_end = end;
        }
        // Copy remaining unhighlighted text
        if last_end < line.len() {
            result.push_str(&line[last_end..]);
        }

        std::borrow::Cow::Owned(result)
    }

    fn highlight_char(&self, line: &str, pos: usize, _kind: rustyline::highlight::CmdKind) -> bool {
        // Bracket matching: indicate if char at pos has a matching partner
        if pos >= line.len() {
            return false;
        }
        let ch = line.as_bytes()[pos] as char;
        if !matches!(ch, '(' | ')' | '{' | '}' | '[' | ']') {
            return false;
        }
        // Check if brackets are balanced (the char at pos has a match)
        let mut stack: Vec<char> = Vec::new();
        for (_i, c) in line.char_indices() {
            match c {
                '(' | '{' | '[' => stack.push(c),
                ')' | '}' | ']' => {
                    if let Some(&open) = stack.last() {
                        let expected = match open {
                            '(' => ')',
                            '{' => '}',
                            '[' => ']',
                            _ => unreachable!(),
                        };
                        if c == expected {
                            stack.pop();
                        } else {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                _ => {}
            }
        }
        stack.is_empty()
    }
}

/// Map a lexer token kind to an ANSI color string (empty for no color).
fn color_for_token(kind: &crate::lexer::TokenKind) -> &'static str {
    use crate::lexer::TokenKind::*;
    match kind {
        // Keywords — bright yellow
        Fn | Let | Rec | In | If | Then | Else | Match | With | Case | Actor | Entity
        | Behavior | State | StateMachine | SelfKw | Spawn | Send | Remote | Ask | Persistent
        | Local | Durable | EventSourced | Crdt | Until | Emit | Workflow | Step | Parallel
        | Compensate | Agent | Database | Receive | Effect | Perform | Handle | Resume | Extern
        | Module | Import | Pub | Migrate | Monitor | Link | Exit | For | While | Break
        | Return | Type | Alias | Iso | Trn | Ref | Val | Box | Tag | True | False | Unit
        | Tool | Handler | Consume | Initial | Throws | As => "\x1b[1;33m",
        // String literals — green
        StringLit(_) => "\x1b[32m",
        // Numeric literals — magenta
        IntLit(_) | FloatLit(_) => "\x1b[35m",
        // Comments — dim
        Comment(_) | DocComment(_) => "\x1b[2m",
        // Delimiters and everything else — no color
        _ => "",
    }
}

impl rustyline::hint::Hinter for ReplHelper {
    type Hint = String;
}

impl rustyline::validate::Validator for ReplHelper {}

impl rustyline::completion::Completer for ReplHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> RlResult<(usize, Vec<String>)> {
        // Find the word boundary before the cursor.
        let start = line[..pos]
            .rfind(|c: char| c.is_whitespace() || c == '(' || c == '{' || c == '[')
            .map(|i| i + 1)
            .unwrap_or(0);
        let prefix = &line[start..pos];
        if prefix.is_empty() {
            return Ok((pos, vec![]));
        }
        let prefix_lower = prefix.to_lowercase();
        // Merge static keywords with dynamic user-defined names, deduplicate.
        let mut seen: HashSet<&str> = HashSet::new();
        let mut matches: Vec<String> = Vec::new();
        for kw in self.keywords.iter().chain(self.user_names.borrow().iter()) {
            if kw.to_lowercase().starts_with(&prefix_lower) && seen.insert(kw.as_str()) {
                matches.push(kw.clone());
            }
        }
        Ok((start, matches))
    }
}

impl Repl {
    pub fn new() -> Self {
        Repl {
            vm: VM::new(),
            accumulated_decls: Vec::new(),
            session_source: String::new(),
            type_ctx: TypeContext::new(),
            type_checker: TypeChecker::new(),
            last_bytecode: None,
            last_ast: None,
            user_names: Rc::new(RefCell::new(HashSet::new())),
        }
    }

    /// Run the interactive REPL loop.
    pub fn run(&mut self) {
        println!(
            "Nulang v{} \u{2014} Actor-Based Distributed Language",
            env!("CARGO_PKG_VERSION")
        );
        println!("Type :help for commands, :quit to exit\n");

        let mut editor = match Editor::<ReplHelper, DefaultHistory>::new() {
            Ok(ed) => ed,
            Err(e) => {
                eprintln!(
                    "Warning: Could not initialize line editor ({}). Falling back to basic input.",
                    e
                );
                run_basic_repl();
                return;
            }
        };
        editor.set_helper(Some(ReplHelper::new(Rc::clone(&self.user_names))));
        let history_path = std::env::var("HOME")
            .map(|h| format!("{}/.nulang_history", h))
            .unwrap_or_else(|_| ".nulang_history".to_string());
        let _ = editor.load_history(&history_path);

        let mut buffer = String::new();
        let mut brace_stack: Vec<char> = Vec::new();

        loop {
            let prompt = if brace_stack.is_empty() {
                "nulang> ".to_string()
            } else {
                ".... ".to_string()
            };

            let line = match editor.readline(&prompt) {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => {
                    println!("^C");
                    buffer.clear();
                    brace_stack.clear();
                    continue;
                }
                Err(ReadlineError::Eof) => {
                    println!("Goodbye!");
                    break;
                }
                Err(e) => {
                    eprintln!("Error reading input: {}", e);
                    continue;
                }
            };

            let trimmed = line.trim();

            // REPL commands (only when not in multi-line mode)
            if brace_stack.is_empty() && trimmed.starts_with('%') {
                editor.add_history_entry(&line).ok();
                let mut parts = trimmed[1..].splitn(2, ' ');
                let cmd = parts.next().unwrap_or("");
                let rest = parts.next().unwrap_or("").trim();

                match cmd {
                    "quit" | "q" => {
                        println!("Goodbye!");
                        break;
                    }
                    "help" | "h" => {
                        if rest.is_empty() {
                            self.print_help();
                        } else {
                            self.print_help_topic(rest);
                        }
                    }
                    "type" => {
                        if rest.is_empty() {
                            eprintln!("Usage: %type <expression>");
                        } else if let Err(e) = self.show_type(rest) {
                            self.print_error(&e);
                        }
                    }
                    "load" => {
                        if rest.is_empty() {
                            eprintln!("Usage: %load <file>");
                        } else if let Err(e) = self.load_file(rest) {
                            self.print_error(&e);
                        }
                    }
                    "ast" => {
                        if rest.is_empty() {
                            eprintln!("Usage: %ast <expression>");
                        } else if let Err(e) = self.show_ast(rest) {
                            self.print_error(&e);
                        }
                    }
                    "bytecode" | "bc" => self.show_bytecode(),
                    "clear" => {
                        print!("\x1B[2J\x1B[1;1H");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    "reset" => {
                        self.accumulated_decls.clear();
                        self.session_source.clear();
                        self.type_ctx = TypeContext::new();
                        self.type_checker = TypeChecker::new();
                        self.last_bytecode = None;
                        self.last_ast = None;
                        self.user_names.borrow_mut().clear();
                        println!("Environment reset.");
                    }
                    "version" | "ver" => {
                        println!("nulang v{}", env!("CARGO_PKG_VERSION"));
                    }
                    "time" => {
                        if rest.is_empty() {
                            eprintln!("Usage: %time <expression>");
                        } else {
                            use std::time::Instant;
                            let start = Instant::now();
                            if let Err(e) = self.evaluate(rest) {
                                self.print_error(&e);
                            }
                            let elapsed = start.elapsed();
                            println!("Elapsed: {:.2?}", elapsed);
                        }
                    }
                    "effect" => {
                        if rest.is_empty() {
                            eprintln!("Usage: %effect <expression>");
                        } else {
                            // Run typecheck to get effect row
                            if let Err(e) = self.show_effect(rest) {
                                self.print_error(&e);
                            }
                        }
                    }
                    "cap" => {
                        if rest.is_empty() {
                            eprintln!("Usage: %cap <expression>");
                        } else {
                            if let Err(e) = self.show_cap(rest) {
                                self.print_error(&e);
                            }
                        }
                    }
                    "actors" => {
                        println!(
                            "Actor inspection not available in REPL mode (no running runtime)."
                        );
                    }
                    "actor" => {
                        if rest.is_empty() {
                            eprintln!("Usage: %actor <id>");
                        } else {
                            println!(
                                "Actor inspection not available in REPL mode (no running runtime)."
                            );
                        }
                    }
                    "save" => {
                        if rest.is_empty() {
                            eprintln!("Usage: %save <file>");
                        } else {
                            if let Err(e) = self.save_session(rest) {
                                self.print_error(&e);
                            }
                        }
                    }
                    "load-session" => {
                        if rest.is_empty() {
                            eprintln!("Usage: %load-session <file>");
                        } else {
                            if let Err(e) = self.load_session(rest) {
                                self.print_error(&e);
                            }
                        }
                    }
                    "stats" => {
                        println!("Runtime stats not available in REPL mode.");
                        println!("(use a .nula file with actors and --verbose for runtime stats)");
                    }
                    unknown => {
                        println!("Unknown command: :{}. Type %help for help.", unknown);
                    }
                }
            }

            buffer.push_str(&line);
            buffer.push('\n');

            // Use the lexer to track brace/paren/bracket stack.
            brace_stack = Self::brace_stack(&buffer);
            if !brace_stack.is_empty() {
                continue; // Wait for more input
            }

            // Execute buffered input.
            let input = buffer.trim();
            if !input.is_empty() {
                editor.add_history_entry(input).ok();
                if let Err(e) = self.evaluate(input) {
                    self.print_error(&e);
                }
            }
            buffer.clear();
        }

        let _ = editor.save_history(&history_path);
        println!();
    }

    /// Evaluate a source string, showing value and type.
    fn evaluate(&mut self, source: &str) -> NuResult<()> {
        crate::types::set_source_map_with_file(source, Some("<repl>"));
        // Parse
        let ast = parse_source(source)?;
        self.last_ast = Some(ast.clone());

        // Separate declarations from the __main expression
        let mut new_decls = Vec::new();
        let mut main_expr: Option<Expr> = None;

        for decl in &ast.decls {
            if let Decl::Function { name, .. } = decl {
                if name == "__main" {
                    // Extract the body expression of __main
                    if let Decl::Function { body, .. } = decl {
                        main_expr = Some(body.clone());
                    }
                    continue;
                }
            }
            new_decls.push(decl.clone());
        }

        // Build combined module: accumulated + new declarations + __main if present
        let mut combined_decls = self.accumulated_decls.clone();
        combined_decls.extend(new_decls.clone());

        if let Some(ref expr) = main_expr {
            combined_decls.push(Decl::Function {
                name: "__main".to_string(),
                type_params: vec![],
                type_param_constraints: vec![],
                params: vec![],
                default_values: vec![],
                using_params: vec![],
                ret_type: None,
                error_type: None,
                effect: None,
                cap: None,
                requires: vec![],
                ensures: vec![],
                body: expr.clone(),
                annotations: vec![],
                public: false,
                span: Span::default(),
            });
        }

        let combined_module = AstModule {
            name: "repl".to_string(),
            decls: combined_decls,
        };

        // Type check the combined module
        let module_type = self.type_checker.check_module(&combined_module)?;

        // Effect check: same two-pass driver as the CLI frontend
        // (`run_frontend` in main.rs) over the combined module — accumulated
        // + new declarations + __main. Registering function rows first lets
        // callee effects propagate to call sites (so new code calling an
        // accumulated IO function is charged its row), and pass 2 enforces
        // declared rows on every body. Errors print through the caller's
        // `print_error` exactly as before.
        let mut effect_checker = EffectChecker::new();
        effect_checker.check_module(&combined_module.decls)?;

        // Capability analysis
        let mut cap_analyzer = CapabilityAnalyzer::new();
        let cap_ctx = CapContext::new();
        if let Some(ref expr) = main_expr {
            let _cap = cap_analyzer.infer_cap(&cap_ctx, expr)?;
        }

        // Compile the combined module via the HIR/MIR pipeline.
        let code_module = compile_with_new_pipeline(&combined_module, "repl", &self.type_checker)?;
        self.last_bytecode = Some(disassemble_module(&code_module));
        // from scratch (see `combined_module` above), so no closure created
        // by a previous evaluation can still be reachable — safe to reclaim
        // their capture environments before this run instead of leaking them
        // for the life of the REPL session.
        self.vm.clear_closure_envs();
        // Load and execute
        self.vm.load_module(code_module);
        let value = self.vm.run()?;

        // Print results
        if let Some(ref _expr) = main_expr {
            let val_str = value_to_pretty_string(&value);
            let expr_ty = extract_return_type(&module_type);
            let ty_str = type_to_string(expr_ty);
            println!("{} : {}", val_str, ty_str);
            // Inline effect row and capability hints for the evaluated expression
            if let Some(ref _expr) = main_expr {
                let effect_str = if let Type::Function { effect, .. } = &module_type {
                    let eff_str = if effect.effects().is_empty() {
                        String::new()
                    } else {
                        format!(" ! {}", effect)
                    };
                    eff_str
                } else {
                    String::new()
                };
                let cap_str = if let Type::Function { cap, .. } = &module_type {
                    let cap_str = if *cap == Capability::Ref {
                        String::new()
                    } else {
                        format!(" :{}", cap)
                    };
                    cap_str
                } else {
                    String::new()
                };
                if !effect_str.is_empty() || !cap_str.is_empty() {
                    println!("  (type: {}{})", effect_str, cap_str);
                }
            }
        } else if !new_decls.is_empty() {
            // Print declaration info. Each new declaration is re-checked in
            // the full session context (accumulated + earlier new decls, in
            // source order) rather than in isolation, so a function calling
            // a previously-defined function resolves and its type prints.
            let mut context_decls = self.accumulated_decls.clone();
            for decl in &new_decls {
                context_decls.push(decl.clone());
                match decl {
                    Decl::Function { name, .. } => {
                        let decl_ty = self.type_checker.check_module(&AstModule {
                            name: "repl".to_string(),
                            decls: context_decls.clone(),
                        })?;
                        println!("{} : {}", name, type_to_string(&decl_ty));
                    }
                    Decl::LetBinding { name, .. } | Decl::Signal { name, .. } => {
                        let decl_ty = self.type_checker.check_module(&AstModule {
                            name: "repl".to_string(),
                            decls: context_decls.clone(),
                        })?;
                        println!("{} : {}", name, type_to_string(&decl_ty));
                    }
                    _ => {}
                }
            }
        }

        // Update accumulated state with new declarations
        self.accumulated_decls.extend(new_decls.clone());
        self.session_source.push_str(source);
        self.session_source.push_str("\n");

        // Collect user-defined identifiers for tab completion.
        for decl in &new_decls {
            let name = match decl {
                Decl::Function { name, .. } => Some(name.as_str()),
                Decl::Actor { name, .. } => Some(name.as_str()),
                Decl::StateMachine { name, .. } => Some(name.as_str()),
                Decl::TypeAlias { name, .. } => Some(name.as_str()),
                Decl::RecordType { name, .. } => Some(name.as_str()),
                Decl::VariantType { name, .. } => Some(name.as_str()),
                Decl::EffectDecl { name, .. } => Some(name.as_str()),
                Decl::Workflow { name, .. } => Some(name.as_str()),
                Decl::Agent { name, .. } => Some(name.as_str()),
                Decl::Database { name, .. } => Some(name.as_str()),
                Decl::NamedHandler { name, .. } => Some(name.as_str()),
                Decl::Class { name, .. } => Some(name.as_str()),
                _ => None,
            };
            if let Some(n) = name {
                self.user_names.borrow_mut().insert(n.to_string());
            }
        }

        Ok(())
    }

    /// Show the inferred type of an expression (without executing).
    fn show_type(&mut self, source: &str) -> NuResult<()> {
        // Wrap in let ... in ... if needed to make it a valid module expression
        let wrapped = if !source.contains("let ") && !source.contains("fn ") {
            format!("{}", source)
        } else {
            source.to_string()
        };

        let ast = parse_source(&wrapped)?;

        // Extract the expression to type-check
        let expr = extract_main_expr(&ast)?;

        // Build combined module with accumulated decls + this expression
        let mut combined_decls = self.accumulated_decls.clone();
        combined_decls.push(Decl::Function {
            name: "__main".to_string(),
            type_params: vec![],
            type_param_constraints: vec![],
            params: vec![],
            default_values: vec![],
            using_params: vec![],
            ret_type: None,
            error_type: None,
            effect: None,
            cap: None,
            requires: vec![],
            ensures: vec![],
            body: expr,
            annotations: vec![],
            public: false,
            span: Span::default(),
        });

        let module = AstModule {
            name: "typecheck".to_string(),
            decls: combined_decls,
        };

        let ty = self.type_checker.check_module(&module)?;
        let expr_ty = extract_return_type(&ty);
        println!("{}", type_to_string(expr_ty));
        Ok(())
    }

    /// Show the AST of an expression.
    fn show_ast(&mut self, source: &str) -> NuResult<()> {
        let ast = parse_source(source)?;
        let expr = extract_main_expr(&ast)?;
        println!("{:#?}", expr);
        Ok(())
    }

    /// Show bytecode for the last compiled expression.
    fn show_bytecode(&self) {
        match &self.last_bytecode {
            Some(bc) => println!("{}", bc),
            None => println!("No bytecode available. Evaluate an expression first."),
        }
    }

    fn print_help(&self) {
        println!("Commands:");
        println!("  %quit, %q        Exit the REPL");
        println!("  %help, %h [t]    Show help (topics: syntax, types, actors, effects, commands)");
        println!("  %type <expr>     Show the inferred type of an expression");
        println!("  %load <file>     Load and run a .nula file");
        println!("  %ast <expr>      Show the AST of an expression");
        println!("  %bytecode, %bc   Show bytecode for the last expression");
        println!("  %time <expr>     Time the evaluation of an expression");
        println!("  %effect <expr>   Show the effect row of an expression");
        println!("  %cap <expr>      Show the capability of an expression");
        println!("  %actors          List actors (stub)");
        println!("  %actor <id>      Inspect actor (stub)");
        println!("  %save <file>     Save session to file");
        println!("  %load <file>     Load session from file");
        println!("  %clear           Clear the screen");
        println!("  %reset           Reset the environment");
        println!("  %stats           Show runtime stats (--verbose mode only)");
        println!("  %version, %ver   Print version and exit (repl keeps running)");
    }

    fn print_help_topic(&self, topic: &str) {
        match topic {
            "syntax" => {
                println!("Nulang Syntax Overview");
                println!("======================");
                println!();
                println!("Basic expressions:");
                println!("  1 + 2 * 3          -- arithmetic");
                println!("  true && false       -- booleans");
                println!("  \"hello\" ^ \" world\"  -- string concatenation");
                println!();
                println!("Let bindings:");
                println!("  let x = 5 in x + 1");
                println!("  let rec factorial = fn(n) {{ if n <= 1 then 1 else n * factorial(n - 1) }} in factorial(5)");
                println!();
                println!("Functions:");
                println!("  fn add(x, y) {{ x + y }}");
                println!("  fn add(x: Int, y: Int) -> Int {{ x + y }}");
                println!();
                println!("Control flow:");
                println!("  if condition then expr1 else expr2");
                println!("  match expr {{ pattern1 => result1, pattern2 => result2 }}");
            }
            "types" => {
                println!("Nulang Type System");
                println!("==================");
                println!();
                println!("Primitive types: Int, Float, String, Bool, Unit, Nil");
                println!("Compound types:");
                println!("  (Int, String)      -- tuple");
                println!("  {{ x: Int, y: Int }} -- record");
                println!("  [Int]              -- array");
                println!("  Int -> String      -- function");
                println!("  Int -> String !{{IO}} -- function with effects");
                println!();
                println!("Type aliases:");
                println!("  type Point = {{ x: Int, y: Int }}");
                println!();
                println!("Capabilities: val, ref, iso, trn, box, tag, linear");
                println!("Use :type <expr> to see the inferred type of any expression.");
            }
            "actors" => {
                println!("Nulang Actors");
                println!("=============");
                println!();
                println!("Actors are the core concurrency primitive in Nulang.");
                println!("Each actor has private state and communicates via message passing.");
                println!();
                println!("  actor Counter {{");
                println!("      state: Int = 0;");
                println!("      behavior inc {{ self.state <- self.state + 1 }}");
                println!("      behavior get -> Int {{ self.state }}");
                println!("  }}");
                println!();
                println!("  let counter = spawn Counter;");
                println!("  send counter.inc;");
                println!("  let v = ask counter.get;");
                println!();
                println!("Actor features:");
                println!("  - Spawn/ask/send for message passing");
                println!("  - Links and monitors for fault tolerance");
                println!("  - Persistent actors with event sourcing");
                println!("  - Workflows with steps, compensation, and signals");
                println!("  - CRDT-based state for automatic conflict resolution");
            }
            "effects" => {
                println!("Nulang Effect System");
                println!("====================");
                println!();
                println!("Effects track side effects in the type system.");
                println!();
                println!("Built-in effects:");
                println!("  IO      -- input/output (print, read, flush)");
                println!("  Timer   -- sleep, now");
                println!("  Signal  -- wait, notify");
                println!("  Net     -- connect, listen");
                println!();
                println!("Function signatures declare effects:");
                println!("  fn pure() -> Int !{{}}        {{ 42 }}           -- no effects");
                println!("  fn io_fn() -> Unit !{{IO}}   {{ perform IO.print(\"x\") }}");
                println!();
                println!("Effects are checked interprocedurally — calling an IO function");
                println!("requires the caller to declare IO in its effect row.");
            }
            "commands" => {
                println!("REPL Commands");
                println!("=============");
                println!();
                println!("All commands start with colon (:).");
                println!();
                println!("  :quit, :q        Exit the REPL");
                println!("  :help, :h [t]    Show help (topics: syntax, types, actors, effects, commands)");
                println!("  :type <expr>     Show the inferred type without evaluating");
                println!("  :load <file>     Load and evaluate a .nula file");
                println!("  :ast <expr>      Show the parsed AST");
                println!("  :bytecode, :bc   Show the last evaluated expression's bytecode");
                println!("  :clear           Clear the terminal screen");
                println!("  :reset           Clear all accumulated definitions and types");
                println!("  :stats           Show runtime statistics");
                println!("  :version, :ver   Print the Nulang version");
                println!();
                println!("Multi-line input:");
                println!("  The REPL automatically enters multi-line mode when it detects");
                println!("  unclosed braces, parens, or brackets. The prompt changes to");
                println!("  '.... ' until the expression is complete.");
            }
            _ => {
                eprintln!(
                    "Unknown help topic: '{}'. Topics: syntax, types, actors, effects, commands.",
                    topic
                );
            }
        }
    }

    /// Load and evaluate a .nula file in the REPL context.
    fn load_file(&mut self, path: &str) -> NuResult<()> {
        let source = std::fs::read_to_string(path).map_err(|e| {
            NuError::parse_error(
                format!("Cannot read file '{}': {}", path, e),
                Span::default(),
            )
        })?;
        println!("Loaded '{}' ({} bytes)", path, source.len());
        self.evaluate(&source)
    }

    fn print_error(&self, err: &NuError) {
        if std::io::stderr().is_terminal() {
            if let Some(rendered) = crate::diagnostic::render(err, true) {
                eprint!("{rendered}");
            } else {
                eprint!("{}", err.format_rich());
            }
        } else {
            eprintln!("Error: {}", err);
        }
    }

    /// Execute source code without running the interactive loop.
    /// Used by the CLI for --eval mode.
    pub fn execute(&mut self, source: &str) -> NuResult<Value> {
        self.evaluate(source)?;
        Ok(Value::unit())
    }

    /// Number of closure capture environments currently retained by the
    /// REPL's VM. Exposed for testing that `clear_closure_envs` keeps this
    /// bounded across repeated evaluations instead of growing forever.
    #[cfg(test)]
    pub(crate) fn closure_env_count(&self) -> usize {
        self.vm.closure_env_count()
    }

    /// The last evaluation's disassembled bytecode, if any. Exposed for
    /// testing which compiler backend actually ran (the two backends use
    /// different register-allocation schemes, so their disassembly differs
    /// even for trivial programs).
    #[cfg(test)]
    pub(crate) fn last_bytecode(&self) -> Option<&str> {
        self.last_bytecode.as_deref()
    }

    /// Track unmatched brace/paren/bracket openers using the lexer, which
    /// naturally skips strings and comments. Returns the stack of still-open
    /// delimiter characters (most recent last). Mismatched closers are
    /// dropped rather than tracked — the parser will report the error.
    /// Show the effect row of an expression.
    fn show_effect(&mut self, source: &str) -> NuResult<()> {
        let wrapped = if !source.contains("let ") && !source.contains("fn ") {
            format!("{}", source)
        } else {
            source.to_string()
        };
        let ast = parse_source(&wrapped)?;
        let expr = extract_main_expr(&ast)?;
        let mut combined_decls = self.accumulated_decls.clone();
        combined_decls.push(Decl::Function {
            name: "__main".to_string(),
            type_params: vec![],
            type_param_constraints: vec![],
            params: vec![],
            default_values: vec![],
            using_params: vec![],
            ret_type: None,
            error_type: None,
            effect: None,
            cap: None,
            requires: vec![],
            ensures: vec![],
            body: expr,
            annotations: vec![],
            public: false,
            span: Span::default(),
        });
        let module = AstModule {
            name: "effectcheck".to_string(),
            decls: combined_decls,
        };
        let _ty = self.type_checker.check_module(&module)?;
        let mut ec = EffectChecker::new();
        ec.register_function_rows(&flatten_decls(&module.decls))?;
        if let Some(row) = ec.function_row("__main") {
            println!("Effect row: {}", row);
        } else {
            println!("No effects.");
        }
        Ok(())
    }

    /// Show the capability of an expression.
    fn show_cap(&mut self, source: &str) -> NuResult<()> {
        let wrapped = if !source.contains("let ") && !source.contains("fn ") {
            format!("{}", source)
        } else {
            source.to_string()
        };
        let ast = parse_source(&wrapped)?;
        let expr = extract_main_expr(&ast)?;
        let mut cap_analyzer = CapabilityAnalyzer::new();
        let cap_ctx = CapContext::new();
        let cap = cap_analyzer.infer_cap(&cap_ctx, &expr)?;
        println!("Capability: {:?}", cap);
        Ok(())
    }

    /// Save REPL session to a file (declarations only).
    fn save_session(&self, path: &str) -> NuResult<()> {
        std::fs::write(path, &self.session_source).map_err(|e| {
            NuError::runtime_error(format!("save session failed: {}", e), Span::default())
        })?;
        println!("Session saved to {}", path);
        Ok(())
    }

    /// Load REPL session from a file.
    fn load_session(&mut self, path: &str) -> NuResult<()> {
        let source = std::fs::read_to_string(path).map_err(|e| {
            NuError::runtime_error(format!("load session failed: {}", e), Span::default())
        })?;
        self.evaluate(&source)?;
        println!("Session loaded from {}", path);
        Ok(())
    }
    fn brace_stack(source: &str) -> Vec<char> {
        let mut lexer = Lexer::new(source);
        let tokens = match lexer.lex() {
            Ok(ts) => ts,
            Err(_) => return Vec::new(), // Lex error → treat as "done"
        };
        let mut stack: Vec<char> = Vec::new();
        use crate::lexer::TokenKind;
        for tok in &tokens {
            match tok.kind {
                TokenKind::LParen => stack.push('('),
                TokenKind::LBrace => stack.push('{'),
                TokenKind::LBracket => stack.push('['),
                TokenKind::RParen => {
                    if stack.last() == Some(&'(') {
                        stack.pop();
                    }
                }
                TokenKind::RBrace => {
                    if stack.last() == Some(&'{') {
                        stack.pop();
                    }
                }
                TokenKind::RBracket => {
                    if stack.last() == Some(&'[') {
                        stack.pop();
                    }
                }
                _ => {}
            }
        }
        stack
    }
}

impl Default for Repl {
    fn default() -> Self {
        Self::new()
    }
}

/// Fallback REPL when rustyline can't initialize (no TTY, pipe, CI, etc.).
fn run_basic_repl() {
    println!(
        "Nulang v{} \u{2014} Actor-Based Distributed Language",
        env!("CARGO_PKG_VERSION")
    );
    println!("Line editing is not available in this environment.");
    println!("Use `nulang --eval '<code>'` to evaluate expressions, or `nulang <file.nula>` to run a file.");
    println!("Run `nulang --help` for all options.");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compile via the HIR/MIR pipeline.
fn compile_with_new_pipeline(
    ast: &AstModule,
    name: &str,
    type_checker: &TypeChecker,
) -> NuResult<crate::bytecode::CodeModule> {
    let hir = crate::hir_lower::lower_module(ast, &type_checker.inferred_decl_types);
    let mut mir = crate::mir_lower::lower_module(&hir)?;
    crate::mir_codegen::compile_mir(&mut mir, name)
}

/// Parse source code into an AST module.
fn parse_source(source: &str) -> NuResult<AstModule> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.lex()?;
    let mut parser = Parser::new(tokens);
    parser.parse_module()
}

/// Extract the body of the synthetic `__main` function, or return an error
/// if the module doesn't contain an expression.
fn extract_main_expr(ast: &AstModule) -> NuResult<Expr> {
    for decl in &ast.decls {
        if let Decl::Function { name, body, .. } = decl {
            if name == "__main" {
                return Ok(body.clone());
            }
        }
    }
    Err(NuError::parse_error(
        "Expected an expression".to_string(),
        Span::default(),
    ))
}

/// Convert a runtime Value to a pretty display string.
fn value_to_pretty_string(value: &Value) -> String {
    value.to_string_repr()
}

/// Extract the return type from a function type, or return the type as-is.
/// This unwraps the __main wrapper function's type (fn(()) -> T) to get
/// the actual expression type T.
fn extract_return_type(ty: &Type) -> &Type {
    match ty {
        Type::Function { ret, .. } => ret,
        _ => ty,
    }
}

/// Convert a Type to a human-readable string.
pub fn type_to_string(ty: &Type) -> String {
    match ty {
        Type::Var(v) => format!("'t{}", v.0),
        Type::Skolem(id) => format!("'sk{}", id),
        Type::Primitive(p) => match p {
            crate::types::PrimitiveType::Int => "Int".to_string(),
            crate::types::PrimitiveType::Float => "Float".to_string(),
            crate::types::PrimitiveType::Bool => "Bool".to_string(),
            crate::types::PrimitiveType::String => "String".to_string(),
            crate::types::PrimitiveType::Unit => "Unit".to_string(),
            crate::types::PrimitiveType::Nil => "Nil".to_string(),
            crate::types::PrimitiveType::Never => "Never".to_string(),
            crate::types::PrimitiveType::Address => "Address".to_string(),
        },
        Type::Tuple(ts) => {
            let parts: Vec<String> = ts.iter().map(type_to_string).collect();
            format!("({})", parts.join(", "))
        }
        Type::Record(fs) => {
            let parts: Vec<String> = fs
                .iter()
                .map(|(n, t)| format!("{}: {}", n, type_to_string(t)))
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
        Type::Variant(vs) => {
            let parts: Vec<String> = vs
                .iter()
                .map(|(n, t)| match t {
                    Some(t) => format!("{} {}", n, type_to_string(t)),
                    None => n.clone(),
                })
                .collect();
            format!("{}", parts.join(" | "))
        }
        Type::Array(t) => format!("[{}]", type_to_string(t)),
        Type::Function {
            param,
            ret,
            effect,
            cap,
        } => {
            let param_str = type_to_string(param);
            let ret_str = type_to_string(ret);
            let eff_str = if effect.effects().is_empty() {
                String::new()
            } else {
                format!(" !{:?}", effect)
            };
            let cap_str = if *cap == Capability::Ref {
                String::new()
            } else {
                format!(" :{:?}", cap)
            };
            format!("{} -> {}{}{}", param_str, ret_str, eff_str, cap_str)
        }
        Type::Actor { state, behavior } => {
            format!(
                "Actor[{}, {}]",
                type_to_string(state),
                type_to_string(behavior)
            )
        }
        Type::App { constructor, args } => {
            let cstr = type_to_string(constructor);
            let args_str: Vec<String> = args.iter().map(type_to_string).collect();
            format!("{}[{}]", cstr, args_str.join(", "))
        }
        Type::Reference { cap, inner } => {
            format!("&{:?} {}", cap, type_to_string(inner))
        }
        Type::Scheme { vars, body } => {
            let var_names: Vec<String> = vars.iter().map(|v| format!("'t{}", v.0)).collect();
            format!("forall {}. {}", var_names.join(", "), type_to_string(body))
        }
        Type::Nominal { name, .. } => name.clone(),
    }
}

/// Disassemble a CodeModule into a human-readable string.
fn disassemble_module(module: &crate::bytecode::CodeModule) -> String {
    use std::fmt::Write;
    let mut output = String::new();

    if !module.constants.is_empty() {
        writeln!(output, "Constants:").unwrap();
        for (i, c) in module.constants.iter().enumerate() {
            writeln!(output, "  {}: {:?}", i, c).unwrap();
        }
        writeln!(output).unwrap();
    }

    writeln!(output, "Instructions:").unwrap();
    for (i, instr) in module.instructions.iter().enumerate() {
        let op_name = format!("{:?}", instr.opcode);
        let comment = match instr.opcode {
            crate::bytecode::OpCode::ConstU => {
                let idx = instr.imm16();
                module
                    .constants
                    .get(idx as usize)
                    .map(|c| format!("; load {:?}", c))
            }
            crate::bytecode::OpCode::Call => Some(format!("; call R{}", instr.op1)),
            crate::bytecode::OpCode::Closure => Some(format!("; closure @{}", instr.imm16())),
            crate::bytecode::OpCode::Jmp
            | crate::bytecode::OpCode::JmpT
            | crate::bytecode::OpCode::JmpF => {
                Some(format!("; -> {}", i as i64 + instr.simm16() as i64))
            }
            _ => None,
        };

        match comment {
            Some(c) => writeln!(
                output,
                "  {:4}: {:12} {:3} {:3} {:3}    {}",
                i, op_name, instr.op1, instr.op2, instr.op3, c
            ),
            None => writeln!(
                output,
                "  {:4}: {:12} {:3} {:3} {:3}",
                i, op_name, instr.op1, instr.op2, instr.op3
            ),
        }
        .unwrap();
    }

    if !module.function_table.is_empty() {
        writeln!(output).unwrap();
        writeln!(output, "Function Table:").unwrap();
        for (i, offset) in module.function_table.iter().enumerate() {
            writeln!(output, "  {}: @{}", i, offset).unwrap();
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test: each REPL evaluation recompiles and reruns the full
    /// accumulated program from scratch, so no closure from a previous
    /// evaluation can still be reachable. Without `clear_closure_envs` in
    /// `evaluate`, every capturing closure ever created in a REPL session
    /// would accumulate in the VM forever.
    #[test]
    fn test_repl_does_not_leak_closure_envs_across_evaluations() {
        let mut repl = Repl::new();
        for _ in 0..20 {
            repl.execute("let a = 40 in let add = fn(x) { x + a } in add(2)")
                .unwrap();
        }
        assert!(
            repl.closure_env_count() <= 1,
            "closure envs should not accumulate across REPL evaluations, got {}",
            repl.closure_env_count()
        );
    }

    /// The REPL compiles through the HIR/MIR pipeline.
    #[test]
    fn test_repl_uses_mir_pipeline() {
        let mut repl = Repl::new();
        repl.execute("1 + 2").unwrap();
        assert!(repl.last_bytecode().unwrap().contains("Function Table"));
    }

    /// Regression test: the REPL effect check is interprocedural, matching
    /// the CLI frontend — a function declared `! {}` that calls an IO
    /// function must be rejected even when the IO function was defined in
    /// an earlier evaluation (the callee row comes from the accumulated
    /// module's function-row map).
    #[test]
    fn test_repl_rejects_pure_fn_calling_io_fn_across_evals() {
        let mut repl = Repl::new();
        repl.execute("fn do_io() -> Unit ! {IO} { perform IO.print(\"x\") }")
            .unwrap();
        let result = repl.execute("fn pure() -> Unit ! {} { do_io() }");
        assert!(
            matches!(result, Err(NuError::EffectError { .. })),
            "pure function calling an IO function must be rejected, got {:?}",
            result
        );
    }

    /// Regression test: same enforcement within a single input — and the
    /// offending declaration must not accumulate into the session state.
    #[test]
    fn test_repl_rejects_pure_fn_calling_io_fn_same_input() {
        let mut repl = Repl::new();
        let result = repl.execute(
            "fn do_io() -> Unit ! {IO} { perform IO.print(\"x\") }\n\
             fn pure() -> Unit ! {} { do_io() }",
        );
        assert!(
            matches!(result, Err(NuError::EffectError { .. })),
            "pure function calling an IO function must be rejected, got {:?}",
            result
        );
    }

    /// Positive control: functions staying within their declared rows still
    /// evaluate, and top-level IO expressions stay allowed in the REPL
    /// (`__main` carries no declared row, so it is inference-only).
    #[test]
    fn test_repl_accepts_matching_declared_effects() {
        let mut repl = Repl::new();
        repl.execute("fn do_io() -> Unit ! {IO} { perform IO.print(\"x\") }")
            .unwrap();
        repl.execute("fn caller() -> Unit ! {IO} { do_io() }")
            .unwrap();
        repl.execute("caller()").unwrap();
    }
}
