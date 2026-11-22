use anyhow::{Context, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use clap::Parser;
use cryo_vault::{lock::CryoLock, schema::ChatSessionV1, storage::Storage};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

#[derive(Parser)]
#[command(about = "Browse your Cryo Vault archive in a local browser")]
struct Args {
    #[arg(long, env = "CRYO_DB_PATH")]
    db: Option<PathBuf>,
    /// Local port; 0 selects an available port.
    #[arg(long, default_value_t = 0)]
    port: u16,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let db = args
        .db
        .or_else(|| dirs::home_dir().map(|p| p.join(".cryo")))
        .context("Cannot determine home directory; specify --db")?;
    anyhow::ensure!(
        db.is_dir(),
        "Archive directory does not exist: {}",
        db.display()
    );
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, args.port)).context("Cannot bind local viewer")?;
    let port = listener.local_addr()?.port();
    let token = Uuid::new_v4().to_string();
    println!(
        "CryoVault viewer: http://127.0.0.1:{port}/#{token}\nOpen this address in your browser. Press Ctrl+C to stop."
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let db = db.clone();
                let token = token.clone();
                std::thread::spawn(move || {
                    let _ = handle(stream, &db, port, &token);
                });
            }
            Err(e) => eprintln!("Connection error: {e}"),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, db: &Path, port: u16, token: &str) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut request = Vec::new();
    let mut chunk = [0; 2048];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        request.extend_from_slice(&chunk[..n]);
        if request.len() > 16384 {
            return respond(&mut stream, 431, "text/plain", b"Request too large");
        }
    }
    let request = std::str::from_utf8(&request).context("Invalid request")?;
    let mut lines = request.split("\r\n");
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let method = first.next().unwrap_or("");
    let target = first.next().unwrap_or("");
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let host = format!("127.0.0.1:{port}");
    if headers.get("host") != Some(&host)
        || headers
            .get("origin")
            .is_some_and(|o| o != &format!("http://{host}"))
    {
        return respond(&mut stream, 403, "text/plain", b"Forbidden");
    }
    if method != "GET" {
        return respond(&mut stream, 405, "text/plain", b"Method not allowed");
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (status, content_type, body) = match path {
        "/" => (
            200,
            "text/html; charset=utf-8",
            include_bytes!("../../ui/index.html").to_vec(),
        ),
        "/style.css" => (
            200,
            "text/css; charset=utf-8",
            include_bytes!("../../ui/style.css").to_vec(),
        ),
        "/app.js" => (
            200,
            "text/javascript; charset=utf-8",
            include_bytes!("../../ui/app.js").to_vec(),
        ),
        p if p.starts_with("/api/") => {
            if headers.get("x-cryo-token").map(String::as_str) != Some(token) {
                return respond(&mut stream, 403, "text/plain", b"Forbidden");
            }
            let result = (|| -> Result<Value> {
                let _lock = CryoLock::acquire(db, 5000)?;
                let store = Storage::new(db.to_owned());
                api(&store, p, &parse_query(query)?)
            })();
            match result {
                Ok(v) => (200, "application/json", serde_json::to_vec(&v)?),
                Err(e) => (
                    500,
                    "application/json",
                    serde_json::to_vec(&json!({"error":e.to_string()}))?,
                ),
            }
        }
        _ => (404, "text/plain", b"Not found".to_vec()),
    };
    respond(&mut stream, status, content_type, &body)
}

fn respond(stream: &mut TcpStream, status: u16, kind: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'none'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    Ok(())
}

fn parse_query(query: &str) -> Result<HashMap<String, String>> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Ok((decode(k)?, decode(v)?))
        })
        .collect()
}
fn decode(s: &str) -> Result<String> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3])?;
                out.push(u8::from_str_radix(h, 16)?);
                i += 3;
            }
            b'%' => anyhow::bail!("Invalid URL encoding"),
            b'+' => {
                out.push(b' ');
                i += 1
            }
            x => {
                out.push(x);
                i += 1
            }
        }
    }
    Ok(String::from_utf8(out)?)
}
fn timestamp(s: &str, end: bool) -> Result<u64> {
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").context("Invalid date")?;
    let time = if end {
        d.and_hms_opt(23, 59, 59)
    } else {
        d.and_hms_opt(0, 0, 0)
    }
    .context("Invalid date")?;
    Ok(Utc.from_utc_datetime(&time).timestamp().try_into()?)
}
fn api(store: &Storage, path: &str, q: &HashMap<String, String>) -> Result<Value> {
    match path {
        "/api/stats" => Ok(serde_json::to_value(store.get_stats()?)?),
        "/api/sources" => {
            let mut sources: Vec<_> = store
                .scan_all()?
                .into_iter()
                .filter_map(|s| s.source)
                .collect();
            sources.sort();
            sources.dedup();
            Ok(json!(sources))
        }
        "/api/session" => {
            let id = q.get("id").context("Missing session ID")?;
            Ok(serde_json::to_value(store.get_session_by_id(id)?)?)
        }
        "/api/sessions" => {
            let query = q.get("q").map(|s| s.trim()).unwrap_or("");
            let after = q
                .get("after")
                .filter(|s| !s.is_empty())
                .map(|s| timestamp(s, false))
                .transpose()?;
            let before = q
                .get("before")
                .filter(|s| !s.is_empty())
                .map(|s| timestamp(s, true))
                .transpose()?;
            // Include titles as well as message text; the archive's indexed
            // search searches message bodies only.
            let mut sessions = store.scan_all()?;
            let needle = query.to_lowercase();
            let source = q.get("source").map(String::as_str).unwrap_or("");
            sessions.retain(|s| {
                (needle.is_empty()
                    || s.title
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&needle)
                    || s.messages
                        .iter()
                        .any(|m| m.content.to_lowercase().contains(&needle)))
                    && (source.is_empty() || s.source.as_deref().unwrap_or("") == source)
                    && after.is_none_or(|v| s.created_at.is_some_and(|t| t >= v))
                    && before.is_none_or(|v| s.created_at.is_some_and(|t| t <= v))
            });
            sessions.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            let offset = q
                .get("offset")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            let total = sessions.len();
            let items: Vec<_> = sessions
                .into_iter()
                .skip(offset)
                .take(50)
                .map(summary)
                .collect();
            Ok(json!({"total":total,"items":items}))
        }
        _ => anyhow::bail!("Unknown endpoint"),
    }
}
fn summary(s: ChatSessionV1) -> Value {
    let preview = s
        .messages
        .iter()
        .find(|m| !m.content.trim().is_empty())
        .map(|m| m.content.chars().take(160).collect::<String>())
        .unwrap_or_default();
    json!({"id":s.id,"title":s.title,"source":s.source,"model":s.model,"created_at":s.created_at,"message_count":s.messages.len(),"preview":preview})
}

