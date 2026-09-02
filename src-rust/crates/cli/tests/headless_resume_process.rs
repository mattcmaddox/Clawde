//! Process-boundary coverage for headless `--resume`.
//!
//! The local HTTP fixture is deterministic and never contacts a real provider:
//! process A performs a Write, then receives controlled provider failures after
//! the tool-result boundary; process B resumes the persisted session and gets a
//! successful response. The fixture records request bodies so the test proves
//! process B received process A's tool result rather than merely reusing the
//! session ID.
//!
//! Scope boundary: terminal plan states (Complete/Blocked), replacement-plan
//! recovery, and stale-authority hash rejection are covered deterministically
//! in-process by the clawde-query integration tests. This test keeps only what
//! is unique to a real process boundary: the user-facing `/spec-review` Accept
//! path, transcript persistence/reload across separate processes, and
//! fail-closed startup for a nonexistent session.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn accept_spec_via_review_command(spec_root: &Path, session_id: &str) {
    // Exercise the same user-facing path as the TUI: dispatch the real
    // no-argument `/spec-review` command, then press Enter on its default
    // Accept action. This deliberately avoids calling the approval API here;
    // the helper proves that the command path materializes the approval hash
    // and plan progress that the later subprocess must reload.
    let mut app = clawde_tui::App::new(
        clawde_core::Config::default(),
        clawde_core::CostTracker::new(),
    );
    app.set_working_directory(spec_root);
    app.session_id = session_id.to_string();
    app.spec_review.set_session_id(session_id);
    assert!(app.intercept_slash_command_with_args("spec-review", ""));
    assert!(
        app.spec_review.visible,
        "spec-review must open the generated spec"
    );
    assert_eq!(
        app.spec_review.selected_action,
        clawde_tui::spec_review::ACTION_ACCEPT
    );
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        !app.spec_review.visible,
        "Accept must close the review dialog"
    );
    assert_eq!(
        app.queued_messages.len(),
        1,
        "Accept must queue implementation"
    );
    let (approved_path, approved_spec) =
        clawde_core::spec::Spec::approved_in(spec_root, session_id)
            .expect("Accept must persist a session-bound approval");
    assert!(
        app.queued_messages[0].contains(&approved_spec.accepted_task_marker()),
        "queued implementation must carry the accepted task marker"
    );
    let approved_raw = std::fs::read_to_string(&approved_path).expect("read accepted spec");
    let progress = clawde_core::plan::PlanProgress::load_for(
        spec_root,
        &approved_spec.task_id,
        session_id,
        &clawde_core::spec::Spec::content_hash(&approved_raw),
    )
    .expect("load progress initialized by Accept")
    .expect("Accept must initialize bound plan progress");
    assert_eq!(
        progress.status,
        clawde_core::PlanStatus::Active,
        "Accept must initialize an active plan"
    );
}

fn binary_path() -> String {
    env!("CARGO_BIN_EXE_clawde").to_string()
}

fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set request timeout");
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    let body_end = loop {
        let count = stream.read(&mut chunk).expect("read HTTP request");
        if count == 0 {
            break None;
        }
        bytes.extend_from_slice(&chunk[..count]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_text = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = header_text
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let end = header_end + 4 + content_length;
        if bytes.len() >= end {
            break Some(end);
        }
    };
    let end = body_end.unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn write_response(stream: &mut TcpStream, status: &str, body: &str, content_type: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .expect("write HTTP response");
}

fn tool_response(path: &Path, content: &str) -> String {
    let arguments = serde_json::json!({
        "file_path": path.display().to_string(),
        "content": content
    })
    .to_string();
    let first = serde_json::json!({
        "id": "resume-tool",
        "object": "chat.completion.chunk",
        "model": "gpt-4o-mini",
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": "resume-call",
                    "type": "function",
                    "function": {
                        "name": "Write",
                        "arguments": arguments
                    }
                }]
            },
            "finish_reason": null
        }]
    });
    let finish = serde_json::json!({
        "id": "resume-tool",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "tool_calls"
        }]
    });
    format!("data: {first}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}

fn resumed_response() -> &'static str {
    "data: {\"id\":\"resume-ok\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"RESUMED_OK\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"resume-ok\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n"
}

