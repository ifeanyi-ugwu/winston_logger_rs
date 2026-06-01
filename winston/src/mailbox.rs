use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::stream::Stream;
use futures::task::AtomicWaker;
use parking_lot::{Condvar, Mutex};

/// A bounded SPSC mailbox tailored to the per-transport pump.
///
/// `fmpsc::channel` is unsuitable here for two reasons:
/// - Its capacity is `buffer + num_senders`, so any `Sender::clone` inflates
///   the bound. The fanout's per-dispatch borrow pattern would have either
///   to never clone (awkward with `&self`) or accept a defeated cap.
/// - It has no "drop oldest, push newest" primitive, which `OverflowPolicy::
///   DropOldest` requires.
///
/// This mailbox is single-producer / single-consumer with a fixed capacity,
/// supports synchronous `try_push`, a synchronous `push_blocking` that parks
/// the calling thread on a `Condvar` when full, an async `send` that awaits
/// room, and a drop-oldest `force_push` that overwrites the head when full.
/// Wake-up uses one `AtomicWaker` per side (for the async paths) plus a
/// producer-side `Condvar` (for `push_blocking`). Closing the sender
/// drains-then-ends the receiver; dropping the receiver unblocks any waiting
/// producer with an error.
pub(crate) struct MailboxInner<T> {
    queue: Mutex<VecDeque<T>>,
    capacity: usize,
    consumer_waker: AtomicWaker,
    producer_waker: AtomicWaker,
    producer_condvar: Condvar,
    /// Sender dropped — consumer's `poll_next` returns `None` after draining.
    closed: AtomicBool,
    /// Receiver dropped — producer's `push_blocking` / `send` return Err.
    receiver_dropped: AtomicBool,
}

pub(crate) struct MailboxSender<T> {
    inner: Arc<MailboxInner<T>>,
}