#[cfg(test)]
mod tests {
    use super::*;
    use cryo_vault::schema::{MessageRole, MessageV1};
    use tempfile::TempDir;

    fn setup() -> (TempDir, Storage) {
        let dir = TempDir::new().unwrap();
        let store = Storage::new(dir.path().to_path_buf());
        for (id, title, source, ts, text) in [
            (
                "first",
                "Archive review",
                "codex",
                1_700_000_000,
                "hello apple",
            ),
            (
                "second",
                "Testing notes",
                "cursor",
                1_700_100_000,
                "pear <script>alert(1)</script>",
            ),
        ] {
            store
                .append_session(ChatSessionV1 {
                    id: id.into(),
                    title: Some(title.into()),
                    source: Some(source.into()),
                    model: None,
                    created_at: Some(ts),
                    metadata_json: "{}".into(),
                    messages: vec![MessageV1 {
                        role: MessageRole::User,
                        content: text.into(),
                        tool_calls: None,
                        tool_outputs: None,
                        id: None,
                        parent_id: None,
                        metadata_json: "{}".into(),
                    }],
                })
                .unwrap();
        }
        (dir, store)
    }
    fn q(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    #[test]
    fn search_filter_detail_and_stats() {
        let (_dir, store) = setup();
        let all = api(&store, "/api/sessions", &q(&[])).unwrap();
        assert_eq!(all["total"], 2);
        assert_eq!(all["items"][0]["id"], "second");
        assert_eq!(
            api(&store, "/api/sessions", &q(&[("q", "archive")])).unwrap()["items"][0]["id"],
            "first"
        );
        assert_eq!(
            api(
                &store,
                "/api/sessions",
                &q(&[("q", "pear"), ("source", "cursor")])
            )
            .unwrap()["total"],
            1
        );
        assert_eq!(
            api(
                &store,
                "/api/sessions",
                &q(&[("q", "pear"), ("source", "codex")])
            )
            .unwrap()["total"],
            0
        );
        assert_eq!(
            api(&store, "/api/sessions", &q(&[("after", "2023-11-16")])).unwrap()["total"],
            1
        );
        assert_eq!(
            api(&store, "/api/session", &q(&[("id", "first")])).unwrap()["title"],
            "Archive review"
        );
        assert!(
            api(&store, "/api/session", &q(&[("id", "missing")]))
                .unwrap()
                .is_null()
        );
        assert_eq!(
            api(&store, "/api/stats", &q(&[])).unwrap()["session_count"],
            2
        );
        assert_eq!(
            api(&store, "/api/sources", &q(&[])).unwrap(),
            json!(["codex", "cursor"])
        );
        assert!(api(&store, "/api/sessions", &q(&[("after", "bad")])).is_err());
    }
    #[test]
    fn url_decoding_and_html_are_safe() {
        assert_eq!(
            parse_query("id=a%2Fb%2Bc&q=hello+world").unwrap()["id"],
            "a/b+c"
        );
        assert!(parse_query("id=%zz").is_err());
        let js = include_str!("../../ui/app.js");
        assert!(!js.contains("innerHTML"));
        assert!(js.contains("textContent"));
    }
    #[test]
    fn http_rejects_wrong_host_and_missing_token() {
        use std::net::TcpListener;
        let (dir, _store) = setup();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = dir.path().to_owned();
        let worker = std::thread::spawn(move || {
            for s in listener.incoming().take(3) {
                handle(s.unwrap(), &path, port, "secret").unwrap();
            }
        });
        let request = |host: &str, token: &str| {
            let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            write!(
                s,
                "GET /api/stats HTTP/1.1\r\nHost: {host}\r\nX-Cryo-Token: {token}\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            s.read_to_string(&mut response).unwrap();
            response
        };
        assert!(request(&format!("127.0.0.1:{port}"), "wrong").starts_with("HTTP/1.1 403"));
        assert!(request("evil.example", "secret").starts_with("HTTP/1.1 403"));
        assert!(request(&format!("127.0.0.1:{port}"), "secret").starts_with("HTTP/1.1 200"));
        worker.join().unwrap();
    }
}
