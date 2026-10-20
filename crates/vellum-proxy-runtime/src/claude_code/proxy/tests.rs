use super::*;
use crate::claude_code::certificate::CodeCertificate;
use axum::{extract::State, routing::any, Router};
use serde_json::json;
use tokio::sync::{mpsc, RwLock};

struct Accounts(
    RwLock<Option<String>>,
    Mutex<Vec<(String, UsageObservation)>>,
);
#[async_trait]
impl ClaudeAccounts for Accounts {
    async fn observe_usage(&self, id: &str, usage: UsageObservation) {
        self.1.lock().unwrap().push((id.to_owned(), usage));
    }
    async fn selected_token(&self) -> Result<Option<AccountToken>, String> {
        Ok(self.0.read().await.clone().map(|id| AccountToken {
            access_token: format!("token-{id}"),
            account_id: id,
        }))
    }
}
struct Captured {
    path: String,
    headers: HeaderMap,
    body: Bytes,
}
struct Fixture {
    proxy_url: String,
    ca_pem: String,
    client: reqwest::Client,
    proxy: Arc<ClaudeCodeProxy>,
    accounts: Arc<Accounts>,
    captured: mpsc::Receiver<Captured>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    upstream: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new() -> Self {
        Self::scoped(ProxyScope::Project).await
    }
    async fn scoped(scope: ProxyScope) -> Self {
        let (send, captured) = mpsc::channel(32);
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream_listener.local_addr().unwrap();
        let app=Router::new().fallback(any(|State(send):State<mpsc::Sender<Captured>>,request:Request<Body>| async move {
            let (parts,body)=request.into_parts();
            let bytes=to_bytes(body,MAX_BODY).await.unwrap();
            let is_reject=parts.headers.contains_key("x-test-reject");
            let streaming=parts.headers.contains_key("x-test-stream");
            let native_stream=serde_json::from_slice::<serde_json::Value>(&bytes).ok().is_some_and(|v|v["stream"]==true);
            let path=parts.uri.path().to_string();
            let method=parts.method.clone();
            send.send(Captured {path:parts.uri.to_string(),headers:parts.headers,body:bytes}).await.unwrap();
            if is_reject {return failure(StatusCode::TOO_MANY_REQUESTS,"rate_limit_error");}
            if native_stream {return native_completion();}
            if method==Method::GET {return Response::new(Body::from("{}"));}
            if path=="/v1/messages/count_tokens" {return Response::new(Body::from("{\"input_tokens\":10}"));}
            if streaming {
                let stream=async_stream::stream! {
                    yield Ok::<Bytes,std::io::Error>(Bytes::from_static(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n"));
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    yield Ok(Bytes::from_static(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
                };
                let mut response=Response::new(Body::from_stream(stream));
                response.headers_mut().insert(header::CONTENT_TYPE,HeaderValue::from_static("text/event-stream"));
                return response;
            }
            let mut response = Response::new(Body::from("upstream-response"));
            for (name, value) in [
                ("anthropic-ratelimit-unified-5h-utilization", "0.85"),
                ("anthropic-ratelimit-unified-5h-reset", "1900000000"),
                ("anthropic-ratelimit-unified-7d-utilization", "0.5"),
                ("anthropic-ratelimit-unified-7d-reset", "1900600000"),
            ] { response.headers_mut().insert(name, value.parse().unwrap()); }
            response
        })).with_state(send);
        let upstream = tokio::spawn(async move {
            axum::serve(upstream_listener, app).await.unwrap();
        });
        let accounts = Arc::new(Accounts(
            RwLock::new(Some("B".into())),
            Mutex::new(Vec::new()),
        ));
        let proxy = ClaudeCodeProxy::build_scoped(
            "test-secret",
            accounts.clone(),
            format!("http://{address}"),
            scope,
        )
        .unwrap();
        let certificate = CodeCertificate::generate().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, wait) = watch::channel(false);
        let task = tokio::spawn(proxy.clone().serve(
            listener,
            certificate.server_config().unwrap(),
            wait,
        ));
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .add_root_certificate(
                reqwest::Certificate::from_pem(certificate.ca_pem.as_bytes()).unwrap(),
            )
            .proxy(
                reqwest::Proxy::all(format!("http://{address}"))
                    .unwrap()
                    .basic_auth("vellum", "test-secret"),
            )
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        Self {
            proxy_url: format!("http://vellum:test-secret@{address}"),
            ca_pem: certificate.ca_pem,
            client,
            proxy,
            accounts,
            captured,
            stop,
            task,
            upstream,
        }
    }
    fn request(&self, path: &str, body: Vec<u8>) -> reqwest::RequestBuilder {
        self.client
            .post(format!("https://api.anthropic.com{path}"))
            .header(
                header::USER_AGENT,
                "claude-cli/2.1.299 (external, claude-desktop)",
            )
            .bearer_auth("original-A")
            .header("anthropic-beta", "oauth-2025-04-20")
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
    }
    async fn stop(self) {
        let _ = self.stop.send(true);
        self.task.await.unwrap();
        self.upstream.abort();
    }
}

fn native_completion() -> Response {
    let events = [
        (
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_offline","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"OFFLINE_OK"}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ];
    let text = events
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect::<String>();
    let mut response = Response::new(Body::from(text));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
}

/// Opt-in local compatibility probe. Uses fake credentials, an isolated config
/// root, and refuses all external tunnels. It never connects to Anthropic.
#[tokio::test]
#[ignore = "set VELLUM_CLAUDE_CODE_EXE to the installed bundled CLI; no real accounts are used"]
async fn bundled_cli_offline_probe() {
    bundled_probe(false, false, false).await;
}

#[tokio::test]
#[ignore = "set VELLUM_CLAUDE_CODE_EXE; isolated Desktop SDK startup with fake credentials"]
async fn bundled_sdk_offline_probe() {
    bundled_probe(true, false, false).await;
}

#[tokio::test]
#[ignore = "set VELLUM_CLAUDE_CODE_EXE; isolated host base URL precedence probe"]
async fn bundled_sdk_project_base_url_does_not_override_host() {
    bundled_probe(true, true, false).await;
}

#[tokio::test]
#[ignore = "set VELLUM_CLAUDE_CODE_EXE; isolated project auth precedence probe with fake tokens"]
async fn bundled_sdk_project_auth_token_does_not_override_host() {
    bundled_probe(true, false, true).await;
}

/// Python 3.13+ enables OpenSSL's X509_STRICT checks, which caught the missing
/// authority key identifier that permissive TLS clients accepted.
#[tokio::test]
#[ignore = "set VELLUM_TLS_PROBE_PYTHON to Python 3.13+; strict TLS, loopback only"]
async fn strict_openssl_certificate_probe() {
    let executable =
        std::env::var_os("VELLUM_TLS_PROBE_PYTHON").expect("set VELLUM_TLS_PROBE_PYTHON");
    let f = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let ca = temp.path().join("ca.pem");
    std::fs::write(&ca, &f.ca_pem).unwrap();
    let code = r#"
import base64, socket, ssl, sys
from urllib.parse import urlsplit
proxy = urlsplit(sys.argv[1])
auth = base64.b64encode(f'{proxy.username}:{proxy.password}'.encode()).decode()
with socket.create_connection((proxy.hostname, proxy.port), timeout=5) as connection:
    connection.sendall(f'CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: api.anthropic.com:443\r\nProxy-Authorization: Basic {auth}\r\n\r\n'.encode())
    response = b''
    while not response.endswith(b'\r\n\r\n'):
        chunk = connection.recv(1)
        assert chunk, 'Unexpected CONNECT EOF'
        response += chunk
    assert response.startswith(b'HTTP/1.1 200')
    context = ssl.create_default_context(cafile=sys.argv[2])
    context.verify_flags |= ssl.VERIFY_X509_STRICT
    with context.wrap_socket(connection, server_hostname='api.anthropic.com'):
        print('STRICT_TLS_OK')
"#;
    let mut command = tokio::process::Command::new(executable);
    command
        .args(["-c", code, &f.proxy_url])
        .arg(&ca)
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("STRICT_TLS_OK"));
    assert_eq!(f.proxy.status().tls_failures, 0);
    assert_eq!(f.proxy.status().desktop_requests, 0);
    f.stop().await;
}

async fn bundled_probe(sdk: bool, gateway: bool, project_auth: bool) {
    let executable =
        std::env::var_os("VELLUM_CLAUDE_CODE_EXE").expect("set VELLUM_CLAUDE_CODE_EXE");
    let mut f = Fixture::new().await;
    if project_auth {
        // Observe the native client's credential choice without proxy rewriting.
        *f.accounts.0.write().await = None;
    }
    let temp = tempfile::tempdir().unwrap();
    let ca = temp.path().join("ca.pem");
    std::fs::write(&ca, &f.ca_pem).unwrap();
    let settings = temp.path().join(".claude/settings.local.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(
        &settings,
        serde_json::to_vec(&if project_auth {
            json!({"env":{"ANTHROPIC_AUTH_TOKEN":"sk-ant-oat01-offline-fake-B","HTTPS_PROXY":f.proxy_url,"NODE_EXTRA_CA_CERTS":ca}})
        } else if gateway {
            // Keep the offline proxy in place even if the native client ignores
            // the requested base URL. No request may reach the real API.
            json!({"env":{"ANTHROPIC_BASE_URL":f.proxy.upstream,"HTTPS_PROXY":f.proxy_url,"NODE_EXTRA_CA_CERTS":ca}})
        } else {
            json!({"env":{"HTTPS_PROXY":f.proxy_url,"NODE_EXTRA_CA_CERTS":ca}})
        })
        .unwrap(),
    )
    .unwrap();
    let mut command = tokio::process::Command::new(executable);
    command
        .env_clear()
        .current_dir(temp.path())
        .kill_on_drop(true);
    for key in ["SystemRoot", "SystemDrive", "PATH", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("CLAUDE_CONFIG_DIR", temp.path().join("config"))
        .env("ANTHROPIC_BASE_URL", "https://api.anthropic.com")
        .env("CLAUDE_CODE_ENTRYPOINT", "claude-desktop")
        .env("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-offline-fake-A")
        .env("CLAUDE_CODE_OAUTH_SCOPES", "user:profile user:inference")
        .env("CLAUDE_CODE_SUBSCRIPTION_TYPE", "max")
        .env("NODE_USE_SYSTEM_CA", "1")
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1")
        .args([
            "--no-session-persistence",
            "--max-turns",
            "1",
            "--model",
            "claude-sonnet-4-6",
        ]);
    if sdk {
        command
            .env("CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH", "1")
            .env("CLAUDE_CODE_SDK_HAS_OAUTH_REFRESH", "1")
            .env("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1")
            .env("CLAUDE_CODE_DESKTOP_APP_VERSION", "2.42304.2")
            .env("CLAUDE_CODE_HOST_SESSION_ID", "local_offline_probe")
            .env("CLAUDE_CODE_EAGER_FLUSH", "1")
            .env("CLAUDE_CODE_SIMPLE", "0");
        command.args([
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--await-initialize",
            "--setting-sources=user,project,local",
        ]);
        command.stdin(std::process::Stdio::piped());
    } else {
        command.args(["--print", "Return OFFLINE_OK."]);
    }
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    if sdk {
        use tokio::io::AsyncWriteExt;
        let mut input = child.stdin.take().unwrap();
        let initialize = json!({"type":"control_request","request_id":"offline-initialize","request":{"subtype":"initialize"}});
        let user = json!({"type":"user","session_id":"","message":{"role":"user","content":"Return OFFLINE_OK."},"parent_tool_use_id":null});
        input
            .write_all(format!("{initialize}\n{user}\n").as_bytes())
            .await
            .unwrap();
        input.shutdown().await.unwrap();
    }
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("bundled CLI probe timed out")
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("OFFLINE_OK"),
        "CLI probe failed: status={} stdout={} stderr={}",
        output.status,
        stdout
            .chars()
            .rev()
            .take(2000)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>(),
        stderr.chars().take(2000).collect::<String>()
    );
    let mut saw_inference = false;
    while let Ok(sent) = f.captured.try_recv() {
        if sent.path.starts_with("/v1/messages") {
            let expected = if project_auth {
                // Desktop SDK host authentication wins over project auth env.
                "Bearer sk-ant-oat01-offline-fake-A"
            } else {
                "Bearer token-B"
            };
            assert_eq!(sent.headers[header::AUTHORIZATION], expected);
            saw_inference = true;
        }
    }
    assert!(saw_inference, "native client did not traverse the proxy");
    assert!(f.proxy.status().desktop_requests > 0);
    f.stop().await;
}
fn body() -> Vec<u8> {
    serde_json::to_vec(&json!({"model":"claude-sonnet-4-6","metadata":{"user_id":"{\"device_id\":\"device-A\",\"account_uuid\":\"A\",\"session_id\":\"existing-session-123\"}"},"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"opaque","signature":"preserve-exactly"},{"type":"tool_use","id":"tool_1","name":"Read","input":{"path":"test"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":"result"}]}]})).unwrap()
}

