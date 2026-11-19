//! Streaming responses and WebSockets for PHP. Both belong to a request running in a fiber;
//! Rust holds the channels, PHP talks to them through the C layer.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_int;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use hyper::body::{Body, Frame};
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_rustls::server::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};

use crate::php::{self, OpResult, Reply, bytes};
use crate::plugins::Adopter;
use crate::{WriteTimeout, ops};

/// This many chunks may wait for a slow client, then the fiber waits when writing.
const STREAM_BUFFER: usize = 16;
/// This many messages may wait in either direction. PHP not reading: tenma disconnects the client.
/// The client not reading: send() waits.
const SOCKET_BACKLOG: usize = 256;
/// Largest WebSocket message in bytes, like --max-body.
pub static MAX_MESSAGE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(10 << 20);

/// Body of a streaming response: what PHP writes, as soon as it is there. Ends when the channel
/// closes; unless the writer finished, with an error, so hyper aborts the connection instead of
/// sending the last chunk and the client sees that the response was cut off.
pub struct ChannelBody {
    chunks: mpsc::Receiver<Bytes>,
    finished: Arc<AtomicBool>,
    /// Cut off and polled once more: hyper has flushed what came before, now the error may follow.
    flushed: bool,
    /// The request of a PHP stream: dropped while PHP still writes, it learns that the client left.
    rid: Option<u64>,
}

impl Drop for ChannelBody {
    fn drop(&mut self) {
        if let Some(rid) = self.rid
            && STREAMS
                .with_borrow_mut(|streams| streams.remove(&rid))
                .is_some()
        {
            php::gone(rid);
        }
    }
}