fn run_child(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("poll child") {
            Some(_) => return child.wait_with_output().expect("collect child output"),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("clawde child exceeded {timeout:?}");
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn common_args(api_base: &str, cwd: &Path, session_id: &str) -> Vec<String> {
    vec![
        "--print".to_string(),
        "--provider".to_string(),
        "openai".to_string(),
        "--model".to_string(),
        "gpt-4o-mini".to_string(),
        "--api-key".to_string(),
        "test-key".to_string(),
        "--api-base".to_string(),
        api_base.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--max-turns".to_string(),
        "2".to_string(),
        "--bare".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--cwd".to_string(),
        cwd.display().to_string(),
        "--session-id".to_string(),
        session_id.to_string(),
    ]
}

fn spawn_child(args: &[String], prompt: &str, home: &Path) -> Child {
    let mut command = Command::new(binary_path());
    command
        .args(args)
        .arg(prompt)
        .env("CLAWDE_HOME", home)
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.spawn().expect("spawn clawde headless child")
}

#[test]
fn headless_resume_survives_two_processes_and_tool_result_boundary() {
    let fixture = std::env::temp_dir().join(format!(
        "clawde-headless-resume-fixture-{}",
        uuid::Uuid::new_v4()
    ));
    let home = std::env::temp_dir().join(format!(
        "clawde-headless-resume-home-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(fixture.join("src")).expect("fixture directory");
    std::fs::create_dir_all(&home).expect("home directory");
    std::fs::write(
        fixture.join("src/lib.rs"),
        "pub fn baseline() -> u32 { 1 }\n",
    )
    .expect("fixture source");

    let session_id = "headless-process-resume-session";
    let task_id = "headless-process-resume-task";
    let spec_path = fixture.join("specs/process-resume.json");
    let spec = clawde_core::spec::Spec {
        task_id: task_id.to_string(),
        task: "Resume a headless implementation safely".to_string(),
        session_id: Some(session_id.to_string()),
        title: "Headless process resume plan".to_string(),
        requirements: vec!["Persist the process-boundary transcript".to_string()],
        ..Default::default()
    };
    spec.write_to(&spec_path)
        .expect("write approved-plan fixture");
    accept_spec_via_review_command(&fixture, session_id);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local provider fixture");
    listener
        .set_nonblocking(false)
        .expect("configure provider fixture");
    let address = listener.local_addr().expect("provider fixture address");
    let request_bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let bodies_for_server = request_bodies.clone();
    let write_path = fixture.join("src/process-a.rs");
    let server = thread::spawn(move || {
        let mut initial_tool_sent = false;
        let mut initial_failure_sent = false;
        let mut edited_tool_sent = false;
        for mut stream in listener.incoming().flatten() {
            let body = read_request(&mut stream);
            bodies_for_server.lock().unwrap().push(body.clone());
            if body.contains("RESUME_EDITED_SPEC") {
                if !edited_tool_sent {
                    edited_tool_sent = true;
                    let response = tool_response(
                        Path::new("src/edited-must-not-write.rs"),
                        "EDITED_MUST_NOT_WRITE\n",
                    );
                    write_response(&mut stream, "200 OK", &response, "text/event-stream");
                } else {
                    write_response(
                        &mut stream,
                        "500 Internal Server Error",
                        "controlled edited-spec failure",
                        "text/plain",
                    );
                    // Last scripted phase: exit so the server thread can join.
                    break;
                }
                continue;
            }
            if body.contains("RESUME_PROCESS_B") {
                write_response(
                    &mut stream,
                    "200 OK",
                    resumed_response(),
                    "text/event-stream",
                );
                continue;
            }
            if !initial_tool_sent {
                initial_tool_sent = true;
                let response = tool_response(Path::new("src/process-a.rs"), "PROCESS_A_WRITE\n");
                write_response(&mut stream, "200 OK", &response, "text/event-stream");
            } else if !initial_failure_sent {
                initial_failure_sent = true;
                write_response(
                    &mut stream,
                    "500 Internal Server Error",
                    "controlled process-A failure",
                    "text/plain",
                );
            }
        }
    });

    let api_base = format!("http://{}", address);
    let first_args = common_args(&api_base, &fixture, session_id);
    let first = run_child(
        spawn_child(
            &first_args,
            &format!(
                "START_PROCESS_A [{task_marker}]",
                task_marker = spec.accepted_task_marker()
            ),
            &home,
        ),
        Duration::from_secs(30),
    );
    assert_ne!(
        first.status.code(),
        Some(0),
        "process A must stop on the controlled provider failure; stderr={}\nstdout={}",
        String::from_utf8_lossy(&first.stderr),
        String::from_utf8_lossy(&first.stdout)
    );
    let written = std::fs::read_to_string(&write_path).ok();
    assert_eq!(
        written.as_deref(),
        Some("PROCESS_A_WRITE\n"),
        "process A did not materialize its tool call; status={:?}, stderr={}, stdout={}, requests={:?}",
        first.status.code(),
        String::from_utf8_lossy(&first.stderr),
        String::from_utf8_lossy(&first.stdout),
        request_bodies.lock().unwrap()
    );

    // A nonexistent session must fail before starting a fresh conversation.
    let stale_session_id = "headless-process-stale-session";
    let mut stale_args = common_args(&api_base, &fixture, stale_session_id);
    stale_args.extend(["--resume".to_string(), stale_session_id.to_string()]);
    let stale_path = fixture.join("src/stale-session-must-not-write.rs");
    let stale = run_child(
        spawn_child(&stale_args, "STALE_SESSION", &home),
        Duration::from_secs(30),
    );
    assert!(
        !stale.status.success(),
        "stale session must fail before starting a fresh conversation; stderr={}\nstdout={}",
        String::from_utf8_lossy(&stale.stderr),
        String::from_utf8_lossy(&stale.stdout)
    );
    assert!(
        String::from_utf8_lossy(&stale.stderr).contains("could not load headless resume session"),
        "stale-session error must be explicit: {}",
        String::from_utf8_lossy(&stale.stderr)
    );
    assert!(!stale_path.exists(), "stale session must not create a file");

    // A fresh process must reload the persisted transcript, including the
    // tool result that crossed the boundary in process A.
    let mut second_args = common_args(&api_base, &fixture, session_id);
    second_args.extend(["--resume".to_string(), session_id.to_string()]);
    let second = run_child(
        spawn_child(&second_args, "RESUME_PROCESS_B", &home),
        Duration::from_secs(30),
    );
    assert!(
        second.status.success(),
        "process B must resume successfully; stderr={}\nstdout={}",
        String::from_utf8_lossy(&second.stderr),
        String::from_utf8_lossy(&second.stdout)
    );
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("RESUMED_OK"),
        "process B response missing from stream: {}",
        String::from_utf8_lossy(&second.stdout)
    );

    // Editing the approved spec must invalidate the approval hash: a resumed
    // process attempting a write receives the real fail-closed plan error and
    // creates no file.
    let mut edited_spec = spec.clone();
    edited_spec.title = "Edited after process A approval".to_string();
    edited_spec
        .write_to(&spec_path)
        .expect("edit approved spec after resume");
    let edited_path = fixture.join("src/edited-must-not-write.rs");
    let mut edited_args = common_args(&api_base, &fixture, session_id);
    edited_args.extend(["--resume".to_string(), session_id.to_string()]);
    let edited = run_child(
        spawn_child(&edited_args, "RESUME_EDITED_SPEC", &home),
        Duration::from_secs(30),
    );
    assert!(
        !edited.status.success(),
        "edited spec must fail closed after the model attempts a write; stderr={}\nstdout={}",
        String::from_utf8_lossy(&edited.stderr),
        String::from_utf8_lossy(&edited.stdout)
    );
    assert!(
        !edited_path.exists(),
        "edited spec must not authorize a write"
    );

    server.join().expect("provider fixture server");
    let bodies = request_bodies.lock().unwrap();
    assert!(
        bodies.len() >= 3,
        "expected process A requests plus process B, requests={}",
        bodies.len()
    );
    assert!(
        bodies.iter().any(|body| body.contains("PROCESS_A_WRITE")),
        "a later request must contain process A's tool-result content"
    );
    assert!(
        bodies.iter().any(|body| body.contains("RESUME_PROCESS_B")),
        "a request from process B must reach the fixture"
    );
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("Plan approval required before")),
        "edited-spec tool result must contain the fail-closed plan error"
    );
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("RESUME_EDITED_SPEC")),
        "edited-spec process must reach the provider fixture"
    );

    let session_path = home.join("sessions").join(format!("{session_id}.json"));
    let session: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(session_path).expect("saved session"))
            .expect("valid saved session JSON");
    assert!(session["messages"]
        .as_array()
        .is_some_and(|messages| messages.len() >= 3));

    std::fs::remove_dir_all(fixture).expect("remove fixture");
    std::fs::remove_dir_all(home).expect("remove home");
}

