//! Hermetic live-transport tests: the REAL `UreqTransport` against a std
//! `TcpListener` speaking minimal HTTP/1.1 on 127.0.0.1 — zero new dev-deps.
//! Proves the wire facts the fakes can't: raw `Authorization` header (no `Bearer`),
//! POST + JSON content type, status classification without body echo, and the
//! bounded timeout.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use lane::linear::transport::{linear_to_lane, LinearTransport, TransportError, UreqTransport};
use lane::secrets::SecretValue;
use serde_json::json;

/// Serve exactly one request: capture it fully (headers + Content-Length body),
/// respond with `status_line` + `body`, and return the raw captured request text.
fn serve_once(
    status_line: &'static str,
    body: &'static str,
) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let url = format!("http://{addr}/graphql");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let raw = read_full_request(&mut stream);
        let response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
        raw
    });
    (url, handle)
}

/// Read one HTTP/1.1 request: headers to CRLFCRLF, then Content-Length body bytes.
fn read_full_request(stream: &mut std::net::TcpStream) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).expect("read request");
        assert!(n > 0, "client closed before sending a full request");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
    let content_length: usize = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map(|v| v.trim().parse().expect("content-length"))
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).expect("read body");
        assert!(n > 0, "client closed mid-body");
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[test]
fn ureq_transport_posts_graphql_with_raw_authorization() {
    let (url, server) = serve_once("200 OK", r#"{"data":{"ok":true}}"#);
    let transport = UreqTransport::new();
    let auth = SecretValue::new("lin_api_raw-key-sentinel");
    let resp = transport
        .post_json(&url, &auth, &json!({"query": "query { viewer { id } }"}))
        .expect("round trip");
    assert_eq!(resp.pointer("/data/ok"), Some(&json!(true)));

    let raw = server.join().expect("server thread");
    let lower = raw.to_lowercase();
    assert!(lower.starts_with("post /graphql http/1.1"), "raw: {raw}");
    assert!(
        lower.contains("authorization: lin_api_raw-key-sentinel"),
        "personal keys ride raw in the Authorization header"
    );
    assert!(
        !lower.contains("bearer"),
        "Linear personal keys must NOT use a Bearer prefix"
    );
    assert!(lower.contains("content-type: application/json"));
    assert!(raw.contains("\"query\""), "GraphQL envelope in the body");
}

#[test]
fn non_2xx_maps_to_actionable_network_error_without_body_echo() {
    let (url, server) = serve_once(
        "401 Unauthorized",
        r#"{"error":"BODY-SENTINEL should never surface"}"#,
    );
    let transport = UreqTransport::new();
    let auth = SecretValue::new("k");
    let err = transport
        .post_json(&url, &auth, &json!({"query": "q"}))
        .expect_err("401 must error");
    let TransportError::Http { status, hint } = &err else {
        panic!("wrong class: {err:?}");
    };
    assert_eq!(*status, 401);
    assert!(hint.contains("linear_api"), "actionable hint: {hint}");
    assert!(!hint.contains("BODY-SENTINEL"));

    let lane_err = linear_to_lane(err);
    assert_eq!(lane_err.exit_code(), 2);
    let msg = lane_err.to_string();
    assert!(msg.contains("401"));
    assert!(!msg.contains("BODY-SENTINEL"));
    let _ = server.join();
}

#[test]
fn silent_server_hits_the_bounded_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let url = format!("http://{addr}/graphql");
    // Accept and read, but never respond; hold the stream open well past the client
    // timeout (generous margins, not a tight window — the ZER-83 anti-pattern).
    let _server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = read_full_request(&mut stream);
        thread::sleep(Duration::from_secs(5));
    });
    let transport = UreqTransport::with_timeout(Duration::from_millis(300));
    let auth = SecretValue::new("k");
    let start = Instant::now();
    let err = transport
        .post_json(&url, &auth, &json!({"query": "q"}))
        .expect_err("must time out");
    let elapsed = start.elapsed();
    assert!(matches!(err, TransportError::Network(_)), "{err:?}");
    assert!(
        elapsed < Duration::from_secs(3),
        "timeout did not bound the wait: {elapsed:?}"
    );
}

#[test]
fn non_loopback_http_is_refused_before_any_connection() {
    let transport = UreqTransport::new();
    let auth = SecretValue::new("k");
    // Unroutable host — if the policy check failed, this would attempt a connection
    // and hang; the UrlPolicy refusal must be immediate.
    let start = Instant::now();
    let err = transport
        .post_json("http://10.255.255.1/graphql", &auth, &json!({"q": 1}))
        .expect_err("must refuse plain http to a non-loopback host");
    assert!(matches!(err, TransportError::UrlPolicy(_)), "{err:?}");
    assert!(start.elapsed() < Duration::from_secs(1));
    let msg = linear_to_lane(err).to_string();
    assert!(msg.contains("https"));
}
