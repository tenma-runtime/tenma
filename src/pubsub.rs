//! Messages between workers: Messaging\Channel::publish() hands a message to every subscription of its channel,
//! in every worker of this process, the publishing one included. A subscription is a channel per
//! worker; PHP fetches the next message with an op and runs its callback in a fiber.
//!
//! With --pubsub redis://… or Channel::connect() from the script, messages go through Redis instead: a
//! thread of its own publishes them there and subscribes to every channel a worker listens on, so every
//! process on that Redis hears them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{
    Receiver as Queue, Sender, UnboundedReceiver, UnboundedSender, unbounded_channel,
};

use crate::ops;
use crate::php::{OpResult, bytes};

type Receiver = Rc<RefCell<Option<Queue<Vec<u8>>>>>;

/// Messages a subscription holds that its callback has not taken yet; beyond them new ones are dropped.
const QUEUED: usize = 1024;

/// Every subscription of the process by channel: its ID and where its messages go.
static CHANNELS: LazyLock<Mutex<HashMap<Vec<u8>, Vec<(u64, Sender<Vec<u8>>)>>>> =
    LazyLock::new(Mutex::default);
/// Unique across workers: unsubscribe finds the subscription in CHANNELS by it.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// --pubsub or Channel::connect(): the Redis URL and what the Redis thread should do.
static REDIS: OnceLock<(String, UnboundedSender<Command>)> = OnceLock::new();
/// Publishes handed to the Redis thread that it has not taken yet. While Redis is down they wait for the
/// reconnect, up to QUEUED of them; beyond that new ones are dropped.
static WAITING: AtomicUsize = AtomicUsize::new(0);

enum Command {
    Publish(Vec<u8>, Vec<u8>),
    Subscribe(Vec<u8>),
    Unsubscribe(Vec<u8>),
}

thread_local! {
    /// This worker's subscriptions: ID → channel and receiver.
    static SUBSCRIPTIONS: RefCell<HashMap<u64, (Vec<u8>, Receiver)>> = RefCell::new(HashMap::new());
}

fn channels() -> MutexGuard<'static, HashMap<Vec<u8>, Vec<(u64, Sender<Vec<u8>>)>>> {
    CHANNELS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Sends a command to the Redis thread, if there is one. Called while CHANNELS is locked, so subscribe
/// and unsubscribe of the same channel reach Redis in the order they happened.
fn tell_redis(command: Command) {
    if let Some((_, redis)) = REDIS.get() {
        let _ = redis.send(command);
    }
}

fn publish(channel: &[u8], message: &[u8]) {
    match REDIS.get() {
        Some((_, redis)) => {
            queue_publish(redis, channel, message);
        }
        None => deliver(channel, message),
    }
}

/// Hands a publish to the Redis thread unless QUEUED are waiting already; false when it is dropped.
fn queue_publish(redis: &UnboundedSender<Command>, channel: &[u8], message: &[u8]) -> bool {
    if WAITING.fetch_add(1, Ordering::Relaxed) >= QUEUED {
        WAITING.fetch_sub(1, Ordering::Relaxed);
        return false;
    }
    let sent = redis.send(Command::Publish(channel.to_vec(), message.to_vec()));
    if sent.is_err() {
        WAITING.fetch_sub(1, Ordering::Relaxed);
    }
    sent.is_ok()
}

/// Hands the message to every subscription of the channel in this process. Also what a native
/// plugin's api->post() does, from any thread.
pub fn deliver(channel: &[u8], message: &[u8]) {
    let mut channels = channels();
    if let Some(subscriptions) = channels.get_mut(channel) {
        // A worker that went away without unsubscribing (restart after a fatal error) closed its
        // receivers; a subscription whose callback lags QUEUED messages behind misses this one
        subscriptions.retain(|(_, sender)| {
            !matches!(
                sender.try_send(message.to_vec()),
                Err(TrySendError::Closed(_))
            )
        });
        if subscriptions.is_empty() {
            channels.remove(channel);
            tell_redis(Command::Unsubscribe(channel.to_vec()));
        }
    }
}

