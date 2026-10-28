//! Port of `tests/hub-client.test.ts` (vitest) — REST-layer coverage:
//! payload validation, error mapping (401 / HTTP / network), verbatim
//! cursor pagination, fs line-window parsing, and the transparent 404
//! retry after a bridge upgrade. The scripted HTTP double plays the role of
//! the test's `globalThis.fetch` replacement.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::{HubApiError, HubClient};
use crate::clock::{BoxFuture, MockClock};
use crate::http::{
    parse_query, HttpBytesResponse, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpTransport,
};
use crate::hub::HubHistoryCursor;

// -- scripted fetch double ---------------------------------------------------

#[derive(Default)]
struct ScriptedHttp {
    responses: Mutex<VecDeque<Result<HttpResponse, HttpError>>>,
    bytes_responses: Mutex<VecDeque<Result<HttpBytesResponse, HttpError>>>,
    requested: Mutex<Vec<HttpRequest>>,
}

impl ScriptedHttp {
    fn push(&self, response: Result<HttpResponse, HttpError>) {
        self.responses.lock().unwrap().push_back(response);
    }

    fn push_bytes(&self, response: Result<HttpBytesResponse, HttpError>) {
        self.bytes_responses.lock().unwrap().push_back(response);
    }

    fn urls(&self) -> Vec<String> {
        self.requested.lock().unwrap().iter().map(|r| r.url.clone()).collect()
    }
}

impl HttpTransport for ScriptedHttp {
    fn fetch(
        &self,
        req: HttpRequest,
        _abort: crate::http::AbortSignal,
    ) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        self.requested.lock().unwrap().push(req);
        let next = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("test bug: no scripted response left");
        Box::pin(async move { next })
    }

    fn fetch_bytes(
        &self,
        req: HttpRequest,
        _abort: crate::http::AbortSignal,
    ) -> BoxFuture<'static, Result<HttpBytesResponse, HttpError>> {
        self.requested.lock().unwrap().push(req);
        let next = self
            .bytes_responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("test bug: no scripted bytes response left");
        Box::pin(async move { next })
    }
}

fn res_json(status: u16, body: Value) -> Result<HttpResponse, HttpError> {
    Ok(HttpResponse {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.to_string(),
    })
}

fn res_text(status: u16, body: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpError> {
    Ok(HttpResponse {
        status,
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: body.to_string(),
    })
}

fn client(http: Arc<ScriptedHttp>, clock: Arc<MockClock>) -> HubClient {
    HubClient::new(
        "http://hub/",
        "tok",
        http as Arc<dyn HttpTransport>,
        clock as Arc<dyn crate::clock::Clock>,
    )
}

/// Poll a future once without consuming it (so mock-clock deadlines
/// register deterministically), asserting it is still pending.
async fn assert_pending<T>(fut: &mut (impl Future<Output = T> + Unpin)) {
    tokio::select! {
        biased;
        value = fut => {
            let _ = value;
            panic!("future settled early");
        }
        _ = tokio::task::yield_now() => {}
    }
}

// -- ports -------------------------------------------------------------------

#[tokio::test]
async fn instances_parses_the_payload_and_forwards_the_probe_flag() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_json(200, json!([{ "id": "inst1", "workspace": "/w", "sessions": [] }])));
    let list = client(http.clone(), clock.clone()).instances(false).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(http.urls()[0], "http://hub/api/instances");

    http.push(res_json(200, json!([])));
    client(http.clone(), clock).instances(true).await.unwrap();
    assert_eq!(http.urls()[1], "http://hub/api/instances?probe=1");
}

#[tokio::test]
async fn instances_rejects_a_non_array_payload() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_json(200, json!({})));
    let err = client(http, clock).instances(false).await.unwrap_err();
    assert_eq!(err.message, "unexpected /api/instances payload");
}

#[tokio::test]
async fn projects_parses_the_payload_and_rejects_a_non_array() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_json(
        200,
        json!([{ "workspacePath": "/w/one", "sessions": 3, "lastActive": 1792045487151i64 }]),
    ));
    let list = client(http.clone(), clock.clone()).projects().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].workspace_path, "/w/one");
    assert_eq!(list[0].sessions, 3);
    assert_eq!(http.urls()[0], "http://hub/api/projects");

    http.push(res_json(200, json!({ "oops": true })));
    let err = client(http, clock).projects().await.unwrap_err();
    assert_eq!(err.message, "unexpected /api/projects payload");
}