/// Effort-precedence coverage across a real process boundary: persisted
/// `config.defaultEffort` must reach the provider request as `reasoning_effort`
/// on a reasoning model, `--effort` must outrank it, and a resumed session's
/// saved `effort` must outrank both. The mock fixture records the request
/// bodies so the assertions inspect exactly what the binary sent.
#[test]
fn headless_effort_precedence_flows_into_provider_request() {
    fn end_turn_response() -> &'static str {
        "data: {\"id\":\"effort-ok\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-5-mini\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"EFFORT_OK\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"effort-ok\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n"
    }

    /// Bind a fixture server, spawn the real binary against it, and return
    /// the recorded request bodies. `extra` are additional CLI args (e.g.
    /// `--effort` / `--resume`).
    fn run_scenario(
        extra: &[&str],
        prompt: &str,
        home: &Path,
        session_id: &str,
    ) -> (std::process::Output, Vec<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind effort fixture");
        let address = listener.local_addr().expect("effort fixture address");
        let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
        let server_bodies = bodies.clone();
        // Detached: the thread blocks on `incoming()` until the test process
        // exits, so it must never be joined. Each scenario binds its own port.
        let _server = thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let body = read_request(&mut stream);
                server_bodies.lock().unwrap().push(body.clone());
                if body.contains("EFFORT_PROBE") {
                    write_response(
                        &mut stream,
                        "200 OK",
                        end_turn_response(),
                        "text/event-stream",
                    );
                } else {
                    write_response(
                        &mut stream,
                        "500 Internal Server Error",
                        "unexpected request",
                        "text/plain",
                    );
                }
            }
        });
        let api_base = format!("http://{}", address);
        let cwd = std::env::temp_dir().join(format!("clawde-effort-cwd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).expect("effort fixture cwd");
        let mut args = common_args(&api_base, &cwd, session_id);
        // reasoning_effort is only emitted for OpenAI reasoning families, and
        // the OpenAI provider blocks gpt-5/o-series (Responses API) — so route
        // through the OpenAI-compatible openrouter provider instead.
        let model_pos = args.iter().position(|a| a == "gpt-4o-mini").unwrap();
        args[model_pos] = "gpt-5-mini".to_string();
        let provider_pos = args.iter().position(|a| a == "openai").unwrap();
        args[provider_pos] = "openrouter".to_string();
        args.extend(extra.iter().map(|s| s.to_string()));
        let child = spawn_child(&args, prompt, home);
        let output = run_child(child, Duration::from_secs(30));
        std::fs::remove_dir_all(&cwd).expect("remove effort fixture cwd");
        let recorded = bodies.lock().unwrap().clone();
        (output, recorded)
    }

    let base_home =
        std::env::temp_dir().join(format!("clawde-effort-home-{}", uuid::Uuid::new_v4()));
    let session_id = "effort-precedence-session";

    // Scenario 1: persisted defaultEffort alone reaches the request body.
    let home1 = base_home.join("s1");
    std::fs::create_dir_all(&home1).expect("home s1");
    std::fs::write(
        home1.join("settings.json"),
        r#"{"config": {"defaultEffort": "high"}}"#,
    )
    .expect("write settings s1");
    let (out1, bodies1) = run_scenario(&[], "EFFORT_PROBE", &home1, session_id);
    assert!(
        out1.status.success(),
        "s1 stderr={}",
        String::from_utf8_lossy(&out1.stderr)
    );
    assert!(
        bodies1
            .iter()
            .any(|b| b.contains("\"reasoning_effort\":\"high\"")),
        "persisted defaultEffort=high must reach the request as reasoning_effort; bodies={:?}",
        bodies1
    );

    // Scenario 2: CLI --effort outranks the persisted default.
    let home2 = base_home.join("s2");
    std::fs::create_dir_all(&home2).expect("home s2");
    std::fs::write(
        home2.join("settings.json"),
        r#"{"config": {"defaultEffort": "high"}}"#,
    )
    .expect("write settings s2");
    let (out2, bodies2) = run_scenario(&["--effort", "low"], "EFFORT_PROBE", &home2, session_id);
    assert!(
        out2.status.success(),
        "s2 stderr={}",
        String::from_utf8_lossy(&out2.stderr)
    );
    assert!(
        bodies2
            .iter()
            .any(|b| b.contains("\"reasoning_effort\":\"low\"")),
        "CLI --effort low must outrank the persisted high default; bodies={:?}",
        bodies2
    );

    // Scenario 3: a resumed session's saved effort outranks CLI --effort.
    let home3 = base_home.join("s3");
    std::fs::create_dir_all(home3.join("sessions")).expect("home s3 sessions");
    std::fs::write(
        home3.join("settings.json"),
        r#"{"config": {"defaultEffort": "medium"}}"#,
    )
    .expect("write settings s3");
    std::fs::write(
        home3.join("sessions").join(format!("{session_id}.json")),
        r#"{"id":"effort-precedence-session","created_at":"2026-08-17T00:00:00Z","updated_at":"2026-08-17T00:00:00Z","messages":[{"role":"user","content":"earlier"}],"model":"gpt-5-mini","effort":"high"}"#,
    )
    .expect("write session s3");
    let (out3, bodies3) = run_scenario(
        &["--effort", "low", "--resume", session_id],
        "EFFORT_PROBE",
        &home3,
        session_id,
    );
    assert!(
        out3.status.success(),
        "s3 stderr={}",
        String::from_utf8_lossy(&out3.stderr)
    );
    assert!(
        bodies3
            .iter()
            .any(|b| b.contains("\"reasoning_effort\":\"high\"")),
        "resumed session effort must outrank CLI --effort low; bodies={:?}",
        bodies3
    );

    // Scenario 4: a resumed session WITHOUT a saved effort must NOT clear the
    // CLI override — fall through to --effort low (not the persisted high).
    let home4 = base_home.join("s4");
    std::fs::create_dir_all(home4.join("sessions")).expect("home s4 sessions");
    std::fs::write(
        home4.join("settings.json"),
        r#"{"config": {"defaultEffort": "high"}}"#,
    )
    .expect("write settings s4");
    // Older/untouched session files omit the effort field.
    std::fs::write(
        home4.join("sessions").join(format!("{session_id}.json")),
        r#"{"id":"effort-precedence-session","created_at":"2026-08-17T00:00:00Z","updated_at":"2026-08-17T00:00:00Z","messages":[{"role":"user","content":"earlier"}],"model":"gpt-5-mini"}"#,
    )
    .expect("write session s4");
    let (out4, bodies4) = run_scenario(
        &["--effort", "low", "--resume", session_id],
        "EFFORT_PROBE",
        &home4,
        session_id,
    );
    assert!(
        out4.status.success(),
        "s4 stderr={}",
        String::from_utf8_lossy(&out4.stderr)
    );
    assert!(
        bodies4
            .iter()
            .any(|b| b.contains("\"reasoning_effort\":\"low\"")),
        "an absent session effort must fall through to CLI --effort low; bodies={:?}",
        bodies4
    );

    std::fs::remove_dir_all(&base_home).expect("remove effort homes");
}

