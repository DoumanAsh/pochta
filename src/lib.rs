//! Addressable channel registry

#![warn(missing_docs)]
#![cfg_attr(feature = "cargo-clippy", allow(clippy::style))]

mod waker;

use core::{fmt, task};
use core::pin::Pin;
use core::future::Future;
use core::hash::Hash;
use core::mem::ManuallyDrop;
use std::sync::mpsc;
use std::sync::Arc;
use std::collections::{HashMap, hash_map};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
///Describes sending error
pub enum SendErrorKind {
    //For now do not allow bounded channels to avoid dealing with back-pressure
    //Once strategy found, consider implementing it
    /////Capacity overflow
    //Full,
    ///Remote end is closed
    Closed
}

impl SendErrorKind {
    ///Returns `true` if kind indicates closed channel
    pub const fn is_closed(&self) -> bool {
        match self {
            SendErrorKind::Closed => true,
            //SendErrorKind::Full => false,
        }
    }
}

///Send error
pub struct SendError<T> {
    ///Error kind
    pub kind: SendErrorKind,
    ///Message, that could not be delivered
    pub message: T
}

impl<T> fmt::Debug for SendError<T> {
    #[inline(always)]
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.kind, fmt)
    }
}

impl<T> fmt::Display for SendError<T> {
    #[inline(always)]
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.kind, fmt)
    }
}

impl<T> std::error::Error for SendError<T> {}

///Channel sender
pub trait Sender<T: Send> {
    //#[inline(always)]
    /////Send method
    /////
    /////Defaults to calling `try_send`
    //fn send(&self, value: T) -> impl Future<Output=Result<(), SendError<T>>> + Send {
    //    future::ready(self.try_send(value))
    //}

    ///Attempts to deliver message to remote end, and is expected to be successful as long as
    ///remote end has not shut down.
    fn try_send(&self, value: T) -> Result<(), SendError<T>>;
}

impl<T: Send> Sender<T> for mpsc::Sender<T> {
    #[inline]
    fn try_send(&self, value: T) -> Result<(), SendError<T>> {
        match mpsc::Sender::send(self, value) {
            Ok(()) => Ok(()),
            Err(error) => Err(SendError {
                kind: SendErrorKind::Closed,
                message: error.0
            })
        }
    }
}

enum Message<K: PartialEq + Eq, T: Send, S: Sender<T>> {
    Subscribe(K, S),
    Unsubscribe(K),
    Msg(K, T)
}

struct State {
    waker: waker::AtomicWaker,
}

impl State {
    fn new() -> Self {
        Self {
            waker: waker::AtomicWaker::new(),
        }
    }
}

///Indicates remote end has been dropped, making this end unusable
pub struct Cancelled;

impl fmt::Debug for Cancelled {
    #[inline(always)]
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, fmt)
    }
}

impl fmt::Display for Cancelled {
    #[inline(always)]
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.write_str("Cancelled")
    }
}

impl std::error::Error for Cancelled {}

#[must_use = "You must run Registry task"]
///Task to manage messages within Registry
///
///It is expected running as either async task or on dedicated thread worker.
///
///This future is never ending, unless Registry gets dropped, resulting in error.
pub struct Registry<K: PartialEq + Eq, T: Send, S: Sender<T>> {
    state: Arc<State>,
    registry: HashMap<K, S>,
    recv: mpsc::Receiver<Message<K, T, S>>
}

impl<K: PartialEq + Eq + Hash, T: Send, S: Sender<T>> Registry<K, T, S> {
    #[inline(always)]
    fn new(state: Arc<State>, recv: mpsc::Receiver<Message<K, T, S>>) -> Self {
        Self {
            state,
            registry: HashMap::new(),
            recv,
        }
    }

    ///Process registry messages until cancelled.
    ///
    ///This function blocks, until all sending channels gets closed
    pub fn run(&mut self) -> Cancelled {
        let waker = waker::thread::waker(std::thread::current());

        loop {
            match self.process(&waker) {
                task::Poll::Ready(error) => break error,
                task::Poll::Pending => std::thread::park(),

            }
        }
    }