#[tokio::test]
async fn untrusted_client_ca_is_observable_before_any_inference() {
    let mut f = Fixture::new().await;
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&f.proxy_url).unwrap())
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    assert!(client
        .post("https://api.anthropic.com/v1/messages")
        .body(body())
        .send()
        .await
        .is_err());
    for _ in 0..100 {
        if f.proxy.status().tls_failures > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let status = f.proxy.status();
    assert_eq!(status.connect_requests, 1);
    assert_eq!(status.tls_failures, 1);
    assert_eq!(status.api_requests, 0);
    assert_eq!(status.desktop_requests, 0);
    assert!(status
        .last_error
        .as_deref()
        .is_some_and(|s| s.starts_with("client_")));
    assert!(f.captured.try_recv().is_err());
    f.request("/v1/messages", body())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(f.proxy.status().last_error.is_none());
    f.stop().await;
}

#[tokio::test]
async fn desktop_inference_changes_only_auth_and_keeps_existing_history() {
    let mut f = Fixture::new().await;
    let bytes = body();
    let response = f
        .request("/v1/messages?beta=true", bytes.clone())
        .header("x-api-key", "must-remove")
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "upstream-response");
    let sent = f.captured.recv().await.unwrap();
    assert_eq!(sent.path, "/v1/messages?beta=true");
    assert_eq!(sent.body, bytes);
    assert_eq!(sent.headers[header::AUTHORIZATION], "Bearer token-B");
    assert!(!sent.headers.contains_key("x-api-key"));
    assert!(!sent.headers.contains_key(header::PROXY_AUTHORIZATION));
    assert_eq!(f.proxy.status().records[0].account_id.as_deref(), Some("B"));
    assert_eq!(f.proxy.status().records[0].operation, "messages");
    *f.accounts.0.write().await = Some("C".into());
    f.request("/v1/messages/count_tokens", body())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        f.captured.recv().await.unwrap().headers[header::AUTHORIZATION],
        "Bearer token-C"
    );
    assert_eq!(f.proxy.status().records[1].operation, "count_tokens");
    f.stop().await;
}