pub(crate) struct MailboxReceiver<T> {
    inner: Arc<MailboxInner<T>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TryPushError<T> {
    Full(T),
    Closed(T),
}

#[derive(Debug)]
pub(crate) struct SendError<T>(#[allow(dead_code)] pub T);

pub(crate) fn channel<T>(capacity: usize) -> (MailboxSender<T>, MailboxReceiver<T>) {
    let inner = Arc::new(MailboxInner {
        queue: Mutex::new(VecDeque::with_capacity(capacity.max(1))),
        capacity: capacity.max(1),
        consumer_waker: AtomicWaker::new(),
        producer_waker: AtomicWaker::new(),
        producer_condvar: Condvar::new(),
        closed: AtomicBool::new(false),
        receiver_dropped: AtomicBool::new(false),
    });
    (
        MailboxSender {
            inner: Arc::clone(&inner),
        },
        MailboxReceiver { inner },
    )
}

impl<T> MailboxSender<T> {
    /// Non-blocking push. Returns `Full(msg)` if the queue is at capacity,
    /// `Closed(msg)` if the receiver dropped.
    ///
    /// `&self` is safe because the internal `Mutex<VecDeque>` serialises
    /// concurrent callers; the SPSC contract is preserved by not exposing
    /// `Clone` on `MailboxSender`. Lets callers hold `&MailboxSender`
    /// through a `RwLock` read lock without escalating to `&mut`.
    pub(crate) fn try_push(&self, msg: T) -> Result<(), TryPushError<T>> {
        if self.inner.receiver_dropped.load(Ordering::Acquire) {
            return Err(TryPushError::Closed(msg));
        }
        let mut q = self.inner.queue.lock();
        if q.len() >= self.inner.capacity {
            return Err(TryPushError::Full(msg));
        }
        q.push_back(msg);
        drop(q);
        self.inner.consumer_waker.wake();
        Ok(())
    }

    /// Non-blocking push that overwrites the head when full. Returns the
    /// popped (now-dropped) message if one was evicted to make room.
    pub(crate) fn force_push_dropping_oldest(&self, msg: T) -> Option<T> {
        let mut q = self.inner.queue.lock();
        let dropped = if q.len() >= self.inner.capacity {
            q.pop_front()
        } else {
            None
        };
        q.push_back(msg);
        drop(q);
        self.inner.consumer_waker.wake();
        dropped
    }

    /// Synchronous push that parks the calling thread on a `Condvar` when
    /// the queue is full. Returns `Err(SendError(msg))` if the receiver
    /// drops while the push is waiting (no point completing it).
    ///
    /// Designed for the sync `logger.log()` caller path under
    /// `OverflowPolicy::Block`.
    pub(crate) fn push_blocking(&self, msg: T) -> Result<(), SendError<T>> {
        let capacity = self.inner.capacity;
        let receiver_dropped = &self.inner.receiver_dropped;
        let mut q = self.inner.queue.lock();
        self.inner.producer_condvar.wait_while(&mut q, |q| {
            q.len() >= capacity && !receiver_dropped.load(Ordering::Acquire)
        });
        if receiver_dropped.load(Ordering::Acquire) {
            return Err(SendError(msg));
        }
        q.push_back(msg);
        drop(q);
        self.inner.consumer_waker.wake();
        Ok(())
    }

    /// Async push that awaits room. Errors only if the receiver dropped
    /// while waiting.
    pub(crate) fn send(&self, msg: T) -> Send<'_, T> {
        Send {
            sender: self,
            msg: Some(msg),
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }
}

impl<T> Drop for MailboxSender<T> {
    fn drop(&mut self) {
        self.inner.closed.store(true, Ordering::Release);
        self.inner.consumer_waker.wake();
    }
}

pub(crate) struct Send<'a, T> {
    sender: &'a MailboxSender<T>,
    msg: Option<T>,
}

impl<'a, T: Unpin> Future for Send<'a, T> {
    type Output = Result<(), SendError<T>>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.sender.inner.receiver_dropped.load(Ordering::Acquire) {
            return Poll::Ready(Err(SendError(this.msg.take().expect("polled after Ready"))));
        }
        let mut q = this.sender.inner.queue.lock();
        if q.len() < this.sender.inner.capacity {
            q.push_back(this.msg.take().expect("polled after Ready"));
            drop(q);
            this.sender.inner.consumer_waker.wake();
            return Poll::Ready(Ok(()));
        }
        drop(q);
        this.sender.inner.producer_waker.register(cx.waker());
        // Re-check to avoid a race with a consumer that popped between
        // the length check and waker registration.
        let mut q = this.sender.inner.queue.lock();
        if q.len() < this.sender.inner.capacity {
            q.push_back(this.msg.take().expect("polled after Ready"));
            drop(q);
            this.sender.inner.consumer_waker.wake();
            return Poll::Ready(Ok(()));
        }
        if this.sender.inner.receiver_dropped.load(Ordering::Acquire) {
            return Poll::Ready(Err(SendError(this.msg.take().expect("polled after Ready"))));
        }
        Poll::Pending
    }
}

impl<T> Stream for MailboxReceiver<T> {
    type Item = T;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let this = self.get_mut();
        let mut q = this.inner.queue.lock();
        if let Some(msg) = q.pop_front() {
            drop(q);
            this.inner.producer_waker.wake();
            this.inner.producer_condvar.notify_one();
            return Poll::Ready(Some(msg));
        }
        if this.inner.closed.load(Ordering::Acquire) {
            return Poll::Ready(None);
        }
        drop(q);
        this.inner.consumer_waker.register(cx.waker());
        let mut q = this.inner.queue.lock();
        if let Some(msg) = q.pop_front() {
            drop(q);
            this.inner.producer_waker.wake();
            this.inner.producer_condvar.notify_one();
            return Poll::Ready(Some(msg));
        }
        if this.inner.closed.load(Ordering::Acquire) {
            return Poll::Ready(None);
        }
        Poll::Pending
    }
}