impl ChannelBody {
    /// A body whose end is checked by its Content-Length, as for files.
    pub fn sized(chunks: mpsc::Receiver<Bytes>) -> Self {
        Self {
            chunks,
            finished: Arc::new(AtomicBool::new(true)),
            flushed: false,
            rid: None,
        }
    }
}

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        match self.chunks.poll_recv(cx) {
            Poll::Ready(Some(bytes)) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
            Poll::Ready(None) if self.finished.load(Ordering::Relaxed) => Poll::Ready(None),
            // hyper drops what it has not flushed when the body fails: first a round in which it
            // flushes (it does after every write attempt), then the error.
            // ponytail: if the client's socket is full right then, the rest of the buffer is lost too
            Poll::Ready(None) if !self.flushed => {
                self.flushed = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(None) => {
                Poll::Ready(Some(Err(std::io::Error::other("the stream was cut off"))))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The channels of a WebSocket, as the HTTP part gets them for the upgrade.
pub struct Pipes {
    outgoing: mpsc::Receiver<Message>,
    /// Each message with whether it came as a binary frame.
    incoming: mpsc::Sender<(Vec<u8>, bool)>,
    /// Bytes of the incoming messages PHP has not read yet.
    unread: Rc<Cell<usize>>,
    /// Bytes of outgoing messages queued and not yet written, for a plugin's back pressure.
    queued: Arc<AtomicUsize>,
    /// The plugin that adopts the socket, if one ever does.
    adopter: oneshot::Receiver<Adopter>,
    gone: Gone,
}

/// Dropped when the connection ends: a handler waiting for something other than receive() learns
/// that the client is gone, as with streams. One in receive() gets null instead and ends itself.
struct Gone(u64);

impl Drop for Gone {
    fn drop(&mut self) {
        let receiving = SOCKETS.with_borrow(|sockets| {
            sockets
                .get(&self.0)
                .map(|socket| socket.incoming.borrow().is_none())
        });
        if receiving == Some(false) {
            SOCKETS.with_borrow_mut(|sockets| sockets.remove(&self.0));
            php::gone(self.0);
        }
    }
}

/// The PHP side of a WebSocket.
struct Socket {
    outgoing: mpsc::Sender<Message>,
    /// Held by the op of a running receive(), otherwise here.
    incoming: Rc<RefCell<Option<mpsc::Receiver<(Vec<u8>, bool)>>>>,
    unread: Rc<Cell<usize>>,
    queued: Arc<AtomicUsize>,
    /// Tells the socket's task which plugin adopted it.
    adopter: oneshot::Sender<Adopter>,
}

/// What a plugin gets of a WebSocket it adopts.
pub struct Taken {
    /// The writing end, and the bytes queued on it.
    pub outgoing: mpsc::Sender<Message>,
    pub queued: Arc<AtomicUsize>,
    /// The messages that came while the socket was PHP's.
    pub unread: Vec<(Vec<u8>, bool)>,
    /// Hands the reading over: from then on the socket's task tells the plugin every message itself.
    pub adopter: oneshot::Sender<Adopter>,
}

/// A plugin adopts the WebSocket of request `rid`: PHP loses it. None if the request has no socket,
/// or receive() is running on it.
pub fn take_socket(rid: u64) -> Option<Taken> {
    let socket = SOCKETS.with_borrow_mut(|sockets| sockets.remove(&rid))?;
    let receiver = socket.incoming.borrow_mut().take();
    match receiver {
        Some(mut receiver) => {
            let mut unread = Vec::new();
            while let Ok(message) = receiver.try_recv() {
                unread.push(message);
            }
            Some(Taken {
                outgoing: socket.outgoing,
                queued: socket.queued,
                unread,
                adopter: socket.adopter,
            })
        }
        None => {
            SOCKETS.with_borrow_mut(|sockets| sockets.insert(rid, socket));
            None
        }
    }
}

thread_local! {
    /// A stream's channel, and whether its writer finished.
    static STREAMS: RefCell<HashMap<u64, (mpsc::Sender<Bytes>, Arc<AtomicBool>)>> = RefCell::new(HashMap::new());
    static SOCKETS: RefCell<HashMap<u64, Socket>> = RefCell::new(HashMap::new());
}

/// Forgets all streams and sockets of the interpreter when it restarts after a fatal error: what they
/// sent so far is all, so their clients see them cut off.
pub fn clear() {
    STREAMS.with_borrow_mut(HashMap::clear);
    SOCKETS.with_borrow_mut(|sockets| sockets.drain().for_each(|(_, socket)| fail(&socket)));
}

/// Closes a WebSocket with 1011: the server broke off.
fn fail(socket: &Socket) {
    let _ = socket.outgoing.try_send(Message::Close(Some(CloseFrame {
        code: CloseCode::Error,
        reason: "Internal error".into(),
    })));
}

/// Sends the head of the response; the body follows with tenma_rs_stream_write. 0 if the request is not waiting.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_stream_start(
    rid: u64,
    status: u16,
    headers: *const u8,
    headers_len: usize,
) -> c_int {
    let (sender, chunks) = mpsc::channel(STREAM_BUFFER);
    let finished = Arc::new(AtomicBool::new(false));
    let headers = php::parse_header_lines(bytes(headers, headers_len));
    if !php::reply(
        rid,
        Reply::Stream {
            status,
            headers,
            body: ChannelBody {
                chunks,
                finished: Arc::clone(&finished),
                flushed: false,
                rid: Some(rid),
            },
        },
    ) {
        return 0;
    }
    STREAMS.with_borrow_mut(|streams| streams.insert(rid, (sender, finished)));
    1
}

/// 0: written. Above 0: buffer full, an op the fiber must wait on. -1: client is gone.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_stream_write(rid: u64, chunk: *const u8, len: usize) -> i64 {
    let Some(sender) =
        STREAMS.with_borrow(|streams| streams.get(&rid).map(|(sender, _)| sender.clone()))
    else {
        return -1;
    };
    match sender.try_send(Bytes::copy_from_slice(bytes(chunk, len))) {
        Ok(()) => 0,
        Err(mpsc::error::TrySendError::Full(chunk)) => {
            match ops::spawn("stream.write", rid.to_string(), async move {
                match sender.send(chunk).await {
                    Ok(()) => OpResult::Null,
                    Err(_) => OpResult::Error("connection closed".into()),
                }
            }) {
                0 => -1,
                op => op as i64,
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            STREAMS.with_borrow_mut(|streams| streams.remove(&rid));
            -1
        }
    }
}

/// $finished: the writer returned; otherwise (an error, exit()) the client sees the stream cut off.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_stream_end(rid: u64, finished: c_int) {
    if let Some((_, flag)) = STREAMS.with_borrow_mut(|streams| streams.remove(&rid)) {
        flag.store(finished != 0, Ordering::Relaxed);
    }
}

/// Accepts the WebSocket: the request responds with 101, after that the messages belong to PHP.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_ws_accept(rid: u64) -> c_int {
    let (outgoing, outgoing_receiver) = mpsc::channel(SOCKET_BACKLOG);
    let (incoming_sender, incoming) = mpsc::channel(SOCKET_BACKLOG);
    let unread = Rc::new(Cell::new(0));
    let queued = Arc::new(AtomicUsize::new(0));
    let (adopter, adopter_receiver) = oneshot::channel();
    if !php::reply(
        rid,
        Reply::Upgrade(Pipes {
            outgoing: outgoing_receiver,
            incoming: incoming_sender,
            unread: Rc::clone(&unread),
            queued: Arc::clone(&queued),
            adopter: adopter_receiver,
            gone: Gone(rid),
        }),
    ) {
        return 0;
    }
    let socket = Socket {
        outgoing,
        incoming: Rc::new(RefCell::new(Some(incoming))),
        unread,
        queued,
        adopter,
    };
    SOCKETS.with_borrow_mut(|sockets| sockets.insert(rid, socket));
    1
}

