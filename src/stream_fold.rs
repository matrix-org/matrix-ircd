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

use futures3::future::{Future, FutureExt};
use futures3::stream::{Stream, StreamExt};
use futures3::task::{Context, Poll};

use std::boxed::Box;
use std::cell::RefCell;
use std::mem;
use std::ops::Deref;
use std::pin::Pin;

/// A Stream adapater, similar to fold, that consumes the start of the stream to build up an
/// object, but then returns both the object *and* the stream.
#[must_use = "futures do nothing unless polled"]
pub struct StreamFold<I, E, S: Stream<Item = Result<I, E>>, V, F: FnMut(I, V) -> (bool, V)> {
    func: F,
    state: State<S, V>,
}

impl<I, E, S: Stream<Item = Result<I, E>>, V, F: FnMut(I, V) -> (bool, V)>
    StreamFold<I, E, S, V, F>
{
    pub fn new(stream: S, value: V, func: F) -> StreamFold<I, E, S, V, F> {
        StreamFold {
            func,
            state: State {
                stream: Box::pin(stream),
                value,
            },
        }
    }
}

enum StreamFoldState<S, V> {
    Empty,
    Full(State<S, V>),
}

struct State<S, V> {
    stream: Pin<Box<S>>,
    value: V,
}

struct StreamCell<I, E, S, V, F>
where
    S: Stream<Item = Result<I, E>>,
    F: FnMut(I, V) -> (bool, V),
{
    inner: std::cell::RefCell<StreamFold<I, E, S, V, F>>,
}

impl<
        I,
        E,
        S: Stream<Item = Result<I, E>> + std::marker::Unpin + Clone,
        V: Default,
        F: FnMut(I, V) -> (bool, V),
    > Future for StreamCell<I, E, S, V, F>
{
    type Output = Result<Option<(V, S)>, E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        //let (mut stream, mut value) = match mem::replace(&mut self.state, StreamFoldState::Empty) {
        //    StreamFoldState::Empty => panic!("cannot poll Fold twice"),
        //    StreamFoldState::Full { stream, value } => (stream, value),
        //};

        let inner = self.inner.get_mut();

        loop {
            match inner.state.stream.as_mut().poll_next(cx)? {
                Poll::Ready(Some(item)) => {
                    let (done, val) = (inner.func)(item, inner.state.value);
                    inner.state.value = val;
                    if done {
                        let inner_box = Pin::into_inner(inner.state.stream);
                        //let inner = *inner;
                        return Poll::Ready(Ok(Some((inner.state.value, *inner_box))));
                    }
                }
                Poll::Ready(None) => return Poll::Ready(Ok(None)),
                Poll::Pending => {
                    //let stream = Pin::into_inner(self.state.stream);
                    //self.state = {
                    //    stream: self.state.stream,
                    //    value: self.state.value,
                    //};
                    return Poll::Pending;
                }
            }
        }
    }
}
