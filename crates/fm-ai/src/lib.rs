//! fm-ai: blocking client for the AI engine (Python service). Protocol: newline-delimited JSON over a
//! Unix socket, one request → one response, `{"id", "method", "params"}` / `{"id", "result"}|{"id", "error"}`.
//! Connections are pooled, so the client is `Sync` and can be shared by worker threads.
//! Every failure to reach the engine maps to [`AiError::Unavailable`]: callers degrade, they don't crash.

use fm_types::*;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// engine not running, socket missing, or Ollama behind it is down
    #[error("AI engine unavailable: {0}")]
    Unavailable(String),
    #[error("AI request timed out")]
    Timeout,
    #[error("AI engine error [{code}]: {message}")]
    Remote { code: String, message: String },
    #[error("AI protocol error: {0}")]
    Protocol(String),
}

impl AiError {
    pub fn is_unavailable(&self) -> bool {
        matches!(self, AiError::Unavailable(_))
    }
}

type Conn = BufReader<UnixStream>;

pub struct AiClient {
    socket: PathBuf,
    /// timeout for slow calls (analysis, prompt parsing)
    timeout: Duration,
    idle: Mutex<Vec<Conn>>,
    next_id: AtomicU64,
}

impl AiClient {
    pub fn new(socket: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self { socket: socket.into(), timeout, idle: Mutex::new(vec![]), next_id: AtomicU64::new(1) }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn connect(&self) -> Result<Conn, AiError> {
        UnixStream::connect(&self.socket)
            .map(BufReader::new)
            .map_err(|e| AiError::Unavailable(format!("{}: {e}", self.socket.display())))
    }

    fn roundtrip(conn: &mut Conn, line: &str, timeout: Duration) -> Result<String, std::io::Error> {
        let s = conn.get_ref();
        s.set_read_timeout(Some(timeout))?;
        s.set_write_timeout(Some(Duration::from_secs(10)))?;
        conn.get_mut().write_all(line.as_bytes())?;
        let mut resp = String::new();
        if conn.read_line(&mut resp)? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "engine closed the connection"));
        }
        Ok(resp)
    }

    fn call_raw(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, AiError> {
        let id = self.next_id.fetch_add(1, Relaxed);
        let mut line = serde_json::to_string(&json!({"id": id, "method": method, "params": params})).map_err(|e| AiError::Protocol(e.to_string()))?;
        line.push('\n');

        // a pooled connection may have gone stale (engine restarted): retry once on a fresh one
        let mut reused = self.idle.lock().unwrap().pop();
        let (resp, conn) = loop {
            let was_reused = reused.is_some();
            let mut conn = match reused.take() {
                Some(c) => c,
                None => self.connect()?,
            };
            match Self::roundtrip(&mut conn, &line, timeout) {
                Ok(r) => break (r, conn),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return Err(AiError::Timeout),
                Err(_) if was_reused => continue,
                Err(e) => return Err(AiError::Unavailable(e.to_string())),
            }
        };
        self.idle.lock().unwrap().push(conn);

        let v: Value = serde_json::from_str(&resp).map_err(|e| AiError::Protocol(format!("bad response: {e}")))?;
        if v.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(AiError::Protocol("response id mismatch".into()));
        }
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            let code = err.get("code").and_then(Value::as_str).unwrap_or("error").to_string();
            let message = err.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            return Err(if code == "ollama_unavailable" { AiError::Unavailable(message) } else { AiError::Remote { code, message } });
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    fn call<T: DeserializeOwned>(&self, method: &str, params: Value, timeout: Duration) -> Result<T, AiError> {
        serde_json::from_value(self.call_raw(method, params, timeout)?).map_err(|e| AiError::Protocol(format!("{method}: {e}")))
    }

    /// Cheap liveness check of the engine process itself (does not need Ollama).
    pub fn ping(&self) -> Result<(), AiError> {
        self.call_raw("ping", json!({}), Duration::from_secs(2)).map(|_| ())
    }

    /// Engine + Ollama health and model availability.
    pub fn status(&self) -> Result<ServiceStatus, AiError> {
        self.call("status", json!({}), Duration::from_secs(8))
    }

    pub fn analyze_file(&self, req: &AnalyzeRequest) -> Result<AnalysisResult, AiError> {
        self.call("analyze_file", json!(req), self.timeout)
    }

    pub fn embed(&self, texts: &[String]) -> Result<EmbedResponse, AiError> {
        self.call("embed", json!({ "texts": texts }), self.timeout)
    }

    /// Natural-language sorting instruction → validated rule set.
    pub fn parse_prompt(&self, prompt: &str, known_categories: &[String]) -> Result<RuleSet, AiError> {
        self.call("parse_prompt", json!({ "prompt": prompt, "known_categories": known_categories }), self.timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A scripted fake engine: `handler(method, params) -> Result<result, (code, message)>`.
    fn fake_engine(dir: &Path, handler: impl Fn(&str, &Value) -> Result<Value, (String, String)> + Send + Sync + 'static) -> PathBuf {
        let sock = dir.join("ai.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let handler = std::sync::Arc::new(handler);
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let h = handler.clone();
                std::thread::spawn(move || {
                    let mut r = BufReader::new(s.try_clone().unwrap());
                    let mut w = s;
                    let mut line = String::new();
                    while r.read_line(&mut line).unwrap_or(0) > 0 {
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let resp = match h(req["method"].as_str().unwrap(), &req["params"]) {
                            Ok(v) => json!({"id": req["id"], "result": v}),
                            Err((c, m)) => json!({"id": req["id"], "error": {"code": c, "message": m}}),
                        };
                        writeln!(w, "{resp}").unwrap();
                        line.clear();
                    }
                });
            }
        });
        sock
    }

    #[test]
    fn missing_socket_is_unavailable_not_a_panic() {
        let c = AiClient::new("/nonexistent/ai.sock", Duration::from_secs(1));
        assert!(c.ping().unwrap_err().is_unavailable());
        assert!(c.embed(&["x".into()]).unwrap_err().is_unavailable());
    }

    #[test]
    fn roundtrips_typed_calls() {
        let t = tempfile::tempdir().unwrap();
        let sock = fake_engine(t.path(), |m, p| match m {
            "ping" => Ok(json!({"pong": true})),
            "embed" => Ok(json!({"model": "m", "vectors": [[1.0, 0.0], [0.0, 1.0]], "n": p["texts"].as_array().unwrap().len()})),
            "analyze_file" => Ok(json!({"category": "Docs", "tags": ["a"], "summary": "s", "model": "x"})),
            "parse_prompt" => Ok(json!({"name": "p", "rules": [{"dest": "{year}"}]})),
            _ => Err(("unknown_method".into(), m.into())),
        });
        let c = AiClient::new(sock, Duration::from_secs(5));
        c.ping().unwrap();
        assert_eq!(c.embed(&["a".into(), "b".into()]).unwrap().vectors.len(), 2);
        let r = c.analyze_file(&AnalyzeRequest { path: "/x".into(), ..Default::default() }).unwrap();
        assert_eq!((r.category.as_str(), r.tags.len()), ("Docs", 1));
        assert_eq!(c.parse_prompt("by year", &[]).unwrap().rules[0].dest, "{year}");
    }

    #[test]
    fn remote_errors_are_mapped() {
        let t = tempfile::tempdir().unwrap();
        let sock = fake_engine(t.path(), |m, _| match m {
            "analyze_file" => Err(("ollama_unavailable".into(), "connection refused".into())),
            _ => Err(("model_missing".into(), "run: ollama pull llava".into())),
        });
        let c = AiClient::new(sock, Duration::from_secs(5));
        assert!(c.analyze_file(&AnalyzeRequest::default()).unwrap_err().is_unavailable());
        match c.embed(&[]).unwrap_err() {
            AiError::Remote { code, message } => assert_eq!((code.as_str(), message.contains("ollama pull")), ("model_missing", true)),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn slow_engine_times_out() {
        let t = tempfile::tempdir().unwrap();
        let sock = fake_engine(t.path(), |_, _| {
            std::thread::sleep(Duration::from_millis(600));
            Ok(json!({}))
        });
        let c = AiClient::new(sock, Duration::from_millis(100));
        assert!(matches!(c.embed(&[]), Err(AiError::Timeout)));
    }

    #[test]
    fn reconnects_after_engine_restart() {
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("ai.sock");
        let serve = |sock: &Path, once: bool| {
            let l = UnixListener::bind(sock).unwrap();
            std::thread::spawn(move || {
                for s in l.incoming().flatten() {
                    let mut r = BufReader::new(s.try_clone().unwrap());
                    let mut w = s;
                    let mut line = String::new();
                    while r.read_line(&mut line).unwrap_or(0) > 0 {
                        let req: Value = serde_json::from_str(&line).unwrap();
                        writeln!(w, "{}", json!({"id": req["id"], "result": {"ok": true}})).unwrap();
                        line.clear();
                        if once {
                            return; // drop the connection after one answer: pooled conn goes stale
                        }
                    }
                    if once {
                        return;
                    }
                }
            })
        };
        let h = serve(&sock, true);
        let c = AiClient::new(&sock, Duration::from_secs(2));
        c.ping().unwrap();
        h.join().unwrap();
        std::fs::remove_file(&sock).unwrap();
        let _h2 = serve(&sock, false);
        c.ping().unwrap(); // stale pooled connection → transparent retry on a fresh one
    }
}
