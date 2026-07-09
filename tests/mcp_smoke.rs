//! Stdio smoke test for the `malkuth mcp` MCP server.
//!
//! Boots the server as a subprocess, performs the `initialize` handshake, then
//! issues `tools/list` and asserts the supervision + probe tools are
//! advertised. It does *not* launch a real supervisor (that would spawn child
//! processes); the point is to prove the server starts, speaks the protocol,
//! and exposes the surface.
//!
//! Requires the `cli` feature (to build the binary) and the `mcp` feature.

#![cfg(all(feature = "mcp", feature = "cli"))]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn malkuth_binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_malkuth"))
}

fn write_msg<W: Write>(w: &mut W, id: Option<u64>, method: &str, params: serde_json::Value) {
    let mut msg = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    if let Some(id) = id {
        msg["id"] = serde_json::json!(id);
    }
    serde_json::to_writer(&mut *w, &msg).unwrap();
    writeln!(&mut *w).unwrap();
    w.flush().unwrap();
}

fn read_response<R: BufRead>(r: &mut R, want_id: u64, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    let mut line = String::new();
    loop {
        if Instant::now() > deadline {
            panic!("timed out waiting for response id={want_id}");
        }
        line.clear();
        let n = r.read_line(&mut line).expect("read line");
        if n == 0 {
            panic!("server closed stdout before responding to id={want_id}");
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("id").and_then(|i| i.as_u64()) == Some(want_id) {
            return v;
        }
    }
}

#[test]
#[allow(clippy::zombie_processes)] // best-effort teardown: try_wait then kill
fn mcp_server_lists_supervision_tools() {
    let bin = malkuth_binary();
    let mut child = Command::new(&bin)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn `malkuth mcp` ({bin:?}): {e}"));

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    write_msg(
        &mut stdin,
        Some(1),
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "malkuth-mcp-smoke", "version": "0.0.0" },
        }),
    );
    let init = read_response(&mut stdout, 1, Duration::from_secs(20));
    assert_eq!(
        init["result"]["protocolVersion"].as_str(),
        Some("2024-11-05"),
        "initialize response: {init}"
    );
    assert!(
        init["result"]["capabilities"].get("tools").is_some(),
        "server did not advertise tools capability: {init}"
    );

    write_msg(
        &mut stdin,
        None,
        "notifications/initialized",
        serde_json::json!({}),
    );

    write_msg(&mut stdin, Some(2), "tools/list", serde_json::json!({}));
    let list = read_response(&mut stdout, 2, Duration::from_secs(20));
    let tools: Vec<String> = list["result"]["tools"]
        .as_array()
        .expect("tools is an array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();

    for expected in ["malkuth_supervise", "malkuth_probe"] {
        assert!(
            tools.iter().any(|t| t == expected),
            "missing tool `{expected}`; got: {tools:?}"
        );
    }

    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if child.try_wait().map(|o| o.is_some()).unwrap_or(false) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
}
