#![cfg(unix)]

use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jcode_operator_backend::platform_api::{PlatformRequestMessage, PlatformResponseMessage};
use jcode_operator_backend::platform_codec::{
    FrameDecode, LENGTH_PREFIX_BYTES, decode_frame, encode_frame,
};
use jcode_operator_backend::platform_contract::{
    Capabilities, CursorTopic, Freshness, FreshnessState, PlatformCursor, PlatformRequest,
    PlatformResponse, PlatformText, ResourceAuthority, ResourceCoordinate, ResourceId,
    ResourceKind, ResourceRecord, SessionList, SessionRecord, Snapshot, Subscription,
};
use jcode_operator_backend::platform_primitives::{EpochMillis, Revision};

fn cursor(topic: &str, sequence: u64) -> PlatformCursor {
    PlatformCursor {
        authority: ResourceAuthority::Automonique,
        topic: CursorTopic::new(topic).expect("topic"),
        sequence: Revision::new(sequence).expect("sequence"),
    }
}

fn record(kind: ResourceKind, id: &str, summary: &str) -> ResourceRecord {
    ResourceRecord {
        resource: ResourceCoordinate::new(
            if kind == ResourceKind::Model {
                ResourceAuthority::Provider
            } else {
                ResourceAuthority::Automonique
            },
            kind,
            ResourceId::new(id).expect("id"),
        ),
        freshness: Freshness {
            state: FreshnessState::Fresh,
            observed_at: EpochMillis::from_millis(1),
            revision: Revision::FIRST,
        },
        summary: PlatformText::new(summary).expect("summary"),
    }
}

fn response(request: &PlatformRequest) -> PlatformResponse {
    match request {
        PlatformRequest::Capabilities => {
            PlatformResponse::Capabilities(Capabilities::platform_v1())
        }
        PlatformRequest::Snapshot(_) => PlatformResponse::Snapshot(
            Snapshot::new(
                vec![
                    record(ResourceKind::Node, "node-fixture", "daemon ready"),
                    record(ResourceKind::Model, "model-fixture", "available=true"),
                    record(
                        ResourceKind::Client,
                        "platform-action-submit_request",
                        "registry=platform-v1;action=submit_request;target=node;parameter=text;confirmation=required",
                    ),
                    record(
                        ResourceKind::Client,
                        "platform-action-follow_up",
                        "registry=platform-v1;action=follow_up;target=session;parameter=text;confirmation=required",
                    ),
                ],
                cursor("resources", 1),
            )
            .expect("snapshot"),
        ),
        PlatformRequest::ListSessions(_) => {
            let session = record(ResourceKind::Session, "session-fixture", "open");
            PlatformResponse::Sessions(
                SessionList::new(
                    vec![SessionRecord {
                        session,
                        run: None,
                        attachable: true,
                        controllable: true,
                    }],
                    cursor("sessions", 1),
                )
                .expect("sessions"),
            )
        }
        PlatformRequest::Subscribe(request) => PlatformResponse::Subscription(
            Subscription::new(
                Vec::new(),
                request
                    .cursor
                    .clone()
                    .unwrap_or_else(|| cursor("resources", 1)),
            )
            .expect("subscription"),
        ),
        _ => PlatformResponse::Refused {
            outcome: jcode_operator_backend::platform_contract::ReceiptOutcome::Rejected,
            explanation: PlatformText::new("fixture_read_only").expect("explanation"),
        },
    }
}

fn spawn_fixture_server(
    socket: &std::path::Path,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let listener = UnixListener::bind(socket).expect("bind fixture socket");
    listener.set_nonblocking(true).expect("nonblocking");
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::clone(&stop);
    let handle = std::thread::spawn(move || {
        while !server_stop.load(Ordering::Acquire) {
            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => return,
            };
            let mut prefix = [0_u8; LENGTH_PREFIX_BYTES];
            if stream.read_exact(&mut prefix).is_err() {
                continue;
            }
            let Ok(length) = usize::try_from(u32::from_be_bytes(prefix)) else {
                continue;
            };
            let mut frame = Vec::with_capacity(LENGTH_PREFIX_BYTES + length);
            frame.extend_from_slice(&prefix);
            frame.resize(LENGTH_PREFIX_BYTES + length, 0);
            if stream
                .read_exact(&mut frame[LENGTH_PREFIX_BYTES..])
                .is_err()
            {
                continue;
            }
            let Ok(FrameDecode::Frame { payload, .. }) = decode_frame(&frame) else {
                continue;
            };
            let Ok(message) = PlatformRequestMessage::from_canonical_bytes(payload) else {
                continue;
            };
            let response = PlatformResponseMessage::new(
                message.request_id().clone(),
                response(message.request()),
            )
            .to_message()
            .expect("response message")
            .to_canonical_bytes();
            let mut encoded = Vec::new();
            encode_frame(&response, &mut encoded).expect("encode response");
            let _ = stream.write_all(&encoded);
            let _ = stream.flush();
        }
    });
    (stop, handle)
}

