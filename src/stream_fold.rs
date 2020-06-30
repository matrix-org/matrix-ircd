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
    W: std::fmt::Debug + StateUpdate<Result<T, V>>,
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
    W: StateUpdate<Result<T, V>>,
    S: Stream<Item = Result<T, V>>,
{
    type Output = Option<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            match self.stream.lock().unwrap().as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    if self.state.lock().unwrap().state_update(item) {
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