#[tokio::test]
async fn profile_and_standalone_code_retain_original_authorization() {
    let mut f = Fixture::new().await;
    for (path, ua) in [
        (
            "/api/oauth/profile",
            "claude-cli/2.1.299 (external, claude-desktop)",
        ),
        ("/v1/messages", "claude-cli/2.1.299 (external, cli)"),
        (
            "/v1/messages/other",
            "claude-cli/2.1.299 (external, claude-desktop)",
        ),
    ] {
        f.request(path, body())
            .header(header::USER_AGENT, ua)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(
            f.captured.recv().await.unwrap().headers[header::AUTHORIZATION],
            "Bearer original-A"
        );
    }
    assert_eq!(f.proxy.status().desktop_requests, 0);
    f.stop().await;
}

#[tokio::test]
async fn gzip_is_inspected_but_forwarded_byte_for_byte_and_unknown_identity_is_refused() {
    use std::io::Write;
    let mut f = Fixture::new().await;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&body()).unwrap();
    let bytes = encoder.finish().unwrap();
    f.request("/v1/messages", bytes.clone())
        .header(header::CONTENT_ENCODING, "gzip")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let sent = f.captured.recv().await.unwrap();
    assert_eq!(sent.body, bytes);
    assert_eq!(sent.headers[header::CONTENT_ENCODING], "gzip");
    let response = f
        .request("/v1/messages", b"{}".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(f.captured.try_recv().is_err());
    f.stop().await;
}

