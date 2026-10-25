//! Starts the binary with a script and sends real HTTP requests.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server(Child);

/// A test that needs a service tenma is not running next to: skipped on a laptop, failed where TENMA_SERVICES is set:
/// the Linux job starts the service and a skip would hide a broken database path.
fn skip(reason: &str) {
    if std::env::var_os("TENMA_SERVICES").is_some() {
        panic!("{reason}, but TENMA_SERVICES says it is running");
    }
    eprintln!("{reason}, test skipped");
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn get(port: u16, path: &str) -> String {
    request(
        port,
        &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
    )
}

/// A body "<text> <seconds>s": the text as expected, and the time below `max`, which waits in a row would
/// exceed. Exact times vary on slow runners; only waiting one after another is a failure.
fn timed(body: &str, text: &str, max: f64) {
    let (got, seconds) = body
        .trim_end()
        .rsplit_once(' ')
        .unwrap_or_else(|| panic!("no time in {body:?}"));
    assert_eq!(got, text, "{body:?}");
    let seconds: f64 = seconds.trim_end_matches('s').parse().unwrap();
    assert!(seconds < max, "{body:?} took {seconds} s, {max} s at most");
}

/// The body of a raw HTTP response.
fn body(response: String) -> String {
    response
        .split_once("\r\n\r\n")
        .map_or(response.clone(), |(_, body)| body.to_owned())
}

fn request(port: u16, raw: &str) -> String {
    let started = Instant::now();
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => break stream,
            Err(_) if started.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => panic!("server does not respond: {error}"),
        }
    };
    stream.write_all(raw.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// A request that leaves the connection open, as browsers do; read until the server closes it or 3 s pass.
fn keep_alive(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    String::from_utf8_lossy(&response).into_owned()
}

#[test]
fn serves_requests_and_survives_exceptions() {
    let port = 18_080;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/hello.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    assert!(get(port, "/world").ends_with("Hello GET /world\n"));
    assert!(get(port, "/boom").starts_with("HTTP/1.1 500"));
    assert!(get(port, "/").starts_with("HTTP/1.1 200"));

    // A handler without a response only ends its request, the worker keeps running
    let before = get(port, "/count");
    assert!(get(port, "/null").starts_with("HTTP/1.1 500"));
    let after = get(port, "/count");
    let number = |response: &str| {
        response
            .trim_end()
            .rsplit(' ')
            .next()
            .unwrap()
            .parse::<u32>()
            .unwrap()
    };
    assert_eq!(
        number(&after),
        number(&before) + 2,
        "no restart: {before} / {after}"
    );
}

#[test]
fn async_calls_outside_a_handler() {
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("tests/fixtures/top_level_op.php")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "rejected\n");
}

#[test]
fn classic_code_with_superglobals_header_and_echo() {
    let port = 18_081;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/classic.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    let first = request(
        port,
        "GET /?name=world HTTP/1.1\r\nHost: localhost\r\nCookie: session=abc\r\nConnection: close\r\n\r\n",
    );
    assert!(first.starts_with("HTTP/1.1 200"), "{first}");
    assert!(first.contains("x-count: 1"));
    assert!(first.contains("set-cookie: a=1") && first.contains("set-cookie: b=2"));
    assert!(first.ends_with("GET world abc"));

    // Headers and status of the first request do not stick to the second, the state in the process does
    let second = get(port, "/?name=x&missing=1");
    assert!(second.starts_with("HTTP/1.1 404"), "{second}");
    assert!(second.contains("x-count: 2"));
    assert_eq!(second.matches("set-cookie").count(), 2);

    // A status HTTP has no room for is a 500, and the worker keeps its state
    assert!(get(port, "/?name=x&status=65736").starts_with("HTTP/1.1 500"));
    assert!(get(port, "/?name=x").contains("x-count: 4"));
}

#[test]
fn fibers_wait_without_blocking_the_server() {
    let port = 18_082;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/async.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    get(port, "/");

    // Ten requests that each sleep half a second are done together after just over half a second
    let started = Instant::now();
    let sleepers: Vec<_> = (0..10)
        .map(|_| std::thread::spawn(move || get(port, "/slow")))
        .collect();
    for sleeper in sleepers {
        assert!(sleeper.join().unwrap().ends_with("slept enough\n"));
    }
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );

    assert!(get(port, "/proxy").ends_with("proxy: hello\n"));

    // Task::all: three waits run at the same time, not one after another
    let started = Instant::now();
    assert!(get(port, "/all").ends_with("{\"a\":\"one\",\"b\":\"two\",\"c\":\"three\"}\n"));
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "{:?}",
        started.elapsed()
    );
    assert!(get(port, "/all-error").ends_with("caught: broken\n"));
    // The interval from the script's startup keeps running next to the requests
    let ticks = |response: String| {
        response
            .trim_end()
            .rsplit(' ')
            .next()
            .unwrap()
            .parse::<u32>()
            .unwrap()
    };
    let before = ticks(get(port, "/ticks"));
    std::thread::sleep(Duration::from_millis(300));
    assert!(ticks(get(port, "/ticks")) >= before + 4);
    let started = Instant::now();
    assert!(get(port, "/exit-task").starts_with("HTTP/1.1 500"));
    assert!(get(port, "/self-await").ends_with("rejected\n"));
    assert!(get(port, "/all-fast").ends_with("failed fast\n"));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert!(get(port, "/cancel-race").ends_with("b cancelled\n"));
    assert!(get(port, "/cancel-all").ends_with("children cancelled\n"));
    assert!(get(port, "/nested").ends_with("[\"hello\\n\",\"inner\"]\n"));
    assert!(get(port, "/unreachable").starts_with("HTTP/1.1 500"));
    assert!(get(port, "/bad-method").ends_with("invalid method\n"));
    let started = Instant::now();
    assert!(get(port, "/fetch-timeout").ends_with("timeout\n"));
    assert!(started.elapsed() < Duration::from_secs(1));
    // Output buffers belong to their request, even while many of them wait inside ob_start()
    let buffered: Vec<_> = (0..20)
        .map(|id| std::thread::spawn(move || (id, get(port, &format!("/buffered?id={id}")))))
        .collect();
    for request in buffered {
        let (id, response) = request.join().unwrap();
        assert!(
            response.ends_with(&format!("\r\n\r\n<{id}>INNER {id}</{id}>\n")),
            "{response}"
        );
    }
    // A normal response ends what the request started; only detach() outlives it
    assert!(get(port, "/leave-behind").ends_with("started\n"));
    let left = get(port, "/left");
    assert!(
        left.ends_with(
            "{\"interval\":0,\"message\":0,\"task\":0,\"detached\":1,\"retried\":0,\"late\":0}\n"
        ),
        "{left}"
    );
    assert!(get(port, "/buffer-left-open").ends_with("level 1\n"));
    assert!(get(port, "/level").ends_with("level 0\n"));
    assert!(get(port, "/").ends_with("hello\n"));
}

#[test]
fn several_workers_with_their_own_state() {
    let port = 18_083;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--workers", "3", "tests/fixtures/classic.php"])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    // Round robin: each of the three workers counts for itself, so 1, 2 and 3 twice
    let mut counts: Vec<String> = (0..6)
        .map(|_| {
            let response = get(port, "/?name=x");
            response
                .lines()
                .find_map(|line| line.strip_prefix("x-count: "))
                .unwrap()
                .to_owned()
        })
        .collect();
    counts.sort();
    assert_eq!(counts, ["1", "1", "1", "2", "2", "2"]);
}

fn post(port: u16, path: &str, body: &str) -> String {
    request(
        port,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}

#[test]
fn fatal_error_restarts_only_the_worker() {
    let port = 18_084;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/fatal.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    assert!(get(port, "/").ends_with("request 1\n"));
    // The broken interpreter takes no further request on this connection
    let failed = keep_alive(port, "/oom");
    assert!(
        failed.starts_with("HTTP/1.1 500") && failed.contains("connection: close"),
        "{failed}"
    );
    // New interpreter, script ran again: the counter starts over
    assert!(get(port, "/").ends_with("request 1\n"));
    // After an op the 500 comes right away, not only after the drain
    let started = std::time::Instant::now();
    assert!(get(port, "/slow-oom").starts_with("HTTP/1.1 500"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    let after = get(port, "/");
    assert!(after.ends_with("request 1\n"), "{after}");
}

#[test]
fn limits_for_body_and_requests_per_worker() {
    let port = 18_085;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--max-body",
                "1k",
                "--max-requests",
                "2",
                "tests/fixtures/fatal.php",
            ])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    // Two requests per interpreter, then the worker restarts
    let counts: Vec<bool> = (0..4)
        .map(|_| get(port, "/").ends_with("request 1\n"))
        .collect();
    assert_eq!(counts, [true, false, true, false]);

    assert!(post(port, "/upload", &"x".repeat(1024)).ends_with("1024\n"));
    assert!(post(port, "/upload", &"x".repeat(1025)).starts_with("HTTP/1.1 413"));
}

#[test]
fn sigterm_lets_running_requests_finish() {
    let port = 18_086;
    let server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/fatal.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    get(port, "/");

    let slow = std::thread::spawn(move || get(port, "/slow"));
    std::thread::sleep(Duration::from_millis(150));
    Command::new("kill")
        .args(["-TERM", &server.0.id().to_string()])
        .status()
        .unwrap();
    assert!(slow.join().unwrap().ends_with("slept enough\n"));
}

#[test]
fn classic_with_input_uploads_and_exit() {
    let port = 18_087;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/classic_io.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    assert!(post(port, "/input", "raw body").ends_with("input: raw body"));

    let boundary = "tenmaboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nImage\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"hello.txt\"\r\nContent-Type: text/plain\r\n\r\nHello Mars\r\n\
         --{boundary}--\r\n"
    );
    let upload = request(
        port,
        &format!(
            "POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(
        upload.ends_with("Image hello.txt 10 Hello Mars"),
        "{upload}"
    );

    let exited = get(port, "/exit");
    assert!(
        exited.starts_with("HTTP/1.1 200")
            && exited.contains("x-before-exit: yes")
            && exited.ends_with("before exit"),
        "{exited}"
    );
    assert!(get(port, "/flush-all").ends_with("\r\n\r\nabc"));
    assert!(get(port, "/async").ends_with("\r\n\r\nrejected async too"));
    assert!(get(port, "/locked-buffer").ends_with("\r\n\r\ninside outside"));
    let warned = get(port, "/warn");
    assert!(warned.ends_with("\r\n\r\nonly this"), "{warned}");
    assert!(get(port, "/").ends_with("ok"));
}

#[test]
fn a_fatal_error_while_parsing_a_form_leaves_nothing_for_the_next_request() {
    let port = 18_137;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "-d",
                "memory_limit=16M",
                "-d",
                "max_input_vars=1000000",
                "tests/fixtures/classic_io.php",
            ])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    // So many fields that PHP's multipart parser runs out of memory halfway through
    let boundary = "tenmaboundary";
    let body: String = (0..120_000).map(|i| format!("--{boundary}\r\nContent-Disposition: form-data; name=\"f[]\"\r\n\r\nleftover{i}\r\n")).collect::<String>() + &format!("--{boundary}--\r\n");
    let failed = request(
        port,
        &format!(
            "POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(
        failed.starts_with("HTTP/1.1 500"),
        "{}",
        &failed[..failed.len().min(200)]
    );
    // The fresh interpreter's php://input is this request's body, not the old one's freed memory
    let input = post(port, "/input", "raw body");
    assert!(input.ends_with("\r\n\r\ninput: raw body"), "{input}");
}

#[test]
fn a_silent_connection_does_not_hold_a_restart() {
    let port = 18_135;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--max-requests", "1", "tests/fixtures/hello.php"])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    get(port, "/");
    // Like a browser's preconnect: open, but no request yet
    let silent = TcpStream::connect(("127.0.0.1", port)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    // This request restarts the interpreter; the next one waits for the fresh one, not for the silent socket
    get(port, "/");
    let started = Instant::now();
    assert!(get(port, "/world").ends_with("Hello GET /world\n"));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    drop(silent);
}

#[test]
fn a_throwing_destructor_at_recycle_ends_only_the_interpreter() {
    let port = 18_136;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--max-requests",
                "1",
                "tests/fixtures/scripts/destructor_on_recycle.php",
            ])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    for _ in 0..3 {
        assert!(get(port, "/").ends_with("\r\n\r\nok\n"));
    }
}

#[test]
fn sessions_per_visitor_without_leaks() {
    let port = 18_088;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/session.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    let first = get(port, "/");
    assert!(first.ends_with("visit 1"), "{first}");
    let cookie = first
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: "))
        .expect("session cookie missing");
    let cookie = cookie.split(';').next().unwrap();

    let with_cookie = |path: &str| {
        request(
            port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
            ),
        )
    };
    assert!(with_cookie("/").ends_with("visit 2"));
    assert!(with_cookie("/").ends_with("visit 3"));

    // A new visitor gets neither the ID nor the data of the previous one
    assert!(get(port, "/peek").ends_with("empty"));
    let stranger = get(port, "/");
    assert!(stranger.ends_with("visit 1"), "{stranger}");
    assert!(!stranger.contains(cookie));
}

#[test]
fn fiber_mode_refuses_sessions() {
    let port = 18_196;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/session_fiber.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    let response = get(port, "/");
    assert!(
        response.ends_with("visitors would share one session"),
        "{response}"
    );
    assert!(!response.contains("set-cookie"), "{response}");
}

#[test]
fn classic_requests_do_not_leave_async_local_values() {
    let port = 18_197;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/classic_async_local.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    assert!(get(port, "/?set=alice").ends_with("script"));
    assert!(get(port, "/").ends_with("script"));
    assert!(get(port, "/?fiber=1").ends_with("script"));
    assert!(get(port, "/").ends_with("script"));
}