    fn process(&mut self, waker: &task::Waker) -> task::Poll<Cancelled> {
        loop {
            match self.recv.try_recv() {
                Ok(message) => match message {
                    Message::Subscribe(key, channel) => {
                        self.registry.insert(key, channel);
                        continue
                    }
                    Message::Unsubscribe(key) => {
                        self.registry.remove(&key);
                        continue
                    }
                    Message::Msg(key, message) => match self.registry.entry(key) {
                        hash_map::Entry::Occupied(entry) => match entry.get().try_send(message) {
                            Ok(()) => continue,
                            Err(error) => match error.kind {
                                SendErrorKind::Closed => {
                                    entry.remove();
                                },
                                //SendErrorKind::Full => {
                                //    todo!();
                                //}
                            }
                        },
                        hash_map::Entry::Vacant(_) => continue,
                    }
                },
                Err(mpsc::TryRecvError::Disconnected) => break task::Poll::Ready(Cancelled),
                Err(mpsc::TryRecvError::Empty) => {
                    self.state.waker.register_ref(waker);
                    break task::Poll::Pending;
                }
            }
        }
    }
}

impl<K: PartialEq + Eq + Hash + Unpin, T: Send, S: Sender<T> + Unpin> Future for Registry<K, T, S> {
    type Output = Cancelled;

    #[inline(always)]
    fn poll(self: Pin<&mut Self>, ctx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        let waker = ctx.waker();
        self.get_mut().process(waker)
    }
}

///Channel registry
///
///This is communication pipe towards channel
///As long as at least one instance exist, registry task will continue running
pub struct Channel<K: PartialEq + Eq + Hash, T: Send, S: Sender<T>> {
    state: Arc<State>,
    channel: ManuallyDrop<mpsc::Sender<Message<K, T, S>>>,
}

impl<K: PartialEq + Eq + Hash, T: Send, S: Sender<T>> Channel<K, T, S> {
    fn send(&self, msg: Message<K, T, S>) -> Result<(), Cancelled> {
        match self.channel.send(msg) {
            Ok(()) => {
                self.state.waker.wake();
                Ok(())
            },
            Err(_) => Err(Cancelled)
        }
    }

    #[inline(always)]
    ///Subscribes provided `channel` with specified `key`, potentially removing existing channel.
    ///
    ///Returns `Ok(())` if registry is still running
    ///Returns `Err(Cancelled)` if message ignored due to registry not running
    pub fn subscribe(&self, key: K, channel: S) -> Result<(), Cancelled> {
        self.send(Message::Subscribe(key, channel))
    }

    #[inline(always)]
    ///Removes `channel` with specified `key` from registry
    ///
    ///Returns `Ok(())` if registry is still running
    ///Returns `Err(Cancelled)` if message ignored due to registry not running
    pub fn unsubscribe(&self, key: K) -> Result<(), Cancelled> {
        self.send(Message::Unsubscribe(key))
    }

    #[inline(always)]
    ///Sends message `msg` over to channel registered by `key`.
    ///
    ///Returns `Ok(())` if registry is still running
    ///Returns `Err(Cancelled)` if message ignored due to registry not running
    pub fn send_to(&self, key: K, msg: T) -> Result<(), Cancelled> {
        self.send(Message::Msg(key, msg))
    }
}

impl<K: PartialEq + Eq + Hash, T: Send, S: Sender<T>> Clone for Channel<K, T, S> {
    #[inline(always)]
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            channel: self.channel.clone(),
        }
    }
}

impl<K: PartialEq + Eq + Hash, T: Send, S: Sender<T>> Drop for Channel<K, T, S> {
    #[inline(always)]
    fn drop(&mut self) {
        //Drop channel pipe first, to ensure it gets broken on receiver task
        unsafe {
            ManuallyDrop::drop(&mut self.channel)
        }

        //There is always only 1 receiver therefore count of 2 means it is last sender
        //Drop order doesn't really matter for senders as long as we wake task
        if Arc::strong_count(&self.state) <= 2 {
            //If it is last sender
            //In order to terminate task
            //Wake it up, if it is still listening
            self.state.waker.wake();
        }
    }
}

///Creates new registry returning sending channel and registry task
pub fn registry<K: PartialEq + Eq + Hash, T: Send, S: Sender<T>>() -> (Channel<K, T, S>, Registry<K, T, S>) {
    let (channel, recv) = mpsc::channel();
    let state = Arc::new(State::new());
    let chan = Channel {
        channel: ManuallyDrop::new(channel),
        state: state.clone(),
    };
    (chan, Registry::new(state, recv))
}
