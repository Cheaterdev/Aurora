//! A tiny stand-in for a coding agent, used by the runner's tests.
//!
//! It reads standard input to the end and prints it, then the names (never
//! the values) of the `AURORA_*` variables it got, the issue and run id, its
//! arguments and working folder. Flags, in any order:
//!
//! * `--sleep <seconds>`  wait (fractions allowed) after printing
//! * `--exit <code>`      exit with this code
//! * `--flood <bytes>`    write that many bytes of filler, then `TAIL-MARKER`
//! * `--comment <text>`   call the server's MCP `add_comment` tool on
//!   `AURORA_ISSUE` with `AURORA_API_KEY`, to prove the per-run key works
//!
//! It never prints the key.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

fn mcp_comment(text: &str) -> Result<String, String> {
    let url = std::env::var("AURORA_MCP_URL").map_err(|_| "no AURORA_MCP_URL")?;
    let key = std::env::var("AURORA_API_KEY").map_err(|_| "no AURORA_API_KEY")?;
    let issue = std::env::var("AURORA_ISSUE").map_err(|_| "no AURORA_ISSUE")?;
    let rest = url.strip_prefix("http://").ok_or("not an http url")?;
    let (host, path) = rest.split_once('/').ok_or("no path")?;
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "add_comment", "arguments": {"issue": issue, "body": text}}
    })
    .to_string();
    let mut stream = TcpStream::connect(host).map_err(|e| e.to_string())?;
    let request = format!(
        "POST /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\
         Authorization: Bearer {key}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).map_err(|e| e.to_string())?;
    let status = raw.split(' ').nth(1).unwrap_or("?").to_string();
    let isolated = raw.split("\r\n\r\n").nth(1).unwrap_or("");
    Ok(format!(
        "http {status} error={}",
        isolated.contains("\"isError\":true") || status != "200"
    ))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    println!("STDIN-BEGIN\n{input}\nSTDIN-END");
    let mut names: Vec<String> = std::env::vars()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with("AURORA_"))
        .collect();
    names.sort();
    println!("ENV: {}", names.join(","));
    for name in [
        "AURORA_ISSUE",
        "AURORA_RUN_ID",
        "AURORA_TRIGGER",
        "AURORA_PROJECT",
        "AURORA_URL",
    ] {
        println!("{name}={}", std::env::var(name).unwrap_or_default());
    }
    println!("ARGS: {}", args.join("|"));
    println!(
        "CWD: {}",
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    let _ = std::io::stdout().flush();

    let mut at = 0;
    let mut code = 0;
    while at < args.len() {
        let value = args.get(at + 1).cloned().unwrap_or_default();
        match args[at].as_str() {
            "--sleep" => {
                let seconds: f64 = value.parse().unwrap_or(0.0);
                std::thread::sleep(Duration::from_secs_f64(seconds));
            }
            "--exit" => code = value.parse().unwrap_or(1),
            "--flood" => {
                let total: usize = value.parse().unwrap_or(0);
                let line = "x".repeat(99) + "\n";
                let mut out = std::io::stdout();
                let mut written = 0;
                while written < total {
                    let _ = out.write_all(line.as_bytes());
                    written += line.len();
                }
                let _ = writeln!(out, "TAIL-MARKER");
                let _ = out.flush();
            }
            "--comment" => match mcp_comment(&value) {
                Ok(result) => println!("COMMENT: {result}"),
                Err(why) => println!("COMMENT-FAILED: {why}"),
            },
            _ => {
                at += 1;
                continue;
            }
        }
        at += 2;
    }
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