#[tokio::test]
async fn quota_dock_returns_the_formatted_line_and_null_hides_the_dock() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_json(200, json!({ "formatted": "34% 12:59 · go 7.5% 11-03" })));
    let dock = client(http.clone(), clock.clone()).quota_dock().await.unwrap();
    assert_eq!(dock.as_deref(), Some("34% 12:59 · go 7.5% 11-03"));
    assert_eq!(http.urls()[0], "http://hub/api/quota/dock");

    http.push(res_json(200, json!({ "formatted": null })));
    let dock = client(http, clock).quota_dock().await.unwrap();
    assert_eq!(dock, None);
}

#[tokio::test]
async fn http_401_keeps_its_status_and_other_errors_surface_as_http_code() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_text(401, "no", &[]));
    let err = client(http.clone(), clock.clone())
        .health()
        .await
        .unwrap_err();
    assert_eq!(err.status, Some(401));
    assert_eq!(err.message, "unauthorized: check token");
    assert!(!err.network);

    http.push(res_text(502, "boom", &[]));
    let err = client(http, clock).health().await.unwrap_err();
    assert_eq!(err.status, Some(502));
    assert_eq!(err.message, "HTTP 502");
}

#[tokio::test]
async fn fetch_failures_are_marked_network_as_expected_hub_offline_state() {
    struct ThrowingHttp;
    impl HttpTransport for ThrowingHttp {
        fn fetch(
            &self,
            _req: HttpRequest,
            _abort: crate::http::AbortSignal,
        ) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
            Box::pin(async { Err(HttpError { message: "dispatch error".into(), aborted: false }) })
        }
    }
    let http: Arc<dyn HttpTransport> = Arc::new(ThrowingHttp);
    let clock = Arc::new(MockClock::new());
    let err = HubClient::new("http://hub/", "tok", http, clock as Arc<dyn crate::clock::Clock>)
        .health()
        .await
        .unwrap_err();
    assert!(err.network);
}

#[tokio::test]
async fn project_sessions_passes_the_composite_cursor_verbatim() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_json(200, json!({ "workspacePath": "/my proj", "instance": { "id": "i" }, "sessions": [], "nextCursor": null })));
    client(http.clone(), clock)
        .project_sessions(
            "/my proj",
            Some(&HubHistoryCursor { before: 1_700_000_000, before_id: "row9".into() }),
        )
        .await
        .unwrap();
    let url = &http.urls()[0];
    let (path, query) = url.split_once('?').unwrap();
    assert_eq!(path, "http://hub/api/projects/sessions");
    let params = parse_query(query).into_iter().collect::<std::collections::HashMap<_, _>>();
    assert_eq!(params.get("workspacePath").map(String::as_str), Some("/my proj"));
    assert_eq!(params.get("before").map(String::as_str), Some("1700000000"));
    assert_eq!(params.get("beforeId").map(String::as_str), Some("row9"));
}

#[tokio::test]
async fn fs_browse_omits_the_query_for_home_and_encodes_a_path_when_given() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_json(
        200,
        json!({ "path": "/Users/me", "parent": "/Users", "creatable": false, "entries": [], "truncated": false }),
    ));
    client(http.clone(), clock.clone()).fs_browse(None).await.unwrap();
    assert_eq!(http.urls()[0], "http://hub/api/fs/list");

    http.push(res_json(
        200,
        json!({ "path": "/Users/me/My Dir", "parent": "/Users/me", "creatable": true, "entries": [{ "name": "src" }], "truncated": false }),
    ));
    let listing = client(http.clone(), clock).fs_browse(Some("/Users/me/My Dir")).await.unwrap();
    assert_eq!(http.urls()[1], "http://hub/api/fs/list?path=%2FUsers%2Fme%2FMy%20Dir");
    assert!(listing.creatable);
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].name, "src");
}

#[tokio::test]
async fn fs_browse_surfaces_a_404_with_its_status() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_text(404, "not found", &[]));
    let err = client(http, clock).fs_browse(None).await.unwrap_err();
    assert_eq!(err.status, Some(404));
}

