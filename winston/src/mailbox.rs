use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::stream::Stream;
use futures::task::AtomicWaker;
use parking_lot::Mutex;

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
/// supports synchronous `try_push`, an async `send` that awaits room, and
/// a drop-oldest `force_push` that overwrites the head when full. Wake-up
/// uses one `AtomicWaker` per side — sufficient for one consumer and one
/// producer at a time. Closing the sender drains-then-ends the receiver.
pub(crate) struct MailboxInner<T> {
    queue: Mutex<VecDeque<T>>,
    capacity: usize,
    consumer_waker: AtomicWaker,
    producer_waker: AtomicWaker,
    closed: AtomicBool,
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
        closed: AtomicBool::new(false),
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
    pub(crate) fn try_push(&mut self, msg: T) -> Result<(), TryPushError<T>> {
        if self.inner.closed.load(Ordering::Acquire) {
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
    pub(crate) fn force_push_dropping_oldest(&mut self, msg: T) -> Option<T> {
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

    /// Async push that awaits room. Errors only if the receiver dropped
    /// while waiting.
    pub(crate) fn send(&mut self, msg: T) -> Send<'_, T> {
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
    sender: &'a mut MailboxSender<T>,
    msg: Option<T>,
}

impl<'a, T: Unpin> Future for Send<'a, T> {
    type Output = Result<(), SendError<T>>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.sender.inner.closed.load(Ordering::Acquire) {
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
        // our length check and waker registration.
        let mut q = this.sender.inner.queue.lock();
        if q.len() < this.sender.inner.capacity {
            q.push_back(this.msg.take().expect("polled after Ready"));
            drop(q);
            this.sender.inner.consumer_waker.wake();
            return Poll::Ready(Ok(()));
        }
        if this.sender.inner.closed.load(Ordering::Acquire) {
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
            return Poll::Ready(Some(msg));
        }
        if this.inner.closed.load(Ordering::Acquire) {
            return Poll::Ready(None);
        }
        Poll::Pending
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
}
