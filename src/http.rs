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

use futures::{self, Async, Future, Poll, task};

use std::collections::VecDeque;
use std::{mem, io, str};
use std::io::Write;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use tokio_core::reactor::Handle;
use tokio_core::io::Io;
use tokio_dns::tcp_connect;

use httparse;
use netbuf;


#[derive(Default)]
struct HttpClientInner {
    requests: VecDeque<(Request, futures::Complete<Result<Response, io::Error>>)>,
    task: Option<task::Task>,
    errored: bool,
}

pub struct HttpClient {
    inner: Arc<Mutex<HttpClientInner>>,
}

impl HttpClient {
    pub fn new(host: String, port: u16, handle: Handle) -> HttpClient {
        let inner: Arc<Mutex<HttpClientInner>> = Arc::default();
        let inner_handle = inner.clone();

        let tcp = tcp_connect(
            (host.as_str(), port), handle.remote().clone()
        ).and_then(move |socket| {
            HttpClientHandler::new(inner_handle, host, socket)
        }).map_err(|_| ()); // TODO: Handle error better

        handle.spawn(tcp);

        HttpClient {
            inner: inner,
        }
    }

    pub fn send_request(&mut self, request: Request) -> HttpResponseFuture {
        let mut inner = self.inner.lock().expect("lock is poisoned");

        let (c, o) = futures::oneshot();
        inner.requests.push_back((request, c));

        if let Some(ref mut task) = inner.task {
            task.unpark();
        }

        o.into()
    }
}


#[must_use = "http stream must be polled"]
struct HttpClientHandler<T: Io> {
    inner: Arc<Mutex<HttpClientInner>>,
    response_buffer: netbuf::Buf,
    write_buffer: netbuf::Buf,
    curr_state: HttpStreamState,
    partial_response: Response,
    requests: VecDeque<futures::Complete<Result<Response, io::Error>>>,
    stream: T,
    host: String,
}

impl<T: Io> HttpClientHandler<T> {
    pub fn new(inner: Arc<Mutex<HttpClientInner>>, host: String, stream: T) -> HttpClientHandler<T> {
        HttpClientHandler {
            inner: inner,
            response_buffer: netbuf::Buf::new(),
            write_buffer: netbuf::Buf::new(),
            curr_state: HttpStreamState::Headers,
            partial_response: Response::default(),
            requests: VecDeque::new(),
            stream: stream,
            host: host,
        }
    }

