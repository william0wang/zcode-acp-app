//! Minimal async HTTP/1.1 transport for the hub REST API — the app layer's
//! supplier of protocol's [`HttpTransport`] (protocol's `hub_client` module
//! doc: "the app layer supplies one").
//!
//! Scope: `http://` for the local hub and `https://` for remote Connection
//! Profiles (wrapped with tokio-rustls against the system trust store — the
//! same policy the WS side gets). No keep-alive, no redirects: one request
//! per connection with `Connection: close`.
//!
//! [`SettingsApiTransport`] is the same socket-per-request policy exposed
//! through protocol's SEPARATE settings-client transport trait (the
//! configuration pages, bridge 0.47.0+).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use protocol::clock::BoxFuture;
use protocol::http::{
    AbortSignal, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpTransport,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

/// Shared handle; stateless beyond the socket-per-request policy.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalHttpTransport;

impl HttpTransport for LocalHttpTransport {
    fn fetch(
        &self,
        req: HttpRequest,
        abort: AbortSignal,
    ) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            // Deadline watcher protocol's clients arm: any settle inside the
            // budget wins; only a fired abort turns into the "aborted" error
            // shape the hub client maps to its network-error branch.
            tokio::select! {
                biased;
                _ = abort.wait() => Err(HttpError::aborted()),
                out = perform(req) => out,
            }
        })
    }
}

struct ParsedUrl {
    host: String,
    port: u16,
    path: String,
    tls: bool,
}

/// The Settings API's injectable transport (`settings_client::HttpTransport`)
/// bridged onto [`LocalHttpTransport`]: maps the settings client's request
/// shape onto the hub transport's, and enforces the resolved deadline
/// (`timeout_ms`) with a watcher task firing the abort signal — the deadline
/// discipline the settings client delegates to its transport.
#[derive(Debug, Clone, Copy, Default)]
pub struct SettingsApiTransport;

impl protocol::settings_client::HttpTransport for SettingsApiTransport {
    type Error = String;

    fn fetch(
        &self,
        request: protocol::settings_client::HttpRequest,
    ) -> impl Future<Output = Result<protocol::settings_client::HttpResponse, Self::Error>> + Send
    {
        async move {
            let method = match request.method {
                protocol::settings_client::Method::Get => HttpMethod::Get,
                protocol::settings_client::Method::Post => HttpMethod::Post,
                protocol::settings_client::Method::Put => HttpMethod::Put,
                protocol::settings_client::Method::Delete => HttpMethod::Delete,
            };
            let abort = AbortSignal::new();
            // None = no deadline: the settings client resolved an explicit
            // "wait indefinitely" for this call.
            let watcher = request.timeout_ms.map(|ms| {
                let abort = abort.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    abort.fire();
                })
            });
            let out = LocalHttpTransport
                .fetch(
                    HttpRequest {
                        url: request.url,
                        method,
                        headers: request.headers,
                        body: request.body,
                    },
                    abort,
                )
                .await
                .map_err(|e| {
                    // Only the watcher fires the signal here, so an aborted
                    // read is a deadline, and the caller's network-error
                    // branch deserves the word for it.
                    if e.aborted {
                        "request timed out".to_string()
                    } else {
                        e.message
                    }
                });
            if let Some(watcher) = watcher {
                watcher.abort();
            }
            out.map(|res| protocol::settings_client::HttpResponse {
                status: res.status,
                body: res.body,
            })
        }
    }
}

fn parse_url(raw: &str) -> Result<ParsedUrl, HttpError> {
    let (scheme, rest) = raw.split_once("://").ok_or_else(|| {
        HttpError {
            message: format!("bad url (no scheme): {raw}"),
            aborted: false,
        }
    })?;
    let (tls, default_port) = match scheme {
        "http" => (false, 80),
        "https" => (true, 443),
        other => {
            return Err(HttpError {
                message: format!("unsupported scheme {other:?}: expected http or https"),
                aborted: false,
            });
        }
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        // [::1]:8377
        let (host, tail) = stripped
            .split_once(']')
            .ok_or_else(|| bad_url(raw))?;
        let port = tail
            .strip_prefix(':')
            .map(|p| p.parse::<u16>())
            .transpose()
            .map_err(|_| bad_url(raw))?
            .unwrap_or(default_port);
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>().map_err(|_| bad_url(raw))?,
            ),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err(bad_url(raw));
    }
    Ok(ParsedUrl {
        host,
        port,
        path: path.to_string(),
        tls,
    })
}

fn bad_url(raw: &str) -> HttpError {
    HttpError {
        message: format!("bad url: {raw}"),
        aborted: false,
    }
}

fn net_err(e: std::io::Error) -> HttpError {
    HttpError {
        message: e.to_string(),
        aborted: false,
    }
}

/// Either wire the socket-per-request transport can speak.
enum Wire {
    Plain(TcpStream),
    Tls(tokio_rustls::client::TlsStream<TcpStream>),
}

