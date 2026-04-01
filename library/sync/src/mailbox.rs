//! Mailbox based on MPMC channels.

use crate::asynchronous::{Receiver, RecvError, SendError, Sender, bounded, unbounded};

pub enum Message<T, R> {
    Ask { ret: Sender<R>, payload: T },
    Tell(T),
}

pub struct MailboxSender<T, R> {
    inner: Sender<Message<T, R>>,
}

impl<T, R> Clone for MailboxSender<T, R> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T, R> MailboxSender<T, R> {
    pub async fn ask(&self, msg: T) -> Result<R, SendError<T>> {
        let (tx, rx) = bounded::<R>(1);
        self.inner
            .send(Message::Ask {
                ret: tx,
                payload: msg,
            })
            .await
            .map_err(|err| match err {
                SendError::Closed(Message::Ask { payload, .. }) => SendError::Closed(payload),
                SendError::Closed(Message::Tell(payload)) => SendError::Closed(payload),
            })?;
        match rx.recv().await {
            Ok(reply) => Ok(reply),
            Err(_) => panic!("mailbox ask reply channel unexpectedly closed"),
        }
    }

    pub async fn tell(&self, msg: T) -> Result<(), SendError<T>> {
        self.inner
            .send(Message::Tell(msg))
            .await
            .map_err(|err| match err {
                SendError::Closed(Message::Ask { payload, .. }) => SendError::Closed(payload),
                SendError::Closed(Message::Tell(payload)) => SendError::Closed(payload),
            })
    }

    pub fn ask_blocking(&self, msg: T) -> Result<R, SendError<T>> {
        let (tx, rx) = bounded::<R>(1);
        self.inner
            .send_blocking(Message::Ask {
                ret: tx,
                payload: msg,
            })
            .map_err(|err| match err {
                SendError::Closed(Message::Ask { payload, .. }) => SendError::Closed(payload),
                SendError::Closed(Message::Tell(payload)) => SendError::Closed(payload),
            })?;
        match rx.recv_blocking() {
            Ok(reply) => Ok(reply),
            Err(_) => panic!("mailbox ask reply channel unexpectedly closed"),
        }
    }

    pub fn tell_blocking(&self, msg: T) -> Result<(), SendError<T>> {
        self.inner
            .send_blocking(Message::Tell(msg))
            .map_err(|err| match err {
                SendError::Closed(Message::Ask { payload, .. }) => SendError::Closed(payload),
                SendError::Closed(Message::Tell(payload)) => SendError::Closed(payload),
            })
    }
}

pub struct MailboxReceiver<T, R> {
    inner: Receiver<Message<T, R>>,
}

impl<T, R> MailboxReceiver<T, R> {
    pub fn try_recv(&self) -> Result<Message<T, R>, RecvError> {
        self.inner.try_recv()
    }

    pub async fn recv(&self) -> Result<Message<T, R>, RecvError> {
        self.inner.recv().await
    }

    pub fn recv_blocking(&self) -> Result<Message<T, R>, RecvError> {
        self.inner.recv_blocking()
    }
}

pub fn bounded_mailbox<T, R>(capacity: usize) -> (MailboxSender<T, R>, MailboxReceiver<T, R>) {
    let (tx, rx) = bounded::<Message<T, R>>(capacity);
    (MailboxSender { inner: tx }, MailboxReceiver { inner: rx })
}

pub fn unbounded_mailbox<T, R>() -> (MailboxSender<T, R>, MailboxReceiver<T, R>) {
    let (tx, rx) = unbounded::<Message<T, R>>();
    (MailboxSender { inner: tx }, MailboxReceiver { inner: rx })
}