/// 0: on its way. Above 0: SOCKET_BACKLOG messages wait already, an op the fiber must wait on, as
/// for streams. -1: socket closed. A client that stops reading stalls the write to it and is
/// disconnected after --request-timeout (WriteTimeout), which ends the op too.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_ws_send(rid: u64, text: *const u8, len: usize) -> i64 {
    let Some((sender, queued)) = SOCKETS.with_borrow(|sockets| {
        sockets
            .get(&rid)
            .map(|socket| (socket.outgoing.clone(), Arc::clone(&socket.queued)))
    }) else {
        return -1;
    };
    // Text if it is UTF-8, otherwise a binary frame: bytes arrive as they were sent
    let data = bytes(text, len).to_vec();
    let message = match String::from_utf8(data) {
        Ok(text) => Message::text(text),
        Err(error) => Message::binary(error.into_bytes()),
    };
    queued.fetch_add(len, Ordering::Relaxed);
    match sender.try_send(message) {
        Ok(()) => 0,
        Err(mpsc::error::TrySendError::Full(message)) => {
            match ops::spawn("ws.send", rid.to_string(), async move {
                match sender.send(message).await {
                    Ok(()) => OpResult::Null,
                    Err(_) => OpResult::Error("connection closed".into()),
                }
            }) {
                0 => -1,
                op => op as i64,
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            SOCKETS.with_borrow_mut(|sockets| sockets.remove(&rid));
            -1
        }
    }
}

/// Op for the next message: a string, or null if the client has closed. 0: no socket.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_ws_receive(rid: u64) -> u64 {
    let Some((incoming, unread)) = SOCKETS.with_borrow(|sockets| {
        sockets
            .get(&rid)
            .map(|socket| (Rc::clone(&socket.incoming), Rc::clone(&socket.unread)))
    }) else {
        return 0;
    };
    ops::spawn("ws.receive", rid.to_string(), async move {
        let Some(receiver) = incoming.borrow_mut().take() else {
            return OpResult::Error("receive() is already running on this WebSocket".into());
        };
        let mut lent = Lent {
            slot: incoming,
            receiver: Some(receiver),
        };
        let message = lent.receiver.as_mut().unwrap().recv().await;
        message.map_or(OpResult::Null, |(message, _)| {
            unread.set(unread.get() - message.len());
            OpResult::Text(message)
        })
    })
}

/// A receiver taken out for one receive(): goes back when the op ends, also when it is cancelled.
struct Lent {
    slot: Rc<RefCell<Option<mpsc::Receiver<(Vec<u8>, bool)>>>>,
    receiver: Option<mpsc::Receiver<(Vec<u8>, bool)>>,
}

impl Drop for Lent {
    fn drop(&mut self) {
        *self.slot.borrow_mut() = self.receiver.take();
    }
}

/// $finished: a normal close; otherwise (an error in the handler, exit()) the client gets 1011.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_ws_close(rid: u64, finished: c_int) {
    if let Some(socket) = SOCKETS.with_borrow_mut(|sockets| sockets.remove(&rid))
        && finished == 0
    {
        fail(&socket);
    }
}

