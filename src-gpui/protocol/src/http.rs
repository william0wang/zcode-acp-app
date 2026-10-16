//! Async HTTP transport abstraction — the Rust port of the TS client's
//! `fetch` usage (`src/lib/hub.ts`). The hub REST client is written against
//! [`HttpTransport`]; production wiring supplies an implementation over a
//! real HTTP stack, tests install scripted/behavioral doubles (the exact
//! role `globalThis.fetch` replacement plays in the vitest suites).
//!
//! Cancellation mirrors `AbortController`: the deadline watcher fires the
//! [`AbortSignal`] and a well-behaved transport settles its in-flight future
//! promptly. A transport that ignores the signal (the "zombie" case the
//! reconnect-wedge regression covers) leaves the await dangling on purpose —
//! that wedge is what the supervisor's watchdog exists for.

use std::sync::Arc;

use serde_json::Value;

use crate::clock::BoxFuture;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
}

impl HttpMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
        }
    }
}

/// One outbound request. Headers carry the bearer token; `body` is an
/// already-serialized JSON string (present only on writes, mirroring the TS
/// client's `Content-Type` coupling).
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub url: String,
    pub method: HttpMethod,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

/// One response: status, headers (names lowercased for lookup), raw body.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn json(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.body)
    }

    /// Case-insensitive header lookup; first match wins.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Transport-level failure (the fetch itself threw / was aborted).
#[derive(Debug, Clone)]
pub struct HttpError {
    pub message: String,
    /// Deadline abort — the `AbortError` equivalent.
    pub aborted: bool,
}

impl HttpError {
    pub fn aborted() -> Self {
        Self {
            message: "aborted".into(),
            aborted: true,
        }
    }
}

/// Level-triggered cancellation token (an `AbortSignal`): `fire()` stores one
/// permit, so a waiter that registers later still observes it.
#[derive(Clone, Default)]
pub struct AbortSignal(Arc<tokio::sync::Notify>);

impl AbortSignal {
    pub fn new() -> Self {
        Self(Arc::new(tokio::sync::Notify::new()))
    }

    pub fn fire(&self) {
        self.0.notify_one();
    }

    /// Resolves once the signal has fired.
    pub async fn wait(&self) {
        self.0.notified().await;
    }
}

/// The `fetch` equivalent. Implementations must honor `abort` — once it
/// fires, an in-flight request should fail promptly (an aborted fetch is how
/// the connect deadline turns a hung hub into a retryable network error).
pub trait HttpTransport: Send + Sync {
    fn fetch(
        &self,
        req: HttpRequest,
        abort: AbortSignal,
    ) -> BoxFuture<'static, Result<HttpResponse, HttpError>>;
}

/// `encodeURIComponent` equivalent: RFC 3986 unreserved characters pass
/// through, plus `! ~ * ' ( )` — the exact JS passthrough set — everything
/// else becomes uppercase `%XX`.
pub fn encode_query_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Minimal query-string parser for tests and URL inspection: splits on `&`,
/// takes the first `=` as the separator, decodes `%XX` and `+` (the
/// `URLSearchParams` decoding rules).
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((k, v)) => (decode_query_component(k), decode_query_component(v)),
            None => (decode_query_component(part), String::new()),
        })
        .collect()
}

pub(crate) fn decode_query_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hex = bytes.get(i + 1..i + 3);
                match hex
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Path-plus-query helper: appends `params` (already-encoded values) to
/// `path`, prefixing `?` only when there is at least one param.
pub(crate) fn with_query(path: &str, params: &[String]) -> String {
    if params.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", params.join("&"))
    }
}
