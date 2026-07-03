//! A read-only stdio MCP server: an agent can SEE which account is active but
//! can NEVER switch it. Hand-rolled newline-delimited JSON-RPC 2.0 (mirrors the
//! sessionwiki MCP). Two tools, both readOnlyHint: `whoami`, `list_accounts`.
//! Field allowlist + secret-free errors (A13); no switch/add/use tool exists.

use crate::paths::Paths;
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};

const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const KNOWN_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const LATEST_VERSION: &str = "2025-11-25";
const MAX_LINE_BYTES: u64 = 1024 * 1024;

fn ok_response(id: Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}
fn err_response(id: Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

pub(crate) fn handle_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Some(err_response(Value::Null, -32700, "parse error")),
    };
    if msg.is_array() {
        return Some(err_response(
            Value::Null,
            -32600,
            "batch requests are not supported",
        ));
    }
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").cloned();
    match (method, id) {
        (Some(method), Some(id)) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let id2 = id.clone();
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                dispatch(method, &params, id)
            }));
            Some(out.unwrap_or_else(|_| err_response(id2, -32603, "internal error")))
        }
        _ => None,
    }
}

fn dispatch(method: &str, params: &Value, id: Value) -> String {
    match method {
        "initialize" => ok_response(id, initialize_result(params)),
        "ping" => ok_response(id, json!({})),
        "tools/list" => ok_response(id, tools_list()),
        "tools/call" => match tool_call(params) {
            Ok(result) => ok_response(id, result),
            Err((code, msg)) => err_response(id, code, &msg),
        },
        other => err_response(id, -32601, &format!("Method not found: {other}")),
    }
}

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = if KNOWN_VERSIONS.contains(&requested) {
        requested
    } else {
        LATEST_VERSION
    };
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "swapdex", "version": SERVER_VERSION},
    })
}

fn tools_list() -> Value {
    json!({"tools": [
        {
            "name": "whoami",
            "title": "Which account is active",
            "description": "The active Claude Code / Codex account per tool (read-only).",
            "annotations": {"readOnlyHint": true},
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "list_accounts",
            "title": "List saved account profiles",
            "description": "The names of saved account profiles and which is active (read-only, no credentials).",
            "annotations": {"readOnlyHint": true},
            "inputSchema": {"type": "object", "properties": {}}
        }
    ]})
}

fn text_result(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": false})
}

fn tool_call(params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    // Every internal failure maps to one fixed, secret-free string (A13).
    let paths =
        Paths::resolve().map_err(|_| (-32603i64, "account data unavailable".to_string()))?;
    match name {
        "whoami" => Ok(whoami(&paths)),
        "list_accounts" => Ok(list_accounts(&paths)),
        other => Err((-32602, format!("Unknown tool: {other}"))),
    }
}

fn whoami(paths: &Paths) -> Value {
    let mut out = Vec::new();
    for adapter in crate::adapters::all() {
        // whoami MAY show email (opt-in); never a token, uuid, or path.
        if let Ok(Some(id)) = adapter.identity(paths) {
            out.push(json!({
                "tool": id.tool,
                "display": id.display,
                "email": id.email,
                "tier": id.tier,
            }));
        } else {
            out.push(json!({"tool": adapter.name(), "display": "not logged in"}));
        }
    }
    text_result(serde_json::to_string(&out).unwrap_or_else(|_| "[]".into()))
}

fn list_accounts(paths: &Paths) -> Value {
    let store = match crate::store::Store::open(paths) {
        Ok(s) => s,
        Err(_) => return text_result("[]".into()),
    };
    // Allowlist: name, tools, active. NEVER email/uuid/path/token (A13).
    let active: Vec<String> = crate::adapters::all()
        .iter()
        .filter_map(|a| {
            a.identity(paths)
                .ok()
                .flatten()
                .map(|id| (a.name().to_string(), id.account_id))
        })
        .filter_map(|(tool, acct)| crate::commands::matched_profile_name(&store, &tool, &acct))
        .collect();
    let rows: Vec<Value> = store
        .list()
        .iter()
        .map(|p| json!({"name": p.name, "tools": p.tools, "active": active.contains(&p.name)}))
        .collect();
    text_result(serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into()))
}

pub fn serve() {
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        let mut buf = Vec::new();
        let n = match (&mut reader)
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut buf)
        {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if buf.last() != Some(&b'\n') && n as u64 == MAX_LINE_BYTES {
            let mut discard = Vec::new();
            let _ = reader.read_until(b'\n', &mut discard);
            let msg = err_response(Value::Null, -32700, "message too large");
            if write_line(&mut out, &msg).is_err() {
                break;
            }
            continue;
        }
        let line = String::from_utf8_lossy(&buf);
        if let Some(reply) = handle_line(&line) {
            if write_line(&mut out, &reply).is_err() {
                break;
            }
        }
    }
}

fn write_line<W: Write>(out: &mut W, msg: &str) -> std::io::Result<()> {
    out.write_all(msg.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(line: &str) -> Option<Value> {
        handle_line(line).map(|s| {
            assert!(!s.contains('\n'));
            serde_json::from_str(&s).unwrap()
        })
    }

    #[test]
    fn tools_list_is_read_only_and_has_no_switch_tool() {
        let v = call(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        let tools = v["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["whoami", "list_accounts"]);
        for t in tools {
            assert_eq!(t["annotations"]["readOnlyHint"], true);
        }
        // No mutating tool exists.
        for banned in ["use", "switch", "add", "rm", "apply"] {
            assert!(
                !names.contains(&banned),
                "mutating tool {banned} must not exist"
            );
        }
    }

    #[test]
    fn initialize_and_ping() {
        let v = call(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#).unwrap();
        assert_eq!(v["result"]["serverInfo"]["name"], "swapdex");
        assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
        let p = call(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#).unwrap();
        assert_eq!(p["result"], json!({}));
    }

    #[test]
    fn notification_and_unknown() {
        assert!(call(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        let v = call(r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#).unwrap();
        assert_eq!(v["error"]["code"], -32601);
    }
}
