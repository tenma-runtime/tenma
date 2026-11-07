//! Waiting for sockets PHP owns. PHP's stream transports (tcp, unix, ssl, tls) keep their file
//! descriptors and their OpenSSL state; the C layer only puts the descriptor into non-blocking mode and,
//! wherever PHP would poll(), starts one of these ops instead: the fiber waits until the descriptor is
//! readable or writable, and other requests of the worker run meanwhile. This is what makes mysqlnd,
//! fsockopen() and the http:// wrapper asynchronous under tenma.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::rc::{Rc, Weak};
use std::time::Duration;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::task::JoinSet;

use crate::ops;
use crate::php::OpResult;

/// A descriptor PHP owns: registering it with tokio's reactor does not close it.
struct Borrowed(RawFd);

impl AsRawFd for Borrowed {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// POLLIN and POLLPRI as readable, POLLOUT as writable, both bits as both.
fn interest(events: i32) -> Interest {
    let readable = events & (libc::POLLIN | libc::POLLPRI) as i32 != 0;
    let writable = events & libc::POLLOUT as i32 != 0;
    match (readable, writable) {
        (true, true) => Interest::READABLE | Interest::WRITABLE,
        (_, true) => Interest::WRITABLE,
        _ => Interest::READABLE,
    }
}

thread_local! {
    /// One registration per descriptor and worker: epoll and kqueue take a descriptor only once, and
    /// dropping a second registration would delete the first one's filters. The last waiter drops it.
    static WATCHED: RefCell<HashMap<RawFd, Weak<AsyncFd<Borrowed>>>> = RefCell::default();
}

/// The worker's registration of `fd`, shared by all waiters; None where the descriptor cannot be
/// watched (regular files, /dev/null): those count as ready, as poll() reports them.
fn watch(fd: RawFd) -> Option<Rc<AsyncFd<Borrowed>>> {
    WATCHED.with_borrow_mut(|watched| {
        if let Some(shared) = watched.get(&fd).and_then(Weak::upgrade) {
            return Some(shared);
        }
        let interest = Interest::READABLE | Interest::WRITABLE;
        let shared = Rc::new(AsyncFd::with_interest(Borrowed(fd), interest).ok()?);
        watched.retain(|_, waiter| waiter.strong_count() > 0);
        watched.insert(fd, Rc::downgrade(&shared));
        Some(shared)
    })
}

/// What poll() reports for `fd` right now, as 1 (readable) plus 2 (writable).
fn poll_now(fd: RawFd, events: i32) -> u8 {
    let mut entry = libc::pollfd {
        fd,
        events: events as i16,
        revents: 0,
    };
    if unsafe { libc::poll(&mut entry, 1, 0) } < 1 {
        return 0;
    }
    let readable = (libc::POLLIN | libc::POLLPRI | libc::POLLHUP | libc::POLLERR) as i16;
    let writable = (libc::POLLOUT | libc::POLLHUP | libc::POLLERR) as i16;
    let mut mask = 0;
    if entry.revents & readable != 0 {
        mask |= 1;
    }
    if entry.revents & writable != 0 {
        mask |= 2;
    }
    mask & wanted(events)
}

/// The mask bits `events` asks for.
fn wanted(events: i32) -> u8 {
    match interest(events) {
        interest if interest.is_readable() && interest.is_writable() => 3,
        interest if interest.is_writable() => 2,
        _ => 1,
    }
}

/// Waits until `fd` is ready for `events` and returns the mask. The registration is shared, so
/// readiness tokio remembers may be old: poll() confirms it, and stale readiness is cleared.
async fn ready(fd: RawFd, events: i32) -> u8 {
    let Some(watched) = watch(fd) else {
        return wanted(events);
    };
    let interest = interest(events);
    loop {
        let Ok(mut guard) = watched.ready(interest).await else {
            return wanted(events);
        };
        let mask = poll_now(fd, events);
        if mask != 0 {
            return mask;
        }
        guard.clear_ready();
    }
}

/// Waits until `fd` is ready for `events`, or `timeout_ms` has passed (-1: never). Completes with
/// "ready" or "timeout".
async fn wait(fd: RawFd, events: i32, timeout_ms: i64) -> OpResult {
    if timeout_ms >= 0 {
        let limit = Duration::from_millis(timeout_ms as u64);
        let waited = tokio::time::timeout(limit, ready(fd, events)).await;
        if waited.is_err() {
            return OpResult::Text(b"timeout".to_vec());
        }
    } else {
        ready(fd, events).await;
    }
    OpResult::Text(b"ready".to_vec())
}

/// One descriptor; the op's value is "ready" or "timeout".
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_wait_fd(fd: i32, events: i32, timeout_ms: i64) -> u64 {
    ops::spawn(
        "net",
        format!("wait {fd} {events}"),
        wait(fd, events, timeout_ms),
    )
}

/// stream_select(): several descriptors with their events, `count` of each. The op's value has one
/// byte per descriptor: the sum of 1 (readable) and 2 (writable) as a digit, "0" where nothing
/// happened; all zeros after the timeout.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_select(
    fds: *const i32,
    events: *const i32,
    count: usize,
    timeout_ms: i64,
) -> u64 {
    let fds = unsafe { std::slice::from_raw_parts(fds, count) }.to_vec();
    let events = unsafe { std::slice::from_raw_parts(events, count) }.to_vec();
    ops::spawn("net", format!("select {fds:?}"), async move {
        let mut set = JoinSet::new();
        for (index, (&fd, &events)) in fds.iter().zip(&events).enumerate() {
            set.spawn_local(async move { (index, ready(fd, events).await) });
        }
        let mut result = vec![b'0'; count];
        let first = if timeout_ms < 0 {
            set.join_next().await
        } else {
            match tokio::time::timeout(Duration::from_millis(timeout_ms as u64), set.join_next())
                .await
            {
                Ok(first) => first,
                Err(_) => return OpResult::Text(result),
            }
        };
        let mut done = vec![first];
        // Whatever else is ready by now counts too, as select() would report it
        while let Some(next) = set.try_join_next() {
            done.push(Some(next));
        }
        for finished in done.into_iter().flatten() {
            if let Ok((index, mask)) = finished {
                result[index] = b'0' + mask;
            }
        }
        OpResult::Text(result)
    })
}
