//! Frame-reading limits: the bridge must not let one client's malformed or
//! unbounded input cost it memory or silence.

use super::*;

async fn read_all(input: &'static str) -> Vec<std::io::Result<String>> {
    let mut reader = BufReader::new(input.as_bytes());
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        match read_frame(&mut reader, &mut line).await {
            Ok(0) => return out,
            Ok(_) => out.push(Ok(line)),
            Err(error) => {
                out.push(Err(error));
                return out;
            }
        }
    }
}

#[tokio::test]
async fn frames_are_split_on_newlines() {
    let frames = read_all("{\"a\":1}\n{\"b\":2}\n").await;
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].as_ref().unwrap(), "{\"a\":1}\n");
    assert_eq!(frames[1].as_ref().unwrap(), "{\"b\":2}\n");
}

#[tokio::test]
async fn a_final_frame_without_a_newline_is_still_returned() {
    let frames = read_all("{\"a\":1}").await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].as_ref().unwrap(), "{\"a\":1}");
}

/// `read_line` grows its buffer until it finds a newline. Without a cap, a
/// client that opens a connection and sends bytes forever makes the bridge
/// allocate forever, and the bridge serves every API client on the machine.
#[tokio::test]
async fn an_unterminated_frame_is_refused_rather_than_buffered_forever() {
    let mut reader = BufReader::new(tokio::io::repeat(b'A'));
    let mut line = String::new();
    let error = read_frame(&mut reader, &mut line)
        .await
        .expect_err("an endless frame must fail, not buffer");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        error.to_string().contains("exceeds"),
        "error should say why: {error}"
    );
    assert!(
        line.len() as u64 <= MAX_FRAME_BYTES,
        "buffered {} bytes, above the {MAX_FRAME_BYTES} cap",
        line.len()
    );
}

/// The cap must not clip legitimate traffic: a frame just under it, images
/// included, has to pass intact.
#[tokio::test]
async fn a_large_but_terminated_frame_is_accepted() {
    let payload = "B".repeat(1024 * 1024);
    let input = format!("{payload}\n");
    let mut reader = BufReader::new(input.as_bytes());
    let mut line = String::new();
    let read = read_frame(&mut reader, &mut line)
        .await
        .expect("a 1 MiB frame is legitimate");
    assert_eq!(read, payload.len() + 1);
    assert_eq!(line.trim_end(), payload);
}

/// Each call must start from an empty buffer. Reusing a `String` across frames
/// is the natural way to write the read loop, and forgetting to clear it
/// concatenates every request into one unparseable blob.
#[tokio::test]
async fn each_frame_starts_from_a_clean_buffer() {
    let input = "first\nsecond\n";
    let mut reader = BufReader::new(input.as_bytes());
    let mut line = String::from("stale contents");
    read_frame(&mut reader, &mut line).await.unwrap();
    assert_eq!(line, "first\n");
    read_frame(&mut reader, &mut line).await.unwrap();
    assert_eq!(line, "second\n");
}

/// The supervised stdio transport must use the exact same handshake and
/// framing path as the socket transport. An incompatible hello is useful here
/// because it terminates before attempting to dial a daemon.
#[tokio::test]
async fn generic_io_transport_returns_a_correlated_version_refusal() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (client, server) = tokio::io::duplex(4096);
    let (server_read, server_write) = tokio::io::split(server);
    let task = tokio::spawn(handle_api_io(
        server_read,
        server_write,
        PathBuf::from("unused-daemon.sock"),
    ));

    let (client_read, mut client_write) = tokio::io::split(client);
    client_write
        .write_all(b"{\"v\":1,\"id\":41,\"req\":\"hello\",\"min_version\":9,\"max_version\":9,\"client\":\"test\"}\n")
        .await
        .expect("write hello");
    let mut response = String::new();
    BufReader::new(client_read)
        .read_line(&mut response)
        .await
        .expect("read refusal");

    let value: Value = serde_json::from_str(response.trim()).expect("valid response frame");
    assert_eq!(value["reply_to"], 41);
    assert_eq!(value["ev"], "error");
    assert_eq!(value["code"], "unsupported_version");
    task.await
        .expect("bridge task joins")
        .expect("bridge exits cleanly");
}

#[test]
fn advertised_capabilities_cover_managed_execution_controls() {
    for required in [
        "sessions",
        "streaming",
        "cancellation",
        "soft_interrupt",
        "stdin_requests",
        "history",
        "model_catalog",
        "usage",
    ] {
        assert!(
            HARNESS_CAPABILITIES.contains(&required),
            "managed hosts require {required} capability discovery"
        );
    }
}
