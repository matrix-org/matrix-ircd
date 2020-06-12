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

use futures::stream::Stream;
use futures::{Async, Future, Poll};

use std::mem;

/// A Stream adapater, similar to fold, that consumes the start of the stream to build up an
/// object, but then returns both the object *and* the stream.
#[must_use = "futures do nothing unless polled"]
pub struct StreamFold<I, E, S: Stream<Item = I, Error = E>, V, F: FnMut(I, V) -> (bool, V)> {
    func: F,
    state: StreamFoldState<S, V>,
}

impl<I, E, S: Stream<Item = I, Error = E>, V, F: FnMut(I, V) -> (bool, V)>
    StreamFold<I, E, S, V, F>
{
    pub fn new(stream: S, value: V, func: F) -> StreamFold<I, E, S, V, F> {
        StreamFold {
            func,
            state: StreamFoldState::Full { stream, value },
        }
    }
}

enum StreamFoldState<S, V> {
    Empty,
    Full { stream: S, value: V },
}

impl<I, E, S: Stream<Item = I, Error = E>, V, F: FnMut(I, V) -> (bool, V)> Future
    for StreamFold<I, E, S, V, F>
{
    type Item = Option<(V, S)>;
    type Error = E;

    fn poll(&mut self) -> Poll<Option<(V, S)>, E> {
        let (mut stream, mut value) = match mem::replace(&mut self.state, StreamFoldState::Empty) {
            StreamFoldState::Empty => panic!("cannot poll Fold twice"),
            StreamFoldState::Full { stream, value } => (stream, value),
        };

        loop {
            match stream.poll()? {
                Async::Ready(Some(item)) => {
                    let (done, val) = (self.func)(item, value);
                    value = val;
                    if done {
                        return Ok(Async::Ready(Some((value, stream))));
                    }
                }
                Async::Ready(None) => return Ok(Async::Ready(None)),
                Async::NotReady => {
                    self.state = StreamFoldState::Full {
                        stream: stream,
                        value: value,
                    };
                    return Ok(Async::NotReady);
                }
            }
        }
    }
}