#[tokio::test]
async fn stream_is_incremental_pins_its_account_and_stop_cancels_it() {
    let mut f = Fixture::new().await;
    let mut response = f
        .request("/v1/messages", body())
        .header("x-test-stream", "1")
        .send()
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), response.chunk())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(std::str::from_utf8(&first)
        .unwrap()
        .contains("message_start"));
    assert_eq!(f.proxy.status().active_requests, 1);
    // Drain closes admission atomically, including the gap after a caller's
    // status check. Existing streams stay alive until an explicit cancellation.
    assert!(!f.proxy.begin_drain());
    let rejected = f.request("/v1/messages", body()).send().await.unwrap();
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(f.proxy.status().active_requests, 1);
    assert_eq!(f.proxy.status().records[0].state, "streaming");
    f.proxy.cancel_drain();
    *f.accounts.0.write().await = Some("C".into());
    assert_eq!(
        f.captured.recv().await.unwrap().headers[header::AUTHORIZATION],
        "Bearer token-B"
    );
    assert_eq!(f.proxy.status().records[0].account_id.as_deref(), Some("B"));
    let _ = f.stop.send(true);
    assert!(response.text().await.is_err());
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(f.proxy.status().active_requests, 0);
    assert_eq!(f.proxy.status().records[0].state, "interrupted");
    f.stop().await;
}