#[test]
fn built_in_extensions_work() {
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("tests/fixtures/extensions.php")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn streaming_sse_und_websockets() {
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::{Message, connect};

    let port = 18_089;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/streams.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    get(port, "/");

    // The first word arrives at once, not only when the handler is done after 0.6 s
    let started = Instant::now();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut first = [0; 512];
    let read = stream.read(&mut first).unwrap();
    assert!(String::from_utf8_lossy(&first[..read]).contains("one"));
    assert!(
        started.elapsed() < Duration::from_millis(150),
        "{:?}",
        started.elapsed()
    );
    let mut rest = String::new();
    stream.read_to_string(&mut rest).unwrap();
    assert!(rest.contains("two") && rest.contains("three"));

    let events = get(port, "/events");
    assert!(events.contains("content-type: text/event-stream"));
    assert!(events.contains("id: 1\nevent: greeting\ndata: hello\n\n"));
    assert!(events.contains("data: two\ndata: lines\n\n"));

    // If the client leaves, the handler notices on its next write
    let mut forever = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(forever, "GET /forever HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    forever.read_exact(&mut [0; 4096]).unwrap();
    drop(forever);
    std::thread::sleep(Duration::from_millis(200));
    assert!(get(port, "/closed").ends_with("closed"));
    // Also while the handler waits without writing
    let mut quiet = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(quiet, "GET /quiet HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    quiet.read(&mut [0; 4096]).unwrap();
    drop(quiet);
    std::thread::sleep(Duration::from_millis(200));
    assert!(get(port, "/quiet-closed").ends_with("closed"));

    // exit() in the stream: the client gets what was already written, and the connection ends
    let exited = get(port, "/exit-stream");
    assert!(exited.contains("before exit"), "{exited}");

    // A stream or WebSocket that an error cuts off: the stream lacks its last chunk, the socket closes with 1011
    for path in ["/throw-stream", "/fatal-stream"] {
        let cut = get(port, path);
        assert!(
            cut.contains("first part") && !cut.ends_with("0\r\n\r\n"),
            "{path}: {cut:?}"
        );
    }
    for path in ["/ws-throw", "/ws-fatal"] {
        let (mut socket, _) = connect(format!("ws://127.0.0.1:{port}{path}")).unwrap();
        assert_eq!(socket.read().unwrap(), Message::text("hi"));
        let close = socket.read();
        assert!(
            matches!(&close, Ok(Message::Close(Some(frame))) if frame.code == CloseCode::Error),
            "{path}: {close:?}"
        );
    }

    let (mut socket, _) = connect(format!("ws://127.0.0.1:{port}/ws")).unwrap();
    for word in ["hello", "world"] {
        socket.send(Message::text(word)).unwrap();
        assert_eq!(
            socket.read().unwrap(),
            Message::text(format!("echo: {word}"))
        );
    }
    // Bytes that are no UTF-8 come back unchanged, as a binary message
    socket
        .send(Message::binary(vec![0xff, 0x00, 0xfe]))
        .unwrap();
    assert_eq!(
        socket.read().unwrap(),
        Message::binary(b"echo: \xff\x00\xfe".to_vec())
    );
    socket.close(None).unwrap();
    assert!(get(port, "/").ends_with("ok"));

    // Only a GET for version 13 opens a WebSocket (RFC 6455 4.2)
    let handshake = |method: &str, version: &str| {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        write!(stream, "{method} /ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: {version}\r\nContent-Length: 0\r\n\r\n").unwrap();
        let mut response = [0; 512];
        let read = stream.read(&mut response).unwrap();
        String::from_utf8_lossy(&response[..read]).into_owned()
    };
    assert!(handshake("POST", "13").starts_with("HTTP/1.1 405"));
    let unsupported = handshake("GET", "99");
    assert!(
        unsupported.starts_with("HTTP/1.1 426")
            && unsupported.contains("sec-websocket-version: 13"),
        "{unsupported}"
    );

    // An interval set in the handler ends with the connection instead of sending into a closed socket
    let (mut ticker, _) = connect(format!("ws://127.0.0.1:{port}/ws-ticker")).unwrap();
    assert_eq!(ticker.read().unwrap(), Message::text("tick"));
    ticker.close(None).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    // The bodies only: the Date header may change in between
    let ticks = || {
        get(port, "/ticks")
            .rsplit("\r\n\r\n")
            .next()
            .unwrap()
            .to_owned()
    };
    let after_close = ticks();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(ticks(), after_close);

    // A handler that waits for something else than a message is cancelled when the client leaves
    let (mut sleeper, _) = connect(format!("ws://127.0.0.1:{port}/ws-sleep")).unwrap();
    assert_eq!(sleeper.read().unwrap(), Message::text("ready"));
    sleeper.close(None).unwrap();
    let slept = eventually(port, "/slept", |response| response.ends_with("cancelled"));
    assert!(slept.ends_with("cancelled"), "{slept}");

    // A burst larger than the queue arrives whole: the handler waits for the client
    let (mut burst, _) = connect(format!("ws://127.0.0.1:{port}/ws-burst")).unwrap();
    for i in 1..=1000 {
        assert_eq!(burst.read().unwrap(), Message::text(format!("message {i}")));
    }
    assert!(matches!(burst.read(), Ok(Message::Close(_))));

    // 300 messages to a handler that never reads: tenma disconnects after 256, long before the handler ends
    let started = Instant::now();
    let (mut flood, _) = connect(format!("ws://127.0.0.1:{port}/ws-ignore")).unwrap();
    for _ in 0..300 {
        if flood.send(Message::text("flood")).is_err() {
            break;
        }
    }
    loop {
        match flood.read() {
            Ok(Message::Close(frame)) => {
                assert!(
                    frame.is_some_and(|frame| frame.reason.contains("Too many unread messages"))
                );
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn clients_that_hold_the_server_are_limited() {
    let port = 18_139;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--request-timeout",
                "0.5",
                "--max-connections",
                "3",
                "--max-body",
                "64k",
                "tests/fixtures/streams.php",
            ])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    get(port, "/");

    // A client that stops reading: once the buffers are full, the write stalls and tenma disconnects
    let started = Instant::now();
    let mut stuck = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stuck,
        "GET /forever?kb=64 HTTP/1.1\r\nHost: localhost\r\n\r\n"
    )
    .unwrap();
    while !get(port, "/closed").ends_with("closed") {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the writer still waits for a client that does not read"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(stuck);
    // The same for a WebSocket: send() waits for the client until it has stalled for too long
    let started = Instant::now();
    let (flood, _) =
        tokio_tungstenite::tungstenite::connect(format!("ws://127.0.0.1:{port}/ws-flood")).unwrap();
    while !get(port, "/flooded").ends_with("closed") {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "send() still waits for a client that does not read"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(flood);
    std::thread::sleep(Duration::from_millis(100));

    // Three connections are the limit: the fourth gets 503 at once, and once one closes there is room again
    let idle: Vec<TcpStream> = (0..3)
        .map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap())
        .collect();
    std::thread::sleep(Duration::from_millis(100));
    let refused = get(port, "/");
    assert!(refused.starts_with("HTTP/1.1 503"), "{refused}");
    drop(idle);
    std::thread::sleep(Duration::from_millis(100));
    assert!(get(port, "/").ends_with("ok"));

    // Unread messages are limited by their bytes too: --max-body in all, long before 256 messages
    let (mut flood, _) =
        tokio_tungstenite::tungstenite::connect(format!("ws://127.0.0.1:{port}/ws-ignore"))
            .unwrap();
    for _ in 0..20 {
        if flood
            .send(tokio_tungstenite::tungstenite::Message::binary(vec![
                0;
                8 << 10
            ]))
            .is_err()
        {
            break;
        }
    }
    let close = loop {
        match flood.read() {
            Ok(tokio_tungstenite::tungstenite::Message::Close(frame)) => break frame,
            Ok(_) => continue,
            Err(error) => panic!("{error}"),
        }
    };
    assert!(close.is_some_and(|frame| frame.reason.contains("Too many unread messages")));
    drop(flood);
    std::thread::sleep(Duration::from_millis(100));

    // Open WebSockets count too, after their 101
    let sockets: Vec<_> = (0..3)
        .map(|_| {
            tokio_tungstenite::tungstenite::connect(format!("ws://127.0.0.1:{port}/ws")).unwrap()
        })
        .collect();
    let refused = get(port, "/");
    assert!(refused.starts_with("HTTP/1.1 503"), "{refused}");
    drop(sockets);
    std::thread::sleep(Duration::from_millis(100));
    assert!(get(port, "/").ends_with("ok"));
}

#[test]
fn postgres_and_redis_async() {
    let port = 18_090;
    let user = std::env::var("USER").unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/db.php")
            .env("PORT", port.to_string())
            .env(
                "DATABASE_URL",
                std::env::var("DATABASE_URL")
                    .unwrap_or(format!("postgres://{user}@localhost/postgres")),
            )
            .env(
                "REDIS_URL",
                std::env::var("REDIS_URL").unwrap_or("redis://127.0.0.1:6379".into()),
            )
            .spawn()
            .unwrap(),
    );
    if !get(port, "/setup").starts_with("HTTP/1.1 200") {
        skip("no Postgres reachable");
        return;
    }

    let inserted = get(port, "/insert?name=first");
    assert!(
        inserted.contains(r#""name": "first""#)
            && inserted.contains(r#""amount": 42"#)
            && inserted.contains(r#""price": 9.99"#),
        "{inserted}"
    );
    // SHOW and EXPLAIN return rows but cannot stand inside a WITH: values come as text
    let show = get(port, "/show");
    assert!(
        show.contains(r#""client_encoding": "UTF8""#)
            && show.contains("true")
            && show.contains("SHOW, EXPLAIN and FETCH take no parameters"),
        "{show}"
    );
    let numeric = get(port, "/numeric");
    assert!(
        numeric.contains(r#""big": "123456789012345678.90""#)
            && numeric.contains(r#""small": 9.99"#),
        "{numeric}"
    );
    assert!(
        inserted.contains(r#""tags": ["#) && inserted.contains(r#""created": ""#),
        "{inserted}"
    );
    assert!(get(port, "/list").contains(r#""name": "first""#));
    assert!(get(port, "/rollback").contains(r#""n": 0"#));
    assert!(get(port, "/commit").ends_with("1\n"));
    assert!(get(port, "/script").contains(r#""n": 3"#));
    let modifying = get(port, "/modifying-with");
    assert!(
        modifying.contains(r#""amount": 44"#) && modifying.contains(r#""n": 0"#),
        "{modifying}"
    );
    let failed = get(port, "/script-fails");
    assert!(
        failed.contains(r#""error": true"#) && failed.contains(r#""n": 0"#),
        "{failed}"
    );
    assert!(get(port, "/error").contains("does_not_exist"));
    let started = Instant::now();
    let all_tx = get(port, "/all-tx");
    assert!(all_tx.contains("caught: on purpose"), "{all_tx}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    let ordered = get(port, "/comment");
    assert!(
        ordered.contains("[\n    5,\n    4,\n    3,\n    2,\n    1\n]"),
        "{ordered}"
    );

    // Ten queries with half a second of waiting each run at the same time, the worker does not block
    let started = Instant::now();
    let sleepers: Vec<_> = (0..10)
        .map(|_| std::thread::spawn(move || get(port, "/slow")))
        .collect();
    for sleeper in sleepers {
        assert!(sleeper.join().unwrap().contains("slept"));
    }
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );

    let redis = get(port, "/redis");
    assert!(
        redis.contains(r#""value""#) && redis.contains("1,\n"),
        "{redis}"
    );
    let binary = get(port, "/redis-binary");
    assert!(
        binary.ends_with("\"636166e900ff ff 312e35 32\"\n"),
        "{binary}"
    );
    assert!(get(port, "/drop").starts_with("HTTP/1.1 200"));
}

#[test]
fn standard_streams_like_in_the_cli() {
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("tests/fixtures/cli.php")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "to stdout\ninput: hello\n"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "to stderr\n");
}

#[test]
fn arguments_like_in_the_cli() {
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .args(["tests/fixtures/args.php", "import", "--force", "-w"])
        .output()
        .unwrap();
    let argv = r#"["tests\/fixtures\/args.php","import","--force","-w"]"#;
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("[{argv},4,{argv},4,\"import\"]")
    );
}

#[test]
fn settings_with_minus_d_and_php_ini() {
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .args([
            "-c",
            "tests/fixtures/php.ini",
            "-d",
            "memory_limit=256M",
            "-ddate.timezone=Europe/Berlin",
            "tests/fixtures/ini.php",
        ])
        .output()
        .unwrap();
    // -d wins over the php.ini, values only from the php.ini stay, and so do tenma's own defaults
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"["256M","Europe\/Berlin","7","0"]"#
    );
}

#[test]
fn code_with_minus_r_and_scripts_with_a_shebang_run_as_with_php() {
    let tenma = env!("CARGO_BIN_EXE_tenma");
    let code = Command::new(tenma)
        .args([
            "-d",
            "memory_limit=99M",
            "-r",
            "echo json_encode([$argv, ini_get('memory_limit')]); exit(3);",
            "a",
            "-b",
        ])
        .output()
        .unwrap();
    assert_eq!(code.status.code(), Some(3));
    assert_eq!(
        String::from_utf8_lossy(&code.stdout),
        r#"[["Standard input code","a","-b"],"99M"]"#
    );

    // Composer finds tenma through PHP_BINARY for @php, the variable and the constant; a variable
    // set already stays
    let script = Command::new(tenma)
        .arg("tests/fixtures/shebang.php")
        .env_remove("PHP_BINARY")
        .output()
        .unwrap();
    let binary = std::fs::canonicalize(tenma).unwrap().display().to_string();
    assert_eq!(
        String::from_utf8_lossy(&script.stdout),
        format!("{binary}\n{binary}\n")
    );
    let given = Command::new(tenma)
        .arg("tests/fixtures/shebang.php")
        .env("PHP_BINARY", "/usr/bin/php")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&given.stdout),
        format!("/usr/bin/php\n{binary}\n")
    );
}

#[test]
fn tenma_composer_asks_for_an_install_first() {
    // An empty data directory: no Composer installed, nothing fetched
    let data = std::env::temp_dir().join(format!("tenma-composer-{}", std::process::id()));
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(args)
            .env("XDG_DATA_HOME", &data)
            .output()
            .unwrap();
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let missing = (
        Some(1),
        "tenma: Composer is not installed: tenma install composer\n".to_owned(),
    );
    assert_eq!(run(&["composer", "--version"]), missing);
    assert_eq!(run(&["update", "composer"]), missing);
    assert_eq!(run(&["remove", "composer"]), missing);
}

#[test]
fn server_variables_and_addresses() {
    let port = 18_091;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/server_vars.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    let classic = request(
        port,
        &format!("GET / HTTP/1.1\r\nHost: example.test:{port}\r\nConnection: close\r\n\r\n"),
    );
    let root = std::fs::canonicalize("tests/fixtures").unwrap();
    let expected = format!(
        r#"{{"REMOTE_ADDR":"127.0.0.1","SERVER_ADDR":"127.0.0.1","SERVER_PORT":"{port}","SERVER_NAME":"localhost","REQUEST_SCHEME":"http","SCRIPT_NAME":"\/server_vars.php","PHP_SELF":"\/server_vars.php","DOCUMENT_ROOT":"{}","remote_port":true}}"#,
        root.display().to_string().replace('/', "\\/")
    );
    assert!(classic.ends_with(&expected), "{classic}");
    assert!(get(port + 1, "/").ends_with(&format!("127.0.0.1 {} localhost", port + 1)));
}

#[test]
fn shutdown_functions_per_request() {
    use std::process::Stdio;

    let port = 18_093;
    let server = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("tests/fixtures/shutdown.php")
        .env("PORT", port.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // If a check fails, no server may be left behind on the port
    struct KillOnDrop(u32);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = Command::new("kill")
                .args(["-KILL", &self.0.to_string()])
                .status();
        }
    }
    let guard = KillOnDrop(server.id());

    // Exactly once per request, also after exit() in the request and exit() in the shutdown function
    for path in ["/", "/exit", "/exit-shutdown"] {
        let response = get(port, path);
        assert!(
            response.starts_with("HTTP/1.1 200") && response.ends_with("request after"),
            "{path}: {response}"
        );
    }
    assert!(
        get(port, "/count").ends_with("\r\n\r\n4"),
        "no restart up to here"
    );

    // Exception in the shutdown function: fatal error like in PHP, response with 500, session closed, new worker
    let thrown = get(port, "/throw-shutdown");
    assert!(
        thrown.starts_with("HTTP/1.1 500") && thrown.ends_with("request after"),
        "{thrown}"
    );
    let peek = get(port, "/peek");
    assert!(peek.ends_with("empty"), "{peek}");
    assert!(
        get(port, "/count").ends_with("\r\n\r\n2"),
        "new interpreter"
    );

    // Fatal error in the request: the shutdown function still delivers the error page
    let fatal = get(port, "/fatal");
    // What was already output stays, like under FPM without output_buffering
    assert!(
        fatal.starts_with("HTTP/1.1 500")
            && fatal.ends_with("\r\n\r\nrequest after error page: fatal"),
        "{fatal}"
    );
    assert!(get(port, "/count").ends_with("\r\n\r\n1"));

    let big = get(port, "/big-fatal");
    assert!(
        big.starts_with("HTTP/1.1 500") && big.ends_with(&"x".repeat(3_000_000)),
        "{} bytes",
        big.len()
    );
    assert!(get(port, "/count").ends_with("\r\n\r\n1"));

    let compile = get(port, "/compile-error");
    assert!(
        compile.starts_with("HTTP/1.1 500") && compile.contains(" after trace "),
        "{compile}"
    );
    assert!(get(port, "/count").ends_with("\r\n\r\n1"));

    // The function registered at startup never ran in a response, but at the end of every interpreter
    Command::new("kill")
        .args(["-TERM", &server.id().to_string()])
        .status()
        .unwrap();
    let output = server.wait_with_output().unwrap();
    // Exited and reaped: the PID may now belong to another process
    std::mem::forget(guard);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .matches("REGISTERED-AT-STARTUP")
            .count(),
        5
    );
}

#[test]
fn classic_requests_leave_no_state() {
    let port = 18_094;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/isolation.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    assert!(post(port, "/set", "secret").ends_with("set"));
    // ini_set and set_time_limit back to the state after startup, php://input handle closed
    let after = post(port, "/", "from someone else");
    assert!(after.ends_with("10 0 closed"), "{after}");
}

#[test]
fn postgres_connections_come_back_clean() {
    let port = 18_095;
    let user = std::env::var("USER").unwrap();
    // A pool with exactly one connection: the second request is sure to get the same one
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--db-pool", "1", "tests/fixtures/db.php"])
            .env("PORT", port.to_string())
            .env(
                "DATABASE_URL",
                std::env::var("DATABASE_URL")
                    .unwrap_or(format!("postgres://{user}@localhost/postgres")),
            )
            .env(
                "REDIS_URL",
                std::env::var("REDIS_URL").unwrap_or("redis://127.0.0.1:6379".into()),
            )
            .spawn()
            .unwrap(),
    );
    if !get(port, "/").starts_with("HTTP/1.1 200")
        || !get(port, "/whoami").starts_with("HTTP/1.1 200")
    {
        skip("no Postgres reachable");
        return;
    }
    // Redis connection gone: tenma rebuilds it on the next command instead of failing forever
    get(port, "/redis-kill");
    let pings: Vec<bool> = (0..3)
        .map(|_| get(port, "/redis-ping").contains("PONG"))
        .collect();
    assert!(pings.ends_with(&[true]), "{pings:?}");
    let refused = get(port, "/redis-refused");
    assert!(refused.ends_with("[\n    \"refused\",\n    \"refused\",\n    \"refused\",\n    \"refused\",\n    \"refused\",\n    null\n]\n"), "{refused}");
    let wrongtype = get(port, "/redis-wrongtype");
    assert!(
        wrongtype.contains(r#""failed": true"#) && wrongtype.contains(r#""same": true"#),
        "{wrongtype}"
    );

    let alter = get(port, "/alter");
    assert!(
        alter.contains(r#""a": 1,"#) && alter.contains(r#""b": 2"#),
        "{alter}"
    );
    assert!(get(port, "/taint").starts_with("HTTP/1.1 200"));
    // With only one connection in the pool everything would hang if exit() did not release it
    assert!(get(port, "/exit-tx").starts_with("HTTP/1.1 500"));
    assert!(get(port, "/cancel-tx").contains("cancelled"));
    let whoami = get(port, "/whoami");
    assert!(whoami.contains("\"name\": \"\""), "{whoami}");
    // A transaction that waits for a second connection of its own pool: an error, not a hang
    let (sender, nested) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(get(port, "/nested")));
    let nested = nested
        .recv_timeout(Duration::from_secs(15))
        .expect("the pool waits forever");
    assert!(
        nested.contains("no connection of the pool came free in 10 s (--db-pool 1)"),
        "{nested}"
    );
    assert!(get(port, "/whoami").starts_with("HTTP/1.1 200"));
    // Many requests at once share the one connection: the pool opens no second one while it is cleaned
    let backends: Vec<_> = (0..20)
        .map(|_| std::thread::spawn(move || get(port, "/backend")))
        .collect();
    let pids: std::collections::HashSet<String> = backends
        .into_iter()
        .map(|backend| {
            backend
                .join()
                .unwrap()
                .rsplit("\r\n\r\n")
                .next()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(pids.len(), 1, "{pids:?}");
    // A type the connection met in an earlier request, then one it meets now: the cleanup between
    // them must keep what the driver prepared for looking types up
    assert!(get(port, "/types").starts_with("HTTP/1.1 200"));
    assert!(get(port, "/mood").contains(r#""value": "happy""#));
    let size = get(port, "/size");
    assert!(size.contains(r#""value": "big""#), "{size}");
    get(port, "/types-drop");

    // A client that leaves during a long query: Postgres stops it, before the pool hands out the connection again
    for path in ["/sleep", "/sleep-tx"] {
        let mut leaving = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(leaving, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        drop(leaving);
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        let sleeping = get(port, "/sleeping");
        assert!(
            sleeping.contains(r#""n": 0"#) && started.elapsed() < Duration::from_secs(2),
            "{path}: {sleeping}"
        );
    }
}

#[test]
fn php_input_kept_by_an_object_survives_the_request() {
    let port = 18_153;
    // The system allocator scribbles over freed memory: a stream freed under the object crashes tenma
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/scripts/input_kept.php")
            .env("PORT", port.to_string())
            .env("USE_ZEND_ALLOC", "0")
            .env("MallocScribble", "1")
            .spawn()
            .unwrap(),
    );
    let post = |body: &str| {
        request(
            port,
            &format!(
                "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
    };
    assert!(post("hello").ends_with("\r\n\r\nhello"));
    for _ in 0..3 {
        let response = post("next");
        assert!(response.ends_with("still here\n"), "{response:?}");
    }
}

#[test]
fn classic_without_client_influence() {
    let port = 18_096;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--server-name",
                "app.example",
                "tests/fixtures/security.php",
            ])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    let body = "0=post&only_post=1";
    let response = request(
        port,
        &format!(
            "POST /?0=get&only_get=1 HTTP/1.1\r\nHost: evil.example\r\nCookie: sid=victim; sid=attacker; a[b]=1; x.y=2\r\n\
             X-Forwarded-For: 10.0.0.1\r\nx_forwarded_for: 6.6.6.6\r\nContent-Type: application/x-www-form-urlencoded\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    // SERVER_NAME as configured, not from Host; the first cookie wins, names like under FPM;
    // the header with an underscore is dropped; POST overrides GET without renumbering
    let expected = r#"{"server_name":"app.example","host":"evil.example","cookies":{"sid":"victim","a":{"b":"1"},"x_y":"2"},"forwarded":"10.0.0.1","request":{"0":"post","only_get":"1","only_post":"1"}}"#;
    assert!(response.ends_with(expected), "{response}");
}

#[test]
fn slow_clients_are_disconnected() {
    let port = 18_097;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--request-timeout", "0.5", "tests/fixtures/hello.php"])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    get(port, "/");

    // Slowloris: the headers never arrive completely, tenma disconnects after the timeout
    let started = Instant::now();
    let mut slow = TcpStream::connect(("127.0.0.1", port)).unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    write!(slow, "GET / HTTP/1.1\r\nHost: localhost\r\n").unwrap();
    let mut rest = Vec::new();
    let _ = slow.read_to_end(&mut rest);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );

    // The body arrives only halfway: 408
    let started = Instant::now();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\nonly a part"
    )
    .unwrap();
    let mut response = String::new();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let _ = stream.read_to_string(&mut response);
    assert!(response.starts_with("HTTP/1.1 408"), "{response}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn restarts_without_leaks_or_double_signals() {
    use std::process::Stdio;

    struct KillOnDrop(u32);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = Command::new("kill")
                .args(["-KILL", &self.0.to_string()])
                .status();
        }
    }
    // Linux lists them in /proc, macOS has no /proc but lsof
    let open_files = |pid: u32| match std::fs::read_dir(format!("/proc/{pid}/fd")) {
        Ok(fds) => fds.count(),
        Err(_) => String::from_utf8_lossy(
            &Command::new("lsof")
                .args(["-p", &pid.to_string()])
                .output()
                .unwrap()
                .stdout,
        )
        .lines()
        .count(),
    };

    // One interpreter per request: 30 restarts must not cost file descriptors
    let port = 18_098;
    let restarting = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--max-requests", "1", "tests/fixtures/hello.php"])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    // The first restarts open what every later one reuses (event loops, sockets), on a busy machine
    // spread over more of them: a leak grows with every restart, so compare two rounds of 30
    for _ in 0..30 {
        get(port, "/");
    }
    // The last request of an interpreter closes its connection: the next one goes to the fresh one
    let last = keep_alive(port, "/");
    assert!(
        last.contains("connection: close") && last.ends_with("Hello GET /\n"),
        "{last}"
    );
    std::thread::sleep(Duration::from_millis(300));
    let before = open_files(restarting.0.id());
    for _ in 0..30 {
        get(port, "/");
    }
    // A restart still under way holds a descriptor or two for a moment; a leak stays
    let mut after = open_files(restarting.0.id());
    for _ in 0..10 {
        if after <= before + 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        after = open_files(restarting.0.id());
    }
    assert!(after <= before + 2, "{before} -> {after}");

    // Fatal error exactly on the max-requests request: one restart, not two
    let port = 18_099;
    let server = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .args(["--max-requests", "2", "tests/fixtures/fatal.php"])
        .env("PORT", port.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let guard = KillOnDrop(server.id());
    get(port, "/");
    assert!(get(port, "/oom").starts_with("HTTP/1.1 500"));
    assert!(get(port, "/").ends_with("request 1\n"));
    assert!(get(port, "/").ends_with("request 2\n"));
    Command::new("kill")
        .args(["-TERM", &server.id().to_string()])
        .status()
        .unwrap();
    let output = server.wait_with_output().unwrap();
    std::mem::forget(guard);
    // Start, restart after the fatal error, restart after two more requests
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .matches("start\n")
            .count(),
        3
    );
}

#[test]
fn prints_its_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("--version")
        .output()
        .unwrap();
    let version = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success()
            && version.starts_with(concat!("tenma ", env!("CARGO_PKG_VERSION"), " (PHP 8.")),
        "{version}"
    );
    // The extension reports the same version to PHP
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("tests/fixtures/version.php")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn nonsensical_options_are_rejected() {
    for args in [
        ["--max-body", "99999999999999999999g"],
        ["--max-body", "17179869184g"],
        ["--workers", "0"],
        ["--request-timeout", "inf"],
        ["--request-timeout", "1e30"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(args)
            .arg("tests/fixtures/hello.php")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn timers_and_background_work_like_in_node() {
    // A script without a server runs until nothing is pending, then tenma exits
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("tests/fixtures/timers.php")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "script done\nbackground done\nticks 3\nnested\n"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn exit_and_errors_outside_requests() {
    let run = |script: &str| {
        let started = Instant::now();
        let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg(format!("tests/fixtures/scripts/{script}"))
            .env("PORT", "18112")
            .output()
            .unwrap();
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            started.elapsed(),
        )
    };
    let (code, _, took) = run("exit_in_timer.php");
    assert_eq!(code, Some(3));
    assert!(took < Duration::from_secs(2));
    let (code, stdout, took) = run("die_at_start.php");
    assert_eq!(
        (code, stdout.as_str()),
        (Some(0), "Usage: die_at_start.php <file>\n")
    );
    assert!(took < Duration::from_secs(1));
    let (code, _, took) = run("fatal_in_background.php");
    assert_eq!(code, Some(255));
    assert!(took < Duration::from_secs(2));
    let (code, _, took) = run("fatal_at_server_start.php");
    assert_eq!(code, Some(255));
    assert!(took < Duration::from_secs(2));
    for _ in 0..8 {
        assert_eq!(run("exit_async.php").0, Some(3));
    }
    let (code, stdout, _) = run("bad_delay.php");
    assert_eq!(
        (code, stdout.as_str()),
        (Some(0), "rejected\nrejected\nrejected\n")
    );
}

#[test]
fn file_system_async() {
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("tests/fixtures/scripts/fs.php")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "[true,true,[\"a.txt\"],[\"sub\"],[\"c.txt\"],[21,true,true],true,false,false]\n\
         2 with path\n17 with path\n22 with path 21\nbytes kept\nchdir applies\ncleaned up\n"
    );
    let allowed = format!("open_basedir={}/tests", env!("CARGO_MANIFEST_DIR"));
    let basedir = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .args(["-d", &allowed, "tests/fixtures/scripts/fs_basedir.php"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&basedir.stdout), "forbidden\n");
}

#[test]
fn exit_status_is_not_stale() {
    let port = 18_113;
    let mut server = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("tests/fixtures/scripts/exit_status_stale.php")
        .env("PORT", port.to_string())
        .spawn()
        .unwrap();
    assert!(get(port, "/exit5").starts_with("HTTP/1.1 500"));
    assert!(get(port, "/").ends_with("ok\n"));
    assert_eq!(server.wait().unwrap().code(), Some(0));
}

#[test]
fn forms_at_their_limits() {
    let port = 18_138;
    // 8 MB in one text field: the parser once needed ten times that, now it fits next to the body
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "-d",
                "memory_limit=48M",
                "tests/fixtures/request_response.php",
            ])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    let boundary = "tenmaboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"blob\"\r\n\r\n{}\r\n--{boundary}--\r\n",
        "x".repeat(8 << 20)
    );
    let response = request(
        port,
        &format!(
            "POST /form-size HTTP/1.1\r\nHost: localhost\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(response.contains(r#""blob": 8388608"#), "{response}");

    // Exactly as many parts as allowed: the closing boundary is not one of them
    let form = |parts: usize| {
        (1..=parts)
            .map(|i| {
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"f{i}\"\r\n\r\n{i}\r\n"
                )
            })
            .collect::<String>()
            + &format!("--{boundary}--\r\n")
    };
    let send = |body: String| {
        request(
            port,
            &format!(
                "POST /form-size HTTP/1.1\r\nHost: localhost\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
    };
    let full = send(form(1000));
    assert!(
        full.contains(r#""f1000": 4"#),
        "{}",
        &full[..full.len().min(300)]
    );
    assert!(send(form(1001)).starts_with("HTTP/1.1 500"));
}

#[test]
fn request_and_response_in_fiber_mode() {
    let port = 18_114;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/request_response.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    get(port, "/");
    let send = |raw: String| request(port, &raw);

    // A status out of range is an error, not a wrapped-around 200
    for code in ["65736", "99", "1000", "-1"] {
        let response = get(port, &format!("/status?code={code}"));
        assert!(response.starts_with("HTTP/1.1 500"), "{code}: {response}");
    }

    // Cookies like under FPM: the first one wins, names like a[b] and x.y are converted
    let cookies = send("GET /cookies HTTP/1.1\r\nHost: x\r\nCookie: sid=first; sid=second; a[b]=1; x.y=2\r\nConnection: close\r\n\r\n".into());
    assert!(
        cookies.contains("\"sid\": \"first\"")
            && cookies.contains("\"b\": \"1\"")
            && cookies.contains("\"x_y\": \"2\""),
        "{cookies}"
    );

    let urlencoded = "a=1&list[]=x&list[]=y&deep[k]=v";
    let form = send(format!(
        "POST /form HTTP/1.1\r\nHost: x\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{urlencoded}",
        urlencoded.len()
    ));
    assert!(
        form.contains("\"list\": [\n            \"x\",\n            \"y\"\n        ]")
            && form.contains("\"k\": \"v\""),
        "{form}"
    );

    // Multipart: fields with PHP's semantics, files with names without directory, several under name[]
    let boundary = "boundary123";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nImage\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"tags[]\"\r\n\r\none\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"tags[]\"\r\n\r\ntwo\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"../../etc/hello.txt\"\r\nContent-Type: text/plain\r\n\r\nHello\r\nMars\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"many[]\"; filename=\"a.bin\"\r\n\r\nA\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"many[]\"; filename=\"b.bin\"\r\n\r\nB\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"empty\"; filename=\"\"\r\n\r\n\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"nameless\"; filename=\"\"\r\n\r\ndata\r\n\
         --{boundary}--\r\n"
    );
    let multipart = |path: &str| {
        send(format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
    };
    let parsed = multipart("/form");
    assert!(
        parsed.contains("\"title\": \"Image\"")
            && parsed.contains("\"tags\": [\n            \"one\",\n            \"two\"\n        ]"),
        "{parsed}"
    );
    assert!(parsed.contains("\"hello.txt\",\n            \"text\\/plain\",\n            11,\n            \"Hello\\r\\nMars\""), "{parsed}");
    assert!(
        parsed.contains("\"a.bin\"")
            && parsed.contains("\"b.bin\"")
            && !parsed.contains("\"empty\"")
            && !parsed.contains("\"nameless\""),
        "{parsed}"
    );
    assert!(multipart("/move").ends_with("\r\n\r\nHello\r\nMars"));

    // Response helpers: redirect, two cookies, one header with two values
    let set = get(port, "/set-cookie");
    assert!(
        set.starts_with("HTTP/1.1 303") && set.contains("location: /target"),
        "{set}"
    );
    assert!(
        set.contains("set-cookie: sid=a%20b%3Bc; Path=/app; HttpOnly; SameSite=Lax"),
        "{set}"
    );
    assert!(
        set.contains("set-cookie: theme=dark; Expires=Wed, 18 May 2033 03:33:20 GMT; Max-Age=")
            && set.contains("; Path=/; Secure; HttpOnly; SameSite=Strict"),
        "{set}"
    );
    assert!(set.contains("x-two: one") && set.contains("x-two: two"));
    // A control character in a header fails where it is set, or as a 500 instead of a lost header
    assert!(get(port, "/header-rejected").ends_with("Invalid header: content-disposition"));
    for value in ["a%1Fb", "a%00b"] {
        let response = get(port, &format!("/header-in-array?value={value}"));
        assert!(
            response.starts_with("HTTP/1.1 500") && !response.contains("never sent"),
            "{response}"
        );
    }
    assert!(get(port, "/header-in-array?value=a%09b").contains("x-name: a\tb"));
    let html = get(port, "/html");
    assert!(
        html.starts_with("HTTP/1.1 201")
            && html.contains("content-type: text/html; charset=utf-8")
            && html.ends_with("<h1>Hello</h1>")
    );
}

#[test]
fn static_files() {
    let port = 18_115;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/static.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    get(port, "/start");
    let send = |raw: &str| request(port, raw);

    let css = get(port, "/style.css");
    assert!(
        css.starts_with("HTTP/1.1 200")
            && css.contains("content-type: text/css; charset=utf-8")
            && css.ends_with("color: red; }\n"),
        "{css}"
    );
    assert!(css.contains("x-content-type-options: nosniff"), "{css}");
    // An SVG someone uploaded runs no script on the app's origin when opened directly
    let svg = get(port, "/upload.svg");
    assert!(
        svg.contains("content-type: image/svg+xml")
            && svg.contains("content-security-policy: sandbox"),
        "{svg}"
    );
    assert!(get(port, "/").ends_with("<h1>Start</h1>\n"));
    assert!(get(port, "/missing.css").ends_with("php: /missing.css\n"));

    // Never served: hidden files, ways out of the directory, symlinks to the outside
    for path in [
        "/.env",
        "/../secret.txt",
        "/%2e%2e/secret.txt",
        "/sub/..%2f..%2fsecret.txt",
        "/link.txt",
        "/front.php",
        "/front.PHP",
    ] {
        let response = get(port, path);
        assert!(
            response.contains("php: ")
                && !response.contains("SECRET")
                && !response.contains("secret\n"),
            "{path}: {response}"
        );
    }

    let etag = css
        .lines()
        .find_map(|line| line.strip_prefix("etag: "))
        .unwrap();
    let cached = send(&format!(
        "GET /style.css HTTP/1.1\r\nHost: x\r\nIf-None-Match: {etag}\r\nConnection: close\r\n\r\n"
    ));
    assert!(cached.starts_with("HTTP/1.1 304"), "{cached}");

    let part = send(
        "GET /sub/digits.txt HTTP/1.1\r\nHost: x\r\nRange: bytes=2-4\r\nConnection: close\r\n\r\n",
    );
    assert!(
        part.starts_with("HTTP/1.1 206")
            && part.contains("content-range: bytes 2-4/10")
            && part.ends_with("\r\n\r\n234"),
        "{part}"
    );
    let suffix = send(
        "GET /sub/digits.txt HTTP/1.1\r\nHost: x\r\nRange: bytes=-3\r\nConnection: close\r\n\r\n",
    );
    assert!(suffix.ends_with("\r\n\r\n789"), "{suffix}");
    let outside = send(
        "GET /sub/digits.txt HTTP/1.1\r\nHost: x\r\nRange: bytes=20-\r\nConnection: close\r\n\r\n",
    );
    assert!(outside.starts_with("HTTP/1.1 416"), "{outside}");

    let head = send("HEAD /style.css HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert!(
        head.contains("content-length: 21") && head.ends_with("\r\n\r\n"),
        "{head}"
    );
    // A symlink in the directory to a hidden file in it stays hidden
    assert!(get(port, "/config.txt").contains("php: /config.txt"));
    // A directory without / is redirected, without index.html it goes to PHP
    let directory = get(port, "/sub?a=1");
    assert!(
        directory.starts_with("HTTP/1.1 301") && directory.contains("location: /sub/?a=1"),
        "{directory}"
    );
    let doubled = get(port, "//sub");
    assert!(
        doubled.starts_with("HTTP/1.1 301") && doubled.contains("location: /sub/\r\n"),
        "{doubled}"
    );
    assert!(get(port, "/sub/").ends_with("php: /sub/\n"));
    // If-Range no longer matches: the whole file instead of a piece
    let stale = send(
        "GET /sub/digits.txt HTTP/1.1\r\nHost: x\r\nRange: bytes=2-4\r\nIf-Range: \"old\"\r\nConnection: close\r\n\r\n",
    );
    assert!(
        stale.starts_with("HTTP/1.1 200") && stale.ends_with("0123456789"),
        "{stale}"
    );
    // Invalid ranges are ignored, empty files arrive whole
    for value in ["bytes=5-3", "bytes=abc-", "bytes=+1-+3"] {
        let response = send(&format!(
            "GET /sub/digits.txt HTTP/1.1\r\nHost: x\r\nRange: {value}\r\nConnection: close\r\n\r\n"
        ));
        assert!(
            response.starts_with("HTTP/1.1 200") && response.ends_with("0123456789"),
            "{value}: {response}"
        );
    }
    let empty =
        send("GET /empty.txt HTTP/1.1\r\nHost: x\r\nRange: bytes=0-\r\nConnection: close\r\n\r\n");
    assert!(
        empty.starts_with("HTTP/1.1 200") && empty.contains("content-length: 0"),
        "{empty}"
    );
    // POST always goes to PHP
    assert!(
        send(
            "POST /style.css HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .ends_with("php: /style.css\n")
    );
}

#[test]
fn multipart_edge_cases() {
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("tests/fixtures/scripts/multipart_edge.php")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "{\"f\":\"X\"}\n{\"real\":\"a;name=evil.txt\",\"path\":\"x41.txt\",\"doc\":{\"a\":\"d.txt\"},\"x\":[\"x2\"]}\n\
         Too many parts in the form frugal\nrejected\nrejected\nrejected\nrejected\n\"s=v; Path=\\/; HttpOnly; SameSite=Lax\"\n\"s=v; Path=\\/; Secure; HttpOnly; SameSite=None\"\n"
    );
}

#[test]
fn static_files_with_open_basedir() {
    let allowed = format!(
        "open_basedir={}/tests/fixtures/scripts",
        env!("CARGO_MANIFEST_DIR")
    );
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .args(["-d", &allowed, "tests/fixtures/scripts/static_basedir.php"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "forbidden\n");
}

#[test]
fn client_gone_cancels_handler() {
    let port = 18_117;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--workers", "1", "tests/fixtures/gone.php"])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    get(port, "/status");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .write_all(b"GET /long HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    drop(stream);
    std::thread::sleep(Duration::from_millis(500));
    let status = get(port, "/status");
    assert!(
        status.contains("cancelled after 0.")
            && status.contains("The client closed the connection | - | detach ran"),
        "{status}"
    );
    assert!(get(port, "/cancelled-task").starts_with("HTTP/1.1 500"));
}

#[test]
fn static_files_under_prefixes() {
    let port = 18_118;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/static_prefix.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    get(port, "/start");
    assert!(get(port, "/assets/style.css").ends_with("color: red; }\n"));
    // The longest prefix wins
    assert!(get(port, "/assets/deep/digits.txt").ends_with("\r\n\r\n0123456789"));
    assert!(get(port, "/robots.txt").ends_with("\r\n\r\n0123456789"));
    let directory = get(port, "/assets");
    assert!(
        directory.starts_with("HTTP/1.1 301") && directory.contains("location: /assets/"),
        "{directory}"
    );
    // Outside the prefixes, just next to them or hidden: PHP
    for path in [
        "/style.css",
        "/assetsx/style.css",
        "/robots.txt/x",
        "/assets/.env",
        "/assets/missing.css",
    ] {
        assert!(
            get(port, path).ends_with(&format!("php: {path}\n")),
            "{path}"
        );
    }
}

#[test]
fn async_local_travels_with_tasks() {
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("tests/fixtures/scripts/async_local.php")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("timer a: a\n") && stdout.contains("timer b: b\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("[[\"a\",\"a\",\"a\",null],[\"b\",\"b\",\"b\",null],\"script\"]\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("custom fiber refused, still script\n"),
        "{stdout}"
    );
}

#[test]
fn slow_subscriptions_take_one_message_at_a_time() {
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("tests/fixtures/scripts/subscription_load.php")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(r#"{"most":1,"ordered":true,"dropped":true,"first":0}"#),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn stubs_describe_the_public_api_only() {
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("--stubs")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stubs = String::from_utf8_lossy(&output.stdout);
    assert!(stubs.starts_with("<?php\n"), "{stubs}");
    for expected in [
        "namespace Http {",
        "final class Server",
        "public static function run(callable $work): \\Threading\\Task {}",
        "final class Request",
        "final class AsyncLocal",
        "public function read(): string {}",
        "namespace Lsp {",
        "interface Provider",
        "public function thisClass(string $file): ?string;",
    ] {
        assert!(stubs.contains(expected), "missing {expected}");
    }
    for hidden in [
        "Tenma\\Internal",
        "@internal",
        "class Multipart",
        "function call(string $op",
    ] {
        assert!(!stubs.contains(hidden), "leaks {hidden}");
    }
}

/// A fresh directory for scripts a test changes.
fn scratch(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!("tenma-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// Asks until the response satisfies `expected`; a reload takes a moment, on a busy runner longer.
fn eventually(port: u16, path: &str, expected: impl Fn(&str) -> bool) -> String {
    let started = Instant::now();
    loop {
        let response = get(port, path);
        if expected(&response) || started.elapsed() > Duration::from_secs(10) {
            return response;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Asks until `count` responses in a row satisfy `expected`, and panics after 10 s. Each worker
/// reloads on its own, and connections go to the workers in turn: once a run of them agrees, every
/// worker has caught up.
fn settled(port: u16, path: &str, count: usize, expected: impl Fn(&str) -> bool) {
    let started = Instant::now();
    let mut run = 0;
    loop {
        let response = get(port, path);
        if expected(&response) {
            run += 1;
        } else {
            run = 0;
        }
        if run == count {
            return;
        }
        if started.elapsed() > Duration::from_secs(10) {
            panic!("{run} of {count} in a row after 10 s, the last response: {response}");
        }
        if run == 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn watch_reloads_on_changes() {
    let port = 18_119;
    let directory = scratch("watch");
    let app = directory.join("app.php");
    let lazy = directory.join("lazy.php");
    let script = |version: &str| {
        format!(
            "<?php\n(new \\Http\\Server({port}, function ($request) {{\n    if ($request->path === '/lazy') {{ require_once __DIR__ . '/lazy.php'; return lazy(); }}\n    return \"{version}\\n\";\n}}))->start();\n"
        )
    };
    std::fs::write(&app, script("one")).unwrap();
    std::fs::write(&lazy, "<?php function lazy() { return \"lazy one\\n\"; }").unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--watch", "--workers", "2"])
            .arg(&app)
            .spawn()
            .unwrap(),
    );
    assert!(get(port, "/").ends_with("\r\n\r\none\n"));
    assert!(get(port, "/lazy").ends_with("lazy one\n"));
    // The watcher gets the loaded files from the workers; FSEvents needs a moment for a new directory
    std::thread::sleep(Duration::from_millis(200));

    std::fs::write(&app, script("two")).unwrap();
    let response = eventually(port, "/", |response| response.ends_with("two\n"));
    assert!(response.ends_with("\r\n\r\ntwo\n"), "{response}");
    // Loaded only by a request, after the start: watched as well
    std::fs::write(&lazy, "<?php function lazy() { return \"lazy two\\n\"; }").unwrap();
    let response = eventually(port, "/lazy", |response| response.ends_with("lazy two\n"));
    assert!(response.ends_with("lazy two\n"), "{response}");

    // A syntax error does not end tenma: 503 until the next change
    std::fs::write(&app, "<?php syntax error").unwrap();
    let response = eventually(port, "/", |response| response.starts_with("HTTP/1.1 503"));
    assert!(
        response.starts_with("HTTP/1.1 503") && response.contains("Waiting for changes"),
        "{response}"
    );
    std::fs::write(&app, script("three")).unwrap();
    let response = eventually(port, "/", |response| response.ends_with("three\n"));
    assert!(response.ends_with("\r\n\r\nthree\n"), "{response}");

    // No server for a while, but an interval keeps the script running: 503 from every worker, and
    // every worker serves again after the next change
    std::fs::write(
        &app,
        "<?php\n\\Threading\\Timer::every(1, fn () => null);\n",
    )
    .unwrap();
    // A worker that has not reloaded yet still answers with "three"
    settled(port, "/", 4, |response| {
        response.starts_with("HTTP/1.1 503") && response.contains("serves no requests")
    });
    std::fs::write(&app, script("four")).unwrap();
    settled(port, "/", 4, |response| {
        response.ends_with("\r\n\r\nfour\n")
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn watch_reloads_a_file_saved_while_the_request_that_loaded_it_ran() {
    let port = 18_150;
    let directory = scratch("watch-slow");
    let app = directory.join("app.php");
    let lazy = directory.join("lazy.php");
    std::fs::write(
        &app,
        format!("<?php\n(new \\Http\\Server({port}, function ($request) {{\n    if ($request->path === '/slow') {{ require_once __DIR__ . '/lazy.php'; \\Threading\\Task::delay(0.5); }}\n    return function_exists('lazy') ? lazy() : \"none\\n\";\n}}))->start();\n"),
    )
    .unwrap();
    std::fs::write(&lazy, "<?php function lazy() { return \"lazy one\\n\"; }").unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("--watch")
            .arg(&app)
            .spawn()
            .unwrap(),
    );
    assert!(get(port, "/").ends_with("none\n"));
    std::thread::sleep(Duration::from_millis(200));

    // Saved after the request loaded it, before the request ended and reported it
    let slow = std::thread::spawn(move || get(port, "/slow"));
    std::thread::sleep(Duration::from_millis(200));
    std::fs::write(&lazy, "<?php function lazy() { return \"lazy two\\n\"; }").unwrap();
    assert!(slow.join().unwrap().ends_with("lazy one\n"));
    // The worker still has the old code: it must reload, and the next load gets the new one
    let response = eventually(port, "/slow", |response| response.ends_with("lazy two\n"));
    assert!(response.ends_with("lazy two\n"), "{response}");
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn watch_follows_a_directory_that_was_replaced() {
    let port = 18_151;
    let directory = scratch("watch-replaced");
    let app = directory.join("app.php");
    let lib = directory.join("lib");
    let write_lib = |version: &str| {
        std::fs::write(
            lib.join("a.php"),
            format!("<?php function a() {{ return \"a {version}\\n\"; }}"),
        )
        .unwrap()
    };
    std::fs::write(
        &app,
        format!("<?php\nrequire __DIR__ . '/lib/a.php';\n(new \\Http\\Server({port}, fn () => a()))->start();\n"),
    )
    .unwrap();
    std::fs::create_dir(&lib).unwrap();
    write_lib("one");
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("--watch")
            .arg(&app)
            .spawn()
            .unwrap(),
    );
    assert!(get(port, "/").ends_with("a one\n"));
    std::thread::sleep(Duration::from_millis(200));

    // Deleted and created again, as git checkout or composer install do: inotify loses the watch
    std::fs::remove_dir_all(&lib).unwrap();
    std::fs::create_dir(&lib).unwrap();
    write_lib("two");
    let response = eventually(port, "/", |response| response.ends_with("a two\n"));
    assert!(response.ends_with("a two\n"), "{response}");
    std::thread::sleep(Duration::from_millis(200));
    write_lib("three");
    let response = eventually(port, "/", |response| response.ends_with("a three\n"));
    assert!(response.ends_with("a three\n"), "{response}");
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn watch_reruns_scripts_without_servers() {
    use std::io::BufRead;
    let directory = scratch("watch-script");
    let script = directory.join("script.php");
    std::fs::write(&script, "<?php echo \"run one\\n\";").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("--watch")
        .arg(&script)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let _child = Server(child);
    assert_eq!(lines.next().unwrap().unwrap(), "run one");
    // The script has ended; tenma waits for a change and runs it again
    std::thread::sleep(Duration::from_millis(200));
    // Still running with a pending timer: a change stops it and runs the script again
    std::fs::write(
        &script,
        "<?php echo \"run two\\n\"; \\Threading\\Timer::after(60_000, fn() => null);",
    )
    .unwrap();
    assert_eq!(lines.next().unwrap().unwrap(), "run two");
    std::thread::sleep(Duration::from_millis(200));
    std::fs::write(&script, "<?php echo \"run three\\n\";").unwrap();
    // On a busy machine the watcher may see the half-written file and then the whole one: run two
    // can come twice, but must be followed by run three
    let next = lines
        .find(|line| line.as_deref().map_or(true, |line| line != "run two"))
        .unwrap()
        .unwrap();
    assert_eq!(next, "run three");
    drop(_child);

    // A script that starts serving only now: tenma restarts and listens on its port. Saved by a
    // rename, so no half-written file fails in between (a failed run would hide the bug)
    let port = 18_141;
    std::fs::write(&script, "<?php echo \"no server\\n\";").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("--watch")
        .arg(&script)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let _child = Server(child);
    assert_eq!(lines.next().unwrap().unwrap(), "no server");
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(
        directory.join("next.php"),
        format!("<?php (new \\Http\\Server({port}, fn () => \"served\\n\"))->start();"),
    )
    .unwrap();
    std::fs::rename(directory.join("next.php"), &script).unwrap();
    let started = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "tenma does not listen on the new port"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(get(port, "/").ends_with("served\n"));
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn diagnostics_reach_the_project_under_watch_and_go_with_the_reload_which_the_script_learns_about()
{
    use std::io::BufRead;
    let directory = scratch("diagnostics");
    std::fs::write(directory.join("composer.json"), "{}").unwrap();
    std::fs::create_dir(directory.join("bin")).unwrap();
    let script = directory.join("bin/script.php");
    let list = directory.join(".tenma/diagnostics/app.checks.json");
    let publish = "<?php\n\\Development\\Problems::report('checks', [['file' => __FILE__, 'line' => 2, 'message' => 'run one', 'severity' => 'warning'], ['file' => 'page.tpl.php', 'line' => 1, 'message' => 'relative']]);\necho 'run one' . (\\Development\\Watch::reloadedBy() === null ? '' : ' again') . \"\\n\";";
    std::fs::write(&script, publish).unwrap();
    std::fs::write(
        directory.join("bin/tenma.json"),
        r#"{"hooks": [{"name": "check", "inputs": [], "php": "check.php"}]}"#,
    )
    .unwrap();
    std::fs::write(directory.join("bin/check.php"), "<?php\nreturn fn () => \\Development\\Problems::report('hooks', [['file' => __FILE__, 'line' => 1, 'message' => 'from a hook']]);\n").unwrap();

    // Without --watch nobody shows it: nothing is written
    let plain = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg(&script)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&plain.stdout), "run one\n");
    assert!(!directory.join(".tenma").exists());

    let mut child = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("--watch")
        .arg(&script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut errors = std::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let _child = Server(child);
    assert_eq!(lines.next().unwrap().unwrap(), "run one");
    // In the project's directory, not the script's, for tenma lsp; kept out of the repository
    let published: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&list).unwrap()).unwrap();
    assert_eq!(published[0]["message"], "run one");
    assert_eq!(published[0]["column"], 1);
    // A relative file counts from the working directory
    assert_eq!(
        published[1]["file"],
        format!("{}/page.tpl.php", env!("CARGO_MANIFEST_DIR"))
    );
    assert_eq!(
        std::fs::read_to_string(directory.join(".tenma/.gitignore")).unwrap(),
        "*\n"
    );
    let line = errors
        .find(|line| {
            line.as_deref()
                .map_or(true, |line| line.contains("run one"))
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        line,
        format!(
            "{}:2:1: warning: run one",
            script.canonicalize().unwrap().display()
        )
    );
    // PHP hooks run without a script of their own: their lists go to the same project
    let hooks = directory.join(".tenma/diagnostics/dev.hooks.json");
    for _ in 0..50 {
        if hooks.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        std::fs::read_to_string(&hooks)
            .unwrap()
            .contains("from a hook")
    );
    // Fixed: the reload clears it without the script taking it back
    std::thread::sleep(Duration::from_millis(200));
    // and the script learns what changed
    std::fs::write(
        &script,
        "<?php echo 'run two by ' . basename(\\Development\\Watch::reloadedBy()) . \"\\n\";",
    )
    .unwrap();
    assert_eq!(
        lines
            .find(|line| line.as_deref().map_or(true, |line| line != "run one"))
            .unwrap()
            .unwrap(),
        "run two by script.php"
    );
    assert!(!list.exists());
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn reloaded_by_is_the_change_only_for_the_run_it_caused() {
    let port = 18_170;
    let directory = scratch("reloaded-by");
    let app = directory.join("app.php");
    let script = |version: &str| {
        format!(
            "<?php\n$by = \\Development\\Watch::reloadedBy();\n(new \\Http\\Server({port}, fn () => '{version} ' . ($by === null ? 'none' : basename($by)) . \"\\n\"))->start();\n"
        )
    };
    std::fs::write(&app, script("one")).unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--watch", "--workers", "1", "--max-requests", "20"])
            .arg(&app)
            .spawn()
            .unwrap(),
    );
    assert!(get(port, "/").ends_with("one none\n"));
    std::thread::sleep(Duration::from_millis(200));
    std::fs::write(&app, script("two")).unwrap();
    let response = eventually(port, "/", |response| response.contains("two"));
    assert!(response.ends_with("two app.php\n"), "{response}");
    // --max-requests ran the script again, no change did
    for _ in 0..20 {
        get(port, "/");
    }
    let response = eventually(port, "/", |response| response.contains("two none"));
    assert!(response.ends_with("two none\n"), "{response}");
    let _ = std::fs::remove_dir_all(directory);
}

/// A project with two hooks, listed against their order: a command copies the CSS, PHP minifies
/// the copy and counts its runs.
fn hook_project(name: &str) -> std::path::PathBuf {
    let directory = scratch(name);
    std::fs::create_dir_all(directory.join("assets")).unwrap();
    std::fs::create_dir_all(directory.join("public")).unwrap();
    std::fs::write(directory.join("assets/app.css"), "a { }").unwrap();
    std::fs::write(
        directory.join("copy.sh"),
        "#!/bin/sh\necho \"copying $1\"\ncat \"$1\" > \"$2\"\nprintf '%s' \"$3\" >> \"$2\"\n",
    )
    .unwrap();
    std::fs::set_permissions(
        directory.join("copy.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(directory.join("minify.php"), "<?php\nreturn function (array $changed) {\n    require_once __DIR__ . '/transform.php';\n    file_put_contents(__DIR__ . '/runs.log', 'minify ' . implode(',', array_map('basename', $changed)) . \"\\n\", FILE_APPEND);\n    file_put_contents(__DIR__ . '/public/app.min.css', transform(str_replace(' ', '', file_get_contents(__DIR__ . '/public/app.css'))));\n};\n").unwrap();
    std::fs::write(directory.join("tenma.json"), minify_hooks("")).unwrap();
    // Loaded only when the hook runs
    std::fs::write(
        directory.join("transform.php"),
        "<?php function transform($css) { return $css; }",
    )
    .unwrap();
    directory
}

/// The CSS hook appends `suffix` to its copy.
fn minify_hooks(suffix: &str) -> String {
    serde_json::json!({"hooks": [
        {"name": "minify", "inputs": ["public/app.css"], "outputs": ["public/app.min.css"], "php": "minify.php"},
        {"name": "css", "inputs": ["assets/**/*.css"], "outputs": ["public/app.css"], "run": ["./copy.sh", "assets/app.css", "public/app.css", suffix]},
    ]})
    .to_string()
}

/// `tenma build FILE` with a tenma.json of these hooks.
fn build_hooks(
    directory: &std::path::Path,
    file: &str,
    hooks: serde_json::Value,
) -> std::process::Output {
    std::fs::write(
        directory.join(file),
        serde_json::json!({"hooks": hooks}).to_string(),
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_tenma"))
        .args(["build", file])
        .current_dir(directory)
        .output()
        .unwrap()
}

#[test]
fn build_runs_hooks_in_order_and_fails_on_cycles() {
    let directory = hook_project("build");
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("build")
        .current_dir(&directory)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "[css] copying assets/app.css\n"
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("public/app.min.css")).unwrap(),
        "a{}"
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("runs.log")).unwrap(),
        "minify app.css\n"
    );

    let output = build_hooks(
        &directory,
        "cycle.json",
        serde_json::json!([
            {"name": "a", "inputs": ["b"], "outputs": ["a"], "run": ["true"]},
            {"name": "b", "inputs": ["a"], "outputs": ["b"], "run": ["true"]},
        ]),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("form a cycle"),
        "{output:?}"
    );

    let output = build_hooks(
        &directory,
        "fails.json",
        serde_json::json!([{"inputs": [], "run": ["false"]}]),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("tenma: hook false failed: exited with status 1"),
        "{output:?}"
    );

    // A hook without its command fails before anything runs
    let output = build_hooks(
        &directory,
        "broken.json",
        serde_json::json!([{"inputs": []}]),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("broken.json: hooks[0]: either run (a command) or php (a file)"),
        "{output:?}"
    );

    // Output that is no UTF-8 is printed as well as it goes, and the hook runs on
    let output = build_hooks(
        &directory,
        "latin1.json",
        serde_json::json!([{"name": "latin1", "inputs": [], "run": ["sh", "-c", "printf 'caf\\351\\n'; sleep 0.3; echo after"]}]),
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "[latin1] caf\u{fffd}\n[latin1] after\n"
    );
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn watch_runs_hooks_on_changes() {
    let port = 18_121;
    let directory = hook_project("hooks");
    std::fs::write(directory.join("app.php"), format!("<?php (new \\Http\\Server({port}, fn () => file_get_contents(__DIR__ . '/public/app.min.css')))->start();")).unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--watch", "app.php"])
            .current_dir(&directory)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    // The hooks ran before the first request
    assert!(get(port, "/").ends_with("\r\n\r\na{}"));

    std::fs::write(directory.join("assets/app.css"), "b { }").unwrap();
    let response = eventually(port, "/", |response| response.ends_with("b{}"));
    assert!(response.ends_with("\r\n\r\nb{}"), "{response}");
    // The same content again: the copy does not change, minify does not run
    std::fs::write(directory.join("assets/app.css"), "b { }").unwrap();
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        std::fs::read_to_string(directory.join("runs.log")).unwrap(),
        "minify app.css\nminify app.css\n"
    );

    // A file a PHP hook loaded as it ran reloads the hooks, which run again; so does tenma.json
    std::fs::write(
        directory.join("transform.php"),
        "<?php function transform($css) { return strtoupper($css); }",
    )
    .unwrap();
    let response = eventually(port, "/", |response| response.ends_with("B{}"));
    assert!(response.ends_with("\r\n\r\nB{}"), "{response}");
    std::fs::write(directory.join("tenma.json"), minify_hooks("/*dev*/")).unwrap();
    let response = eventually(port, "/", |response| response.ends_with("B{}/*DEV*/"));
    assert!(response.ends_with("\r\n\r\nB{}/*DEV*/"), "{response}");
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn watch_stops_hooks_that_trigger_themselves() {
    use std::io::BufRead;
    let directory = scratch("brake");
    std::fs::create_dir_all(directory.join("src")).unwrap();
    std::fs::write(directory.join("app.php"), "<?php").unwrap();
    // Writes into its own inputs without declaring it
    let hooks = serde_json::json!({"hooks": [{"name": "loop", "inputs": ["src/*.txt"], "run": ["sh", "-c", "echo $$ > src/self.txt"]}]});
    std::fs::write(directory.join("tenma.json"), hooks.to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .args(["--watch", "app.php"])
        .current_dir(&directory)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stderr.take().unwrap())
        .lines()
        .map_while(Result::ok);
    let _child = Server(child);
    // The script has run, so the hooks have too and their directories are watched
    assert!(lines.any(|line| line.contains("waiting for changes")));
    // FSEvents needs a moment for a new directory
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(directory.join("src/start.txt"), "go").unwrap();
    let stopped = lines.any(|line| line.contains("hook loop keeps triggering itself, stopped"));
    assert!(stopped);
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn watch_keeps_hook_roots_recursive_when_the_app_loads_from_them() {
    use std::io::BufRead;
    let directory = scratch("recursive");
    std::fs::create_dir_all(directory.join("src/sub")).unwrap();
    // The app loads a file from the hook's root: its directory must stay watched recursively
    std::fs::write(directory.join("src/a.php"), "<?php").unwrap();
    std::fs::write(
        directory.join("app.php"),
        "<?php require __DIR__ . '/src/a.php'; echo \"loaded\\n\";",
    )
    .unwrap();
    let hooks = serde_json::json!({"hooks": [{"name": "txt", "inputs": ["src/**/*.txt"], "run": ["sh", "-c", "ls src/sub"]}]});
    std::fs::write(directory.join("tenma.json"), hooks.to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .args(["--watch", "app.php"])
        .current_dir(&directory)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let _child = Server(child);
    let (sender, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .for_each(|line| drop(sender.send(line)))
    });
    let wait_for = |expected: &str| loop {
        match lines.recv_timeout(Duration::from_secs(5)) {
            Ok(line) if line == expected => break,
            Ok(_) => {}
            Err(_) => panic!("no {expected:?}"),
        }
    };
    wait_for("loaded");
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(directory.join("src/sub/deep.txt"), "x").unwrap();
    wait_for("[txt] deep.txt");
    let _ = std::fs::remove_dir_all(directory);
}

/// A minimal DAP client: requests with a Content-Length header, answers in order.
struct Dap {
    stream: TcpStream,
    seq: u64,
    /// Output events that came while waiting for something else.
    output: Vec<serde_json::Value>,
}

impl Dap {
    fn connect(port: u16) -> Dap {
        let started = Instant::now();
        loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    return Dap {
                        stream,
                        seq: 0,
                        output: Vec::new(),
                    };
                }
                Err(_) if started.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(error) => panic!("debugger does not respond: {error}"),
            }
        }
    }

    fn send(&mut self, command: &str, arguments: serde_json::Value) {
        self.seq += 1;
        let body = serde_json::json!({"seq": self.seq, "type": "request", "command": command, "arguments": arguments}).to_string();
        write!(self.stream, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    }

    fn read(&mut self) -> serde_json::Value {
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            self.stream.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        let length: usize = String::from_utf8_lossy(&header)
            .trim()
            .trim_start_matches("Content-Length:")
            .trim()
            .parse()
            .unwrap();
        let mut body = vec![0; length];
        self.stream.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    /// The next message of this kind ("response" or "event") and name, skipping others.
    fn expect(&mut self, kind: &str, name: &str) -> serde_json::Value {
        if (kind, name) == ("event", "output") && !self.output.is_empty() {
            return self.output.remove(0);
        }
        loop {
            let message = self.read();
            let key = if kind == "event" {
                &message["event"]
            } else {
                &message["command"]
            };
            if message["type"] == kind && key == name {
                return message;
            }
            if message["event"] == "output" {
                self.output.push(message);
            }
        }
    }

    fn request(&mut self, command: &str, arguments: serde_json::Value) -> serde_json::Value {
        self.send(command, arguments);
        let response = self.expect("response", command);
        assert_eq!(response["success"], true, "{response}");
        response["body"].clone()
    }

    /// Variables as name, value and the reference to expand them (0: nothing to expand).
    /// The reference of $point among the frame's locals.
    fn stopped_locals(&mut self, frame: &serde_json::Value) -> (String, u64) {
        let locals = self.request("scopes", serde_json::json!({"frameId": frame}))["scopes"][0]["variablesReference"].as_u64().unwrap();
        self.variables(locals)
            .into_iter()
            .find(|(name, _, _)| name == "$point")
            .map(|(_, value, reference)| (value, reference))
            .unwrap()
    }

    fn variables(&mut self, reference: u64) -> Vec<(String, String, u64)> {
        let body = self.request(
            "variables",
            serde_json::json!({"variablesReference": reference}),
        );
        body["variables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|variable| {
                (
                    variable["name"].as_str().unwrap().to_owned(),
                    variable["value"].as_str().unwrap().to_owned(),
                    variable["variablesReference"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    /// Waits for the stop: its event, then the innermost frame's function and line and its locals.
    fn stopped(&mut self) -> Stop {
        let event = self.expect("event", "stopped")["body"].clone();
        let thread = event["threadId"].as_u64().unwrap();
        let frames = self.request("stackTrace", serde_json::json!({"threadId": thread}));
        let top = frames["stackFrames"][0].clone();
        let scopes = self.request("scopes", serde_json::json!({"frameId": top["id"]}));
        let locals = self.variables(scopes["scopes"][0]["variablesReference"].as_u64().unwrap());
        Stop {
            event,
            function: top["name"].as_str().unwrap().to_owned(),
            line: top["line"].as_u64().unwrap(),
            locals,
        }
    }

    fn start(debugger: u16) -> Dap {
        let mut dap = Dap::connect(debugger);
        dap.request("initialize", serde_json::json!({"adapterID": "tenma"}));
        dap.expect("event", "initialized");
        dap
    }

    fn break_at(&mut self, file: &std::path::Path, lines: &[u64]) {
        let path = std::fs::canonicalize(file).unwrap();
        let breakpoints: Vec<_> = lines
            .iter()
            .map(|line| serde_json::json!({"line": line}))
            .collect();
        let set = self.request(
            "setBreakpoints",
            serde_json::json!({"source": {"path": path}, "breakpoints": breakpoints}),
        );
        assert_eq!(set["breakpoints"].as_array().unwrap().len(), lines.len());
    }

    fn command(&mut self, command: &str) {
        self.request(command, serde_json::json!({"threadId": 1}));
    }
}

struct Stop {
    event: serde_json::Value,
    function: String,
    line: u64,
    locals: Vec<(String, String, u64)>,
}

impl Stop {
    fn local(&self, name: &str) -> Option<(String, u64)> {
        self.locals
            .iter()
            .find(|(found, ..)| found == name)
            .map(|(_, value, reference)| (value.clone(), *reference))
    }
}

fn inspected(port: u16, debugger: u16, option: &str) -> Server {
    Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                &format!("{option}={debugger}"),
                "tests/fixtures/inspect.php",
            ])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn inspect_without_an_editor_never_stops() {
    let (port, debugger) = (18_130, 19_236);
    let _server = inspected(port, debugger, "--inspect");
    // An uncaught exception would stop by default: without an editor nobody could resume it
    let started = Instant::now();
    assert!(get(port, "/throw").starts_with("HTTP/1.1 500"));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );

    // A message length nobody could send: the session ends, tenma and the next editor carry on
    for length in ["18446744073709551615", "4611686018427387904"] {
        let mut stream = TcpStream::connect(("127.0.0.1", debugger)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .write_all(format!("Content-Length: {length}\r\n\r\n").as_bytes())
            .unwrap();
        let _ = stream.read_to_end(&mut Vec::new());
        // With a timeout: a worker stopped for a debugger that is gone would never answer
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .write_all(b"GET /throw HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(
            response.starts_with("HTTP/1.1 500"),
            "after Content-Length {length}: {response:?}"
        );
    }
    // A web page can POST to the debugger port; the browser's request line ends the session
    // also with a colon in the path, which makes the request line look like a header
    let message =
        r#"{"seq":1,"type":"request","command":"initialize","arguments":{"adapterID":"tenma"}}"#;
    for path in ["/", "/x:y"] {
        let mut stream = TcpStream::connect(("127.0.0.1", debugger)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream.write_all(format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{debugger}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{message}", message.len()).as_bytes()).unwrap();
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        assert_eq!(answer, "", "POST {path}");
    }
    Dap::start(debugger);
}

#[test]
fn a_debugger_client_that_stops_reading_does_not_stall_the_worker() {
    let (port, debugger) = (18_140, 19_240);
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                &format!("--inspect={debugger}"),
                "tests/fixtures/scripts/loud.php",
            ])
            .env("PORT", port.to_string())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    get(port, "/");
    // Connected, but never reads: echo goes to it as output events until its buffers are full
    let _silent = TcpStream::connect(("127.0.0.1", debugger)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    for i in 0..30 {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(response.ends_with("ok\n"), "request {i}: {response:?}");
    }
}

#[test]
fn inspect_stops_at_breakpoints_and_steps() {
    let (port, debugger) = (18_122, 19_229);
    let _server = inspected(port, debugger, "--inspect");
    let mut dap = Dap::start(debugger);
    dap.break_at("tests/fixtures/inspect.php".as_ref(), &[27]);
    dap.request("configurationDone", serde_json::json!({}));

    let response = std::thread::spawn(move || get(port, "/"));
    let stop = dap.stopped();
    assert_eq!(
        (stop.function.as_str(), stop.line, &stop.event["reason"]),
        (
            "{closure:inspect.php:12}",
            27,
            &serde_json::json!("breakpoint")
        )
    );
    assert_eq!(stop.local("$request").unwrap().0, "Http\\Request");
    assert_eq!(stop.local("$id"), None, "not assigned yet");

    dap.command("next");
    assert_eq!(dap.stopped().local("$id").unwrap().0, "42");
    // Over the constructor, into greet()
    dap.command("next");
    assert_eq!(dap.stopped().line, 29);
    dap.command("stepIn");
    let stop = dap.stopped();
    assert_eq!((stop.function.as_str(), stop.line), ("greet", 4));
    assert_eq!(stop.local("$name").unwrap().0, "'world'");
    // Also in the caller's frame
    let caller = dap.request(
        "stackTrace",
        serde_json::json!({"threadId": stop.event["threadId"]}),
    )["stackFrames"][1]["id"]
        .clone();
    assert_eq!(
        dap.request(
            "evaluate",
            serde_json::json!({"expression": "$id * 2", "frameId": caller})
        )["result"],
        "84"
    );
    // Paged like VS Code asks for long stacks; IDs and references stay below 2^31 as DAP wants
    let page = dap.request(
        "stackTrace",
        serde_json::json!({"threadId": stop.event["threadId"], "startFrame": 1, "levels": 1}),
    );
    assert_eq!(page["stackFrames"].as_array().unwrap().len(), 1);
    assert_eq!(page["stackFrames"][0]["id"], caller);
    assert!(page["totalFrames"].as_u64().unwrap() > 1);
    let scopes = dap.request("scopes", serde_json::json!({"frameId": caller}));
    let locals = scopes["scopes"][0]["variablesReference"].as_u64().unwrap();
    assert!(locals < 1 << 31);
    assert!(
        dap.variables(locals)
            .iter()
            .any(|(name, _, _)| name == "$id")
    );
    dap.command("stepOut");
    assert_eq!(dap.stopped().line, 30);
    // Over the sleep: the fiber waits meanwhile, the step ends in the same fiber
    dap.command("next");
    let stop = dap.stopped();
    assert_eq!(stop.line, 31);
    // Objects and arrays expand, private properties included
    let (point, reference) = stop.local("$point").unwrap();
    assert_eq!(point, "Point");
    let properties = dap.variables(reference);
    assert_eq!(
        properties
            .iter()
            .map(|(name, value, _)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
        [("x", "7"), ("tags", "array(1)")]
    );
    assert_eq!(
        dap.variables(properties[1].2),
        [("a".to_owned(), "1".to_owned(), 0)]
    );
    dap.command("next");
    assert_eq!(dap.stopped().local("$after").unwrap().0, "'slept'");

    dap.command("continue");
    assert!(response.join().unwrap().ends_with("Hello world slept"));

    // Disconnecting while stopped lets the request finish, the breakpoints are gone
    let response = std::thread::spawn(move || get(port, "/"));
    dap.stopped();
    dap.request("disconnect", serde_json::json!({}));
    assert!(response.join().unwrap().ends_with("Hello world slept"));
    assert!(get(port, "/").ends_with("Hello world slept"));
}

#[test]
fn inspect_shows_requests_as_threads_and_stops_at_exceptions() {
    let (port, debugger) = (18_123, 19_230);
    let _server = inspected(port, debugger, "--inspect");
    let mut dap = Dap::start(debugger);
    dap.break_at("tests/fixtures/inspect.php".as_ref(), &[27]);
    dap.request("configurationDone", serde_json::json!({}));

    // One request waits in \\Threading\\Task::delay(), the other stops: both are threads, each with its stack
    let _waiting = std::thread::spawn(move || get(port, "/wait"));
    std::thread::sleep(Duration::from_millis(200));
    let response = std::thread::spawn(move || get(port, "/"));
    let stop = dap.stopped();
    let threads = dap.request("threads", serde_json::json!({}));
    let names: Vec<&str> = threads["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["GET /", "GET /wait (waiting)"]);
    assert_eq!(threads["threads"][0]["id"], stop.event["threadId"]);
    let waiting = dap.request(
        "stackTrace",
        serde_json::json!({"threadId": threads["threads"][1]["id"]}),
    );
    assert_eq!(waiting["stackFrames"][0]["line"], 14);
    dap.command("continue");
    assert!(response.join().unwrap().ends_with("Hello world slept"));

    // Uncaught by default, with the frame still alive; caught ones only with "all"
    let response = std::thread::spawn(move || get(port, "/throw"));
    let stop = dap.stopped();
    assert_eq!(
        (stop.line, &stop.event["reason"], &stop.event["description"]),
        (
            18,
            &serde_json::json!("exception"),
            &serde_json::json!("Paused on uncaught exception")
        )
    );
    assert_eq!(stop.event["text"], "RuntimeException: boom");
    // Evaluating while the exception is pending leaves it pending
    let frame = dap.request(
        "stackTrace",
        serde_json::json!({"threadId": stop.event["threadId"]}),
    )["stackFrames"][0]["id"]
        .clone();
    assert_eq!(
        dap.request(
            "evaluate",
            serde_json::json!({"expression": "$request->path", "frameId": frame})
        )["result"],
        "'/throw'"
    );
    dap.command("continue");
    assert!(response.join().unwrap().starts_with("HTTP/1.1 500"));
    assert!(get(port, "/caught").ends_with("caught"));
    dap.request(
        "setExceptionBreakpoints",
        serde_json::json!({"filters": ["all"]}),
    );
    let response = std::thread::spawn(move || get(port, "/caught"));
    let stop = dap.stopped();
    assert_eq!(
        (stop.line, &stop.event["text"]),
        (22, &serde_json::json!("LogicException: handled"))
    );
    dap.command("continue");
    assert!(response.join().unwrap().ends_with("caught"));
}

#[test]
fn inspect_expands_a_global_after_new_variables_moved_the_symbol_table() {
    let (port, debugger) = (18_152, 19_242);
    // The system allocator scribbles over freed memory: a stale pointer shows instead of going unnoticed
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                &format!("--inspect-brk={debugger}"),
                "tests/fixtures/scripts/inspect_globals.php",
            ])
            .env("PORT", port.to_string())
            .env("USE_ZEND_ALLOC", "0")
            .env("MallocScribble", "1")
            .spawn()
            .unwrap(),
    );
    let mut dap = Dap::start(debugger);
    dap.break_at("tests/fixtures/scripts/inspect_globals.php".as_ref(), &[5]);
    dap.request("configurationDone", serde_json::json!({}));
    dap.stopped();
    dap.command("continue");
    let stop = dap.stopped();
    let (_, config) = stop.local("$config").unwrap();
    assert_eq!(dap.variables(config)[0].0, "database");
    // Enough new globals that the symbol table grows and moves its entries
    let frame = dap.request(
        "stackTrace",
        serde_json::json!({"threadId": stop.event["threadId"]}),
    )["stackFrames"][0]["id"]
        .clone();
    dap.request("evaluate", serde_json::json!({"expression": "for ($i = 0; $i < 500; $i++) { ${'v' . $i} = $i; }", "frameId": frame, "context": "repl"}));
    let children = dap.variables(config);
    assert_eq!(
        children.first().map(|child| child.0.as_str()),
        Some("database"),
        "{children:?}"
    );
    assert_eq!(
        dap.variables(children[0].2)[0],
        ("host".to_owned(), "'localhost'".to_owned(), 0)
    );
    dap.command("continue");
    assert!(get(port, "/").ends_with("ok\n"));
}

#[test]
fn inspect_brk_waits_for_the_debugger_and_stops_at_the_first_statement() {
    let (port, debugger) = (18_124, 19_231);
    let _server = inspected(port, debugger, "--inspect-brk");
    let mut dap = Dap::start(debugger);
    dap.request("configurationDone", serde_json::json!({}));
    let stop = dap.stopped();
    assert_eq!(
        (stop.function.as_str(), stop.line, &stop.event["reason"]),
        ("{main}", 12, &serde_json::json!("entry"))
    );
    dap.command("continue");
    assert!(get(port, "/").ends_with("Hello world slept"));
}

#[test]
fn inspect_wait_waits_for_the_breakpoints_and_stops_at_the_first_statement_only_on_request() {
    let (port, debugger) = (18_160, 19_250);
    // A top-level statement on a line of its own: on line 12 of the fixture, a breakpoint would move
    // into the handler
    let app = scratch("inspect-wait").join("app.php");
    std::fs::write(
        &app,
        format!("<?php\n$started = true;\n(new \\Http\\Server({port}, fn($request) => 'ok'))->start();\n"),
    )
    .unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg(format!("--inspect-wait={debugger}"))
            .arg(&app)
            .spawn()
            .unwrap(),
    );
    let mut dap = Dap::start(debugger);
    // Set before configurationDone, so it applies to the first statement already
    dap.break_at(&app, &[2]);
    dap.request("configurationDone", serde_json::json!({}));
    let stop = dap.stopped();
    assert_eq!(
        (stop.line, &stop.event["reason"]),
        (2, &serde_json::json!("breakpoint"))
    );
    dap.command("continue");
    assert!(get(port, "/").ends_with("ok"));

    let (port, debugger) = (18_161, 19_251);
    let _server = inspected(port, debugger, "--inspect-wait");
    let mut dap = Dap::start(debugger);
    dap.request("attach", serde_json::json!({"stopOnEntry": true}));
    dap.request("configurationDone", serde_json::json!({}));
    assert_eq!(&dap.stopped().event["reason"], &serde_json::json!("entry"));
    dap.command("continue");
    assert!(get(port, "/").ends_with("Hello world slept"));
}

#[test]
fn inspect_brk_runs_on_when_the_editor_says_stop_on_entry_false() {
    let (port, debugger) = (18_162, 19_252);
    let _server = inspected(port, debugger, "--inspect-brk");
    let mut dap = Dap::start(debugger);
    dap.request("attach", serde_json::json!({"stopOnEntry": false}));
    dap.request("configurationDone", serde_json::json!({}));
    assert!(get(port, "/").ends_with("Hello world slept"));
}

#[test]
fn inspect_keeps_the_session_and_breakpoints_across_watch_reloads() {
    let (port, debugger) = (18_125, 19_232);
    let directory = scratch("inspect-watch");
    let app = directory.join("app.php");
    let script = |version: &str| {
        format!(
            "<?php\n(new \\Http\\Server({port}, function ($request) {{\n    $version = '{version}';\n    return \"$version\\n\";\n}}))->start();\n"
        )
    };
    std::fs::write(&app, script("one")).unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--watch", &format!("--inspect={debugger}")])
            .arg(&app)
            .spawn()
            .unwrap(),
    );
    let mut dap = Dap::start(debugger);
    dap.break_at(&app, &[4]);
    dap.request("configurationDone", serde_json::json!({}));
    let response = std::thread::spawn(move || get(port, "/"));
    assert_eq!(dap.stopped().local("$version").unwrap().0, "'one'");
    dap.command("continue");
    assert!(response.join().unwrap().ends_with("one\n"));
    std::thread::sleep(Duration::from_millis(200));

    std::fs::write(&app, script("two")).unwrap();
    let started = Instant::now();
    loop {
        let response = std::thread::spawn(move || get(port, "/"));
        let version = dap.stopped().local("$version").unwrap().0;
        dap.command("continue");
        response.join().unwrap();
        if version == "'two'" {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "no reload");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn inspect_does_not_stop_when_a_reload_ends_waiting_requests() {
    let (port, debugger) = (18_163, 19_253);
    let directory = scratch("inspect-reload-exit");
    let app = directory.join("app.php");
    let script = |version: &str| {
        format!(
            "<?php\n(new \\Http\\Server({port}, function ($request) {{\n    if ($request->path === '/wait') {{\n        \\Threading\\Task::delay(30);\n    }}\n    throw new RuntimeException('{version}');\n}}))->start();\n"
        )
    };
    std::fs::write(&app, script("one")).unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--watch", &format!("--inspect={debugger}")])
            .arg(&app)
            .spawn()
            .unwrap(),
    );
    let mut dap = Dap::start(debugger);
    dap.request("configurationDone", serde_json::json!({}));
    let _waiting = std::thread::spawn(move || {
        request(
            port,
            "GET /wait HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
    });
    std::thread::sleep(Duration::from_millis(300));
    // The reload ends the waiting request's fiber (GracefulExit): Tenma's own business, no stop
    std::fs::write(&app, script("two")).unwrap();
    let started = Instant::now();
    let stop = loop {
        // Not joined: before the fix a stop at the reload kept the response from ever coming
        std::thread::spawn(move || get(port, "/"));
        let event = dap.expect("event", "stopped")["body"].clone();
        dap.command("continue");
        if event["text"] == "RuntimeException: two" {
            break event;
        }
        assert_eq!(event["text"], "RuntimeException: one", "{event}");
        assert!(started.elapsed() < Duration::from_secs(10), "no reload");
    };
    // The class and the message are in the text, which the editor shows at the exception
    assert_eq!(stop["description"], "Paused on uncaught exception");
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn inspect_evaluates_logs_and_changes_variables() {
    let (port, debugger) = (18_126, 19_233);
    let _server = inspected(port, debugger, "--inspect");
    let mut dap = Dap::start(debugger);
    let path = std::fs::canonicalize("tests/fixtures/inspect.php").unwrap();
    let breakpoints = serde_json::json!([
        {"line": 28, "condition": "$id === 1"},
        {"line": 29, "logMessage": "id {$id}, x {$point->x}, {nope(}"},
        {"line": 31, "condition": "$message === 'Hello world'"},
    ]);
    dap.request(
        "setBreakpoints",
        serde_json::json!({"source": {"path": path}, "breakpoints": breakpoints}),
    );
    dap.request("configurationDone", serde_json::json!({}));

    let response = std::thread::spawn(move || get(port, "/"));
    // The false condition does not stop, the logpoint prints instead of stopping
    let logged = dap.expect("event", "output")["body"].clone();
    assert_eq!(
        logged["output"].as_str().unwrap().split(", <").next(),
        Some("id 42, x 7")
    );
    let stop = dap.stopped();
    assert_eq!(stop.line, 31);
    let frame = dap.request(
        "stackTrace",
        serde_json::json!({"threadId": stop.event["threadId"]}),
    )["stackFrames"][0]["id"]
        .clone();

    let evaluate = |dap: &mut Dap, expression: &str, context: &str| {
        dap.send(
            "evaluate",
            serde_json::json!({"expression": expression, "frameId": frame, "context": context}),
        );
        dap.expect("response", "evaluate")
    };
    assert_eq!(
        evaluate(&mut dap, "$id + 1", "watch")["body"]["result"],
        "43"
    );
    assert_eq!(
        evaluate(&mut dap, "strtoupper($message);", "hover")["body"]["result"],
        "'HELLO WORLD'"
    );
    let point = evaluate(&mut dap, "$point", "watch")["body"].clone();
    assert_eq!(
        dap.variables(point["variablesReference"].as_u64().unwrap())[0],
        ("x".to_owned(), "7".to_owned(), 0)
    );
    // Errors come back as failed responses; an exception or a fiber switch does not stop anything
    assert!(
        evaluate(&mut dap, "nope(", "watch")["message"]
            .as_str()
            .unwrap()
            .starts_with("ParseError")
    );
    assert!(
        evaluate(&mut dap, "throw new LogicException('no')", "watch")["message"]
            .as_str()
            .unwrap()
            .starts_with("LogicException: no")
    );
    assert!(
        evaluate(&mut dap, "\\Threading\\Task::delay(0.01)", "watch")["message"]
            .as_str()
            .unwrap()
            .starts_with("FiberError")
    );
    // A warning is the evaluation's error, not output on the console: editors hover names from other files too
    let missing = evaluate(&mut dap, "$point->counters", "hover");
    assert_eq!(
        (&missing["success"], missing["message"].as_str().unwrap()),
        (
            &serde_json::json!(false),
            "Warning: Undefined property: Point::$counters"
        )
    );
    // The debug console runs statements too
    assert_eq!(
        evaluate(&mut dap, "echo 'hi'; $id = 5", "repl")["success"],
        true
    );
    assert_eq!(dap.expect("event", "output")["body"]["output"], "hi");
    assert_eq!(evaluate(&mut dap, "$id", "watch")["body"]["result"], "5");
    // A reference handed out earlier is looked up again: after the variable changed it shows nothing
    let point = dap.stopped_locals(&frame);
    let (_, reference) = point;
    assert_eq!(evaluate(&mut dap, "$point = 5", "repl")["success"], true);
    assert!(dap.variables(reference).is_empty());
    assert_eq!(
        evaluate(&mut dap, "$point = new Point(7)", "repl")["success"],
        true
    );
    // Null bytes in keys (mangled properties of a cast object) do not shift the fields
    let cast = evaluate(&mut dap, "(array) new Point(7)", "watch")["body"]["variablesReference"]
        .as_u64()
        .unwrap();
    let fields: Vec<_> = dap
        .variables(cast)
        .into_iter()
        .map(|(name, value, _)| (name, value))
        .collect();
    assert_eq!(
        fields,
        [
            ("x".to_owned(), "7".to_owned()),
            ("\\0Point\\0tags".to_owned(), "array(1)".to_owned())
        ]
    );

    let locals = dap.request("scopes", serde_json::json!({"frameId": frame}))["scopes"][0]["variablesReference"].clone();
    let set = dap.request(
        "setVariable",
        serde_json::json!({"variablesReference": locals, "name": "$message", "value": "'changed'"}),
    );
    assert_eq!(set["value"], "'changed'");
    dap.command("continue");
    let body = response.join().unwrap();
    assert!(body.ends_with("changed slept"), "{body}");
}

#[test]
fn inspect_moves_breakpoints_to_the_next_statement() {
    let (port, debugger) = (18_127, 19_234);
    let _server = inspected(port, debugger, "--inspect");
    // The script has compiled: the breakpoints are placed right away, not later
    get(port, "/");
    let mut dap = Dap::start(debugger);
    let path = std::fs::canonicalize("tests/fixtures/inspect.php").unwrap();
    // A function's head, its closing brace (PHP's implicit return), a line between two statements,
    // the start of a return over several lines, after the last line
    let breakpoints =
        serde_json::json!([{"line": 3}, {"line": 6}, {"line": 26}, {"line": 36}, {"line": 80}]);
    let set = dap.request(
        "setBreakpoints",
        serde_json::json!({"source": {"path": path}, "breakpoints": breakpoints}),
    );
    let placed: Vec<(bool, u64)> = set["breakpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|breakpoint| {
            (
                breakpoint["verified"].as_bool().unwrap(),
                breakpoint["line"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(placed[..3], [(true, 4), (true, 6), (true, 27)]);
    assert!(
        placed[3].0 && (36..=37).contains(&placed[3].1),
        "{placed:?}"
    );
    assert_eq!(placed[4], (false, 80));
    dap.request("configurationDone", serde_json::json!({}));

    let response = std::thread::spawn(move || get(port, "/"));
    assert_eq!(dap.stopped().line, 27);
    dap.command("continue");
    assert_eq!(dap.stopped().function, "greet");
    // The closing brace stops as greet() returns, after its return statement, with the value
    dap.command("continue");
    let stop = dap.stopped();
    assert_eq!((stop.function.as_str(), stop.line), ("greet", 6));
    assert_eq!(stop.local("return value").unwrap().0, "'Hello world'");
    assert_eq!(stop.local("$text").unwrap().0, "'Hello world'");
    // A step from a return goes on in the caller
    dap.command("next");
    assert_eq!(dap.stopped().line, 30);
    dap.command("continue");
    assert!(response.join().unwrap().ends_with("Hello world slept"));
}

#[test]
fn inspect_hit_counts_exception_classes_debugger_and_setting_children() {
    let (port, debugger) = (18_128, 19_235);
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                &format!("--inspect={debugger}"),
                "tests/fixtures/inspect_more.php",
            ])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    let mut dap = Dap::start(debugger);
    let path = std::fs::canonicalize("tests/fixtures/inspect_more.php").unwrap();
    // Every third pass, from the third on: $i is 3, 6, 9
    let breakpoints = serde_json::json!([{"line": 13, "hitCondition": "% 3"}]);
    dap.request(
        "setBreakpoints",
        serde_json::json!({"source": {"path": path}, "breakpoints": breakpoints}),
    );
    // Only NotFound and its subclasses, though all exceptions are on
    dap.request("setExceptionBreakpoints", serde_json::json!({"filters": [], "filterOptions": [{"filterId": "all", "condition": "\\NotFound"}]}));
    dap.request("configurationDone", serde_json::json!({}));

    let response = std::thread::spawn(move || get(port, "/loop"));
    for expected in ["3", "6", "9"] {
        assert_eq!(dap.stopped().local("$i").unwrap().0, expected);
        dap.command("continue");
    }
    assert!(response.join().unwrap().ends_with("55"));

    let response = std::thread::spawn(move || get(port, "/throw"));
    let stop = dap.stopped();
    assert_eq!(
        (stop.line, &stop.event["text"]),
        (22, &serde_json::json!("NotFound: missing"))
    );
    dap.command("continue");
    assert!(response.join().unwrap().starts_with("HTTP/1.1 500"));

    // Debugger::break() stops in the caller's line; elements and properties can be changed
    let response = std::thread::spawn(move || get(port, "/"));
    let stop = dap.stopped();
    assert_eq!(
        (stop.line, &stop.event["description"]),
        (27, &serde_json::json!("Debugger::break()"))
    );
    let list = dap.variables(stop.local("$list").unwrap().1);
    let set = |dap: &mut Dap, reference: u64, name: &str, value: &str| {
        dap.send(
            "setVariable",
            serde_json::json!({"variablesReference": reference, "name": name, "value": value}),
        );
        dap.expect("response", "setVariable")
    };
    // $shared shares the array: it keeps its value
    assert_eq!(set(&mut dap, list[0].2, "y", "2")["body"]["value"], "2");
    let item = stop.local("$item").unwrap().1;
    assert_eq!(set(&mut dap, item, "count", "5")["body"]["value"], "5");
    assert_eq!(
        set(&mut dap, item, "tags", "['b' => 2]")["body"]["value"],
        "array(1)"
    );
    assert!(
        set(&mut dap, item, "count", "'many'")["message"]
            .as_str()
            .unwrap()
            .starts_with("TypeError")
    );
    assert!(
        set(&mut dap, item, "id", "'y'")["message"]
            .as_str()
            .unwrap()
            .starts_with("Error: Cannot modify readonly property")
    );
    dap.command("continue");
    let body = response.join().unwrap();
    assert!(
        body.ends_with(r#"[{"x":{"y":2}},{"x":{"y":1}},5,{"b":2}]"#),
        "{body}"
    );

    // Without an editor \\Development\\Debugger::break() does nothing
    dap.request("disconnect", serde_json::json!({}));
    assert!(get(port, "/").ends_with(r#"[{"x":{"y":1}},{"x":{"y":1}},1,{"a":1}]"#));
}

#[test]
fn review_fixes() {
    use tokio_tungstenite::tungstenite::{Message, connect};

    let port = 18_129;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/review.php")
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    let body = |response: String| response.rsplit("\r\n\r\n").next().unwrap().to_owned();

    // "//host/admin" stays the path it is; repeated headers join
    let raw = "GET //x/admin?a=1 HTTP/1.1\r\nHost: localhost\r\nCookie: a=1\r\nCookie: b=2\r\nConnection: close\r\n\r\n";
    assert_eq!(
        body(request(port, raw)),
        r#"{"path":"\/\/x\/admin","query":{"a":"1"},"cookie":"a=1; b=2"}"#
    );

    assert!(get(port, "/detach-exit").starts_with("HTTP/1.1 "));
    assert!(
        get(port, "/").starts_with("HTTP/1.1 200"),
        "the server lives on"
    );

    // A lone \r breaks lines in EventSource: no forged fields
    let events = get(port, "/events");
    for part in [
        "data: a\ndata: b\ndata: c\n\n",
        ": x\n: y\n\n",
        "data: rejected\n\n",
    ] {
        assert!(events.contains(part), "{events:?}");
    }

    let (mut socket, _) = connect(format!("ws://127.0.0.1:{port}/ws")).unwrap();
    assert_eq!(socket.read().unwrap(), Message::text("ready"));
    socket.send(Message::text("hi")).unwrap();
    assert_eq!(socket.read().unwrap(), Message::text("got hi"));

    // Classic: the media type ignores case; filter_input() never sees the previous request's fields
    let classic = port + 1;
    let form = "--b\r\nContent-Disposition: form-data; name=\"secret\"\r\n\r\nhunter2\r\n--b--\r\n";
    let raw = format!(
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Type: Multipart/Form-Data; boundary=b\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{form}",
        form.len()
    );
    assert!(
        body(request(classic, &raw))
            .starts_with(r#"{"post":{"secret":"hunter2"},"filter":"hunter2""#)
    );
    assert_eq!(
        body(get(classic, "/")),
        r#"{"post":[],"filter":null,"gc":false}"#
    );
    assert_eq!(body(get(port, "/all-cancelled")), "Threading\\Cancelled");

    // exit() does not leave the cycle collector switched off
    get(classic, "/?exit=1");
    assert_eq!(
        body(get(classic, "/")),
        r#"{"post":[],"filter":null,"gc":false}"#
    );
}

#[test]
fn a_second_sigterm_ends_at_once() {
    let port = 18_131;
    let mut server = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("tests/fixtures/review.php")
        .env("PORT", port.to_string())
        .spawn()
        .unwrap();
    get(port, "/");
    // An endless stream would hold the drain for 10 s
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(stream, "GET /forever HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    stream.read_exact(&mut [0; 64]).unwrap();
    let started = Instant::now();
    for _ in 0..2 {
        Command::new("kill")
            .args(["-TERM", &server.id().to_string()])
            .status()
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }
    server.wait().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn publish_reaches_every_worker() {
    let port = 18_132;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--workers", "3", "tests/fixtures/pubsub.php"])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );

    assert!(get(port, "/publish?message=a").ends_with("published\n"));
    // A subscription made in a request waits there for the next message, from another worker
    let waiting = std::thread::spawn(move || get(port, "/wait"));
    std::thread::sleep(Duration::from_millis(200));
    assert!(get(port, "/publish?message=b").ends_with("published\n"));
    let waited = waiting.join().unwrap();
    assert!(waited.ends_with("waited for b\n"), "{waited}");

    // Round robin: all three workers heard both messages, the cancelled subscription nothing
    for _ in 0..3 {
        let heard = get(port, "/");
        assert!(heard.ends_with("\r\n\r\na,b\n"), "{heard}");
    }
}

#[test]
fn publish_through_redis_reaches_other_processes() {
    let redis = std::env::var("REDIS_URL").unwrap_or("redis://127.0.0.1:6379".into());
    if TcpStream::connect(redis.trim_start_matches("redis://")).is_err() {
        skip("no Redis reachable");
        return;
    }
    let ports = [18_133, 18_134];
    let _servers = ports.map(|port| {
        Server(
            Command::new(env!("CARGO_BIN_EXE_tenma"))
                .args([
                    "--workers",
                    "2",
                    "--pubsub",
                    &redis,
                    "tests/fixtures/pubsub.php",
                ])
                .env("PORT", port.to_string())
                .spawn()
                .unwrap(),
        )
    });
    for port in ports {
        assert!(get(port, "/").starts_with("HTTP/1.1 200"));
    }
    // The bridges subscribe on their own thread once the workers listen
    std::thread::sleep(Duration::from_millis(300));

    assert!(get(ports[0], "/publish?message=a").ends_with("published\n"));
    std::thread::sleep(Duration::from_millis(100));
    for port in ports {
        for _ in 0..2 {
            let heard = get(port, "/");
            assert!(heard.ends_with("\r\n\r\na\n"), "{heard}");
        }
    }

    // A publish that meets a broken connection goes out after the reconnect
    assert!(get(ports[0], "/kill-publisher").ends_with("killed\n"));
    assert!(get(ports[0], "/publish?message=b").ends_with("published\n"));
    std::thread::sleep(Duration::from_millis(1500));
    for port in ports {
        let heard = get(port, "/");
        assert!(heard.ends_with("\r\n\r\na,b\n"), "{heard}");
    }
}

/// A connection over TLS that trusts only the test CA of tests/fixtures/tls (cert.pem and
/// renewed.pem are for localhost and 127.0.0.1, valid until 2126; the CA key is not kept).
fn tls_connect(port: u16) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;

    let mut roots = rustls::RootCertStore::empty();
    let ca = CertificateDer::from_pem_file("tests/fixtures/tls/ca.pem").unwrap();
    roots.add(ca).unwrap();
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let name = "localhost".try_into().unwrap();
    let client = rustls::ClientConnection::new(std::sync::Arc::new(config), name).unwrap();
    let started = Instant::now();
    let socket = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(socket) => break socket,
            Err(_) if started.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => panic!("server does not respond: {error}"),
        }
    };
    rustls::StreamOwned::new(client, socket)
}

/// A GET over TLS; see tls_connect().
fn get_tls(port: u16, path: &str) -> String {
    let mut stream = tls_connect(port);
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();

    let mut response = Vec::new();
    // The server may close without close_notify after Connection: close: what came is the answer
    let _ = stream.read_to_end(&mut response);
    String::from_utf8_lossy(&response).into_owned()
}

/// The certificate the server on `port` shows in the handshake, DER.
fn served_certificate(port: u16) -> Vec<u8> {
    let mut stream = tls_connect(port);
    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(request).unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    stream.conn.peer_certificates().unwrap()[0].to_vec()
}

fn certificate_der(file: &str) -> Vec<u8> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;

    CertificateDer::from_pem_file(file).unwrap().to_vec()
}

#[test]
fn serves_https_with_tls_cert_and_key() {
    let port = 18_171;
    let directory = scratch("tls");
    let app = directory.join("app.php");
    std::fs::write(
        &app,
        format!("<?php\n(new \\Http\\Server({port}, fn ($request) => $request->https ? \"https\\n\" : \"http\\n\"))->start();\n"),
    )
    .unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--tls-cert",
                "tests/fixtures/tls/cert.pem",
                "--tls-key",
                "tests/fixtures/tls/key.pem",
            ])
            .arg(&app)
            .spawn()
            .unwrap(),
    );

    let response = get_tls(port, "/");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("\r\n\r\nhttps\n"), "{response}");

    // Plain HTTP on that port gets no answer, the handshake fails
    let mut plain = TcpStream::connect(("127.0.0.1", port)).unwrap();
    plain
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    plain
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut answer = Vec::new();
    let _ = plain.read_to_end(&mut answer);
    assert!(
        !answer.starts_with(b"HTTP/"),
        "{}",
        String::from_utf8_lossy(&answer)
    );

    // The server still serves TLS after the failed handshake
    assert!(get_tls(port, "/").ends_with("https\n"));
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn classic_mode_sets_https_over_tls() {
    let port = 18_172;
    let directory = scratch("tls-classic");
    let app = directory.join("app.php");
    std::fs::write(
        &app,
        format!("<?php\n(new \\Http\\ClassicServer({port}, function () {{\n    echo ($_SERVER['HTTPS'] ?? 'off') . ' ' . $_SERVER['REQUEST_SCHEME'] . \"\\n\";\n}}))->start();\n"),
    )
    .unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--tls-cert",
                "tests/fixtures/tls/cert.pem",
                "--tls-key",
                "tests/fixtures/tls/key.pem",
            ])
            .arg(&app)
            .spawn()
            .unwrap(),
    );
    let response = get_tls(port, "/");
    assert!(response.ends_with("\r\n\r\non https\n"), "{response}");
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn tls_options_are_checked_at_the_start() {
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(args)
            .arg("tests/fixtures/hello.php")
            .env("PORT", "18173")
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };

    let (code, error) = run(&["--tls-cert", "tests/fixtures/tls/cert.pem"]);
    assert_eq!(code, Some(2), "{error}");
    assert!(
        error.contains("--tls-cert and --tls-key go together"),
        "{error}"
    );

    let (code, error) = run(&["--tls-port", "443"]);
    assert_eq!(code, Some(2), "{error}");
    assert!(
        error.contains("--tls-port needs --tls-cert and --tls-key"),
        "{error}"
    );

    let (code, error) = run(&[
        "--tls-cert",
        "missing.pem",
        "--tls-key",
        "tests/fixtures/tls/key.pem",
    ]);
    assert_eq!(code, Some(2), "{error}");
    assert!(error.contains("--tls-cert missing.pem"), "{error}");

    // A key that does not belong to the certificate
    let (code, error) = run(&[
        "--tls-cert",
        "tests/fixtures/tls/ca.pem",
        "--tls-key",
        "tests/fixtures/tls/key.pem",
    ]);
    assert_eq!(code, Some(2), "{error}");
    assert!(error.contains("do not fit together"), "{error}");
}

#[test]
fn fetch_stops_at_memory_limit() {
    let port = 18_199;
    let source = 18_200;
    // 200 MB, once with Content-Length and once without: neither may be buffered whole
    let listener = std::net::TcpListener::bind(("127.0.0.1", source)).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            std::thread::spawn(move || {
                let mut head = [0; 1024];
                let read = stream.read(&mut head).unwrap();
                let length = if String::from_utf8_lossy(&head[..read]).starts_with("GET /length") {
                    "Content-Length: 209715200\r\n"
                } else {
                    ""
                };
                let head = format!("HTTP/1.1 200 OK\r\n{length}Connection: close\r\n\r\n");
                let _ = stream.write_all(head.as_bytes());
                let chunk = vec![b'x'; 1 << 20];
                for _ in 0..200 {
                    if stream.write_all(&chunk).is_err() {
                        break;
                    }
                }
            });
        }
    });
    let directory = scratch("fetch-limit");
    let app = directory.join("app.php");
    std::fs::write(
        &app,
        format!(
            r#"<?php
(new \Http\Server({port}, function ($request) {{
    try {{
        (new \Http\Client())->get('http://127.0.0.1:{source}' . $request->path);
        return "no error\n";
    }} catch (RuntimeException $error) {{
        return $error->getMessage() . "\n";
    }}
}}))->start();
"#
        ),
    )
    .unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["-d", "memory_limit=64M"])
            .arg(&app)
            .spawn()
            .unwrap(),
    );

    for path in ["/length", "/stream"] {
        let response = get(port, path);
        assert!(
            response.ends_with("the response does not fit under memory_limit\n"),
            "{response}"
        );
    }
}

#[test]
fn fetch_errors_name_their_cause() {
    let port = 18_174;
    let directory = scratch("fetch-errors");
    let app = directory.join("app.php");
    std::fs::write(
        &app,
        format!(
            r#"<?php
(new \Http\Server({port}, function ($request) {{
    if ($request->path === '/slow') {{
        \Threading\Task::delay(2);
        return "late\n";
    }}
    $url = $request->path === '/timeout' ? 'http://127.0.0.1:{port}/slow' : 'http://127.0.0.1:1/';
    try {{
        (new \Http\Client(0.2))->get($url);
        return "no error\n";
    }} catch (RuntimeException $error) {{
        return $error->getMessage() . "\n";
    }}
}}))->start();
"#
        ),
    )
    .unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg(&app)
            .spawn()
            .unwrap(),
    );

    let timeout = get(port, "/timeout");
    assert!(
        timeout.ends_with("/slow: timed out after 0.2 s\n"),
        "{timeout}"
    );
    let refused = get(port, "/refused");
    assert!(
        refused.contains("127.0.0.1:1/: cannot connect: "),
        "{refused}"
    );
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn tls_port_mixes_http_and_https() {
    let (plain, secure) = (18_175, 18_176);
    let directory = scratch("tls-ports");
    let app = directory.join("app.php");
    let handler =
        "fn ($request) => ($request->https ? 'https ' : 'http ') . $request->serverPort . \"\\n\"";
    std::fs::write(
        &app,
        format!("<?php\n(new \\Http\\Server({plain}, {handler}))->start();\n(new \\Http\\Server({secure}, {handler}))->start();\n"),
    )
    .unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--tls-cert",
                "tests/fixtures/tls/cert.pem",
                "--tls-key",
                "tests/fixtures/tls/key.pem",
                "--tls-port",
                &secure.to_string(),
            ])
            .arg(&app)
            .spawn()
            .unwrap(),
    );

    let response = get(plain, "/");
    assert!(
        response.ends_with(&format!("\r\n\r\nhttp {plain}\n")),
        "{response}"
    );
    let response = get_tls(secure, "/");
    assert!(
        response.ends_with(&format!("\r\n\r\nhttps {secure}\n")),
        "{response}"
    );
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn sighup_reloads_the_certificate() {
    let port = 18_177;
    let directory = scratch("tls-reload");
    let (certificate, key) = (directory.join("cert.pem"), directory.join("key.pem"));
    std::fs::copy("tests/fixtures/tls/cert.pem", &certificate).unwrap();
    std::fs::copy("tests/fixtures/tls/key.pem", &key).unwrap();
    let app = directory.join("app.php");
    std::fs::write(
        &app,
        format!("<?php\n(new \\Http\\Server({port}, fn () => \"ok\\n\"))->start();\n"),
    )
    .unwrap();
    let log = directory.join("stderr.log");
    let server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("--tls-cert")
            .arg(&certificate)
            .arg("--tls-key")
            .arg(&key)
            .arg(&app)
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let hangup = || {
        let pid = server.0.id().to_string();
        let status = Command::new("kill").args(["-HUP", &pid]).status().unwrap();
        assert!(status.success());
    };
    let first = certificate_der("tests/fixtures/tls/cert.pem");
    let renewed = certificate_der("tests/fixtures/tls/renewed.pem");
    assert!(served_certificate(port) == first);

    // A renewal: new files, then SIGHUP
    std::fs::copy("tests/fixtures/tls/renewed.pem", &certificate).unwrap();
    std::fs::copy("tests/fixtures/tls/renewed-key.pem", &key).unwrap();
    hangup();
    let started = Instant::now();
    while served_certificate(port) != renewed {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "still the old certificate"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Broken files: the renewed certificate stays, and tenma says so
    std::fs::write(&certificate, "not a certificate").unwrap();
    hangup();
    let started = Instant::now();
    while !std::fs::read_to_string(&log)
        .unwrap()
        .contains("keeping the previous certificate")
    {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no message about the broken file"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(served_certificate(port) == renewed);
    assert!(get_tls(port, "/").ends_with("\r\n\r\nok\n"));
    drop(server);
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn https_speaks_http2_when_the_client_offers_it() {
    let port = 18_178;
    let directory = scratch("http2");
    let app = directory.join("app.php");
    std::fs::write(
        &app,
        format!(
            "<?php\n(new \\Http\\Server({port}, function ($request) {{\n    \\Threading\\Task::delay(0.3);\n    return $request->protocol . ' ' . $request->host() . ' ' . $request->uri . \"\\n\";\n}}))->start();\n"
        ),
    )
    .unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--tls-cert",
                "tests/fixtures/tls/cert.pem",
                "--tls-key",
                "tests/fixtures/tls/key.pem",
            ])
            .arg(&app)
            .spawn()
            .unwrap(),
    );
    // Without h2 in ALPN the client gets HTTP/1.1 as before
    assert!(get_tls(port, "/").ends_with("\r\n\r\nHTTP/1.1 localhost /\n"));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (bodies, elapsed) = runtime.block_on(http2_get_both(port));
    assert_eq!(bodies[0], "HTTP/2 localhost /a?b=1\n");
    assert_eq!(bodies[1], "HTTP/2 localhost /c\n");
    // Two streams on one connection run at the same time: about 0.3 s, not 0.6
    assert!(elapsed < Duration::from_millis(550), "{elapsed:?}");
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn http2_streams_share_the_body_limit() {
    let port = 18_198;
    let directory = scratch("http2-body");
    let app = directory.join("app.php");
    std::fs::write(
        &app,
        format!(
            "<?php\n(new \\Http\\Server({port}, fn ($request) => strlen($request->body) . \"\\n\"))->start();\n"
        ),
    )
    .unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--max-body",
                "1000",
                "--tls-cert",
                "tests/fixtures/tls/cert.pem",
                "--tls-key",
                "tests/fixtures/tls/key.pem",
            ])
            .arg(&app)
            .spawn()
            .unwrap(),
    );
    assert!(get_tls(port, "/").ends_with("\r\n\r\n0\n"));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let statuses = runtime.block_on(async {
        let mut client = http2_connect(port).await;
        let post = || {
            hyper::Request::post(format!("https://localhost:{port}/"))
                .body(())
                .unwrap()
        };
        // The first stream holds 600 bytes and is not done; 600 more on a second stream pass the limit
        let (first, mut first_body) = client.send_request(post(), false).unwrap();
        first_body.send_data(vec![b'a'; 600].into(), false).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (second, mut second_body) = client.send_request(post(), false).unwrap();
        second_body.send_data(vec![b'b'; 600].into(), true).unwrap();
        let second = second.await.unwrap().status();
        first_body.send_data(vec![b'a'; 100].into(), true).unwrap();
        let first = first.await.unwrap().status();
        (first, second)
    });
    assert_eq!(statuses.0, 200);
    assert_eq!(statuses.1, 413);
    let _ = std::fs::remove_dir_all(directory);
}

/// An HTTP/2 connection to the test server, ready for requests.
async fn http2_connect(port: u16) -> h2::client::SendRequest<bytes::Bytes> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;

    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_file("tests/fixtures/tls/ca.pem").unwrap())
        .unwrap();
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];

    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
    let socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let name = "localhost".try_into().unwrap();
    let stream = connector.connect(name, socket).await.unwrap();
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));

    let (client, connection) = h2::client::handshake(stream).await.unwrap();
    tokio::spawn(connection);
    client.ready().await.unwrap()
}

/// Two requests as streams of one HTTP/2 connection, sent together; their bodies and the time both took.
async fn http2_get_both(port: u16) -> (Vec<String>, Duration) {
    let mut client = http2_connect(port).await;

    let started = Instant::now();
    let mut responses = Vec::new();
    for path in ["/a?b=1", "/c"] {
        let request = hyper::Request::get(format!("https://localhost:{port}{path}"))
            .body(())
            .unwrap();
        let (response, _) = client.send_request(request, true).unwrap();
        responses.push(response);
    }
    let mut bodies = Vec::new();
    for response in responses {
        let mut body = response.await.unwrap().into_body();
        let mut text = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.unwrap();
            let _ = body.flow_control().release_capacity(chunk.len());
            text.extend_from_slice(&chunk);
        }
        bodies.push(String::from_utf8(text).unwrap());
    }
    (bodies, started.elapsed())
}

/// A server that answers every line half a second later; a line "never" is never answered.
fn echo_server(port: u16) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = std::io::BufRead::read_line(&mut reader, &mut line);
                if line.trim() == "never" {
                    std::thread::sleep(Duration::from_secs(30));
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
                let _ = stream.write_all(format!("echo:{line}").as_bytes());
            });
        }
    });
}

#[test]
fn php_sockets_suspend_only_the_fiber() {
    let port = 18_180;
    let echo = 18_181;
    echo_server(echo);
    let log = scratch("sockets").join("cancel.log");
    let _ = std::fs::remove_file(&log);
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--tls-cert",
                "tests/fixtures/tls/cert.pem",
                "--tls-key",
                "tests/fixtures/tls/key.pem",
                "tests/fixtures/sockets.php",
            ])
            .env("PORT", port.to_string())
            .env("ECHO_PORT", echo.to_string())
            .env("CANCEL_LOG", &log)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let tail = |path: &str| {
        let response = get_tls(port, path);
        response.split("\r\n\r\n").nth(1).unwrap_or("").to_owned()
    };
    // Three connections at once take as long as one
    // Each wait is half a second: together under one second, never the sum
    timed(&tail("/three"), "echo:a,echo:b,echo:c", 1.0);
    timed(&tail("/select"), "select ok", 1.0);
    let except = tail("/select-except");
    assert!(except.starts_with("except 0 0."), "{except}");
    timed(&tail("/timeout"), "false timed_out=true", 0.4);
    // TLS through ext/openssl, two fetches of the server's own slow page at once
    timed(&tail("/https"), "slowslow", 0.9);
    timed(&tail("/tls"), "tls slow", 0.9);
    assert_eq!(tail("/unverified"), "refused\n");
    // ECONNREFUSED is 61 on macOS and 111 on Linux
    let refused = tail("/refused");
    assert!(
        refused == "refused 61 0s\n" || refused == "refused 111 0s\n",
        "{refused}"
    );
    // The client leaves while the handler waits in fgets(): Cancelled is thrown there
    let mut early = tls_connect(port);
    early
        .write_all(b"GET /cancelled HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    drop(early);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        std::fs::read_to_string(&log).unwrap_or_default(),
        "cancelled in fgets\n"
    );
    server.0.kill().unwrap();
    let mut stdout = String::new();
    server
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert!(stdout.starts_with("echo:startup 0."), "{stdout}");
}

#[test]
fn mysql_through_mysqlnd_is_async() {
    let port = 18_182;
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/mysql.php")
            .env("PORT", port.to_string())
            .env(
                "MYSQL_URL",
                std::env::var("MYSQL_URL").unwrap_or("mysql://root@127.0.0.1:3306/tenma".into()),
            )
            .spawn()
            .unwrap(),
    );
    if !get(port, "/setup").starts_with("HTTP/1.1 200") {
        skip("no MySQL reachable");
        return;
    }
    timed(&body(get(port, "/pdo")), "first,first", 0.9);
    timed(&body(get(port, "/mysqli")), "second,second", 0.9);
    let inserted = get(port, "/insert?name=pie");
    assert!(
        inserted.contains(r#""name": "pie""#) && inserted.contains(r#""amount": "9.99""#),
        "{inserted}"
    );
}

#[test]
fn pdo_pgsql_runs_on_tokio_postgres() {
    let port = 18_183;
    let user = std::env::var("USER").unwrap();
    let log = scratch("pdo").join("cancel.log");
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .arg("tests/fixtures/pdo_pgsql.php")
            .env("PORT", port.to_string())
            .env(
                "DATABASE_URL",
                std::env::var("DATABASE_URL")
                    .unwrap_or(format!("postgres://{user}@localhost/postgres")),
            )
            .env("CANCEL_LOG", &log)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    if !get(port, "/setup").starts_with("HTTP/1.1 200") {
        skip("no Postgres reachable");
        return;
    }
    timed(&body(get(port, "/parallel")), "1,2", 0.9);
    let inserted = get(port, "/insert?name=O'Reilly");
    for expected in [
        r#""name": "O'Reilly""#,
        r#""amount": "9.99""#,
        r#""flag": true"#,
        r#""data": "00016869""#,
        r#""tags": "{a,b}""#,
        r#""same": true"#,
    ] {
        assert!(inserted.contains(expected), "{inserted}");
    }
    assert!(get(port, "/rollback").ends_with("O'Reilly\n"));
    assert!(get(port, "/error").ends_with("42703 column \"nope\" does not exist\n"));
    let placeholders = get(port, "/placeholders");
    assert!(
        placeholders.contains("\"x\",\n        5\n")
            && placeholders.contains(r#""a": "q'1""#)
            && placeholders.contains(r#""b": 7"#)
            && placeholders.contains(r#""int4""#)
            && placeholders.contains(r#""'it''s'""#),
        "{placeholders}"
    );
    assert!(get(port, "/scroll").ends_with("5 3 2 UTF8\n"));
    let modifying = get(port, "/modifying-with");
    assert!(modifying.ends_with("Pie 1\n"), "{modifying}");
    let mut early = TcpStream::connect(("127.0.0.1", port)).unwrap();
    early
        .write_all(b"GET /cancelled HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    drop(early);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        std::fs::read_to_string(&log).unwrap_or_default(),
        "cancelled in query\n"
    );
    server.0.kill().unwrap();
    let mut stdout = String::new();
    server
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert_eq!(stdout, "pgsql 2\n");

    // --pdo-pgsql native keeps libpq: ext/pdo_pgsql reports its client version
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .args([
            "--pdo-pgsql",
            "native",
            "tests/fixtures/scripts/pdo_native.php",
        ])
        .env(
            "DATABASE_URL",
            std::env::var("DATABASE_URL")
                .unwrap_or(format!("postgres://{user}@localhost/postgres")),
        )
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("libpq"),
        "{output:?}"
    );
}

/// Records requests against tests/fixtures/journal.php into `directory` and returns the first journal.
fn record_journal(port: u16, directory: &std::path::Path, extra: &[&str]) -> std::path::PathBuf {
    let user = std::env::var("USER").unwrap();
    let mut args = vec![
        "-d",
        "memory_limit=77M",
        "--record",
        directory.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    args.push("tests/fixtures/journal.php");
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(&args)
            .env("PORT", port.to_string())
            .env("JOURNAL_MARKER", "recorded")
            .env(
                "DATABASE_URL",
                std::env::var("DATABASE_URL")
                    .unwrap_or(format!("postgres://{user}@localhost/postgres")),
            )
            .spawn()
            .unwrap(),
    );
    let first = get(port, "/");
    assert!(first.contains(r#""marker": "recorded""#), "{first}");
    assert!(first.contains(r#""memory": "77M""#), "{first}");
    if get(port, "/data").starts_with("HTTP/1.1 200") {
        assert!(get(port, "/data").contains(r#""n": 42"#));
    }
    assert!(get(port, "/boom").starts_with("HTTP/1.1 500"));
    // On disk while the worker still runs: every response is flushed
    let running: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .flatten()
        .map(|entry| std::fs::read(entry.path()).unwrap())
        .collect();
    assert!(
        running
            .iter()
            .any(|data| String::from_utf8_lossy(data).contains("boom")),
        "the journal is not written yet"
    );
    // Only the owner reads journals: they hold the environment and every request
    for entry in std::fs::read_dir(directory).unwrap().flatten() {
        let mode =
            std::os::unix::fs::PermissionsExt::mode(&entry.metadata().unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600, "{}", entry.path().display());
    }
    // SIGTERM: the worker closes its journal with the list of files
    unsafe { libc::kill(server.0.id() as i32, libc::SIGTERM) };
    let _ = server.0.wait();
    let mut journals: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "trace"))
        .collect();
    journals.sort();
    assert_eq!(journals.len(), 1, "{journals:?}");
    journals.remove(0)
}

fn replay(journal: &std::path::Path, extra: &[&str]) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg("replay")
        .arg(journal)
        .args(extra)
        .env_remove("JOURNAL_MARKER")
        .output()
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn replay_plays_a_recorded_journal_again() {
    let port = 18_184;
    let directory = scratch("journal");
    let journal = record_journal(port, &directory, &[]);

    // Time, random values, the timer, the fetch, the query and the environment repeat exactly
    let (status, log) = replay(&journal, &["--check"]);
    assert_eq!(status, 0, "{log}");
    assert!(
        log.contains("request(s) replayed, 0 response(s) differ"),
        "{log}"
    );
    let (status, log) = replay(&journal, &["--until", "1", "--check"]);
    assert_eq!(status, 0, "{log}");
    assert!(log.contains("stopped after request 1"), "{log}");

    // A handler that now waits longer diverges where it starts the other op
    let changed = directory.join("changed.php");
    std::fs::write(
        &changed,
        std::fs::read_to_string("tests/fixtures/journal.php")
            .unwrap()
            .replace("Task::delay(0.05)", "Task::delay(0.07)"),
    )
    .unwrap();
    let patched = with_script(&std::fs::read(&journal).unwrap(), &changed);
    let diverging = directory.join("changed.trace");
    std::fs::write(&diverging, patched).unwrap();
    let (status, log) = replay(&diverging, &["--check"]);
    assert_eq!(status, 1, "{log}");
    assert!(
        log.contains("diverged: the journal has op")
            && log.contains("sleep 50 ms")
            && log.contains("sleep 70 ms"),
        "{log}"
    );
    assert!(
        log.contains("file(s) differ from the recording") || log.contains("diverged"),
        "{log}"
    );
}

/// A journal whose header names another script.
fn with_script(data: &[u8], script: &std::path::Path) -> Vec<u8> {
    let length = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
    let header_length = u32::from_le_bytes(data[5..9].try_into().unwrap()) as usize;
    let mut header: serde_json::Value =
        serde_json::from_slice(&data[9..9 + header_length]).unwrap();
    header["script"] = serde_json::Value::String(script.to_str().unwrap().into());
    let json = header.to_string();
    let mut patched = vec![1u8];
    patched.extend_from_slice(&((json.len() + 4) as u32).to_le_bytes());
    patched.extend_from_slice(&(json.len() as u32).to_le_bytes());
    patched.extend_from_slice(json.as_bytes());
    patched.extend_from_slice(&data[5 + length..]);
    patched
}

#[test]
fn replay_does_not_wake_objects_from_the_journal() {
    let port = 18_201;
    let directory = scratch("journal-gadget");
    let journal = record_journal(port, &directory, &[]);

    // The script now has a class that acts when unserialized; the journal's uniqid() value becomes one
    let marker = directory.join("woken");
    let script = directory.join("gadget.php");
    std::fs::write(
        &script,
        std::fs::read_to_string("tests/fixtures/journal.php").unwrap()
            + &format!(
                "\nclass Gadget123 {{ public function __wakeup() {{ touch('{}'); }} }}\n",
                marker.display()
            ),
    )
    .unwrap();
    let mut data = with_script(&std::fs::read(&journal).unwrap(), &script);
    let value = data
        .windows(21)
        .position(|window| {
            window.starts_with(b"s:13:\"")
                && window.ends_with(b"\";")
                && window[6..19].iter().all(u8::is_ascii_hexdigit)
        })
        .expect("no uniqid() value in the journal");
    data[value..value + 21].copy_from_slice(b"O:9:\"Gadget123\":0:{};");
    let patched = directory.join("gadget.trace");
    std::fs::write(&patched, data).unwrap();

    let (_, log) = replay(&patched, &[]);
    assert!(!marker.exists(), "{log}");
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn record_on_error_keeps_only_failed_lives() {
    let port = 18_185;
    let directory = scratch("journal-error");
    // The one life answers a 500 (/boom): its journal stays
    let journal = record_journal(port, &directory, &["--record-on", "error"]);
    assert!(journal.exists());

    // A life without any failure leaves nothing behind
    let clean = scratch("journal-clean");
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--record",
                clean.to_str().unwrap(),
                "--record-on",
                "error",
                "tests/fixtures/hello.php",
            ])
            .env("PORT", (port + 1).to_string())
            .spawn()
            .unwrap(),
    );
    assert!(get(port + 1, "/").starts_with("HTTP/1.1 200"));
    unsafe { libc::kill(server.0.id() as i32, libc::SIGTERM) };
    let _ = server.0.wait();
    assert_eq!(std::fs::read_dir(&clean).unwrap().count(), 0);
}

#[test]
fn replay_steps_back_in_the_debugger() {
    let port = 18_186;
    let debugger = 18_187;
    let directory = scratch("journal-back");
    let journal = record_journal(port, &directory, &[]);
    let _replay = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([&format!("--inspect-wait={debugger}"), "replay"])
            .arg(&journal)
            .env_remove("JOURNAL_MARKER")
            .spawn()
            .unwrap(),
    );
    let mut dap = Dap::connect(debugger);
    let capabilities = dap.request("initialize", serde_json::json!({"adapterID": "tenma"}));
    assert_eq!(capabilities["supportsStepBack"], serde_json::json!(true));
    dap.expect("event", "initialized");
    // The first request's handler: Task::delay on line 20, the fetch on line 21
    let script = std::path::Path::new("tests/fixtures/journal.php");
    dap.break_at(script, &[21]);
    dap.request("configurationDone", serde_json::json!({}));
    let stop = dap.stopped();
    assert_eq!(
        (stop.line, &stop.event["reason"]),
        (21, &serde_json::json!("breakpoint"))
    );
    dap.command("next");
    let stop = dap.stopped();
    assert_eq!(
        (stop.line, &stop.event["reason"]),
        (22, &serde_json::json!("step"))
    );
    // Back: the replay starts over and stops at the statement before, line 21 again
    dap.command("stepBack");
    let stop = dap.stopped();
    assert_eq!(
        (stop.line, &stop.event["reason"]),
        (21, &serde_json::json!("step"))
    );
    // And on from there as if nothing happened
    dap.command("next");
    assert_eq!(dap.stopped().line, 22);
    // reverseContinue goes back to the last stop before this one: line 21
    dap.command("reverseContinue");
    assert_eq!(dap.stopped().line, 21);
    dap.command("continue");
}

#[test]
fn durable_functions_survive_a_crash_and_finish_elsewhere() {
    let user = std::env::var("USER").unwrap();
    let url =
        std::env::var("DATABASE_URL").unwrap_or(format!("postgres://{user}@localhost/postgres"));
    if TcpStream::connect("127.0.0.1:5432").is_err() {
        skip("no Postgres reachable");
        return;
    }
    let directory = scratch("durable");
    let marker = directory.join("steps.log");
    let run = |port: u16, who: &str, version: &str| {
        Server(
            Command::new(env!("CARGO_BIN_EXE_tenma"))
                .args(["--durable", &url, "tests/fixtures/durable.php"])
                .env("PORT", port.to_string())
                .env("WHO", who)
                .env("VERSION", version)
                .env("MARKER", &marker)
                .spawn()
                .unwrap(),
        )
    };
    // The tables are shared with earlier runs: keys of this run only, and only their lines
    let suffix = std::process::id().to_string();
    let key = |name: &str| format!("{name}-{suffix}");
    let steps = || {
        let mut current = String::new();
        let mut mine = false;
        for line in std::fs::read_to_string(&marker).unwrap_or_default().lines() {
            if line.starts_with("started ") {
                mine = line.contains(&suffix);
            }
            if mine {
                current.push_str(line);
                current.push('\n');
            }
        }
        current
    };

    let mut first = run(18_188, "A", "1");
    assert_eq!(
        body(get(18_188, &format!("/start?key={}", key("one")))),
        "started\n"
    );
    assert_eq!(
        body(get(18_188, &format!("/start?key={}", key("one")))),
        "not started\n"
    );
    // A signal nobody waits for yet is kept
    assert_eq!(
        body(get(18_188, &format!("/early?key={}", key("early")))),
        "sent\n"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        steps().contains(&format!("started {} by A", key("one"))),
        "{}",
        steps()
    );
    // The process dies with the function waiting for its payment
    first.0.kill().unwrap();
    let _ = first.0.wait();

    let _second = run(18_189, "B", "1");
    assert_eq!(
        body(get(18_189, &format!("/pay?key={}", key("one")))),
        "sent\n"
    );
    assert_eq!(
        body(get(18_189, &format!("/start?key={}", key("early")))),
        "started\n"
    );
    // The poller wakes both within a second, the timer of 1.5 s later
    std::thread::sleep(Duration::from_millis(4500));
    let log = steps();
    for expected in [
        &format!("paid {{\"failed\":false,\"amount\":42}} by B"),
        "paid \"kept\" by B",
        "timer by B",
        &format!("done {}", key("one")),
        &format!("done {}", key("early")),
    ] {
        assert!(log.contains(expected), "missing {expected} in:\n{log}");
    }
    assert_eq!(
        log.matches(&format!("done {}", key("one"))).count(),
        1,
        "{log}"
    );
    // The random value is journaled: it appears once per function, the same whenever the code runs again
    assert_eq!(log.matches("random ").count(), 2, "{log}");

    // A journal of version 1 refuses to run with version 2
    assert_eq!(
        body(get(18_189, &format!("/start?key={}", key("two")))),
        "started\n"
    );
    std::thread::sleep(Duration::from_millis(300));
    let mut third = run(18_190, "C", "2");
    assert_eq!(
        body(get(18_190, &format!("/pay?key={}", key("two")))),
        "sent\n"
    );
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        !steps().contains(&format!("done {}", key("two"))),
        "{}",
        steps()
    );
    third.0.kill().unwrap();
    let _ = third.0.wait();
}

/// Builds the echo plugin of tests/plugins/echo with the system's C compiler, once.
fn echo_plugin() -> std::path::PathBuf {
    static BUILT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = BUILT.lock().unwrap();
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/plugins/echo");
    let library = directory.join("libecho.plugin");
    let source = directory.join("echo.c");
    let header = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tenma-plugin");
    let fresh = library.metadata().and_then(|m| m.modified()).ok()
        >= source.metadata().and_then(|m| m.modified()).ok();
    if !library.exists() || !fresh {
        let status = Command::new("cc")
            .args(["-shared", "-fPIC", "-O1", "-o"])
            .arg(&library)
            .arg("-I")
            .arg(&header)
            .arg(&source)
            .status()
            .expect("cc runs");
        assert!(status.success(), "the echo plugin does not build");
    }
    library
}

#[test]
fn check_reads_tenma_json_and_loads_its_plugins_without_starting() {
    let plugin = echo_plugin();
    let directory = scratch("check");
    let app = directory.join("app");
    std::fs::create_dir_all(&app).unwrap();
    let check = |target: &str| {
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["check", target])
            .current_dir(&directory)
            .output()
            .unwrap()
    };

    let missing = check("app/index.php");
    assert!(missing.status.success(), "{missing:?}");
    assert!(String::from_utf8_lossy(&missing.stdout).contains("no app/tenma.json"));

    let config = format!(
        "{{\"plugins\": [\"{}\"], \"workers\": 2}}",
        plugin.display()
    );
    std::fs::write(app.join("tenma.json"), &config).unwrap();
    let fine = check("app/index.php");
    assert!(fine.status.success(), "{fine:?}");
    assert!(String::from_utf8_lossy(&fine.stderr).contains("plugin echo loaded"));
    assert!(String::from_utf8_lossy(&fine.stdout).contains("app/tenma.json is fine"));

    // Written next to the wrong script: the one in the working directory does not count
    std::fs::write(directory.join("tenma.json"), "{\"wrokers\": 2}").unwrap();
    let elsewhere = check("app/index.php");
    assert!(elsewhere.status.success(), "{elsewhere:?}");
    assert!(
        String::from_utf8_lossy(&elsewhere.stderr)
            .contains("./tenma.json is not the one app/index.php gets")
    );
    let unknown = check("tenma.json");
    assert_eq!(unknown.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown key wrokers"));
}

#[test]
fn native_plugins_shut_down_after_a_script_without_servers() {
    let plugin = echo_plugin();
    let directory = scratch("plugin-script");
    let log = directory.join("shutdown.log");
    let script = directory.join("script.php");
    std::fs::write(&script, "<?php echo \"no server\\n\";").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .args([
            "--plugin",
            plugin.to_str().unwrap(),
            script.to_str().unwrap(),
        ])
        .env("ECHO_SHUTDOWN_FILE", &log)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "no server\n");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "plugin shut down\n");
}

#[test]
fn native_plugins_answer_calls_adopt_sockets_and_shut_down() {
    let port = 18_191;
    let plugin = echo_plugin();
    let directory = scratch("plugin");
    let log = directory.join("shutdown.log");
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--plugin",
                plugin.to_str().unwrap(),
                "--stop-timeout",
                "5",
                "--max-body",
                "1k",
                "tests/fixtures/plugin.php",
            ])
            .env("PORT", port.to_string())
            .env("ECHO_SHUTDOWN_FILE", &log)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    assert_eq!(body(get(port, "/call?text=hello")), "HELLO\n");
    assert_eq!(body(get(port, "/call-error")), "echo: unknown function\n");
    // Both at once: a tenth of a second, a little more on a slow runner, never the two in a row
    let awaited = body(get(port, "/await"));
    assert!(
        awaited.starts_with("later:a,later:b 0.") && awaited < "later:a,later:b 0.3".to_owned(),
        "{awaited}"
    );
    assert_eq!(body(get(port, "/await-error")), "echo: async failure\n");
    assert_eq!(
        body(get(port, "/missing")),
        "no plugin named nope is loaded (tenma --plugin)\n"
    );

    // The plugin takes the WebSocket over: it greets, echoes text and binary from its own side, reports
    // its queue and closes on request; PHP hears about it through a channel
    let (mut socket, _) =
        tokio_tungstenite::tungstenite::connect(format!("ws://127.0.0.1:{port}/ws")).unwrap();
    use tokio_tungstenite::tungstenite::Message;
    assert_eq!(socket.read().unwrap(), Message::text("hello from php"));
    assert_eq!(
        socket.read().unwrap(),
        Message::text("hello from the plugin")
    );
    socket.send(Message::text("ping")).unwrap();
    assert_eq!(socket.read().unwrap(), Message::text("ping"));
    socket.send(Message::binary(vec![0, 1, 2])).unwrap();
    assert_eq!(socket.read().unwrap(), Message::binary(vec![0, 1, 2]));
    socket.send(Message::text("buffered")).unwrap();
    assert!(socket.read().unwrap().to_string().starts_with("buffered "));
    // More than --max-body in all: read by the plugin, so the unread limit never trips
    for _ in 0..4 {
        socket.send(Message::binary(vec![7; 400])).unwrap();
        assert_eq!(socket.read().unwrap(), Message::binary(vec![7; 400]));
    }
    socket.send(Message::text("bye")).unwrap();
    assert!(matches!(socket.read(), Ok(Message::Close(_)) | Err(_)));
    std::thread::sleep(Duration::from_millis(200));
    let events = body(get(port, "/events"));
    assert!(
        events.contains("adopted ") && events.contains("closed "),
        "{events}"
    );

    // SIGTERM: the PHP stop handler runs, then the plugin's shutdown; the subscription that is still
    // open does not hold the stop up until --stop-timeout
    let stopping = Instant::now();
    unsafe { libc::kill(server.0.id() as i32, libc::SIGTERM) };
    let _ = server.0.wait();
    assert!(
        stopping.elapsed() < Duration::from_secs(2),
        "{:?}",
        stopping.elapsed()
    );
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "php stop handler ran\nplugin shut down\n"
    );
    let mut stderr = String::new();
    server
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.contains("plugin echo loaded from"), "{stderr}");
}

#[test]
fn tenma_json_configures_the_run() {
    let port = 18_192;
    echo_plugin();
    // tenma.json next to the script: workers, a plugin by a relative path, -d and the stop timeout;
    // the command line wins for a single value
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["--max-body", "3m", "tests/fixtures/config/app.php"])
            .env("PORT", port.to_string())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    assert_eq!(body(get(port, "/?text=json")), "JSON 96M\n");
    unsafe { libc::kill(server.0.id() as i32, libc::SIGTERM) };
    let _ = server.0.wait();
    let mut stderr = String::new();
    server
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.contains("plugin echo loaded from"), "{stderr}");

    // An unknown key stops tenma with its name
    let bad = scratch("config");
    std::fs::write(bad.join("tenma.json"), r#"{"worker": 2}"#).unwrap();
    std::fs::write(bad.join("app.php"), "<?php echo 'ran';").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .arg(bad.join("app.php"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown key worker"));
}

#[test]
fn one_request_plays_alone_and_journals_are_cut() {
    let port = 18_193;
    let directory = scratch("journal-cut");
    let _ = std::fs::remove_dir_all(&directory);
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([
                "--record",
                directory.to_str().unwrap(),
                "--record-max-size",
                "20k",
                "tests/fixtures/journal.php",
            ])
            .env("PORT", port.to_string())
            .spawn()
            .unwrap(),
    );
    for _ in 0..30 {
        assert!(get(port, "/").starts_with("HTTP/1.1 200"));
    }
    assert!(get(port, "/boom").starts_with("HTTP/1.1 500"));
    assert!(get(port, "/").starts_with("HTTP/1.1 200"));
    unsafe { libc::kill(server.0.id() as i32, libc::SIGTERM) };
    let _ = server.0.wait();

    // Cut at 20 KB: several files, the later ones marked partial; how many depends on the size of the
    // header, which holds the environment. The file with /boom stays in any case
    let mut files: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    files.sort();
    assert!(files.len() >= 2, "{files:?}");
    let list = |file: &std::path::Path| {
        let output = Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args(["replay", file.to_str().unwrap(), "--list"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let (file, listing) = files
        .iter()
        .map(|file| (file, list(file)))
        .find(|(_, listing)| listing.contains("GET /boom"))
        .unwrap_or_else(|| panic!("no file has /boom: {files:?}"));
    let boom = listing
        .lines()
        .find(|line| line.ends_with("GET /boom"))
        .unwrap();
    assert!(boom.contains(" 500 "), "{boom}");
    let number = boom.split_whitespace().next().unwrap();
    let cut = files
        .iter()
        .find(|file| list(file).starts_with("cut from an earlier file"))
        .unwrap_or_else(|| panic!("no file was cut: {files:?}"));

    // A cut file cannot play whole: its start is in the file before
    let (status, log) = replay(cut, &[]);
    assert_eq!(status, 2, "{log}");
    assert!(log.contains("--request N"), "{log}");
    // One request alone: the startup part, then only its own ops and values
    let (status, log) = replay(file, &["--request", number, "--check"]);
    assert_eq!(status, 0, "{log}");
    assert!(
        log.contains("1 request(s) replayed, 0 response(s) differ"),
        "{log}"
    );
    let (status, log) = replay(cut, &["--request", "1", "--check"]);
    assert_eq!(status, 0, "{log}");
    let (status, log) = replay(file, &["--request", "999"]);
    assert_eq!(status, 1, "{log}");
    assert!(log.contains("no request 999"), "{log}");
}

#[test]
fn one_request_steps_back_from_its_exception() {
    let port = 18_194;
    let debugger = 18_195;
    let directory = scratch("journal-alone-back");
    let journal = record_journal(port, &directory, &[]);
    let listing = Command::new(env!("CARGO_BIN_EXE_tenma"))
        .args(["replay", journal.to_str().unwrap(), "--list"])
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
    let boom = listing
        .lines()
        .find(|line| line.ends_with("GET /boom"))
        .unwrap_or_else(|| panic!("{listing}"))
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let _replay = Server(
        Command::new(env!("CARGO_BIN_EXE_tenma"))
            .args([&format!("--inspect-wait={debugger}"), "replay"])
            .arg(&journal)
            .args(["--request", &boom])
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut dap = Dap::start(debugger);
    let script = std::path::Path::new("tests/fixtures/journal.php");
    dap.break_at(script, &[11]);
    dap.request("configurationDone", serde_json::json!({}));
    // /boom alone: the switch, then the throw stops as an exception
    assert_eq!(dap.stopped().line, 11);
    dap.command("next");
    assert_eq!(dap.stopped().line, 15);
    dap.command("next");
    let stop = dap.stopped();
    assert_eq!(
        (stop.line, &stop.event["reason"]),
        (15, &serde_json::json!("exception"))
    );
    // Back from the exception: to the statement that threw, then before it; on the way nothing stops
    dap.command("stepBack");
    let stop = dap.stopped();
    assert_eq!(
        (stop.line, &stop.event["reason"]),
        (15, &serde_json::json!("step"))
    );
    dap.command("stepBack");
    assert_eq!(dap.stopped().line, 11);
    dap.command("continue");
}