/// The connection under a WebSocket. A plain or TLS connection can move to the I/O thread; one of a
/// kind this code does not know stays on the loop of its worker.
enum Carrier {
    Plain(WriteTimeout<TcpStream>),
    Tls(Box<WriteTimeout<TlsStream<TcpStream>>>),
    Other(TokioIo<Upgraded>),
}

impl Carrier {
    /// The connection hyper gave up, and what hyper had read of it beyond the request.
    fn of(upgraded: Upgraded) -> (Self, Bytes) {
        let upgraded = match upgraded.downcast::<TokioIo<WriteTimeout<TcpStream>>>() {
            Ok(parts) => return (Self::Plain(parts.io.into_inner()), parts.read_buf),
            Err(upgraded) => upgraded,
        };
        match upgraded.downcast::<TokioIo<WriteTimeout<TlsStream<TcpStream>>>>() {
            Ok(parts) => (Self::Tls(Box::new(parts.io.into_inner())), parts.read_buf),
            Err(upgraded) => (Self::Other(TokioIo::new(upgraded)), Bytes::new()),
        }
    }

    /// Moves the connection to another loop: from then on that loop hears when the client sent
    /// something or takes more, whatever the loop before is busy with. False: it stays where it is.
    fn move_to(&mut self, runtime: &tokio::runtime::Handle) -> bool {
        let stream = match self {
            Self::Plain(stream) => stream.moved(),
            Self::Tls(stream) => stream.moved().get_mut().0,
            Self::Other(_) => return false,
        };
        // A second descriptor of the same connection, known to the other loop; the first one goes
        let Ok(copy) = std::os::fd::AsFd::as_fd(stream).try_clone_to_owned() else {
            return false;
        };
        let _there = runtime.enter();
        match TcpStream::from_std(copy.into()) {
            Ok(moved) => {
                *stream = moved;
                true
            }
            Err(_) => false,
        }
    }
}

impl AsyncRead for Carrier {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(context, buffer),
            Self::Tls(stream) => Pin::new(&mut **stream).poll_read(context, buffer),
            Self::Other(stream) => Pin::new(stream).poll_read(context, buffer),
        }
    }
}

impl AsyncWrite for Carrier {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(context, data),
            Self::Tls(stream) => Pin::new(&mut **stream).poll_write(context, data),
            Self::Other(stream) => Pin::new(stream).poll_write(context, data),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(context),
            Self::Tls(stream) => Pin::new(&mut **stream).poll_flush(context),
            Self::Other(stream) => Pin::new(stream).poll_flush(context),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(context),
            Self::Tls(stream) => Pin::new(&mut **stream).poll_shutdown(context),
            Self::Other(stream) => Pin::new(stream).poll_shutdown(context),
        }
    }
}

/// The I/O thread: a loop of its own for the WebSockets plugins adopted, so a worker busy in PHP
/// holds up neither their reads nor their writes, and a worker that restarts does not end them.
struct IoThread {
    runtime: tokio::runtime::Handle,
    stop: Arc<Notify>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

static IO_THREAD: OnceLock<IoThread> = OnceLock::new();

/// The loop of the I/O thread, started with the first adopted socket.
fn io_thread() -> &'static tokio::runtime::Handle {
    let started = IO_THREAD.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio does not start");
        let handle = runtime.handle().clone();
        let stop = Arc::new(Notify::new());
        let stopped = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("tenma-io".into())
            .spawn(move || runtime.block_on(stopped.notified()))
            .expect("the I/O thread does not start");
        IoThread {
            runtime: handle,
            stop,
            thread: Mutex::new(Some(thread)),
        }
    });
    &started.runtime
}

/// tenma stops: the adopted sockets on the I/O thread end, and each plugin has heard closed() for
/// its connections when this returns.
pub fn stop_io_thread() {
    let Some(started) = IO_THREAD.get() else {
        return;
    };
    started.stop.notify_one();
    if let Some(thread) = started.thread.lock().unwrap().take() {
        let _ = thread.join();
    }
}