#[tokio::test]
async fn non_inference_http_work_is_visible_to_safe_stop_controls() {
    let f = Fixture::new().await;
    let response = f
        .client
        .post("https://api.anthropic.com/api/profile-test")
        .header("x-test-stream", "1")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(f.proxy.status().desktop_requests, 0);
    assert_eq!(f.proxy.status().active_requests, 1);
    assert!(!f.proxy.begin_drain());
    let proxy = f.proxy.clone();
    f.stop().await;
    drop(response);
    assert_eq!(proxy.status().active_requests, 0);
}

#[tokio::test]
async fn quota_rejection_is_preserved_without_replaying_or_falling_back() {
    let mut f = Fixture::new().await;
    let response = f
        .request("/v1/messages", body())
        .header("x-test-reject", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.text().await.unwrap().contains("rate_limit_error"));
    let _ = f.captured.recv().await.unwrap();
    assert!(f.captured.try_recv().is_err());
    assert_eq!(f.proxy.status().records[0].state, "upstream_rejected");
    f.stop().await;
}

#[tokio::test]
async fn connect_requires_secret_and_rejects_private_destinations() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let f = Fixture::new().await;
    // Exercise the same CONNECT handler without creating an external connection.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let certificate = CodeCertificate::generate().unwrap();
    let (stop, wait) = watch::channel(false);
    let task = tokio::spawn(f.proxy.clone().serve(
        listener,
        certificate.server_config().unwrap(),
        wait,
    ));
    for (auth, expected) in [
        ("", 407),
        (
            "Proxy-Authorization: Basic dmVsbHVtOnRlc3Qtc2VjcmV0\r\n",
            403,
        ),
    ] {
        let mut socket = TcpStream::connect(addr).await.unwrap();
        socket
            .write_all(
                format!("CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1:443\r\n{auth}\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut bytes = [0u8; 2048];
        let n = socket.read(&mut bytes).await.unwrap();
        assert!(std::str::from_utf8(&bytes[..n])
            .unwrap()
            .contains(&expected.to_string()));
    }
    let _ = stop.send(true);
    task.await.unwrap();
    f.stop().await;
}

#[tokio::test]
async fn usage_headers_only_observe_the_desktop_inference_execution_account() {
    let mut fixture = Fixture::new().await;
    let body =
        serde_json::to_vec(&json!({"metadata":{"user_id":"user_session_12345678"}})).unwrap();
    fixture
        .request("/v1/messages", body.clone())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    fixture.captured.recv().await.unwrap();
    {
        let observations = fixture.accounts.1.lock().unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].0, "B");
        assert_eq!(observations[0].1.five_hour.unwrap().utilization, 85.0);
    }
    for (path, ua) in [
        ("/v1/messages", "claude-cli/2.1.299 (external, cli)"),
        (
            "/api/oauth/usage",
            "claude-cli/2.1.299 (external, claude-desktop)",
        ),
    ] {
        fixture
            .request(path, body.clone())
            .header(header::USER_AGENT, ua)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let captured = fixture.captured.recv().await.unwrap();
        assert_eq!(captured.headers[header::AUTHORIZATION], "Bearer original-A");
        assert_eq!(captured.body.as_ref(), body.as_slice());
    }
    *fixture.accounts.0.write().await = None;
    fixture
        .request("/v1/messages", body)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    fixture.captured.recv().await.unwrap();
    assert_eq!(fixture.accounts.1.lock().unwrap().len(), 1);
    assert!(!serde_json::to_string(&fixture.proxy.status())
        .unwrap()
        .contains("utilization"));
}