#[tokio::test]
async fn close_session_posts_the_retire_route() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_text(200, "ok", &[]));
    client(http.clone(), clock).close_session("inst 1", "sess/a", false).await.unwrap();
    let calls = http.requested.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].url, "http://hub/api/instances/inst 1/sessions/sess%2Fa/close");
    assert_eq!(calls[0].method, HttpMethod::Post);
    // A plain retire carries force:false — the bridge keeps the
    // refuse-when-running contract (ADR-0006).
    assert_eq!(
        serde_json::from_str::<Value>(&calls[0].body.clone().unwrap()).unwrap(),
        json!({ "force": false })
    );
}

#[tokio::test]
async fn forced_close_session_carries_force_true() {
    // The desktop's End session: an explicit terminate gesture, so a running
    // turn is cancelled instead of the close being refused with 409.
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_text(200, "ok", &[]));
    client(http.clone(), clock).close_session("inst 1", "sess/a", true).await.unwrap();
    let calls = http.requested.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(&calls[0].body.clone().unwrap()).unwrap(),
        json!({ "force": true })
    );
}

#[tokio::test]
async fn reincubate_carries_auto_intent_and_surfaces_the_close_tombstone() {
    // The reconnect ladder's resume is the TAB re-incubating itself, so it
    // rides the hub's `intent: "auto"` channel; the tombstone refusal (409)
    // must be distinguishable from any other failure — the ladder retires
    // the tab on it instead of parking in Failed.
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_text(409, "session closed", &[("content-type", "text/plain")]));
    let err = client(http.clone(), clock)
        .reincubate_instance("/w", "sess_1")
        .await
        .unwrap_err();
    assert_eq!(err.status, Some(409));
    assert!(err.session_closed());
    let calls = http.requested.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(&calls[0].body.clone().unwrap()).unwrap(),
        json!({ "workspacePath": "/w", "sessionId": "sess_1", "intent": "auto" })
    );

    // Any other refusal is NOT a close verdict.
    let other_http = Arc::new(ScriptedHttp::default());
    let other_clock = Arc::new(MockClock::new());
    other_http.push(res_text(502, "bridge unreachable", &[]));
    let other = client(other_http, other_clock)
        .reincubate_instance("/w", "sess_1")
        .await
        .unwrap_err();
    assert!(!other.session_closed());
}

#[tokio::test]
async fn create_instance_resume_keeps_the_default_user_intent() {
    // A human picking the session: no intent key (older-hub compatible), so
    // the hub's close tombstone never refuses a manual re-launch.
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_json(200, json!({ "id": "i1", "reused": false })));
    client(http.clone(), clock)
        .create_instance("/w", Some("sess_1"))
        .await
        .unwrap();
    let calls = http.requested.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(&calls[0].body.clone().unwrap()).unwrap(),
        json!({ "workspacePath": "/w", "sessionId": "sess_1" })
    );
}

#[tokio::test]
async fn rename_session_posts_the_title_body() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_text(200, "ok", &[]));
    client(http.clone(), clock)
        .rename_session("inst 1", "sess/a", "my title")
        .await
        .unwrap();
    let calls = http.requested.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].url, "http://hub/api/instances/inst 1/sessions/sess%2Fa/rename");
    assert_eq!(calls[0].method, HttpMethod::Post);
    assert_eq!(
        serde_json::from_str::<Value>(&calls[0].body.clone().unwrap()).unwrap(),
        json!({ "title": "my title" })
    );
}

#[tokio::test]
async fn delete_session_posts_the_per_instance_tombstone_route() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_json(200, json!({ "ok": true, "deleted": true })));
    client(http.clone(), clock).delete_session("inst 1", "sess/a").await.unwrap();
    let calls = http.requested.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    // The session id is encoded, the instance id rides raw — same shape as
    // closeSession/renameSession (instance ids are hub-numeric).
    assert_eq!(calls[0].url, "http://hub/api/instances/inst 1/sessions/sess%2Fa/delete");
    assert_eq!(calls[0].method, HttpMethod::Post);
}

#[tokio::test]
async fn delete_project_posts_the_workspace_tombstone_route_with_the_path_in_the_body() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_json(200, json!({ "ok": true, "deletedTasks": 3 })));
    client(http.clone(), clock).delete_project("/Users/me/my proj").await.unwrap();
    let calls = http.requested.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].url, "http://hub/api/projects/delete");
    assert_eq!(calls[0].method, HttpMethod::Post);
    // Hub-level route (no instance scoping); the workspace path is JSON,
    // not a query param, so any path shape survives unencoded.
    assert_eq!(
        serde_json::from_str::<Value>(&calls[0].body.clone().unwrap()).unwrap(),
        json!({ "workspacePath": "/Users/me/my proj" })
    );
}

