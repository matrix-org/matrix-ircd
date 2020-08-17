// Copyright 2016 Openmarket
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use futures::future::Future;
use futures::stream::Stream;
use futures::task::{Context, Poll};

use std::boxed::Box;
use std::pin::Pin;
use std::sync::Mutex;

/// `StreamFold` provides a way to fold over a stream and update internal state with every new value.
///
/// StreamFold is used here instead of futures::stream::StreamExt::fold because this method has no
/// way of returning _both_ the input stream and resulting state. Additionally, `StreamFold` has
/// a mechanism for early-exits while StreamExt::fold will fold over the entire stream.

// The stream must be Box<Pin<S>> since we call `poll_next` the Future impl. However, since
// `DerefMut` is not implemented for Pin<&mut Self>, we cannot do `self.stream.as_mut()`, which is
// required to call `poll_next`. To circumvent this, the stream is placed in a RefCell for interior
// mutability.
//
// Since the state must be modified in (and the state update requires &mut self) the lack of
// `DerefMut` on `Pin<&mut Self>` again requires RefCell. Hopefully these get optimized out.
#[must_use = "futures do nothing unless polled"]
pub struct StreamFold<S, W, T, V>
where
    S: Stream<Item = Result<T, V>>,
    W: StateUpdate<Result<T, V>>,
{
    stream: Mutex<Pin<Box<S>>>,
    // The variable that is being updated with every new value from self.stream
    state: Mutex<W>,
}

impl<W, T, V, S> StreamFold<S, W, T, V>
where
    W: StateUpdate<Result<T, V>>,
    S: Stream<Item = Result<T, V>>,
{
    pub fn new(stream: S, state: W) -> StreamFold<S, W, T, V> {
        StreamFold {
            state: Mutex::new(state),
            stream: Mutex::new(Box::pin(stream)),
        }
    }

    pub fn into_parts(self) -> (Pin<Box<S>>, W) {
        (
            self.stream.into_inner().unwrap(),
            self.state.into_inner().unwrap(),
        )
    }
}

impl<W, T, V, S> Future for &StreamFold<S, W, T, V>
where
    W: StateUpdate<Result<T, V>> + std::fmt::Debug,
    S: Stream<Item = Result<T, V>>,
{
    type Output = Option<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        // Since these are only used for interior mutability and we dont give out references to
        // these fields we can be sure that this will not deadlock
        let mut stream = self.stream.lock().unwrap();
        let mut state = self.state.lock().unwrap();

        loop {
            match stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    if state.state_update(item) {
                        return Poll::Ready(Some(()));
                    } else {
                        continue;
                    }
                }
                Poll::Ready(None) => {
                    return Poll::Ready(None);
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

pub trait StateUpdate<Z> {
    fn state_update(&mut self, new_item: Z) -> bool;
}

#[cfg(test)]
mod tests {
    use super::{StateUpdate, StreamFold};
    use tokio::sync::mpsc;

    #[derive(Debug)]
    enum Parts {
        A,
        B,
        C,
        D,
        End,
    }

    #[derive(Default, Debug)]
    struct PartsCollected {
        a: Option<()>,
        b: Option<()>,
        c: Option<()>,
        d: Option<()>,
    }
    impl PartsCollected {
        fn all_some(&self) -> bool {
            self.a.is_some() && self.b.is_some() && self.c.is_some() && self.d.is_some()
        }
    }
    impl StateUpdate<Result<Parts, ()>> for PartsCollected {
        fn state_update(&mut self, new_item: Result<Parts, ()>) -> bool {
            match new_item.unwrap() {
                Parts::A => self.a = Some(()),
                Parts::B => self.b = Some(()),
                Parts::C => self.c = Some(()),
                Parts::D => self.d = Some(()),
                Parts::End => return true,
            }
            self.all_some()
        }
    }

    // Sends all four of the required parts to the PartsCollected struct. Does not include a
    // Parts::End since we expect the state to complete without it
    #[tokio::test]
    async fn good_fold() {
        let (mut tx, rx) = mpsc::channel(10);
        tx.send(Ok(Parts::A)).await.unwrap();
        tx.send(Ok(Parts::B)).await.unwrap();
        tx.send(Ok(Parts::C)).await.unwrap();
        tx.send(Ok(Parts::D)).await.unwrap();
        let stream_fold = StreamFold::new(rx, PartsCollected::default());
        (&stream_fold).await;
        let (_, state) = stream_fold.into_parts();

        assert_eq!(state.all_some(), true)
    }

    // Only send 3/4 parts to the PartsCollected state, and then send the EOF. The returned state
    // should be incomplete
    #[tokio::test]
    async fn incomplete_fold() {
        let (mut tx, rx) = mpsc::channel(10);

        // we only send 3 of the 4 parts to the fold
        tx.send(Ok(Parts::A)).await.unwrap();
        tx.send(Ok(Parts::B)).await.unwrap();
        tx.send(Ok(Parts::C)).await.unwrap();

        // Simulate an EOF
        tx.send(Ok(Parts::End)).await.unwrap();

        let stream_fold = StreamFold::new(rx, PartsCollected::default());
        (&stream_fold).await;
        let (_, state) = stream_fold.into_parts();

        assert_eq!(state.all_some(), false)
    }
}
