//! Process-boundary coverage for headless `--resume`.
//!
//! The local HTTP fixture is deterministic and never contacts a real provider:
//! process A performs a Write, then receives controlled provider failures after
//! the tool-result boundary; process B resumes the persisted session and gets a
//! successful response. The fixture records request bodies so the test proves
//! process B received process A's tool result rather than merely reusing the
//! session ID.

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
    let approved_raw = std::fs::read_to_string(&spec_path).expect("read approved fixture");

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
        let mut complete_tool_sent = false;
        let mut replacement_tool_sent = false;
        let mut replacement_after_blocked_tool_sent = false;
        let mut blocked_tool_sent = false;
        for mut stream in listener.incoming().flatten() {
            let body = read_request(&mut stream);
            bodies_for_server.lock().unwrap().push(body.clone());
            if body.contains("RESUME_REPLACEMENT_AFTER_BLOCKED") {
                if !replacement_after_blocked_tool_sent {
                    replacement_after_blocked_tool_sent = true;
                    let response = tool_response(
                        Path::new("src/replacement-after-blocked.rs"),
                        "REPLACEMENT_AFTER_BLOCKED_WRITE\n",
                    );
                    write_response(&mut stream, "200 OK", &response, "text/event-stream");
                } else {
                    write_response(
                        &mut stream,
                        "200 OK",
                        resumed_response(),
                        "text/event-stream",
                    );
                    break;
                }
                continue;
            }
            if body.contains("RESUME_BLOCKED_PLAN") {
                if !blocked_tool_sent {
                    blocked_tool_sent = true;
                    let response = tool_response(
                        Path::new("src/blocked-must-not-write.rs"),
                        "BLOCKED_MUST_NOT_WRITE\n",
                    );
                    write_response(&mut stream, "200 OK", &response, "text/event-stream");
                } else {
                    write_response(
                        &mut stream,
                        "500 Internal Server Error",
                        "controlled blocked-plan failure",
                        "text/plain",
                    );
                }
                continue;
            }
            if body.contains("RESUME_REPLACEMENT_PLAN") {
                if !replacement_tool_sent {
                    replacement_tool_sent = true;
                    let response = tool_response(
                        Path::new("src/replacement-plan.rs"),
                        "REPLACEMENT_PLAN_WRITE\n",
                    );
                    write_response(&mut stream, "200 OK", &response, "text/event-stream");
                } else {
                    write_response(
                        &mut stream,
                        "200 OK",
                        resumed_response(),
                        "text/event-stream",
                    );
                }
                continue;
            }
            if body.contains("RESUME_COMPLETE_PLAN") {
                if !complete_tool_sent {
                    complete_tool_sent = true;
                    let response = tool_response(
                        Path::new("src/complete-must-not-write.rs"),
                        "COMPLETE_MUST_NOT_WRITE\n",
                    );
                    write_response(&mut stream, "200 OK", &response, "text/event-stream");
                } else {
                    write_response(
                        &mut stream,
                        "500 Internal Server Error",
                        "controlled complete-plan failure",
                        "text/plain",
                    );
                }
                continue;
            }
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

    // Restore the original approved bytes, then persist a terminal Complete
    // progress artifact before the next process starts. This simulates a
    // previous process finishing the approved plan and exiting.
    std::fs::write(&spec_path, &approved_raw).expect("restore approved spec bytes");
    clawde_core::spec::Spec::write_approval_for_session(&spec_path, session_id)
        .expect("re-approve restored spec");
    let spec_hash = clawde_core::spec::Spec::content_hash(&approved_raw);
    let mut complete_progress =
        clawde_core::PlanProgress::load_for(&fixture, task_id, session_id, &spec_hash)
            .expect("load restored plan progress")
            .expect("restored plan progress");
    while complete_progress.active_step_id.is_some() {
        complete_progress
            .record_evidence(clawde_core::PlanEvidence {
                kind: "complete".to_string(),
                summary: "Approved step completed before process restart.".to_string(),
                reference: Some("plans/process-resume.json".to_string()),
            })
            .expect("record complete-plan evidence");
        complete_progress
            .complete_active_step()
            .expect("complete restored plan step");
    }
    assert_eq!(
        complete_progress.status,
        clawde_core::PlanStatus::Complete,
        "fixture must persist a complete plan"
    );
    complete_progress
        .save(&fixture)
        .expect("persist complete plan");
    assert!(
        clawde_core::spec::Spec::approved_in(&fixture, session_id).is_some(),
        "fixture must retain valid approval for a complete plan"
    );

    let complete_path = fixture.join("src/complete-must-not-write.rs");
    let mut complete_args = common_args(&api_base, &fixture, session_id);
    complete_args.extend(["--resume".to_string(), session_id.to_string()]);
    let complete = run_child(
        spawn_child(&complete_args, "RESUME_COMPLETE_PLAN", &home),
        Duration::from_secs(30),
    );
    assert!(
        !complete.status.success(),
        "complete plan must fail closed after restart; stderr={}\nstdout={}",
        String::from_utf8_lossy(&complete.stderr),
        String::from_utf8_lossy(&complete.stdout)
    );
    assert!(
        !complete_path.exists(),
        "complete plan must not authorize a post-completion write"
    );

    // Approve a replacement task after the completed plan, then prove a fresh
    // resumed process can write under that new accepted-task marker.
    let replacement_spec = clawde_core::spec::Spec {
        task_id: "replacement-process-resume-task".to_string(),
        task: "Continue safely with a replacement approved plan".to_string(),
        session_id: Some(session_id.to_string()),
        title: "Replacement process-resume plan".to_string(),
        ..Default::default()
    };
    replacement_spec
        .write_to(&spec_path)
        .expect("write replacement spec");
    clawde_core::spec::Spec::write_approval_for_session(&spec_path, session_id)
        .expect("approve replacement spec");
    let replacement_raw = std::fs::read_to_string(&spec_path).expect("read replacement spec");
    let replacement_hash = clawde_core::spec::Spec::content_hash(&replacement_raw);
    let replacement_progress = clawde_core::PlanProgress::load_for(
        &fixture,
        &replacement_spec.task_id,
        session_id,
        &replacement_hash,
    )
    .expect("load replacement plan")
    .expect("replacement plan progress");
    assert_eq!(
        replacement_progress.status,
        clawde_core::PlanStatus::Active,
        "replacement approval must reset the plan to active"
    );
    let replacement_path = fixture.join("src/replacement-plan.rs");
    let mut replacement_args = common_args(&api_base, &fixture, session_id);
    replacement_args.extend(["--resume".to_string(), session_id.to_string()]);
    let replacement = run_child(
        spawn_child(
            &replacement_args,
            &format!(
                "RESUME_REPLACEMENT_PLAN {}",
                replacement_spec.accepted_task_marker()
            ),
            &home,
        ),
        Duration::from_secs(30),
    );
    assert!(
        replacement.status.success(),
        "replacement plan must resume successfully; stderr={}\nstdout={}",
        String::from_utf8_lossy(&replacement.stderr),
        String::from_utf8_lossy(&replacement.stdout)
    );
    assert!(
        String::from_utf8_lossy(&replacement.stdout).contains("RESUMED_OK"),
        "replacement response missing from stream: {}",
        String::from_utf8_lossy(&replacement.stdout)
    );
    assert_eq!(
        std::fs::read_to_string(&replacement_path).expect("replacement write result"),
        "REPLACEMENT_PLAN_WRITE\n",
        "replacement approval must authorize only the new plan write"
    );

    // Re-approve the original bytes, then persist a terminal Blocked progress
    // artifact before the final process starts. This simulates a previous
    // process exhausting its replan budget and exiting.
    std::fs::write(&spec_path, &approved_raw).expect("restore original approved bytes");
    clawde_core::spec::Spec::write_approval_for_session(&spec_path, session_id)
        .expect("reinitialize restored plan");
    let mut blocked_progress =
        clawde_core::PlanProgress::load_for(&fixture, task_id, session_id, &spec_hash)
            .expect("load reinitialized plan progress")
            .expect("reinitialized plan progress");
    blocked_progress
        .block_active_step(clawde_core::PlanEvidence {
            kind: "blocked".to_string(),
            summary: "Replan budget exhausted before process restart.".to_string(),
            reference: Some("plans/process-resume.json".to_string()),
        })
        .expect("block restored plan");
    blocked_progress
        .save(&fixture)
        .expect("persist blocked plan");
    assert_eq!(
        blocked_progress.status,
        clawde_core::PlanStatus::Blocked,
        "fixture must persist a blocked plan"
    );
    assert!(
        clawde_core::spec::Spec::approved_in(&fixture, session_id).is_some(),
        "fixture must retain valid approval after restoring the spec"
    );

    let blocked_path = fixture.join("src/blocked-must-not-write.rs");
    let mut blocked_args = common_args(&api_base, &fixture, session_id);
    blocked_args.extend(["--resume".to_string(), session_id.to_string()]);
    let blocked = run_child(
        spawn_child(
            &blocked_args,
            &format!("RESUME_BLOCKED_PLAN {}", spec.accepted_task_marker()),
            &home,
        ),
        Duration::from_secs(30),
    );
    assert!(
        !blocked.status.success(),
        "blocked plan must fail closed after restart; stderr={}\nstdout={}",
        String::from_utf8_lossy(&blocked.stderr),
        String::from_utf8_lossy(&blocked.stdout)
    );
    assert!(
        !blocked_path.exists(),
        "blocked plan must not authorize a write"
    );

    let replacement_after_blocked_spec = clawde_core::spec::Spec {
        task_id: "replacement-after-blocked-task".to_string(),
        task: "Recover safely with a replacement after a blocked plan".to_string(),
        session_id: Some(session_id.to_string()),
        title: "Replacement after blocked plan".to_string(),
        ..Default::default()
    };
    replacement_after_blocked_spec
        .write_to(&spec_path)
        .expect("write replacement-after-blocked spec");
    clawde_core::spec::Spec::write_approval_for_session(&spec_path, session_id)
        .expect("approve replacement-after-blocked spec");
    let replacement_after_blocked_raw =
        std::fs::read_to_string(&spec_path).expect("read replacement-after-blocked spec");
    let replacement_after_blocked_progress = clawde_core::PlanProgress::load_for(
        &fixture,
        &replacement_after_blocked_spec.task_id,
        session_id,
        &clawde_core::spec::Spec::content_hash(&replacement_after_blocked_raw),
    )
    .expect("load replacement-after-blocked plan")
    .expect("replacement-after-blocked progress");
    assert_eq!(
        replacement_after_blocked_progress.status,
        clawde_core::PlanStatus::Active,
        "a replacement approval must reopen a blocked plan"
    );
    let replacement_after_blocked_path = fixture.join("src/replacement-after-blocked.rs");
    let mut replacement_after_blocked_args = common_args(&api_base, &fixture, session_id);
    replacement_after_blocked_args.extend(["--resume".to_string(), session_id.to_string()]);
    let replacement_after_blocked = run_child(
        spawn_child(
            &replacement_after_blocked_args,
            &format!(
                "RESUME_REPLACEMENT_AFTER_BLOCKED {}",
                replacement_after_blocked_spec.accepted_task_marker()
            ),
            &home,
        ),
        Duration::from_secs(30),
    );
    assert!(
        replacement_after_blocked.status.success(),
        "replacement after blocked must resume successfully; stderr={}\nstdout={}",
        String::from_utf8_lossy(&replacement_after_blocked.stderr),
        String::from_utf8_lossy(&replacement_after_blocked.stdout)
    );
    assert_eq!(
        std::fs::read_to_string(&replacement_after_blocked_path)
            .expect("replacement-after-blocked write result"),
        "REPLACEMENT_AFTER_BLOCKED_WRITE\n",
        "replacement approval must reopen writes after a blocked plan"
    );
    server.join().expect("provider fixture server");
    let bodies = request_bodies.lock().unwrap();
    assert!(
        bodies.len() >= 3,
        "expected process A retries plus process B, requests={}",
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
    assert!(
        bodies.iter().any(|body| {
            body.contains("the approved plan for task 'headless-process-resume-task' is COMPLETE")
        }),
        "complete-plan tool result must contain the terminal plan error; bodies={bodies:?}"
    );
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("RESUME_REPLACEMENT_PLAN")),
        "replacement-plan process must reach the provider fixture"
    );
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("RESUME_REPLACEMENT_AFTER_BLOCKED")),
        "replacement-after-blocked process must reach the provider fixture"
    );
    assert!(
        bodies.iter().any(|body| {
            body.contains(
                "the approved plan for task 'headless-process-resume-task' is BLOCKED after exhausting its replan budget",
            )
        }),
        "blocked-plan tool result must contain the terminal plan error; bodies={bodies:?}"
    );
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("RESUME_COMPLETE_PLAN")),
        "complete-plan process must reach the provider fixture"
    );
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("RESUME_BLOCKED_PLAN")),
        "blocked-plan process must reach the provider fixture"
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