#[tokio::test]
async fn fs_file_text_trusts_first_line_header_and_strips_the_trailing_newline() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_text(200, "a\nb\nc\n", &[("X-Zcode-First-Line", "7")]));
    let (first_line, text) = client(http.clone(), clock.clone())
        .fs_file_text("inst", "sess", "/f.txt", 1, 3)
        .await
        .unwrap();
    assert_eq!(first_line, 7);
    assert_eq!(text, "a\nb\nc");
}

#[tokio::test]
async fn fs_file_text_falls_back_to_the_requested_line_without_the_header() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_text(200, "only\n", &[]));
    let (first_line, text) = client(http, clock)
        .fs_file_text("inst", "sess", "/f.txt", 4, 1)
        .await
        .unwrap();
    assert_eq!(first_line, 4);
    assert_eq!(text, "only");
}

#[tokio::test]
async fn fs_file_url_embeds_token_and_session_in_the_query_for_img_src() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    let url = client(http, clock).fs_file_url("inst", "sess", "/a b.png");
    let (path, query) = url.split_once('?').unwrap();
    assert_eq!(path, "http://hub/api/instances/inst/fs/file");
    let params = parse_query(query).into_iter().collect::<std::collections::HashMap<_, _>>();
    assert_eq!(params.get("sessionId").map(String::as_str), Some("sess"));
    assert_eq!(params.get("path").map(String::as_str), Some("/a b.png"));
    assert_eq!(params.get("token").map(String::as_str), Some("tok"));
}

/// The image-preview fetch (batch K): same URL shape as fs_file_url — the
/// token rides the query because a binary GET has no header story here — and
/// the body comes back as raw bytes, untouched by the lossy text pipeline.
#[tokio::test]
async fn fs_file_bytes_fetches_the_token_query_url_and_returns_raw_bytes() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    // Bytes that are invalid UTF-8 — the lossy text body would mangle them.
    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0xff, 0xfe, 0x80];
    http.push_bytes(Ok(HttpBytesResponse {
        status: 200,
        headers: vec![("content-type".into(), "image/png".into())],
        body: png.clone(),
    }));
    let got = client(http.clone(), clock)
        .fs_file_bytes("inst", "sess", "/a b.png")
        .await
        .unwrap();
    assert_eq!(got, png);
    let url = http.urls().remove(0);
    let (path, query) = url.split_once('?').unwrap();
    assert_eq!(path, "http://hub/api/instances/inst/fs/file");
    let params = parse_query(query).into_iter().collect::<std::collections::HashMap<_, _>>();
    assert_eq!(params.get("sessionId").map(String::as_str), Some("sess"));
    assert_eq!(params.get("path").map(String::as_str), Some("/a b.png"));
    assert_eq!(params.get("token").map(String::as_str), Some("tok"));
}

#[tokio::test]
async fn fs_file_bytes_maps_http_and_network_failures() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push_bytes(Ok(HttpBytesResponse {
        status: 404,
        headers: Vec::new(),
        body: b"unknown instance".to_vec(),
    }));
    let err = client(http.clone(), clock)
        .fs_file_bytes("inst", "sess", "/x.png")
        .await
        .unwrap_err();
    assert_eq!(err.status, Some(404));
    assert!(!err.network);

    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push_bytes(Err(HttpError {
        message: "conn refused".into(),
        aborted: false,
    }));
    let err = client(http, clock).fs_file_bytes("inst", "sess", "/x.png").await.unwrap_err();
    assert!(err.network);
    assert_eq!(err.status, None);
}

#[tokio::test]
async fn a_404_right_after_a_bridge_upgrade_is_retried_once_transparently() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_text(404, "stale route", &[]));
    // The TS fixture answers `{entries: []}` unvalidated; the typed Rust
    // result needs the full FsListing shape — the retry, not the payload
    // laxness, is the behavior under test.
    http.push(res_json(200, json!({ "root": "/", "entries": [], "truncated": false })));
    let hub = client(http.clone(), clock.clone());
    let load = hub.fs_list("inst", "sess", "/");
    // The retry's 400ms bridge-upgrade-race delay rides the mock clock.
    let (listing, ()) = tokio::join!(load, clock.advance(400));
    assert_eq!(listing.unwrap().entries.len(), 0);
    assert_eq!(http.urls().len(), 2);
}

