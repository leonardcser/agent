#![cfg(unix)]

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct ChildProcessGroup {
    child: Child,
    id: i32,
}

impl Drop for ChildProcessGroup {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            unsafe {
                libc::kill(-self.id, libc::SIGKILL);
            }
        }
        let _ = self.child.wait();
    }
}

fn open_pty() -> (File, File) {
    let mut master = -1;
    let mut slave = -1;
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(result, 0, "openpty: {}", std::io::Error::last_os_error());

    let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
    assert_ne!(flags, -1, "fcntl(F_GETFL)");
    assert_ne!(
        unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        -1,
        "fcntl(F_SETFL)"
    );

    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

fn spawn_in_pty(mut command: Command) -> (File, ChildProcessGroup) {
    let (master, slave) = open_pty();
    let stdin = slave.try_clone().expect("clone PTY slave for stdin");
    let stdout = slave.try_clone().expect("clone PTY slave for stdout");
    command
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn().expect("launch smelt in a PTY");
    let process = ChildProcessGroup {
        id: child.id() as i32,
        child,
    };
    (master, process)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn drain_provider_requests(listener: &TcpListener) -> bool {
    let mut model_request_seen = false;
    loop {
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => panic!("accept provider request: {error}"),
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set provider read timeout");
        let mut request = [0_u8; 16 * 1024];
        let read = stream.read(&mut request).unwrap_or(0);
        model_request_seen |= request[..read].starts_with(b"POST ");
        let body = r#"{"data":[]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
    }
    model_request_seen
}

#[test]
fn interactive_startup_renders_and_queues_prompt_while_mcp_discovery_is_stalled() {
    let home = tempfile::tempdir().expect("temporary home");
    let provider = TcpListener::bind("127.0.0.1:0").expect("bind test provider");
    provider
        .set_nonblocking(true)
        .expect("make test provider nonblocking");
    let config = home.path().join("init.lua");
    std::fs::write(
        &config,
        format!(
            r#"
smelt.settings.autoupgrade = "off"

smelt.provider.register("local", {{
  type = "openai-compatible",
  api_base = "http://{}/v1",
  models = {{ "test-model" }},
}})

smelt.mcp.register("stalled", {{
  type = "local",
  command = {{ "sh", "-c", "sleep 30" }},
  timeout = 30000,
}})
"#,
            provider.local_addr().unwrap()
        ),
    )
    .expect("write init.lua");

    for name in ["state", "cache", "data"] {
        std::fs::create_dir(home.path().join(name)).expect("create XDG directory");
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args([
            "--config",
            config.to_str().unwrap(),
            "--ephemeral",
            "wait for MCP tools",
        ])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();
    let mut rendered_at = None;

    while Instant::now() < deadline {
        drain_pty(&mut master, &mut captured);

        assert!(
            !drain_provider_requests(&provider),
            "initial prompt reached the model before MCP discovery settled:\n{}",
            String::from_utf8_lossy(&captured)
        );
        let alternate_screen = contains(&captured, b"\x1b[?1049h");
        let rendered_model = contains(&captured, b"local/test-model");
        if alternate_screen && rendered_model {
            let first_render = rendered_at.get_or_insert_with(Instant::now);
            if first_render.elapsed() >= Duration::from_secs(1) {
                return;
            }
        }
        if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
            panic!(
                "smelt exited before rendering ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    panic!(
        "smelt did not render before stalled MCP discovery timed out:\n{}",
        String::from_utf8_lossy(&captured)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_error_does_not_dispatch_queued_input() {
    interactive_quota_pause(false, QuotaRecovery::Wait).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_background_completion_does_not_bypass_quota_pause() {
    interactive_quota_pause(true, QuotaRecovery::Wait).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_reset_resumes_original_work_before_queued_input() {
    interactive_quota_pause(true, QuotaRecovery::Resume).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_retry_detects_early_reset_without_consuming_queued_input() {
    interactive_quota_pause(true, QuotaRecovery::EarlyResume).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_double_escape_cancels_quota_retry_without_losing_queued_input() {
    interactive_quota_pause(true, QuotaRecovery::Cancel).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_recovery_preserves_command_overrides() {
    interactive_quota_pause(true, QuotaRecovery::CommandResume).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_recovery_cancels_foreground_work() {
    interactive_quota_pause(true, QuotaRecovery::CancelBusy).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_recovery_retains_deadline_without_new_metadata() {
    interactive_quota_pause(true, QuotaRecovery::MissingReset).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_recovery_can_be_cancelled_from_turn_end_hook() {
    interactive_quota_pause(true, QuotaRecovery::CancelHook).await;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuotaRecovery {
    Wait,
    Resume,
    EarlyResume,
    CommandResume,
    MissingReset,
    Cancel,
    CancelBusy,
    CancelHook,
}

async fn interactive_quota_pause(background: bool, recovery: QuotaRecovery) {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn main_request(request: &Request) -> Option<serde_json::Value> {
        request
            .body_json::<serde_json::Value>()
            .ok()
            .filter(|body| {
                body["tools"]
                    .as_array()
                    .is_some_and(|tools| !tools.is_empty())
            })
    }

    let home = tempfile::tempdir().expect("temporary home");
    let provider = MockServer::start().await;
    let attempts = AtomicUsize::new(0);
    let resume = matches!(
        recovery,
        QuotaRecovery::Resume
            | QuotaRecovery::EarlyResume
            | QuotaRecovery::CommandResume
            | QuotaRecovery::MissingReset
    );
    let short_wait = matches!(
        recovery,
        QuotaRecovery::Resume
            | QuotaRecovery::CommandResume
            | QuotaRecovery::Cancel
            | QuotaRecovery::CancelBusy
    );
    let command_resume = recovery == QuotaRecovery::CommandResume;
    let cancel_hook = recovery == QuotaRecovery::CancelHook;
    let quota_attempts = if recovery == QuotaRecovery::MissingReset {
        2
    } else {
        1
    };
    let retry_after = if recovery == QuotaRecovery::MissingReset {
        "90"
    } else if short_wait {
        "2"
    } else {
        "3600"
    };
    let resumed_at = Arc::new(AtomicU64::new(0));
    let resumed_at_response = Arc::clone(&resumed_at);
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let main = main_request(request).is_some();
            let attempt = if main {
                attempts.fetch_add(1, Ordering::SeqCst)
            } else {
                0
            };
            if !resume || (main && attempt < quota_attempts) {
                let response = ResponseTemplate::new(429)
                    .set_body_json(serde_json::json!({"error": {"code": "insufficient_quota"}}))
                    .set_delay(Duration::from_secs(1));
                return if recovery == QuotaRecovery::MissingReset && main && attempt == 1 {
                    response
                } else {
                    response.insert_header("retry-after", retry_after)
                };
            }
            if main && attempt == quota_attempts {
                resumed_at_response.store(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64,
                    Ordering::SeqCst,
                );
            }
            let content = if main {
                "completed requested work"
            } else {
                "test session"
            };
            let chunk = serde_json::json!({
                "id": "chatcmpl-quota", "object": "chat.completion.chunk",
                "choices": [{ "index": 0, "delta": { "role": "assistant", "content": content } }],
            });
            let finish = serde_json::json!({
                "id": "chatcmpl-quota", "object": "chat.completion.chunk",
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            });
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(format!(
                    "data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"
                ))
                .set_delay(Duration::from_secs(1))
        })
        .mount(&provider)
        .await;
    let config = home.path().join("init.lua");
    std::fs::write(
        &config,
        format!(
            r#"
smelt.settings.autoupgrade = "off"
smelt.settings.auto_continue = "always"
local function record_busy_state()
  local file = assert(io.open("foreground-busy", "w"))
  file:write(tostring(smelt.work.is_busy()))
  file:close()
end
local started_background = false
smelt.events.on("turn_end", function(ev)
  if {resume} and not ev.cancelled then smelt.settings.auto_continue = "off" end
  if ev.error_kind == "quota" and ev.retry_at_ms then
    local file = assert(io.open("quota-deadline", "w"))
    file:write(tostring(ev.retry_at_ms))
    file:close()
  end
  if {background} and ev.error_kind == "quota" and not started_background then
    started_background = true
    smelt.spawn(function() smelt.process.spawn_bg("sleep 0.1") end)
  end
  if {cancel_hook} and ev.error_kind == "quota" then
    _G.quota_busy = smelt.work.busy("foreground quota check")
    smelt.engine.cancel()
    record_busy_state()
    local file = assert(io.open("hook-pause-kind", "w"))
    file:write(tostring(smelt.engine.continuation_state().error_kind))
    file:close()
  end
end)
smelt.provider.register("local", {{
  type = "openai-compatible",
  api_base = "{}",
  models = {{ "test-model", "command-model" }},
}})
smelt.lifecycle.on_ready(function() smelt.model.set("local/test-model") end)
smelt.cmd.register("quota-command", function()
  smelt.engine.submit_command("quota-command", "original request", {{
    model = "local/command-model", temperature = 0.3, tools = {{ deny = {{ "bash" }} }},
  }})
end)
smelt.cmd.register("quota-busy", function()
  _G.quota_busy = smelt.work.busy("foreground quota check")
end)
smelt.signal.subscribe("work_busy", record_busy_state)
"#,
            provider.uri()
        ),
    )
    .expect("write init.lua");
    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args(["--config", config.to_str().unwrap(), "--ephemeral"])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    if !command_resume {
        command.arg("original request");
    }
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now()
        + Duration::from_secs(match recovery {
            QuotaRecovery::EarlyResume => 90,
            QuotaRecovery::MissingReset => 120,
            _ => 30,
        });
    let mut captured = Vec::new();
    let mut command_submitted = false;
    let mut busy_submitted = false;
    let mut queued_at = None;
    let mut cancelled_at = None;
    let mut follow_up_at = None;
    loop {
        drain_pty(&mut master, &mut captured);
        assert!(process.child.try_wait().unwrap().is_none(), "smelt exited");
        let requests = provider.received_requests().await.unwrap();
        let bodies: Vec<_> = requests.iter().filter_map(main_request).collect();
        let posts = bodies.len();
        if command_resume && !command_submitted && contains(&captured, b"local/test-model") {
            master
                .write_all(b"/quota-command\r")
                .expect("submit scoped command");
            command_submitted = true;
        }
        assert!(
            posts <= if resume { quota_attempts + 2 } else { 1 },
            "unexpected automatic turn after quota error"
        );
        if posts == 1 && queued_at.is_none() {
            master
                .write_all(b"queued follow-up\r")
                .expect("queue a user message");
            queued_at = Some(Instant::now());
        }
        if recovery == QuotaRecovery::CancelBusy
            && !busy_submitted
            && contains(&captured, b"resuming at")
        {
            master
                .write_all(b"/quota-busy\r")
                .expect("start foreground work while paused");
            busy_submitted = true;
        }
        if matches!(recovery, QuotaRecovery::Cancel | QuotaRecovery::CancelBusy)
            && cancelled_at.is_none()
            && contains(&captured, b"resuming at")
            && (recovery != QuotaRecovery::CancelBusy
                || std::fs::read_to_string(home.path().join("foreground-busy"))
                    .is_ok_and(|value| value == "true"))
        {
            master
                .write_all(b"\x1b\x1b")
                .expect("cancel scheduled retry");
            cancelled_at = Some(Instant::now());
        }
        if cancel_hook && cancelled_at.is_none() {
            if let Ok(kind) = std::fs::read_to_string(home.path().join("hook-pause-kind")) {
                assert_eq!(
                    kind, "quota",
                    "cancelling from turn_end finished the turn twice"
                );
                cancelled_at = Some(Instant::now());
            }
        }
        if command_resume {
            for body in bodies.iter().take(quota_attempts + 1) {
                assert_eq!(
                    body["model"], "command-model",
                    "retry lost command model override"
                );
                assert_eq!(
                    body["temperature"], 0.3,
                    "retry lost command sampling override"
                );
            }
        }
        if resume && posts > quota_attempts {
            let reset: u64 = std::fs::read_to_string(home.path().join("quota-deadline"))
                .expect("quota error published a reset deadline")
                .parse()
                .unwrap();
            let resumed_at = resumed_at.load(Ordering::SeqCst);
            if recovery == QuotaRecovery::EarlyResume {
                assert!(resumed_at < reset, "missed the early reset");
                assert!(
                    resumed_at >= reset.saturating_sub(3_540_000),
                    "retried before the one-minute backoff"
                );
            } else {
                assert!(resumed_at >= reset, "retried before the provider reset");
            }
            let messages = bodies[quota_attempts]["messages"].to_string();
            assert_eq!(messages.matches("original request").count(), 1);
            assert_eq!(
                messages.matches("finished successfully").count(),
                1,
                "background result must reach the resumed request exactly once"
            );
            assert!(
                !messages.contains("queued follow-up"),
                "retry consumed the next turn"
            );
        }
        if resume && posts == quota_attempts + 2 {
            let messages = bodies[quota_attempts + 1]["messages"].to_string();
            assert_eq!(messages.matches("queued follow-up").count(), 1);
            assert!(
                messages.contains("completed requested work"),
                "queue ran before resumed work finished"
            );
            follow_up_at.get_or_insert_with(Instant::now);
        }
        let settled = match recovery {
            QuotaRecovery::Resume
            | QuotaRecovery::EarlyResume
            | QuotaRecovery::CommandResume
            | QuotaRecovery::MissingReset => {
                follow_up_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(3))
            }
            QuotaRecovery::Cancel | QuotaRecovery::CancelBusy | QuotaRecovery::CancelHook => {
                cancelled_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(4))
            }
            QuotaRecovery::Wait => {
                queued_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(3))
            }
        };
        if settled && (!background || contains(&captured, b"finished successfully")) {
            assert!(
                contains(&captured, b"queued follow-up"),
                "queued input was not rendered"
            );
            assert!(
                contains(&captured, b"quota exceeded")
                    && contains(
                        &captured,
                        if cancel_hook {
                            b"paused"
                        } else {
                            b"resuming at"
                        }
                    ),
                "quiet quota status was not rendered"
            );
            if matches!(
                recovery,
                QuotaRecovery::CancelBusy | QuotaRecovery::CancelHook
            ) {
                assert_eq!(
                    std::fs::read_to_string(home.path().join("foreground-busy")).unwrap(),
                    "false",
                    "cancel left foreground work busy"
                );
            }
            if matches!(recovery, QuotaRecovery::Cancel | QuotaRecovery::CancelBusy) {
                assert!(
                    contains(&captured, b"paused"),
                    "cancelled wait did not render as paused: {}",
                    String::from_utf8_lossy(&captured[captured.len().saturating_sub(3500)..])
                );
            }
            return;
        }
        assert!(Instant::now() < deadline,
            "quota pause did not settle (main requests: {posts}, background: {background}, resume: {resume}): {}",
            String::from_utf8_lossy(&captured[captured.len().saturating_sub(2000)..]));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn interactive_startup_reuses_completed_session_catalog() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let config_dir = root.path().join("config/smelt");
    let state_home = root.path().join("state");
    for path in [
        root.path().join("home"),
        state_home.clone(),
        root.path().join("cache"),
        root.path().join("data"),
        config_dir.clone(),
    ] {
        std::fs::create_dir_all(path).expect("create startup runtime directory");
    }
    let config = config_dir.join("init.lua");
    std::fs::write(
        &config,
        r#"
smelt.settings.autoupgrade = "off"
smelt.provider.register("local", {
  type = "openai-compatible",
  api_base = "http://127.0.0.1:9/v1",
  models = { "test-model" },
})
"#,
    )
    .expect("write init.lua");

    let catalog_path = state_home.join("smelt/sessions/catalog.db");
    let mut catalog = smelt_store::Catalog::open(&catalog_path).expect("create session catalog");
    let scan_id = catalog.allocate_scan().expect("allocate catalog scan");
    catalog
        .complete_scan(scan_id, 1_700_000_000_000)
        .expect("complete catalog scan");
    drop(catalog);

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args(["--config", config.to_str().unwrap(), "--ephemeral"])
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();

    loop {
        drain_pty(&mut master, &mut captured);
        if contains(&captured, b"\x1b[?1049h") && contains(&captured, b"local/test-model") {
            break;
        }
        if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
            panic!(
                "smelt exited before rendering ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        assert!(
            Instant::now() < deadline,
            "smelt did not render before catalog startup timeout:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let stable_deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < stable_deadline {
        drain_pty(&mut master, &mut captured);
        assert!(
            process
                .child
                .try_wait()
                .expect("inspect smelt process")
                .is_none(),
            "smelt exited while observing catalog stability:\n{}",
            String::from_utf8_lossy(&captured)
        );
        let metadata = smelt_store::CatalogReader::open_existing(&catalog_path)
            .expect("open session catalog during startup")
            .expect("session catalog exists during startup")
            .metadata()
            .expect("read session catalog metadata during startup");
        assert_eq!(
            metadata.completed_scan_id, scan_id,
            "interactive startup unexpectedly completed a catalog scan"
        );
        assert_eq!(
            metadata.next_scan_id,
            scan_id + 1,
            "interactive startup unexpectedly allocated a catalog scan"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    master.write_all(b"\x03\x03").expect("send Ctrl-C to smelt");
    let exit_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        drain_pty(&mut master, &mut captured);
        if let Some(status) = process.child.try_wait().expect("inspect smelt shutdown") {
            assert!(status.success(), "smelt exited with {status}");
            break;
        }
        assert!(
            Instant::now() < exit_deadline,
            "smelt did not exit after catalog startup test:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let metadata = smelt_store::CatalogReader::open_existing(catalog_path)
        .expect("open session catalog")
        .expect("session catalog exists")
        .metadata()
        .expect("read session catalog metadata");
    assert_eq!(metadata.completed_scan_id, scan_id);
    assert_eq!(metadata.next_scan_id, scan_id + 1);
}

#[test]
fn inspect_startup_explicitly_reconciles_the_session_catalog() {
    let root = tempfile::tempdir().expect("temporary inspect root");
    let state_home = root.path().join("state");
    for path in [
        root.path().join("home"),
        root.path().join("config"),
        state_home.clone(),
        root.path().join("cache"),
        root.path().join("data"),
    ] {
        std::fs::create_dir_all(path).expect("create inspect runtime directory");
    }
    let catalog_path = state_home.join("smelt/sessions/catalog.db");
    let mut catalog = smelt_store::Catalog::open(&catalog_path).expect("create session catalog");
    let scan_id = catalog.allocate_scan().expect("allocate catalog scan");
    catalog
        .complete_scan(scan_id, 1_700_000_000_000)
        .expect("complete catalog scan");
    drop(catalog);

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args(["inspect", "--no-open"])
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();
    loop {
        drain_pty(&mut master, &mut captured);
        let metadata = smelt_store::CatalogReader::open_existing(&catalog_path)
            .expect("open inspector catalog")
            .expect("inspector catalog exists")
            .metadata()
            .expect("read inspector catalog metadata");
        if metadata.completed_scan_id != scan_id {
            assert_eq!(metadata.completed_scan_id, scan_id + 1);
            assert_eq!(metadata.next_scan_id, scan_id + 2);
            break;
        }
        if let Some(status) = process.child.try_wait().expect("inspect inspector process") {
            panic!(
                "inspector exited before catalog reconciliation ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        assert!(
            Instant::now() < deadline,
            "inspector did not reconcile the catalog:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn interactive_startup_applies_dynamic_lua_flags_before_the_first_frame() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let config_dir = root.path().join("config/smelt");
    let ready_marker = root.path().join("ready-kind");
    std::fs::create_dir_all(&config_dir).expect("create config directory");
    for name in ["home", "state", "cache", "data"] {
        std::fs::create_dir(root.path().join(name)).expect("create XDG directory");
    }
    std::fs::write(
        config_dir.join("early.lua"),
        r#"
smelt.cli.register_flag({
  name = "startup-model",
  kind = "string",
})
"#,
    )
    .expect("write early.lua");
    let ready_path = serde_json::to_string(ready_marker.to_str().unwrap()).unwrap();
    std::fs::write(
        config_dir.join("init.lua"),
        format!(
            r#"
smelt.settings.autoupgrade = "off"
local model = assert(smelt.cli.get("startup-model"))
smelt.provider.register("dynamic", {{
  type = "openai-compatible",
  api_base = "http://127.0.0.1:9/v1",
  models = {{ model }},
}})
smelt.lifecycle.on_ready(function(ctx)
  local ok, err = smelt.fs.write({ready_path}, ctx.kind)
  assert(ok, err)
end)
"#
        ),
    )
    .expect("write init.lua");

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args(["--startup-model", "selected-model", "--ephemeral"])
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();

    loop {
        drain_pty(&mut master, &mut captured);
        let selected_model_rendered = contains(&captured, b"dynamic/selected-model");
        let ready_kind = std::fs::read_to_string(&ready_marker).ok();
        if selected_model_rendered && ready_kind.as_deref() == Some("launch") {
            break;
        }
        if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
            panic!(
                "smelt exited before dynamic startup completed ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        assert!(
            Instant::now() < deadline,
            "dynamic Lua flag was not applied before the first frame:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn resumed_session_loads_project_lua_in_generation_zero() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let home = root.path().join("home");
    let config_home = root.path().join("config");
    let state_home = root.path().join("state");
    let cache_home = root.path().join("cache");
    let data_home = root.path().join("data");
    let initial_cwd = root.path().join("initial");
    let restored_cwd = root.path().join("restored");
    let ready_marker = root.path().join("project-ready-kind");
    for path in [
        &home,
        &config_home,
        &state_home,
        &cache_home,
        &data_home,
        &initial_cwd,
        &restored_cwd,
        &config_home.join("smelt"),
        &restored_cwd.join(".smelt"),
    ] {
        std::fs::create_dir_all(path).expect("create startup runtime directory");
    }

    std::fs::write(
        config_home.join("smelt/init.lua"),
        r#"
smelt.settings.autoupgrade = "off"
smelt.provider.register("local", {
  type = "openai-compatible",
  api_base = "http://127.0.0.1:9/v1",
  models = { "test-model" },
})
"#,
    )
    .expect("write global config");
    let ready_path = serde_json::to_string(ready_marker.to_str().unwrap()).unwrap();
    std::fs::write(
        restored_cwd.join(".smelt/init.lua"),
        format!(
            r#"
smelt.lifecycle.on_ready(function(ctx)
  local previous = smelt.fs.read({ready_path}) or ""
  local ok, err = smelt.fs.write({ready_path}, previous .. ctx.kind .. "\n")
  assert(ok, err)
end)
"#
        ),
    )
    .expect("write restored project config");
    smelt_core::trust::TrustStore::new(state_home.join("smelt"))
        .mark_trusted(&restored_cwd)
        .expect("trust restored project");

    let session_id = "a100000000000000000000000000000000000000000000000000000000000001";
    let transcript_marker = "canonical resume startup marker";
    let mut session = smelt_core::session::Session::new(1, restored_cwd.clone());
    session.id = session_id.to_string();
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            transcript_marker,
        )));
    let sessions_root = state_home.join("smelt/sessions");
    let mut writer = smelt_store::OwnedLineageWriter::open(&sessions_root, session_id)
        .expect("create lineage fixture");
    let mut command = smelt_core::session::initial_store_commit_from_session(&session)
        .expect("build lineage fixture");
    let mut transcript = smelt_core::content::transcript::Transcript::new();
    transcript.push(smelt_core::Block::Text {
        content: transcript_marker.into(),
    });
    let records = transcript
        .history
        .block_records()
        .into_iter()
        .enumerate()
        .map(|(index, record)| {
            smelt_core::transcript_model::transcript_block_row_with_block_idx(
                index,
                index as u64,
                &record,
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .expect("build transcript fixture");
    command.transcript_records = Some(smelt_store::TranscriptRecordSuffix {
        start: smelt_store::TranscriptRecordIndex::ZERO,
        records,
    });
    writer
        .commit_session(&command)
        .expect("commit lineage fixture");
    writer
        .refresh_catalog()
        .expect("publish lineage fixture catalog row");
    writer.release().expect("release lineage fixture");

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args(["--resume", session_id])
        .current_dir(&initial_cwd)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();

    loop {
        drain_pty(&mut master, &mut captured);
        let session_rendered = contains(&captured, transcript_marker.as_bytes());
        let ready_kind = std::fs::read_to_string(&ready_marker).ok();
        if session_rendered && ready_kind.is_some() {
            break;
        }
        if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
            panic!(
                "smelt exited before canonical resume completed ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        assert!(
            Instant::now() < deadline,
            "canonical resume did not finish:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        std::fs::read_to_string(&ready_marker).expect("project ready marker"),
        "launch\n",
        "restored project Lua must load once in generation zero"
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/cwd", process.child.id())).expect("process cwd"),
        restored_cwd
    );
}

#[test]
fn interactive_config_error_is_evaluated_once() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let config_dir = root.path().join("config/smelt");
    let evaluation_marker = root.path().join("config-evaluations");
    std::fs::create_dir_all(&config_dir).expect("create config directory");
    for name in ["home", "state", "cache", "data"] {
        std::fs::create_dir(root.path().join(name)).expect("create XDG directory");
    }
    let marker_path = serde_json::to_string(evaluation_marker.to_str().unwrap()).unwrap();
    std::fs::write(
        config_dir.join("init.lua"),
        format!(
            r#"
smelt.settings.autoupgrade = "off"
local previous = smelt.fs.read({marker_path}) or ""
local ok, err = smelt.fs.write({marker_path}, previous .. "loaded\n")
assert(ok, err)
error("single-generation-config-error")
"#
        ),
    )
    .expect("write init.lua");

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .arg("--ephemeral")
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();

    loop {
        drain_pty(&mut master, &mut captured);
        if contains(&captured, b"~/.config/smelt/init.lua: runtime error") {
            break;
        }
        if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
            panic!(
                "smelt exited before rendering the config error ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        assert!(
            Instant::now() < deadline,
            "smelt did not render the config error:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        std::fs::read_to_string(evaluation_marker).expect("config evaluation marker"),
        "loaded\n",
        "normal config must execute exactly once during interactive launch"
    );
}

#[test]
fn graceful_interactive_exit_runs_shutdown_hook_once() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let config_dir = root.path().join("config/smelt");
    let shutdown_marker = root.path().join("shutdown-calls");
    std::fs::create_dir_all(&config_dir).expect("create config directory");
    for name in ["home", "state", "cache", "data"] {
        std::fs::create_dir(root.path().join(name)).expect("create XDG directory");
    }
    let marker_path = serde_json::to_string(shutdown_marker.to_str().unwrap()).unwrap();
    std::fs::write(
        config_dir.join("init.lua"),
        format!(
            r#"
smelt.settings.autoupgrade = "off"
smelt.lifecycle.on_shutdown(function(ctx)
  local previous = smelt.fs.read({marker_path}) or ""
  local call = tostring(ctx.ephemeral) .. ":" .. tostring(ctx.has_messages) .. "\n"
  local ok, err = smelt.fs.write({marker_path}, previous .. call)
  assert(ok, err)
end)
"#
        ),
    )
    .expect("write init.lua");

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .arg("--ephemeral")
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();

    loop {
        drain_pty(&mut master, &mut captured);
        if contains(&captured, b"\x1b[?1049h") && contains(&captured, b"f1 help") {
            break;
        }
        if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
            panic!(
                "smelt exited before its first frame ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        assert!(
            Instant::now() < deadline,
            "smelt did not render before graceful-exit test timeout:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    master.write_all(b"\x03\x03").expect("send Ctrl-C to smelt");
    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        drain_pty(&mut master, &mut captured);
        if let Some(status) = process.child.try_wait().expect("inspect smelt shutdown") {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "smelt did not exit gracefully:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(status.success(), "smelt exited with {status}");
    assert!(
        contains(&captured, b"\x1b[?1049l"),
        "smelt did not restore the terminal:\n{}",
        String::from_utf8_lossy(&captured)
    );
    assert_eq!(
        std::fs::read_to_string(shutdown_marker).expect("shutdown marker"),
        "true:false\n",
        "shutdown hook must run exactly once with the final session context"
    );
}

#[derive(Clone, Copy)]
struct LifecycleSample {
    first_frame: Option<Duration>,
    ready: Duration,
    shutdown: Duration,
    peak_rss_kib: u64,
}

fn drain_pty(master: &mut File, captured: &mut Vec<u8>) {
    const BACKGROUND_QUERY: &[u8] = b"\x1b]11;?\x07\x1b[5n";
    const DARK_BACKGROUND_RESPONSE: &[u8] = b"\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[0n";

    let mut buffer = [0_u8; 64 * 1024];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => {
                let output = &buffer[..read];
                let previous_len = captured.len();
                captured.extend_from_slice(output);
                let query_start =
                    previous_len.saturating_sub(BACKGROUND_QUERY.len().saturating_sub(1));
                // A real terminal answers this probe immediately. Search across
                // read boundaries so the harness cannot accidentally trigger
                // background detection's 100 ms fallback.
                if contains(&captured[query_start..], BACKGROUND_QUERY) {
                    master
                        .write_all(DARK_BACKGROUND_RESPONSE)
                        .expect("answer terminal background query");
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return,
            Err(error) if error.raw_os_error() == Some(libc::EIO) => return,
            Err(error) => panic!("read smelt PTY: {error}"),
        }
    }
}

#[cfg(target_os = "linux")]
fn process_rss_kib(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")?
                .split_ascii_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn process_rss_kib(_pid: u32) -> u64 {
    0
}

fn nearest_rank(sorted: &[f64], percentile: usize) -> f64 {
    let rank = sorted.len().saturating_mul(percentile).div_ceil(100).max(1);
    sorted[rank.saturating_sub(1).min(sorted.len().saturating_sub(1))]
}

fn print_sample_summary(name: &str, unit: &str, mut values: Vec<f64>) {
    values.sort_by(f64::total_cmp);
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / values.len() as f64;
    println!(
        "LIFECYCLE_BENCH_SUMMARY metric={name} runs={} mean_{unit}={mean:.3} stddev_{unit}={:.3} p50_{unit}={:.3} p95_{unit}={:.3} p99_{unit}={:.3} max_{unit}={:.3}",
        values.len(),
        variance.sqrt(),
        nearest_rank(&values, 50),
        nearest_rank(&values, 95),
        nearest_rank(&values, 99),
        values.last().copied().unwrap_or_default(),
    );
}

fn print_duration_summary(name: &str, samples: impl Iterator<Item = Duration>) {
    print_sample_summary(
        name,
        "ms",
        samples
            .map(|duration| duration.as_secs_f64() * 1_000.0)
            .collect(),
    );
}

fn copy_fixture_directory(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("create benchmark fixture destination");
    for entry in std::fs::read_dir(source).expect("read benchmark fixture directory") {
        let entry = entry.expect("read benchmark fixture entry");
        let file_type = entry.file_type().expect("read benchmark fixture file type");
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_fixture_directory(&entry.path(), &target);
        } else {
            assert!(
                file_type.is_file(),
                "benchmark fixtures cannot contain symlinks"
            );
            std::fs::copy(entry.path(), target).expect("copy benchmark fixture file");
        }
    }
}

struct LifecycleBenchmark<'a> {
    binary: &'a Path,
    fixture: Option<&'a Path>,
    session_id: Option<&'a str>,
    config: &'a Path,
    first_frame_marker: &'a [u8],
    ready_marker: &'a [u8],
    timeout: Duration,
}

impl LifecycleBenchmark<'_> {
    fn run_sample(&self, root: &Path) -> LifecycleSample {
        let state_home = root.join("state");
        for name in ["home", "config", "cache", "data"] {
            std::fs::create_dir_all(root.join(name)).expect("create benchmark runtime directory");
        }
        if let Some(fixture) = self.fixture {
            copy_fixture_directory(fixture, &state_home.join("smelt/sessions"));
        }

        let mut command = Command::new(self.binary);
        command.args([
            "--config",
            self.config.to_str().expect("UTF-8 benchmark config path"),
            "--bench",
        ]);
        if let Some(session_id) = self.session_id {
            command.args(["--resume", session_id]);
        }
        command
            .current_dir(root)
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("TERM", "xterm-256color")
            .env("NO_COLOR", "1");
        let started = Instant::now();
        let (mut master, mut process) = spawn_in_pty(command);
        let mut captured = Vec::new();
        let mut first_frame = None;
        let mut peak_rss_kib = 0;
        let ready = loop {
            drain_pty(&mut master, &mut captured);
            if first_frame.is_none() && contains(&captured, self.first_frame_marker) {
                first_frame = Some(started.elapsed());
            }
            if contains(&captured, self.ready_marker) {
                break started.elapsed();
            }
            peak_rss_kib = peak_rss_kib.max(process_rss_kib(process.child.id()));
            if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
                panic!(
                    "smelt exited before lifecycle ready marker ({status}):\n{}",
                    String::from_utf8_lossy(&captured)
                );
            }
            assert!(
                started.elapsed() < self.timeout,
                "smelt lifecycle ready marker was not rendered within {:?}:\n{}",
                self.timeout,
                String::from_utf8_lossy(&captured)
            );
            std::thread::sleep(Duration::from_millis(1));
        };

        let settle_until = Instant::now() + Duration::from_millis(250);
        while Instant::now() < settle_until {
            drain_pty(&mut master, &mut captured);
            peak_rss_kib = peak_rss_kib.max(process_rss_kib(process.child.id()));
            assert!(
                process
                    .child
                    .try_wait()
                    .expect("inspect smelt process")
                    .is_none(),
                "smelt exited while lifecycle benchmark was settling"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        let quit_started = Instant::now();
        master.write_all(b"\x03\x03").expect("send Ctrl-C to smelt");
        let status = loop {
            drain_pty(&mut master, &mut captured);
            peak_rss_kib = peak_rss_kib.max(process_rss_kib(process.child.id()));
            if let Some(status) = process.child.try_wait().expect("inspect smelt shutdown") {
                break status;
            }
            assert!(
                quit_started.elapsed() < self.timeout,
                "smelt did not shut down within {:?}",
                self.timeout
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(
            status.success(),
            "smelt lifecycle benchmark exited {status}"
        );
        drain_pty(&mut master, &mut captured);
        if let Some(capture_dir) = std::env::var_os("SMELT_LIFECYCLE_BENCH_CAPTURE_DIR") {
            let capture_dir = std::path::PathBuf::from(capture_dir);
            std::fs::create_dir_all(&capture_dir).expect("create lifecycle capture directory");
            let run = root
                .file_name()
                .and_then(|name| name.to_str())
                .expect("lifecycle run directory requires a UTF-8 name");
            std::fs::write(capture_dir.join(format!("{run}.terminal.bin")), &captured)
                .expect("write lifecycle terminal capture");
        }

        LifecycleSample {
            first_frame,
            ready,
            shutdown: quit_started.elapsed(),
            peak_rss_kib,
        }
    }
}

#[test]
fn interactive_lifecycle_benchmark_suite() {
    let Some(target) = std::env::var_os("SMELT_LIFECYCLE_BENCH_TARGET") else {
        return;
    };
    let target = std::path::PathBuf::from(target);
    assert!(target.is_file(), "benchmark target must be a smelt binary");
    let fixture = std::env::var_os("SMELT_LIFECYCLE_BENCH_FIXTURE").map(std::path::PathBuf::from);
    let session_id = fixture.as_ref().map(|_| {
        std::env::var("SMELT_LIFECYCLE_BENCH_SESSION_ID")
            .expect("SMELT_LIFECYCLE_BENCH_SESSION_ID must identify a lineage fixture branch")
    });
    let fixture_database =
        fixture
            .as_ref()
            .zip(session_id.as_deref())
            .map(|(fixture, session_id)| {
                smelt_store::LineageSessionReader::open_existing(fixture, session_id)
                    .expect("open benchmark lineage fixture")
                    .database_path()
                    .to_path_buf()
            });
    let fixture_database_before = fixture_database
        .as_ref()
        .map(|database| std::fs::read(database).expect("read benchmark fixture"));
    let ready_marker = std::env::var("SMELT_LIFECYCLE_BENCH_READY_TEXT")
        .expect("SMELT_LIFECYCLE_BENCH_READY_TEXT must identify loaded transcript content");
    assert!(
        !ready_marker.is_empty(),
        "SMELT_LIFECYCLE_BENCH_READY_TEXT must not be empty"
    );
    let first_frame_marker = std::env::var("SMELT_LIFECYCLE_BENCH_FIRST_FRAME_TEXT")
        .unwrap_or_else(|_| "local/test-model".to_string());
    assert!(
        !first_frame_marker.is_empty(),
        "SMELT_LIFECYCLE_BENCH_FIRST_FRAME_TEXT must not be empty"
    );
    let runs = std::env::var("SMELT_LIFECYCLE_BENCH_RUNS")
        .ok()
        .and_then(|runs| runs.parse::<usize>().ok())
        .unwrap_or(10)
        .max(1);
    let timeout = Duration::from_secs(
        std::env::var("SMELT_LIFECYCLE_BENCH_TIMEOUT_SECS")
            .ok()
            .and_then(|seconds| seconds.parse::<u64>().ok())
            .unwrap_or(30),
    );

    let root = tempfile::tempdir().expect("create lifecycle benchmark root");
    let config = root.path().join("init.lua");
    std::fs::write(
        &config,
        r#"smelt.settings.autoupgrade = "off"
smelt.provider.register("local", {
  type = "openai-compatible",
  api_base = "http://127.0.0.1:9/v1",
  models = { "test-model" },
})
"#,
    )
    .expect("write lifecycle benchmark config");
    let benchmark = LifecycleBenchmark {
        binary: &target,
        fixture: fixture.as_deref(),
        session_id: session_id.as_deref(),
        config: &config,
        first_frame_marker: first_frame_marker.as_bytes(),
        ready_marker: ready_marker.as_bytes(),
        timeout,
    };

    let mut samples = Vec::with_capacity(runs);
    for run in 1..=runs {
        let run_root = root.path().join(format!("run-{run:03}"));
        let sample = benchmark.run_sample(&run_root);
        std::fs::remove_dir_all(run_root).expect("remove lifecycle benchmark run directory");
        let first_frame_ms = sample
            .first_frame
            .map(|duration| format!("{:.3}", duration.as_secs_f64() * 1_000.0))
            .unwrap_or_else(|| "na".to_string());
        println!(
            "LIFECYCLE_BENCH_RUN run={run} first_frame_ms={first_frame_ms} ready_ms={:.3} shutdown_ms={:.3} peak_rss_kib={}",
            sample.ready.as_secs_f64() * 1_000.0,
            sample.shutdown.as_secs_f64() * 1_000.0,
            sample.peak_rss_kib,
        );
        samples.push(sample);
    }

    let first_frames = samples.iter().filter_map(|sample| sample.first_frame);
    if first_frames.clone().next().is_some() {
        print_duration_summary("first_frame", first_frames);
    }
    print_duration_summary("ready", samples.iter().map(|sample| sample.ready));
    print_duration_summary("shutdown", samples.iter().map(|sample| sample.shutdown));
    print_sample_summary(
        "peak_rss",
        "kib",
        samples
            .iter()
            .map(|sample| sample.peak_rss_kib as f64)
            .collect(),
    );
    if let (Some(database), Some(before)) = (&fixture_database, fixture_database_before) {
        assert_eq!(
            std::fs::read(database).expect("reread benchmark fixture"),
            before,
            "lifecycle benchmark must not mutate its source fixture"
        );
    }
}