// Read only the CONNECT response head: the following bytes belong to the
// opaque protocol and must not be consumed by a buffered HTTP reader.
async fn connect_tunnel(f: &Fixture, target: &str, authenticated: bool) -> (TcpStream, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let address = f.proxy_url.rsplit_once('@').unwrap().1;
    let mut socket = TcpStream::connect(address).await.unwrap();
    let auth = if authenticated {
        "Proxy-Authorization: Basic dmVsbHVtOnRlc3Qtc2VjcmV0\r\n"
    } else {
        ""
    };
    socket
        .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{auth}\r\n").as_bytes())
        .await
        .unwrap();
    let mut head = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.unwrap());
            assert!(head.len() < 4096);
        }
    })
    .await
    .unwrap();
    (socket, String::from_utf8(head).unwrap())
}

async fn echo_target() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (socket, _) = accepted.unwrap();
                    connections.spawn(async move {
                        let (mut read, mut write) = socket.into_split();
                        let _ = tokio::io::copy(&mut read, &mut write).await;
                    });
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    });
    (address, task)
}

#[tokio::test]
async fn user_scope_private_tunnels_are_authenticated_opaque_and_not_limited_by_mitm_slots() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let f = Fixture::scoped(ProxyScope::User).await;
    let (target, echo) = echo_target().await;
    let (_, denied) = connect_tunnel(&f, &target.to_string(), false).await;
    assert!(denied.starts_with("HTTP/1.1 407"));
    // Deliberately exhaust all 64 MITM slots. More than 64 raw tunnels must
    // still succeed concurrently, including loopback on a nonstandard port.
    let slots = f
        .proxy
        .tunnel_slots
        .clone()
        .acquire_many_owned(64)
        .await
        .unwrap();
    let mut pending = JoinSet::new();
    for _ in 0..96 {
        let address = target.to_string();
        let proxy = &f;
        // Own a minimal handle for 'static spawning without duplicating Fixture.
        let proxy_address = proxy.proxy_url.rsplit_once('@').unwrap().1.to_owned();
        pending.spawn(async move {
            let mut socket = TcpStream::connect(proxy_address).await.unwrap();
            socket.write_all(format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\nProxy-Authorization: Basic dmVsbHVtOnRlc3Qtc2VjcmV0\r\n\r\n").as_bytes()).await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
                assert!(head.len() < 4096);
            }
            assert!(String::from_utf8(head).unwrap().starts_with("HTTP/1.1 200"));
            socket
        });
    }
    let mut sockets = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = pending.join_next().await {
            sockets.push(result.unwrap());
        }
    })
    .await
    .unwrap();
    assert_eq!(sockets.len(), 96);
    for socket in &mut sockets {
        // Opaque bytes round-trip; the proxy never interprets or MITMs them.
        socket.write_all(b"opaque TLS bytes").await.unwrap();
        let mut bytes = [0u8; 16];
        socket.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"opaque TLS bytes");
    }
    let (mut local, head) = connect_tunnel(&f, &format!("localhost:{}", target.port()), true).await;
    assert!(head.starts_with("HTTP/1.1 200"));
    local.write_all(b"x").await.unwrap();
    assert_eq!(local.read_u8().await.unwrap(), b'x');
    let (_, limited) = connect_tunnel(&f, "api.anthropic.com:443", true).await;
    assert!(limited.starts_with("HTTP/1.1 503"));
    let (_, port_denied) = connect_tunnel(&f, "api.anthropic.com:8443", true).await;
    assert!(port_denied.starts_with("HTTP/1.1 403"));
    assert_eq!(f.proxy.status().api_requests, 0);
    assert_eq!(f.proxy.status().tls_failures, 0);
    drop(slots);
    drop(sockets);
    drop(local);
    f.stop().await;
    echo.abort();
    let _ = echo.await;
}

#[tokio::test]
async fn project_scope_keeps_private_port_and_tunnel_limits() {
    let f = Fixture::new().await;
    let (target, echo) = echo_target().await;
    for target in [
        target.to_string(),
        "127.0.0.1:443".into(),
        "localhost:443".into(),
    ] {
        let (_, head) = connect_tunnel(&f, &target, true).await;
        assert!(head.starts_with("HTTP/1.1 403"));
    }
    let slots = f
        .proxy
        .tunnel_slots
        .clone()
        .acquire_many_owned(64)
        .await
        .unwrap();
    let (_, head) = connect_tunnel(&f, "api.anthropic.com:443", true).await;
    assert!(head.starts_with("HTTP/1.1 503"));
    drop(slots);
    f.stop().await;
    echo.abort();
    let _ = echo.await;
}