#[tokio::test]
async fn a_gone_bridge_is_flagged_instance_dead() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    // The retry-404-once path applies here too — the SECOND answer decides.
    http.push(res_text(404, "unknown instance", &[]));
    http.push(res_text(404, "unknown instance", &[]));
    let hub = client(http.clone(), clock.clone());
    let load = hub.fs_list("inst", "sess", "/");
    let (err, ()) = tokio::join!(load, clock.advance(400));
    let err = err.unwrap_err();
    assert_eq!(err.status, Some(404));
    assert!(err.instance_dead);

    http.push(res_text(502, "bridge unreachable", &[]));
    let err = client(http, clock)
        .fs_file_text("inst", "sess", "/f.txt", 1, 5)
        .await
        .unwrap_err();
    assert_eq!(err.status, Some(502));
    assert!(err.instance_dead);
}

#[tokio::test]
async fn a_genuine_404_is_not_flagged_instance_dead() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_text(404, "not found", &[]));
    http.push(res_text(404, "not found", &[]));
    // Body is plain text, so the caller's JSON parse throws — the
    // pre-existing behavior for missing paths, unchanged by the
    // instance-dead plumbing.
    let hub = client(http, clock.clone());
    let load = hub.fs_list("inst", "sess", "nope");
    let (result, ()) = tokio::join!(load, clock.advance(400));
    assert!(result.is_err());
}

#[tokio::test]
async fn system_stats_gets_the_machine_level_route_and_parses_the_payload() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_json(
        200,
        json!({
            "collectedAt": 1_791_106_915_456u64,
            "host": { "hostname": "Mac.local", "osVersion": "15.8 (24H23)", "model": "Mac14,9", "chip": "Apple M2 Pro" },
            "uptimeS": 528_592u64,
            "cpu": { "cores": 10, "usagePct": 12.3, "loadAvg": [2.5, 2.4, 2.4] },
            "memory": {
                "totalBytes": 17_179_869_184u64,
                "availableBytes": 5_428_199_424u64,
                "usedBytes": 11_751_669_760u64,
                "swapTotalBytes": 2_147_483_648u64,
                "swapUsedBytes": 978_384_322u64,
            },
            "storage": {
                "root": { "totalBytes": 494_384_795_648u64, "usedBytes": 350_988_324_864u64, "availableBytes": 143_396_470_784u64 },
                "home": null,
            },
            "battery": { "present": true, "percent": 99, "powerSource": "battery", "status": "discharging", "remainingMin": 1200 },
            "power": { "preventSleep": true, "sleepHolders": ["Amphetamine"], "cpuSpeedLimitPct": null },
            "network": { "addresses": ["10.0.0.2"], "ssid": "HomeNet", "rxBytesPerS": 11633, "txBytesPerS": 450198 },
            "processes": {
                "hub": { "pid": 38402, "rssBytes": 69_255_168u64, "cpuPct": 1.2 },
                "bridges": [ { "id": "i1", "workspace": "/w/proj", "pid": 123, "rssBytes": 300_000_000u64, "cpuPct": 4.2 } ],
            },
            "hub": { "version": "0.62.0", "uptimeS": 9000, "instances": 1 },
        }),
    ));
    let stats = client(http.clone(), clock).system_stats().await.unwrap();
    assert_eq!(http.urls()[0], "http://hub/api/system-stats");
    assert_eq!(stats.cpu.usage_pct, Some(12.3));
    assert_eq!(stats.power.sleep_holders, vec!["Amphetamine".to_string()]);
    assert_eq!(stats.processes.bridges.len(), 1);
}

#[tokio::test]
async fn system_stats_surfaces_a_404_hub_as_an_error_with_status() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_text(404, "no route", &[]));
    let err = client(http, clock).system_stats().await.unwrap_err();
    assert_eq!(err.status, Some(404));
}

// -- extra coverage for the deadline plumbing the TS suite exercises via
//    reconnect-wedge.test.ts ("a hub fetch that never answers aborts into a
//    network error"); kept here next to the client it pins.