fn subscribe(channel: &[u8]) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = tokio::sync::mpsc::channel(QUEUED);
    {
        let mut channels = channels();
        let subscriptions = channels.entry(channel.to_vec()).or_default();
        if subscriptions.is_empty() {
            tell_redis(Command::Subscribe(channel.to_vec()));
        }
        subscriptions.push((id, sender));
    }
    SUBSCRIPTIONS.with_borrow_mut(|subscriptions| {
        subscriptions.insert(
            id,
            (channel.to_vec(), Rc::new(RefCell::new(Some(receiver)))),
        )
    });
    id
}

fn unsubscribe(id: u64) {
    let Some((channel, _)) =
        SUBSCRIPTIONS.with_borrow_mut(|subscriptions| subscriptions.remove(&id))
    else {
        return;
    };
    let mut channels = channels();
    if let Some(subscriptions) = channels.get_mut(&channel) {
        subscriptions.retain(|(other, _)| *other != id);
        if subscriptions.is_empty() {
            channels.remove(&channel);
            tell_redis(Command::Unsubscribe(channel));
        }
    }
}

/// The next message of a subscription; an op, its result is the message.
fn receive(id: u64) -> u64 {
    let Some(slot) = SUBSCRIPTIONS.with_borrow(|subscriptions| {
        subscriptions
            .get(&id)
            .map(|(_, receiver)| Rc::clone(receiver))
    }) else {
        return 0;
    };
    ops::spawn("pubsub", format!("receive {id}"), async move {
        let Some(receiver) = slot.borrow_mut().take() else {
            return OpResult::Error("this subscription is already receiving".into());
        };
        let mut lent = Lent {
            slot,
            receiver: Some(receiver),
        };
        // None once unsubscribed: the sender is gone
        lent.receiver
            .as_mut()
            .unwrap()
            .recv()
            .await
            .map_or(OpResult::Null, OpResult::Text)
    })
}

/// A receiver taken out for one receive(): goes back when the op ends, also when it is cancelled.
struct Lent {
    slot: Receiver,
    receiver: Option<Queue<Vec<u8>>>,
}

impl Drop for Lent {
    fn drop(&mut self) {
        *self.slot.borrow_mut() = self.receiver.take();
    }
}

/// The worker's interpreter ends: its subscriptions end with it.
pub fn clear() {
    let ids: Vec<u64> =
        SUBSCRIPTIONS.with_borrow(|subscriptions| subscriptions.keys().copied().collect());
    for id in ids {
        unsubscribe(id);
    }
}

/// tenma_pubsub(string $op, int $id, string $channel, string $message): int.
/// "publish": 1. "subscribe": the subscription's ID. "receive": an op for the next message of
/// subscription $id, 0 outside the event loop. "unsubscribe": 1. "connect" with the URL in $channel:
/// 1, 0 for a URL Redis does not take, 2 when messages go through another Redis already.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_pubsub(
    op: *const u8,
    op_len: usize,
    id: u64,
    channel: *const u8,
    channel_len: usize,
    message: *const u8,
    message_len: usize,
) -> u64 {
    let channel = bytes(channel, channel_len);
    match bytes(op, op_len) {
        b"publish" => {
            publish(channel, bytes(message, message_len));
            1
        }
        b"subscribe" => subscribe(channel),
        b"receive" => receive(id),
        b"unsubscribe" => {
            unsubscribe(id);
            1
        }
        b"connect" => match std::str::from_utf8(channel).map(connect) {
            Ok(Ok(())) => 1,
            Ok(Err(Refused::Other)) => 2,
            _ => 0,
        },
        _ => 0,
    }
}

/// --pubsub redis://…: starts the thread that carries messages through Redis. Only before the workers.
pub fn use_redis(url: &str) -> Result<(), String> {
    match connect(url) {
        Ok(()) => Ok(()),
        Err(Refused::Url(error)) => Err(format!("--pubsub {url}: {error}")),
        Err(Refused::Other) => Err("--pubsub given twice".into()),
        Err(Refused::Thread(error)) => Err(format!("--pubsub: {error}")),
    }
}

enum Refused {
    Url(redis::RedisError),
    /// Messages go through another Redis already
    Other,
    Thread(std::io::Error),
}

