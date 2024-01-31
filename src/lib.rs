//! Addressable channel registry
//!

#![warn(missing_docs)]
#![cfg_attr(feature = "cargo-clippy", allow(clippy::style))]

use tokio::sync::RwLock;

use core::marker;
use core::borrow::Borrow;
use core::future::Future;
use core::hash::Hash;
use std::collections::HashMap;

///Describes sending error
pub enum SendErrorKind {
    ///Capacity overflow
    Full,
    ///Remote end is closed
    Closed
}

impl SendErrorKind {
    ///Returns `true` if kind indicates closed channel
    pub const fn is_closed(&self) -> bool {
        match self {
            SendErrorKind::Closed => true,
            SendErrorKind::Full => false,
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

///Channel sender
pub trait Sender<T: Send> {
    ///Send method
    fn send(&self, value: T) -> impl Future<Output=Result<(), SendError<T>>> + Send;
}

///Channel registry
pub struct Registry<K, T: Send, S: Sender<T>> {
    inner: RwLock<HashMap<K, S>>,
    _data: marker::PhantomData<T>,
}

impl<T: Send, K: PartialEq + Eq + Hash, S: Sender<T>> Registry<K, T, S> {
    ///Sends value over channel by index `key`, returning `None` if no channel is found
    pub async fn send_to<Q: Hash + Eq + ?Sized>(&self, key: &Q, value: T) -> Option<Result<(), SendError<T>>> where K: Borrow<Q> {
        let result = if let Some(channel) = self.inner.read().await.get(key) {
            channel.send(value).await
        } else {
            return None;
        };

        match result {
            Err(error) if error.kind.is_closed() => {
                let _ = self.inner.write().await.remove(key);
                Some(Err(error))
            }
            result => Some(result)
        }
    }

    #[inline(always)]
    ///Subscribes provided `channel` with specified `key`, returning old channel if any present
    pub async fn subscribe(&self, key: K, channel: S) -> Option<S> {
        self.inner.write().await.insert(key, channel)
    }

    #[inline(always)]
    ///Attempts to removes subscription under the name `key`, returning `Some` if there was one
    pub async fn unsubscribe<Q: Hash + Eq + ?Sized>(&self, key: &Q) -> Option<S> where K: Borrow<Q> {
        self.inner.write().await.remove(key)
    }
}