/// A fetch that hangs but honors the abort signal — the exact shape the 10s
/// deadline converts into a network error.
#[tokio::test]
async fn a_hung_fetch_aborts_into_a_network_error_at_the_deadline() {
    struct HangHttp;
    impl HttpTransport for HangHttp {
        fn fetch(
            &self,
            _req: HttpRequest,
            abort: crate::http::AbortSignal,
        ) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
            Box::pin(async move {
                abort.wait().await;
                Err(HttpError::aborted())
            })
        }
    }
    let http: Arc<dyn HttpTransport> = Arc::new(HangHttp);
    let clock = Arc::new(MockClock::new());
    let hub = HubClient::new(
        "http://hub",
        "t",
        http,
        Arc::clone(&clock) as Arc<dyn crate::clock::Clock>,
    );
    let mut fut = Box::pin(hub.instances(false));
    assert_pending(&mut fut).await;
    // Still pending one tick before the deadline…
    clock.advance(9_999).await;
    assert_pending(&mut fut).await;
    // …and settles as a network-level failure the moment it passes.
    clock.advance(1).await;
    let err: HubApiError = fut.await.unwrap_err();
    assert!(err.network);
    assert!(err.message.contains("no response within"), "message: {}", err.message);
    assert!(err.message.contains("/api/instances"));
}

/// A refused write answers with a REASON — the body becomes the message when
/// it says something (message preferred over the error token).
#[tokio::test]
async fn refused_write_body_becomes_the_error_message() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_json(409, json!({ "error": "stale_nonce", "message": "nonce is stale — refresh the reset card status" })));
    let err = client(http.clone(), clock.clone()).delete_project("/w").await.unwrap_err();
    assert_eq!(err.status, Some(409));
    assert_eq!(err.message, "nonce is stale — refresh the reset card status");

    http.push(res_json(403, json!({ "error": "not_a_known_project" })));
    let err = client(http, clock).delete_project("/w").await.unwrap_err();
    assert_eq!(err.message, "not_a_known_project");
}

/// The Authorization header is the desktop client's bearer auth (the TS app
/// had to put the token in the query for WS; REST always used the header).
/// Every call also stamps the Desktop Presence identity header the hub reads
/// for headless incubation (server ADR-0016 amendment).
#[tokio::test]
async fn every_request_carries_the_bearer_authorization_header() {
    let http = Arc::new(ScriptedHttp::default());
    let clock = Arc::new(MockClock::new());
    http.push(res_json(200, json!([])));
    client(http.clone(), clock.clone()).instances(false).await.unwrap();
    let calls = http.requested.lock().unwrap().clone();
    assert_eq!(calls[0].headers[0], ("authorization".to_string(), "Bearer tok".to_string()));
    assert!(calls[0].headers.contains(&(
        "x-zcode-client".to_string(),
        "zcode-acp-remote-desktop".to_string()
    )));
    // Content-Type rides only on writes.
    http.push(res_json(200, json!({ "ok": true })));
    client(http.clone(), clock).delete_project("/w").await.unwrap();
    let calls = http.requested.lock().unwrap().clone();
    let headers: Vec<_> = calls[1].headers.iter().map(|(k, _)| k.as_str()).collect();
    assert!(headers.contains(&"content-type"));
}

/// `Deadline::Never` (create-instance incubation budget) skips the watchdog.
#[tokio::test]
async fn no_deadline_is_armed_for_the_incubation_route() {
    struct SlowHttp;
    impl HttpTransport for SlowHttp {
        fn fetch(
            &self,
            _req: HttpRequest,
            abort: crate::http::AbortSignal,
        ) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
            // Settles only via the abort signal — with Deadline::Never the
            // client must still be waiting after any deadline would have
            // fired.
            Box::pin(async move {
                abort.wait().await;
                Err(HttpError::aborted())
            })
        }
    }
    let http: Arc<dyn HttpTransport> = Arc::new(SlowHttp);
    let clock = Arc::new(MockClock::new());
    let hub = HubClient::new(
        "http://hub",
        "t",
        http,
        Arc::clone(&clock) as Arc<dyn crate::clock::Clock>,
    );
    let mut fut = Box::pin(hub.create_instance("/w", None));
    assert_pending(&mut fut).await;
    clock.advance(60_000).await;
    // Far past every deadline — still waiting (no abort ever fired).
    assert_pending(&mut fut).await;
    drop(fut);
}
