//! `nulang agent` subcommands.

use crate::types::{NuError, NuResult, Span};
use nulang_ai_local::{init_project, LocalRuntime};
use std::io::{self, Write};
use std::path::PathBuf;
use uuid::Uuid;

fn agent_err(msg: impl Into<String>) -> NuError {
    NuError::PackageError {
        msg: msg.into(),
        span: Span::default(),
    }
}

fn project_dir_from(args: &[String]) -> NuResult<PathBuf> {
    let mut dir: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--project" | "-p" => {
                i += 1;
                if i >= args.len() {
                    return Err(agent_err("missing value for --project"));
                }
                dir = Some(PathBuf::from(&args[i]));
            }
            other if !other.starts_with('-') && dir.is_none() => {
                dir = Some(PathBuf::from(other));
            }
            _ => {}
        }
        i += 1;
    }
    Ok(dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))))
}

pub fn run(args: &[String]) -> NuResult<()> {
    match args.first().map(String::as_str) {
        Some("init") => cmd_init(args.get(1..).unwrap_or(&[])),
        Some("run") => cmd_run(args.get(1..).unwrap_or(&[])),
        Some("chat") => cmd_chat(args.get(1..).unwrap_or(&[])),
        Some("goals") => cmd_goals(args.get(1..).unwrap_or(&[])),
        Some("graph") => cmd_graph(args.get(1..).unwrap_or(&[])),
        Some("mcp") => cmd_mcp(args.get(1..).unwrap_or(&[])),
        Some("help") | Some("-h") | Some("--help") | None => {
            print_help();
            Ok(())
        }
        Some(other) => Err(agent_err(format!(
            "unknown agent subcommand '{}'; try: init, run, chat, goals, graph",
            other
        ))),
    }
}

fn print_help() {
    eprintln!(
        "nulang agent — local NuLang Agent Runtime\n\n\
         Usage:\n\
           nulang agent init [--project <dir>]\n\
           nulang agent run  [--project <dir>] [--message <text>]\n\
           nulang agent chat [--project <dir>]\n\
           nulang agent goals [--project <dir>]\n\
           nulang agent graph --goal <uuid> [--project <dir>]\n           nulang agent mcp serve [--port <n>]\n"
    );
}

fn cmd_init(args: &[String]) -> NuResult<()> {
    let dir = project_dir_from(args)?;
    init_project(&dir).map_err(|e| agent_err(e.to_string()))?;
    println!("Initialized agent project at {}", dir.display());
    Ok(())
}

fn cmd_run(args: &[String]) -> NuResult<()> {
    let mut message: Option<String> = None;
    let mut passthrough = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--message" | "-m" => {
                i += 1;
                if i >= args.len() {
                    return Err(agent_err("missing value for --message"));
                }
                message = Some(args[i].clone());
            }
            other => passthrough.push(other.to_string()),
        }
        i += 1;
    }
    let dir = project_dir_from(&passthrough)?;
    let text =
        message.unwrap_or_else(|| "Explore the repository and summarize architecture".into());
    let mut rt = LocalRuntime::open(dir).map_err(|e| agent_err(e.to_string()))?;
    let mut out = io::stdout();
    rt.handle_user_message(&text, &mut out)
        .map_err(|e| agent_err(e.to_string()))?;
    Ok(())
}

fn cmd_chat(args: &[String]) -> NuResult<()> {
    let dir = project_dir_from(args)?;
    let mut rt = LocalRuntime::open(dir).map_err(|e| agent_err(e.to_string()))?;
    let stdin = io::stdin();
    let mut out = io::stdout();
    eprintln!("NuLang Agent chat (Ctrl-D to exit)");
    loop {
        eprint!("> ");
        let _ = io::stderr().flush();
        let mut line = String::new();
        if stdin.read_line(&mut line).ok().filter(|n| *n > 0).is_none() {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "exit" || trimmed == "quit" {
            break;
        }
        rt.handle_user_message(trimmed, &mut out)
            .map_err(|e| agent_err(e.to_string()))?;
    }
    Ok(())
}

fn cmd_goals(args: &[String]) -> NuResult<()> {
    let dir = project_dir_from(args)?;
    let rt = LocalRuntime::open(dir).map_err(|e| agent_err(e.to_string()))?;
    let goals = rt
        .store()
        .list_goals()
        .map_err(|e| agent_err(e.to_string()))?;
    if goals.is_empty() {
        println!("No goals yet.");
        return Ok(());
    }
    for g in goals {
        println!("{}  {:?}  {}", g.id, g.status, g.intent);
    }
    Ok(())
}

fn cmd_graph(args: &[String]) -> NuResult<()> {
    let mut goal_id: Option<Uuid> = None;
    let mut passthrough = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--goal" | "-g" => {
                i += 1;
                if i >= args.len() {
                    return Err(agent_err("missing value for --goal"));
                }
                goal_id = Some(Uuid::parse_str(&args[i]).map_err(|e| agent_err(e.to_string()))?);
            }
            other => passthrough.push(other.to_string()),
        }
        i += 1;
    }
    let gid = goal_id.ok_or_else(|| agent_err("--goal <uuid> is required"))?;
    let dir = project_dir_from(&passthrough)?;
    let rt = LocalRuntime::open(dir).map_err(|e| agent_err(e.to_string()))?;
    let graph = rt
        .store()
        .get_goal_graph(gid)
        .map_err(|e| agent_err(e.to_string()))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&graph).map_err(|e| agent_err(e.to_string()))?
    );
    Ok(())
}

fn cmd_mcp(args: &[String]) -> NuResult<()> {
    match args.first().map(String::as_str) {
        Some("serve") => {
            let port = args
                .iter()
                .position(|a| a == "--port")
                .and_then(|i| args.get(i + 1))
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(8765);
            eprintln!(
                "MCP stub listening on 127.0.0.1:{} (stdio/json-rpc wiring in P2.1)",
                port
            );
            Ok(())
        }
        _ => Err(agent_err("usage: nulang agent mcp serve [--port <n>]")),
    }
}