/// After the 101 response: pass messages between client and PHP until one side closes, or a plugin
/// adopts the socket and its messages go there.
pub async fn run_websocket(upgrade: OnUpgrade, pipes: Pipes) {
    let Pipes {
        mut outgoing,
        incoming,
        unread,
        queued,
        adopter,
        gone,
    } = pipes;
    let Ok(upgraded) = upgrade.await else {
        return;
    };
    let limit = MAX_MESSAGE.load(std::sync::atomic::Ordering::Relaxed);
    let config = WebSocketConfig::default()
        .max_message_size(Some(limit))
        .max_frame_size(Some(limit));
    let (carrier, read) = Carrier::of(upgraded);
    let mut socket =
        WebSocketStream::from_partially_read(carrier, read.into(), Role::Server, Some(config))
            .await;
    // Without a plugin the sender goes with PHP's side of the socket; that is not asked twice
    let mut adopter = futures_util::FutureExt::fuse(adopter);
    let plugin = loop {
        tokio::select! {
            // First: once a plugin adopted the socket nobody reads what would still go to PHP
            biased;
            adopter = &mut adopter => {
                if let Ok(plugin) = adopter {
                    break plugin;
                }
            }
            message = socket.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let message = (text.as_bytes().to_vec(), false);
                    if !accept_message(&incoming, &unread, limit, message, &mut socket).await {
                        return;
                    }
                }
                Some(Ok(Message::Binary(data))) => {
                    let message = (data.to_vec(), true);
                    if !accept_message(&incoming, &unread, limit, message, &mut socket).await {
                        return;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return,
            },
            message = outgoing.recv() => {
                if !write_message(&mut socket, &queued, message).await {
                    return;
                }
            }
        }
    };

    // PHP's side of the socket is gone already; what is left of it ends here, on its worker
    drop((incoming, unread, gone));
    let runtime = io_thread();
    let moved = socket.get_mut().move_to(runtime);
    let serving = serve_plugin(socket, outgoing, queued, plugin);
    if moved {
        runtime.spawn(serving);
    } else {
        serving.await;
    }
}

/// An adopted socket: its messages go straight to the plugin, what the plugin queued goes out.
async fn serve_plugin(
    mut socket: WebSocketStream<Carrier>,
    mut outgoing: mpsc::Receiver<Message>,
    queued: Arc<AtomicUsize>,
    plugin: Adopter,
) {
    loop {
        tokio::select! {
            message = socket.next() => match message {
                Some(Ok(Message::Text(text))) => plugin.message(text.as_bytes(), false),
                Some(Ok(Message::Binary(data))) => plugin.message(&data, true),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            message = outgoing.recv() => {
                if !write_message(&mut socket, &queued, message).await {
                    break;
                }
            }
        }
    }
}

/// Writes a queued message to the client, or closes once nothing more can come. False means: end
/// the connection.
async fn write_message(
    socket: &mut WebSocketStream<Carrier>,
    queued: &AtomicUsize,
    message: Option<Message>,
) -> bool {
    let Some(message) = message else {
        let _ = socket.close(None).await;
        return false;
    };
    let size = message.len();
    if socket.send(message).await.is_err() {
        return false;
    }
    queued.fetch_sub(size.min(queued.load(Ordering::Relaxed)), Ordering::Relaxed);
    true
}

/// Passes a message on to PHP. If PHP does not keep reading, the backlog does not grow without end:
/// at SOCKET_BACKLOG unread messages or more than $limit unread bytes (one message is at most that)
/// tenma disconnects the client. False means: end the connection.
async fn accept_message(
    incoming: &mpsc::Sender<(Vec<u8>, bool)>,
    unread: &Cell<usize>,
    limit: usize,
    message: (Vec<u8>, bool),
    sink: &mut (impl SinkExt<Message> + Unpin),
) -> bool {
    let size = message.0.len();
    let sent = if !incoming.is_closed() && unread.get() + size > limit {
        Err(mpsc::error::TrySendError::Full(message))
    } else {
        incoming.try_send(message)
    };
    match sent {
        Ok(()) => {
            unread.set(unread.get() + size);
            true
        }
        // PHP no longer reads messages (handler done or only sending): dropping them is right here
        Err(mpsc::error::TrySendError::Closed(_)) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            let reason = "Too many unread messages";
            let _ = sink
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: reason.into(),
                })))
                .await;
            false
        }
    }
}