#[test]
fn json_mode_reads_the_complete_fixture_without_a_provider() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket = temp.path().join("platform.sock");
    let (stop, server) = spawn_fixture_server(&socket);
    let output = Command::new(env!("CARGO_BIN_EXE_jcode"))
        .args(["platform", "--json", "--socket"])
        .arg(&socket)
        .output()
        .expect("run JSON client");
    stop.store(true, Ordering::Release);
    server.join().expect("server");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON output");
    assert_eq!(value["protocol"], "automonique.platform");
    assert!(
        value["actions"]
            .as_array()
            .expect("actions")
            .iter()
            .any(|action| action == "follow_up")
    );
    assert_eq!(value["resources"].as_array().expect("resources").len(), 4);
    assert_eq!(value["sessions"].as_array().expect("sessions").len(), 1);
}

#[test]
#[allow(
    clippy::unnecessary_mut_passed,
    reason = "libc::openpty takes mutable pointers on supported Unix targets"
)]
fn pty_cockpit_starts_handles_input_and_restores_the_terminal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket = temp.path().join("platform.sock");
    let (stop, server) = spawn_fixture_server(&socket);

    let mut master_fd = -1;
    let mut slave_fd = -1;
    let mut size = libc::winsize {
        ws_row: 32,
        ws_col: 110,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    let master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
    let mut input = master.try_clone().expect("input clone");
    let output = Arc::new(Mutex::new(Vec::new()));
    let output_reader = Arc::clone(&output);
    let reader = std::thread::spawn(move || {
        let mut master = master;
        let mut bytes = [0_u8; 4096];
        while let Ok(count) = master.read(&mut bytes) {
            if count == 0 {
                break;
            }
            output_reader
                .lock()
                .expect("output")
                .extend_from_slice(&bytes[..count]);
        }
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_jcode"))
        .args(["platform", "--socket"])
        .arg(&socket)
        .env("TERM", "xterm-256color")
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .stdin(Stdio::from(slave.try_clone().expect("stdin")))
        .stdout(Stdio::from(slave.try_clone().expect("stdout")))
        .stderr(Stdio::from(slave))
        .spawn()
        .expect("spawn cockpit");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        if rendered.contains("AUTOMONIQUE OPERATOR") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cockpit did not render: {rendered}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    input
        .write_all(b"ihello from pty\r")
        .expect("compose request");
    let confirmation_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        if rendered.contains("Submit durable request") && rendered.contains("Exact target") {
            break;
        }
        assert!(
            Instant::now() < confirmation_deadline,
            "typed request confirmation did not render"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    input
        .write_all(b"n?")
        .expect("cancel mutation and open help");
    std::thread::sleep(Duration::from_millis(50));
    input.write_all(b"?q").expect("close help and quit");
    input.flush().expect("flush input");
    while child.try_wait().expect("wait").is_none() {
        assert!(Instant::now() < deadline, "cockpit did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(child.wait().expect("status").success());
    drop(input);
    reader.join().expect("reader");
    stop.store(true, Ordering::Release);
    server.join().expect("server");
    let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
    assert!(rendered.contains("Keyboard-only operator help"));
    assert!(
        rendered.contains("\u{1b}[?1049l"),
        "terminal did not leave the alternate screen"
    );
}

#[test]
#[allow(
    clippy::unnecessary_mut_passed,
    reason = "libc::openpty takes mutable pointers on supported Unix targets"
)]
fn pty_cockpit_restores_across_suspend_resume_and_termination() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket = temp.path().join("platform.sock");
    let (stop, server) = spawn_fixture_server(&socket);

    let mut master_fd = -1;
    let mut slave_fd = -1;
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    let master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
    let output = Arc::new(Mutex::new(Vec::new()));
    let output_reader = Arc::clone(&output);
    let reader = std::thread::spawn(move || {
        let mut master = master;
        let mut bytes = [0_u8; 4096];
        while let Ok(count) = master.read(&mut bytes) {
            if count == 0 {
                break;
            }
            output_reader
                .lock()
                .expect("output")
                .extend_from_slice(&bytes[..count]);
        }
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_jcode"))
        .args(["platform", "--socket"])
        .arg(&socket)
        .env("TERM", "xterm-256color")
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .stdin(Stdio::from(slave.try_clone().expect("stdin")))
        .stdout(Stdio::from(slave.try_clone().expect("stdout")))
        .stderr(Stdio::from(slave))
        .spawn()
        .expect("spawn cockpit");
    let pid = i32::try_from(child.id()).expect("child pid");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !String::from_utf8_lossy(&output.lock().expect("output")).contains("AUTOMONIQUE OPERATOR")
    {
        assert!(Instant::now() < deadline, "cockpit did not render");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTSTP) }, 0);
    let mut stopped = false;
    while !stopped {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG | libc::WUNTRACED) };
        assert!(waited >= 0, "waitpid: {}", std::io::Error::last_os_error());
        stopped = waited == pid && libc::WIFSTOPPED(status);
        assert!(Instant::now() < deadline, "cockpit did not suspend");
        std::thread::sleep(Duration::from_millis(20));
    }
    let before_resume = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
    assert!(
        before_resume.contains("\u{1b}[?1049l"),
        "suspend did not restore the primary screen"
    );

    assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
    while String::from_utf8_lossy(&output.lock().expect("output"))
        .matches("\u{1b}[?1049h")
        .count()
        < 2
    {
        assert!(Instant::now() < deadline, "cockpit did not resume");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let status = child.wait().expect("termination status");
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    reader.join().expect("reader");
    stop.store(true, Ordering::Release);
    server.join().expect("server");
    let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
    assert!(
        rendered.matches("\u{1b}[?1049l").count() >= 2,
        "termination after resume did not restore the terminal"
    );
}