    fn poll_for_response(&mut self) -> Poll<Response, io::Error> {
        let mut stream = &mut self.stream;

        loop {
            match self.curr_state {
                HttpStreamState::Headers => {
                    if self.response_buffer.is_empty() {
                        let num_bytes = try_ready!(read_into_vec(stream, &mut self.response_buffer));
                        if num_bytes == 0 {
                            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF while waiting for new response"));
                        }
                    }

                    self.partial_response = Response::default();

                    let bytes_read_opt = {
                        let mut headers = [httparse::EMPTY_HEADER; 32];
                        let mut resp = httparse::Response::new(&mut headers);

                        match resp.parse(&self.response_buffer[..]) {
                            Ok(httparse::Status::Complete(bytes_read)) => {
                                self.partial_response.code = resp.code.expect("expected response code");
                                self.curr_state = HttpStreamState::RawData(None);

                                for header in resp.headers {
                                    match header.name.to_lowercase().as_str() {
                                        "content-type" => {
                                            self.partial_response.content_type = String::from_utf8(header.value.to_vec()).ok();
                                        }
                                        "content-length" => {
                                            let opt_len = str::from_utf8(header.value).ok().and_then(|s| usize::from_str(s).ok());
                                            if let Some(len) = opt_len {
                                                self.curr_state = HttpStreamState::RawData(Some(len));
                                            }
                                        }
                                        "transfer-encoding" => {
                                            if let Ok(s) = str::from_utf8(header.value) {
                                                if s.to_lowercase().starts_with("chunked") {
                                                    self.curr_state = HttpStreamState::ChunkedData;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                };

                                Some(bytes_read)
                            }
                            Ok(httparse::Status::Partial) => {
                                None
                            }
                            Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "HTTP parse error")),
                        }
                    };

                    if let Some(bytes_read) = bytes_read_opt {
                        self.response_buffer.consume(bytes_read);
                    } else {
                        let num_bytes = try_ready!(read_into_vec(stream, &mut self.response_buffer));
                        if num_bytes == 0 {
                            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Unexpected EOF parsing headers"));
                        }
                    }
                }
                HttpStreamState::RawData(len_opt) => {
                    if let Some(len) = len_opt {
                        if self.response_buffer.len() >= len {
                            if self.response_buffer.len() == len {
                                let buf = mem::replace(&mut self.response_buffer, netbuf::Buf::new());
                                self.partial_response.data = buf.into();
                            } else {
                                self.partial_response.data = self.response_buffer[..len].to_vec();
                                self.response_buffer.consume(len);
                            }

                            self.curr_state = HttpStreamState::Headers;
                            let resp = mem::replace(&mut self.partial_response, Response::default());
                            return Ok(Async::Ready(resp));
                        }
                    }

                    let num_bytes = try_ready!(read_into_vec(stream, &mut self.response_buffer));
                    if num_bytes == 0 {
                        if let HttpStreamState::RawData(None) = self.curr_state {
                            self.curr_state = HttpStreamState::Headers;
                            let resp = mem::replace(&mut self.partial_response, Response::default());
                            return Ok(Async::Ready(resp));
                        } else {
                            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Unexpected EOF parsing body"));
                        }
                    }
                }
                HttpStreamState::ChunkedData => {
                    match httparse::parse_chunk_size(&self.response_buffer[..]) {
                        Ok(httparse::Status::Complete((bytes_read, 0))) => {
                            // +2 as chunks end in \r\n
                            if self.response_buffer.len() >= bytes_read + 2 {
                                self.response_buffer.consume(bytes_read + 2);

                                self.curr_state = HttpStreamState::Headers;
                                let resp = mem::replace(&mut self.partial_response, Response::default());
                                return Ok(Async::Ready(resp));
                            }
                        }
                        Ok(httparse::Status::Complete((bytes_read, chunk_len_64))) => {
                            let chunk_len = chunk_len_64 as usize;

                            // +2 as chunks end in \r\n
                            if self.response_buffer.len() >= bytes_read + chunk_len + 2 {
                                self.partial_response.data.extend_from_slice(
                                    &self.response_buffer[bytes_read..bytes_read + chunk_len]
                                );
                                self.response_buffer.consume(bytes_read + chunk_len + 2);
                                continue;
                            }
                        }
                        Ok(httparse::Status::Partial) => {}
                        Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "HTTP invalid chunked body")),
                    }

                    let num_bytes = try_ready!(read_into_vec(stream, &mut self.response_buffer));
                    if num_bytes == 0 {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Unexpected EOF parsing chunked body"));
                    }
                }
            }
        }
    }

    fn write_request(&mut self, request: Request) {
        write!(self.write_buffer, "{} {} HTTP/1.1\r\n", request.method, request.path).unwrap();
        write!(self.write_buffer, "Host: {}\r\n", &self.host).unwrap();
        write!(self.write_buffer, "Connection: keep-alive\r\n").unwrap();
        write!(self.write_buffer, "Content-Length: {}\r\n", request.body.len()).unwrap();
        for &(ref name, ref value) in &request.headers {
            write!(self.write_buffer, "{}: {}\r\n", name, value).unwrap();
        }
        write!(self.write_buffer, "\r\n").unwrap();
        self.write_buffer.extend(&request.body[..]);
    }