/// Starts the thread that carries messages through Redis, once per process; the same URL again, as
/// every worker's script asks for it, is fine. Channels subscribed before go to Redis on its first
/// connection.
fn connect(url: &str) -> Result<(), Refused> {
    // Two workers asking at once start one thread
    static STARTING: Mutex<()> = Mutex::new(());
    let _starting = STARTING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((current, _)) = REDIS.get() {
        return if current == url {
            Ok(())
        } else {
            Err(Refused::Other)
        };
    }
    let client = redis::Client::open(url).map_err(Refused::Url)?;
    let (sender, commands) = unbounded_channel();
    std::thread::Builder::new()
        .name("tenma-pubsub".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio does not start");
            runtime.block_on(bridge(client, commands));
        })
        .map_err(Refused::Thread)?;
    let _ = REDIS.set((url.to_owned(), sender));
    Ok(())
}

/// Connects to Redis again and again: a Redis restart only loses what was published while it was down.
async fn bridge(client: redis::Client, mut commands: UnboundedReceiver<Command>) {
    // Reported once per outage, not on every attempt
    let mut down = false;
    // A publish that failed on a broken connection: the next connection sends it first
    let mut unsent = None;
    loop {
        if let Err(error) = connected(&client, &mut commands, &mut down, &mut unsent).await
            && !down
        {
            eprintln!("tenma: pubsub: {error}, reconnecting");
            down = true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn connected(
    client: &redis::Client,
    commands: &mut UnboundedReceiver<Command>,
    down: &mut bool,
    unsent: &mut Option<(Vec<u8>, Vec<u8>)>,
) -> redis::RedisResult<()> {
    let mut publisher = client.get_multiplexed_async_connection().await?;
    let (mut sink, mut stream) = client.get_async_pubsub().await?.split();
    // The channels workers listen on now, also those subscribed while Redis was gone
    let listening: Vec<Vec<u8>> = channels().keys().cloned().collect();
    for channel in listening {
        sink.subscribe(channel).await?;
    }
    if let Some((channel, message)) = unsent.take() {
        publish_to(&mut publisher, channel, message, unsent).await?;
    }
    if std::mem::take(down) {
        eprintln!("tenma: pubsub: connected to Redis again");
    }
    loop {
        tokio::select! {
            message = stream.next() => {
                let Some(message) = message else {
                    return Err(redis::RedisError::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset)));
                };
                if let Ok(channel) = message.get_channel::<Vec<u8>>() {
                    deliver(&channel, message.get_payload_bytes());
                }
            }
            Some(command) = commands.recv() => match command {
                Command::Publish(channel, message) => {
                    WAITING.fetch_sub(1, Ordering::Relaxed);
                    publish_to(&mut publisher, channel, message, unsent).await?;
                }
                Command::Subscribe(channel) => sink.subscribe(channel).await?,
                Command::Unsubscribe(channel) => sink.unsubscribe(channel).await?,
            },
        }
    }
}

/// PUBLISH; if it fails, the message waits in $unsent for the next connection.
// ponytail: at least once: if Redis got it but the answer was lost, it goes out twice
async fn publish_to(
    publisher: &mut redis::aio::MultiplexedConnection,
    channel: Vec<u8>,
    message: Vec<u8>,
    unsent: &mut Option<(Vec<u8>, Vec<u8>)>,
) -> redis::RedisResult<()> {
    let result = redis::cmd("PUBLISH")
        .arg(&channel)
        .arg(&message)
        .exec_async(publisher)
        .await;
    if result.is_err() {
        *unsent = Some((channel, message));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_for_redis_stop_at_queued_until_the_thread_takes_one() {
        let (sender, mut commands) = unbounded_channel();
        for _ in 0..QUEUED {
            assert!(queue_publish(&sender, b"news", b"m"));
        }
        assert!(!queue_publish(&sender, b"news", b"dropped"));

        // What the Redis thread does with each publish it takes
        assert!(matches!(commands.try_recv(), Ok(Command::Publish(..))));
        WAITING.fetch_sub(1, Ordering::Relaxed);
        assert!(queue_publish(&sender, b"news", b"later"));
    }
}