impl<T> Drop for MailboxReceiver<T> {
    fn drop(&mut self) {
        self.inner.receiver_dropped.store(true, Ordering::Release);
        // Unblock any producer parked in `push_blocking` or async `send`.
        self.inner.producer_waker.wake();
        self.inner.producer_condvar.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures::StreamExt;

    #[test]
    fn try_push_succeeds_until_full() {
        let (mut tx, _rx) = channel::<u32>(2);
        assert!(tx.try_push(1).is_ok());
        assert!(tx.try_push(2).is_ok());
        assert!(matches!(tx.try_push(3), Err(TryPushError::Full(3))));
    }

    #[test]
    fn force_push_evicts_oldest() {
        let (mut tx, mut rx) = channel::<u32>(2);
        tx.try_push(1).unwrap();
        tx.try_push(2).unwrap();
        assert_eq!(tx.force_push_dropping_oldest(3), Some(1));
        let drained: Vec<u32> = block_on(async {
            let mut v = Vec::new();
            v.push(rx.next().await.unwrap());
            v.push(rx.next().await.unwrap());
            v
        });
        assert_eq!(drained, vec![2, 3]);
    }

    #[test]
    fn force_push_when_not_full_drops_nothing() {
        let (mut tx, _rx) = channel::<u32>(4);
        assert_eq!(tx.force_push_dropping_oldest(1), None);
        assert_eq!(tx.force_push_dropping_oldest(2), None);
    }

    #[test]
    fn receiver_returns_none_after_sender_dropped() {
        let (mut tx, mut rx) = channel::<u32>(2);
        tx.try_push(7).unwrap();
        drop(tx);
        block_on(async {
            assert_eq!(rx.next().await, Some(7));
            assert_eq!(rx.next().await, None);
        });
    }

    #[test]
    fn async_send_awaits_room_then_succeeds() {
        // Single-thread executor: producer fills, consumer pops, producer
        // wakes and completes the awaiting send.
        let (mut tx, mut rx) = channel::<u32>(1);
        tx.try_push(1).unwrap();

        let result = block_on(async {
            let send_fut = tx.send(2);
            futures::pin_mut!(send_fut);

            // First poll: should park (full).
            let popped = rx.next().await.unwrap();
            assert_eq!(popped, 1);
            // After pop, sender's wake clears the producer waker; awaiting
            // send_fut will see room and complete.
            send_fut.await
        });
        assert!(result.is_ok());
    }

    #[test]
    fn push_blocking_waits_for_room_then_pushes() {
        let (mut tx, mut rx) = channel::<u32>(1);
        tx.try_push(1).unwrap();

        // Producer thread parks in push_blocking; main thread pops, which
        // notifies the condvar and wakes the producer.
        let producer = std::thread::spawn(move || tx.push_blocking(2));

        // Give the producer thread time to enter the wait.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let popped = block_on(async { rx.next().await });
        assert_eq!(popped, Some(1));

        let push_result = producer.join().unwrap();
        assert!(push_result.is_ok());

        // The producer's value should now be queued.
        let next = block_on(async { rx.next().await });
        assert_eq!(next, Some(2));
    }

    #[test]
    fn push_blocking_returns_err_when_receiver_dropped_while_waiting() {
        let (mut tx, rx) = channel::<u32>(1);
        tx.try_push(1).unwrap();

        let producer = std::thread::spawn(move || tx.push_blocking(2));

        // Let the producer park, then drop the receiver to wake it with err.
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(rx);

        let push_result = producer.join().unwrap();
        assert!(matches!(push_result, Err(SendError(2))));
    }

    #[test]
    fn try_push_returns_closed_after_receiver_dropped() {
        let (mut tx, rx) = channel::<u32>(2);
        drop(rx);
        assert!(matches!(tx.try_push(1), Err(TryPushError::Closed(1))));
    }
}