    fn poll_inner(&mut self) -> Poll<(), io::Error> {
        loop {
            let new_requests = {
                let mut inner = self.inner.lock().expect("lock is poisoned");
                if inner.task.is_none() {
                    inner.task = Some(task::park());
                }

                mem::replace(&mut inner.requests, VecDeque::new())
            };

            for (req, future) in new_requests {
                self.write_request(req);
                self.requests.push_back(future);
            }

            if !self.write_buffer.is_empty() {
                self.write_buffer.write_to(&mut self.stream)?;
            }

            let resp = try_ready!(self.poll_for_response());
            if let Some(future) = self.requests.pop_front() {
                future.complete(Ok(resp));
            } else {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Response without request"));
            }
        }
    }
}


impl<T: Io> Drop for HttpClientHandler<T> {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().expect("lock is poisoned");
        inner.errored = true;
    }
}

impl<T: Io> Future for HttpClientHandler<T> {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        match self.poll_inner() {
            Ok(r) => Ok(r),
            Err(e) => {
                {
                    let mut inner = self.inner.lock().expect("lock is poisoned");
                    inner.errored = true;
                }

                for f in self.requests.drain(..) {
                    f.complete(Err(clone_io_error(&e)));
                }
                Err(e)
            }
        }
    }
}

fn clone_io_error(err: &io::Error) -> io::Error {
    let kind = err.kind();
    if let Some(inner) = err.get_ref() {
        io::Error::new(kind, inner.description().to_string())
    } else {
        io::Error::new(kind, "")
    }
}


pub struct HttpResponseFuture {
    future: futures::Oneshot<Result<Response, io::Error>>,
}

impl Future for HttpResponseFuture {
    type Item = Response;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Response, io::Error> {
        match self.future.poll() {
            Ok(Async::Ready(Ok(response))) => Ok(Async::Ready(response)),
            Ok(Async::Ready(Err(e))) => Err(e),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(_) => Err(io::Error::new(io::ErrorKind::Other, "stream went away")),
        }
    }
}

impl From<futures::Oneshot<Result<Response, io::Error>>> for HttpResponseFuture {
    fn from(future: futures::Oneshot<Result<Response, io::Error>>) -> HttpResponseFuture {
        HttpResponseFuture { future: future }
    }
}


#[derive(Debug, Default, PartialEq, Eq)]
pub struct Response {
    pub code: u16,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub method: &'static str,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}


#[derive(Debug, Clone, Copy)]
enum HttpStreamState {
    Headers,
    RawData(Option<usize>),
    ChunkedData,
}


fn read_into_vec<R: io::Read>(stream: &mut R, buf: &mut netbuf::Buf) -> Poll<usize, io::Error> {
    let size = try_nb!(buf.read_from(stream));
    Ok(Async::Ready(size))
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor, Read};
    use futures::Async;