/// Snapshot-based incremental replay across a real process boundary.
///
/// A state snapshot is only written once a session crosses the 64-event
/// snapshot cadence, which a single headless subprocess cannot reach (each
/// event needs a provider round-trip). This test reproduces the artifact a
/// snapshot-writing process would leave behind: process A (a real binary run)
/// generates the base transcript and its state events, the test appends a
/// `state-snapshot` entry at the exact event watermark the periodic writer
/// would use, and process B (a second real binary run, same session, with
/// `--resume`) must fold the snapshot into `<task_context>` — proving the
/// headless resume path consumes snapshots for long sessions.
///
/// The snapshot body is richer than the raw events before it (7 tool calls +
/// a passing validation vs. a single ToolObserved event) — impossible for a
/// real writer, whose body is the fold of those events — but it is the only
/// black-box discriminator for which path B took: the loader contract is that
/// a VALIDATED snapshot is trusted as the compacted fold. If the snapshot
/// were discarded, B would fall back to the raw event replay and report "1
/// tool calls" with no Verified evidence.
#[test]
fn headless_snapshot_projection_survives_process_boundary() {
    use clawde_core::session_storage::{
        make_state_snapshot_entry, transcript_dir_in, StateSnapshot, StateSnapshotBody,
        StateSnapshotDecision, StateSnapshotEvidence, STATE_SNAPSHOT_SCHEMA_VERSION,
    };

    let fixture =
        std::env::temp_dir().join(format!("clawde-snapshot-fixture-{}", uuid::Uuid::new_v4()));
    let home = std::env::temp_dir().join(format!("clawde-snapshot-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(fixture.join("src")).expect("fixture src");
    std::fs::create_dir_all(&home).expect("home");
    let session_id = "snapshot-process-boundary-session";
    let project_root = clawde_core::git_utils::project_root(&fixture);
    let transcript_path =
        transcript_dir_in(&home, &project_root).join(format!("{session_id}.jsonl"));

    // --- Process A: a real headless run generates the base transcript. ---
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind seed fixture");
    listener
        .set_nonblocking(false)
        .expect("configure seed fixture");
    let address = listener.local_addr().expect("seed fixture address");
    let seed_write_path = fixture.join("src/process-a.rs");
    let server = thread::spawn(move || {
        let mut tool_sent = false;
        for mut stream in listener.incoming().flatten() {
            let body = read_request(&mut stream);
            if body.contains("SNAPSHOT_SEED_PROCESS") {
                if !tool_sent {
                    tool_sent = true;
                    let response =
                        tool_response(Path::new("src/process-a.rs"), "SNAPSHOT_SEEDED_WRITE\n");
                    write_response(&mut stream, "200 OK", &response, "text/event-stream");
                } else {
                    // Model turn after the tool result: end the conversation.
                    write_response(
                        &mut stream,
                        "200 OK",
                        resumed_response(),
                        "text/event-stream",
                    );
                    // Process A is done — exit so the thread can join.
                    break;
                }
            } else {
                write_response(
                    &mut stream,
                    "500 Internal Server Error",
                    "unexpected request",
                    "text/plain",
                );
            }
        }
    });
    let api_base = format!("http://{}", address);
    let first_args = common_args(&api_base, &fixture, session_id);
    let first = run_child(
        spawn_child(
            &first_args,
            "SNAPSHOT_SEED_PROCESS write the seed file",
            &home,
        ),
        Duration::from_secs(60),
    );
    assert!(
        first.status.success(),
        "process A must complete its seed turn; stderr={}\nstdout={}",
        String::from_utf8_lossy(&first.stderr),
        String::from_utf8_lossy(&first.stdout)
    );
    assert_eq!(
        std::fs::read_to_string(&seed_write_path).ok().as_deref(),
        Some("SNAPSHOT_SEEDED_WRITE\n"),
        "process A did not materialize its tool call; stderr={}",
        String::from_utf8_lossy(&first.stderr)
    );
    server.join().expect("seed fixture server");

    // The seed run persisted at least one state event; a snapshot entry is
    // only valid at that exact watermark (the loader counts the lines).
    let raw = std::fs::read_to_string(&transcript_path).expect("read seeded transcript");
    let event_count = raw
        .lines()
        .filter(|line| line.contains("state-event"))
        .count() as u64;
    assert!(event_count > 0, "process A wrote no state events");

    // The periodic snapshot write a long-running process would append once
    // its session crossed the cadence.
    let snapshot = StateSnapshot {
        schema_version: STATE_SNAPSHOT_SCHEMA_VERSION,
        event_count,
        body: StateSnapshotBody {
            decisions: vec![StateSnapshotDecision {
                statement: "Keep the public API stable".to_string(),
                evidence: None,
            }],
            evidence: vec![StateSnapshotEvidence {
                summary: "3 checks passed".to_string(),
                source: "validation".to_string(),
                status: "verified".to_string(),
            }],
            changed_files: vec!["src/snapshot.rs".to_string()],
            failures: Vec::new(),
            simplification_reviewed: true,
            files_touched: 2,
            tool_calls: 7,
            failed_tools: 0,
            repeated_failures_per_target: 0,
            plan_step: Some("Step 2: verify the snapshot projection".to_string()),
            validation: Some("3 checks passed".to_string()),
            snapshot_files: vec!["src/snapshot.rs".to_string()],
        },
    };
    let line = serde_json::to_string(&make_state_snapshot_entry(session_id, snapshot))
        .expect("serialize snapshot entry");
    let mut transcript = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&transcript_path)
        .expect("open transcript for snapshot append");
    writeln!(transcript, "{line}").expect("append snapshot entry");
    drop(transcript);

    // --- Process B: a fresh process resumes the session and must fold the
    // persisted snapshot (not the raw event list) into the model context.
    let probe_listener = TcpListener::bind("127.0.0.1:0").expect("bind probe fixture");
    let probe_address = probe_listener.local_addr().expect("probe fixture address");
    let request_bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let bodies_for_server = request_bodies.clone();
    // Detached: blocks on `incoming()` until the test process exits, so it
    // is never joined.
    let _probe_server = thread::spawn(move || {
        for mut stream in probe_listener.incoming().flatten() {
            let body = read_request(&mut stream);
            bodies_for_server.lock().unwrap().push(body.clone());
            write_response(
                &mut stream,
                "200 OK",
                "data: {\"id\":\"snapshot-probe\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o-mini\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"SNAPSHOT_PROBE_OK\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"snapshot-probe\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n",
                "text/event-stream",
            );
        }
    });
    let mut second_args = common_args(&format!("http://{}", probe_address), &fixture, session_id);
    second_args.extend(["--resume".to_string(), session_id.to_string()]);
    let second = run_child(
        spawn_child(&second_args, "SNAPSHOT_PROBE verify the projection", &home),
        Duration::from_secs(60),
    );
    assert!(
        second.status.success(),
        "process B must complete; stderr={}\nstdout={}",
        String::from_utf8_lossy(&second.stderr),
        String::from_utf8_lossy(&second.stdout)
    );
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("SNAPSHOT_PROBE_OK"),
        "process B response missing from stream: {}",
        String::from_utf8_lossy(&second.stdout)
    );

    // The provider request must carry the SNAPSHOT-derived facts in
    // <task_context>: verified evidence and counters that the single raw
    // state event before the snapshot cannot produce.
    let probe_body = request_bodies
        .lock()
        .unwrap()
        .iter()
        .find(|body| body.contains("SNAPSHOT_PROBE"))
        .cloned()
        .expect("process B request must reach the probe fixture");
    for marker in [
        "<task_context>",
        "Verified: 3 checks passed",
        "Activity: 7 tool calls, 0 failed",
        "Step 2: verify the snapshot projection",
        "Changed files: src/snapshot.rs",
    ] {
        assert!(
            probe_body.contains(marker),
            "process B request must fold the snapshot body; missing '{marker}': {probe_body}"
        );
    }
    assert!(
        !probe_body.contains("Activity: 1 tool calls"),
        "process B must honor the validated snapshot instead of falling back \
         to the raw event replay: {probe_body}"
    );
    // The transcript still holds the snapshot entry for future loads.
    let after = std::fs::read_to_string(&transcript_path).expect("re-read transcript");
    assert!(
        after.lines().any(|line| line.contains("state-snapshot")),
        "snapshot entry must survive process B's writes"
    );

    std::fs::remove_dir_all(fixture).expect("remove fixture");
    std::fs::remove_dir_all(home).expect("remove home");
}
