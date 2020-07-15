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

use std::cell::RefCell;

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
    //stream: Mutex<Pin<Box<S>>>,
    stream: RefCell<Pin<Box<S>>>,
    // The variable that is being updated with every new value from self.stream
    state: RefCell<W>,
}

impl<W, T, V, S> StreamFold<S, W, T, V>
where
    W: std::fmt::Debug + StateUpdate<Result<T, V>>,
    S: Stream<Item = Result<T, V>>,
{
    pub fn new(stream: S, state: W) -> StreamFold<S, W, T, V> {
        StreamFold {
            state: RefCell::new(state),
            stream: RefCell::new(Box::pin(stream)),
        }
    }

    pub fn into_parts(self) -> (Pin<Box<S>>, W) {
        (self.stream.into_inner(), self.state.into_inner())
    }
}

impl<W, T, V, S> Future for &StreamFold<S, W, T, V>
where
    W: StateUpdate<Result<T, V>>,
    S: Stream<Item = Result<T, V>>,
{
    type Output = Option<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            match self.stream.borrow_mut().as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    if self.state.borrow_mut().state_update(item) {
                        return Poll::Ready(Some(()));
                    } else {
                        return Poll::Pending;
                    }
                }
                Poll::Ready(None) => return Poll::Ready(None),
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