    struct TestReader<'a> {
        chunks: Vec<Option<&'a [u8]>>,
    }

    impl<'a> TestReader<'a> {
        fn new(mut chunks: Vec<Option<&'a [u8]>>) -> TestReader<'a> {
            chunks.reverse();
            TestReader {
                chunks: chunks,
            }
        }
    }

    impl<'a> Read for TestReader<'a> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.chunks.pop() {
                 Some(Some(chunk)) => {
                     (&mut buf[..chunk.len()]).copy_from_slice(chunk);
                     Ok(chunk.len())
                 }
                 Some(None) => {
                     Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
                 }
                 None => Ok(0)
            }
        }
    }

    #[test]
    fn basic_response() {
        let test_resp = Cursor::new(b"HTTP/1.1 200 Ok\r\n\r\n");
        let mut stream = HttpStream::new(test_resp);

        let resp = match stream.poll_for_response().unwrap() {
            Async::Ready(resp) => resp,
            c => panic!("Unexpected res: {:?}", c),
        };

        assert_eq!(resp.code, 200);
        assert_eq!(resp.content_type, None);
        assert_eq!(resp.data.len(), 0);
    }

    #[test]
    fn basic_length_response() {
        let test_resp = Cursor::new(b"HTTP/1.1 200 Ok\r\nContent-Length: 5\r\n\r\nHello".to_vec());
        let mut stream = HttpStream::new(test_resp);

        let resp = match stream.poll_for_response().unwrap() {
            Async::Ready(resp) => resp,
            c => panic!("Unexpected res: {:?}", c),
        };

        assert_eq!(resp.code, 200);
        assert_eq!(resp.content_type, None);
        assert_eq!(&resp.data[..], b"Hello");

        assert!(stream.poll_for_response().is_err());
    }

    #[test]
    fn basic_chunked_response() {
        let test_resp = Cursor::new(
            b"HTTP/1.1 200 Ok\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nH\r\n4\r\nello\r\n0\r\n\r\n"
            .to_vec()
        );
        let mut stream = HttpStream::new(test_resp);

        let resp = match stream.poll_for_response().unwrap() {
            Async::Ready(resp) => resp,
            c => panic!("Unexpected res: {:?}", c),
        };

        assert_eq!(resp.code, 200);
        assert_eq!(resp.content_type, None);
        assert_eq!(&resp.data[..], b"Hello");
    }

    #[test]
    fn chunked_response_missing_last_newline() {
        let test_resp = TestReader::new(vec![
            Some(&b"HTTP/1.1 200 Ok\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nH\r\n4\r\nello\r\n0\r\n\r"[..]),
            Some(b"\n")
        ]);

        let mut stream = HttpStream::new(test_resp);

        let resp = match stream.poll_for_response().unwrap() {
            Async::Ready(resp) => resp,
            c => panic!("Unexpected res: {:?}", c),
        };

        assert_eq!(resp.code, 200);
        assert_eq!(resp.content_type, None);
        assert_eq!(&resp.data[..], b"Hello");
    }

    #[test]
    fn chunked_response_slow() {
        let test_resp = TestReader::new(vec![
            Some(&b"HTTP/1.1 200 Ok"[..]),
            Some(b"\r\nTransfer-Encoding: chunked\r\n\r"),
            Some(b"\n1\r\nH\r\n4\r"),
            Some(b"\nello\r\n0\r\n\r"),
            Some(b"\n")
        ]);

        let mut stream = HttpStream::new(test_resp);

        let resp = match stream.poll_for_response().unwrap() {
            Async::Ready(resp) => resp,
            c => panic!("Unexpected res: {:?}", c),
        };

        assert_eq!(resp.code, 200);
        assert_eq!(resp.content_type, None);
        assert_eq!(&resp.data[..], b"Hello");
    }

    #[test]
    fn chunked_response_slow_block() {
        let test_resp = TestReader::new(vec![
            Some(&b"HTTP/1.1 200 Ok"[..]),
            None,
            Some(b"\r\nTransfer-Encoding: chunked\r\n\r"),
            None,
            Some(b"\n1\r\nH\r\n4\r"),
            None,
            Some(b"\nello\r\n0\r\n\r"),
            None,
            Some(b"\n")
        ]);

        let mut stream = HttpStream::new(test_resp);

        loop {
            match stream.poll_for_response().unwrap() {
                Async::Ready(resp) => {
                    assert_eq!(resp.code, 200);
                    assert_eq!(resp.content_type, None);
                    assert_eq!(&resp.data[..], b"Hello");
                    break;
                },
                Async::NotReady => {}
            };
        }
    }

    #[test]
    fn mutliple() {
        let test_resp = Cursor::new(b"HTTP/1.1 200 Ok\r\nContent-Length: 5\r\n\r\nHelloHTTP/1.1 200 Ok\r\nContent-Length: 5\r\n\r\nHello".to_vec());

        let mut stream = HttpStream::new(test_resp);

        for _ in &[0, 1] {
            let resp = match stream.poll_for_response().unwrap() {
                Async::Ready(resp) => resp,
                c => panic!("Unexpected res: {:?}", c),
            };

            assert_eq!(resp.code, 200);
            assert_eq!(resp.content_type, None);
            assert_eq!(&resp.data[..], b"Hello");
        }
    }
}