impl AsyncRead for Wire {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Wire::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Wire::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Wire {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            Wire::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Wire::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Wire::Plain(s) => Pin::new(s).poll_flush(cx),
            Wire::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Wire::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Wire::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// A rustls client config trusting the system certificate store — remote
/// profiles sit behind tunnels whose CAs are public or locally trusted, and
/// the TS client (system WebView/Node) honors the same store.
fn tls_connector() -> Result<tokio_rustls::TlsConnector, HttpError> {
    let mut roots = rustls::RootCertStore::empty();
    let store = rustls_native_certs::load_native_certs();
    for cert in store.certs {
        // Skip unparseable system entries — one bad cert must not take the
        // whole store down (the OS ships what it ships).
        let _ = roots.add(cert);
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(std::sync::Arc::new(config).into())
}

async fn perform(req: HttpRequest) -> Result<HttpResponse, HttpError> {
    let url = parse_url(&req.url)?;
    let tcp = TcpStream::connect((url.host.as_str(), url.port))
        .await
        .map_err(net_err)?;
    let mut stream = if url.tls {
        let connector = tls_connector()?;
        let name = rustls_pki_types::ServerName::try_from(url.host.clone()).map_err(|e| {
            HttpError {
                message: format!("bad tls server name {:?}: {e}", url.host),
                aborted: false,
            }
        })?;
        let tls = connector.connect(name, tcp).await.map_err(|e| HttpError {
            message: format!("tls handshake with {} failed: {e}", url.host),
            aborted: false,
        })?;
        Wire::Tls(tls)
    } else {
        Wire::Plain(tcp)
    };

    let mut head = format!("{} {} HTTP/1.1\r\n", req.method.as_str(), url.path);
    head.push_str(&format!("host: {}:{}\r\n", url.host, url.port));
    for (name, value) in &req.headers {
        head.push_str(&format!("{}: {}\r\n", name.to_ascii_lowercase(), value));
    }
    if let Some(body) = &req.body {
        head.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    head.push_str("connection: close\r\n\r\n");

    let mut wire = head.into_bytes();
    if let Some(body) = &req.body {
        wire.extend_from_slice(body.as_bytes());
    }
    stream.write_all(&wire).await.map_err(net_err)?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.map_err(net_err)?;

    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, HttpError> {
    let header_end = find_subslice(raw, b"\r\n\r\n").ok_or_else(|| HttpError {
        message: "malformed response (no header terminator)".to_string(),
        aborted: false,
    })?;
    let head = std::str::from_utf8(&raw[..header_end]).map_err(|e| HttpError {
        message: format!("malformed response head: {e}"),
        aborted: false,
    })?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or_else(|| HttpError {
        message: "empty response".to_string(),
        aborted: false,
    })?;
    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next();
    let status = parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| HttpError {
            message: format!("malformed status line: {status_line}"),
            aborted: false,
        })?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };

    let body_raw = &raw[header_end + 4..];
    let chunked = header("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    let body = if chunked {
        decode_chunked(body_raw)?
    } else if let Some(len) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body_raw[..len.min(body_raw.len())].to_vec()
    } else {
        // Connection: close on our side — the rest of the stream is the body.
        body_raw.to_vec()
    };

    Ok(HttpResponse {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_chunked(raw: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let line_end = find_subslice(&raw[pos..], b"\r\n").ok_or_else(|| HttpError {
            message: "malformed chunked body".to_string(),
            aborted: false,
        })? + pos;
        let size_line = std::str::from_utf8(&raw[pos..line_end]).map_err(|e| HttpError {
            message: format!("malformed chunk size: {e}"),
            aborted: false,
        })?;
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| HttpError {
            message: format!("malformed chunk size: {size_line}"),
            aborted: false,
        })?;
        pos = line_end + 2;
        if size == 0 {
            return Ok(out);
        }
        let end = (pos + size).min(raw.len());
        out.extend_from_slice(&raw[pos..end]);
        pos = end + 2; // skip the chunk's trailing CRLF
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::bootstrap::{assess_local_hub, BootstrapEnv};
    use protocol::clock::TokioClock;
    use protocol::hub_client::HubClient;
    use std::sync::Arc;

    #[test]
    fn parses_url_host_port_and_path() {
        let u = parse_url("http://127.0.0.1:8377/api/instances?probe=1").unwrap();
        assert_eq!(u.host, "127.0.0.1");
        assert_eq!(u.port, 8377);
        assert_eq!(u.path, "/api/instances?probe=1");
        let u = parse_url("http://hub").unwrap();
        assert_eq!(u.port, 80);
        assert_eq!(u.path, "/");
        let u = parse_url("http://[::1]:9000/x").unwrap();
        assert_eq!(u.host, "::1");
        assert_eq!(u.port, 9000);
        let u = parse_url("https://hub.example").unwrap();
        assert!(u.tls);
        assert_eq!(u.port, 443);
        assert!(parse_url("ftp://hub").is_err());
        assert!(parse_url("hub").is_err());
    }

    #[test]
    fn parses_content_length_and_chunked_bodies() {
        let res = parse_response(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 6\r\n\r\n{\"a\":1",
        )
        .unwrap();
        assert_eq!(res.status, 200);
        assert_eq!(res.body, "{\"a\":1");
        assert_eq!(res.header("Content-Type").unwrap(), "application/json");

        let res = parse_response(
            b"HTTP/1.1 401 nope\r\ntransfer-encoding: chunked\r\n\r\n4\r\nunau\r\n3\r\ntho\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(res.status, 401);
        assert_eq!(res.body, "unautho");
    }

    /// Live smoke against the real local hub (opt-in: ZCODE_ACP_SMOKE=1 with
    /// a listening hub + configured token). Verifies the hand-rolled
    /// HTTP/1.1 transport against the server's actual Node http responses.
    #[test]
    fn live_hub_discovery_smoke() {
        if std::env::var("ZCODE_ACP_SMOKE").ok().as_deref() != Some("1") {
            return;
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let env = BootstrapEnv::from_env();
            let status = assess_local_hub(&env);
            let config = status.config;
            let token = config.token.expect("token configured");
            let url = format!("http://{}:{}", protocol::bootstrap::dial_host(&config.hub_host), config.hub_port);
            let hub = HubClient::new(
                url,
                token,
                Arc::new(LocalHttpTransport),
                Arc::new(TokioClock),
            );
            let instances = hub.instances(true).await.expect("discovery succeeds");
            println!("instances: {}", instances.len());
        });
    }
}
