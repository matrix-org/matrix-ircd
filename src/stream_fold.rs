use futures::{Async, Future, Poll};
use futures::stream::Stream;

use std::mem;


#[must_use = "futures do nothing unless polled"]
pub struct StreamFold<I, E, S: Stream<Item=I, Error=E>, V, F: FnMut(I, V) -> (bool, V)> {
    func: F,
    state: StreamFoldState<S, V>
}

impl<I, E, S: Stream<Item=I, Error=E>, V, F: FnMut(I, V) -> (bool, V)> StreamFold<I, E, S, V, F> {
    pub fn new(stream: S, value: V, func: F) -> StreamFold<I, E, S, V, F> {
        StreamFold {
            func: func,
            state: StreamFoldState::Full {
                stream: stream,
                value: value,
            }
        }
    }
}

enum StreamFoldState<S, V> {
    Empty,
    Full {
        stream: S,
        value: V,
    }
}

impl<I, E, S: Stream<Item=I, Error=E>, V, F: FnMut(I, V) -> (bool, V)> Future for StreamFold<I, E, S, V, F> {
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
                        return Ok(Async::Ready(Some((value, stream))))
                    }
                }
                Async::Ready(None) => {
                    return Ok(Async::Ready(None))
                }
                Async::NotReady => {
                    self.state = StreamFoldState::Full { stream: stream, value: value };
                    return Ok(Async::NotReady)
                }
            }
        }
    }
}
