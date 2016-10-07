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

use futures::{self, Async, Poll};

use std::collections::VecDeque;
use std::{self, mem, io, str};
use std::io::{Read, Write};
use std::str::FromStr;


use httparse;
use netbuf;


#[derive(Debug, Default, PartialEq, Eq)]
pub struct Response {
    pub code: u16,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub host: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
}


#[derive(Debug, Clone, Copy)]
enum HttpStreamState {
    Headers,
    RawData(Option<usize>),
    ChunkedData,
}


pub struct HttpStream<Stream: Read> {
    stream: Stream,
    response_buffer: Vec<u8>,
    write_buffer: netbuf::Buf,
    curr_state: HttpStreamState,
    partial_response: Response,
    requests: VecDeque<futures::Complete<Response>>,
}

impl<S: Read> HttpStream<S> {
    pub fn new(stream: S) -> HttpStream<S> {
        HttpStream {
            stream: stream,
            response_buffer: Vec::with_capacity(4096),
            write_buffer: netbuf::Buf::new(),
            curr_state: HttpStreamState::Headers,
            partial_response: Response::default(),
            requests: VecDeque::new(),
        }
    }

    pub fn send_request(&mut self, request: &Request) -> futures::Oneshot<Response> {
        write!(self.write_buffer, "{} {} HTTP/1.1\r\n", request.method, request.path).unwrap();
        write!(self.write_buffer, "Host: {}\r\n", request.host).unwrap();
        write!(self.write_buffer, "Content-Length: {}\r\n", request.body.len()).unwrap();
        for &(name, value) in request.headers {
            write!(self.write_buffer, "{}: {}\n\n", name, value).unwrap();
        }
        write!(self.write_buffer, "\r\n").unwrap();
        self.write_buffer.extend(request.body);

        let (c, o) = futures::oneshot();

        self.requests.push_back(c);

        o
    }

    pub fn poll(&mut self) -> Poll<(), io::Error> {
        loop {
            let resp = try_ready!(self.poll_for_response());
            if let Some(future) = self.requests.pop_front() {
                future.complete(resp);
            } else {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Response without request"));
            }
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
                            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Unexpected EOF"));
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
                        self.response_buffer.drain(..bytes_read);
                    } else {
                        let num_bytes = try_ready!(read_into_vec(stream, &mut self.response_buffer));
                        if num_bytes == 0 {
                            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Unexpected EOF"));
                        }
                    }
                }
                HttpStreamState::RawData(len_opt) => {
                    if let Some(len) = len_opt {
                        if self.response_buffer.len() >= len {
                            if self.response_buffer.len() == len {
                                mem::swap(&mut self.response_buffer, &mut self.partial_response.data);
                            } else {
                                self.partial_response.data = self.response_buffer.drain(..len).collect();
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
                            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Unexpected EOF"));
                        }
                    }
                }
                HttpStreamState::ChunkedData => {
                    println!("Got chunk: {:?}", &self.response_buffer);
                    match httparse::parse_chunk_size(&self.response_buffer[..]) {
                        Ok(httparse::Status::Complete((bytes_read, 0))) => {
                            println!("Got chunk: {} {}", bytes_read, 0);

                            // +2 as chunks end in \r\n
                            if self.response_buffer.len() >= bytes_read + 2 {
                                self.response_buffer.drain(..bytes_read + 2);

                                self.curr_state = HttpStreamState::Headers;
                                let resp = mem::replace(&mut self.partial_response, Response::default());
                                return Ok(Async::Ready(resp));
                            }
                        }
                        Ok(httparse::Status::Complete((bytes_read, chunk_len_64))) => {
                            println!("Got chunk: {} {}", bytes_read, chunk_len_64);
                            let chunk_len = chunk_len_64 as usize;

                            // +2 as chunks end in \r\n
                            if self.response_buffer.len() >= bytes_read + chunk_len + 2 {
                                self.partial_response.data.extend_from_slice(
                                    &self.response_buffer[bytes_read..bytes_read + chunk_len]
                                );
                                self.response_buffer.drain(..bytes_read + chunk_len + 2);
                                continue;
                            }
                        }
                        Ok(httparse::Status::Partial) => {}
                        Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "HTTP invalid chunked body")),
                    }

                    let num_bytes = try_ready!(read_into_vec(stream, &mut self.response_buffer));
                    if num_bytes == 0 {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Unexpected EOF"));
                    }
                }
            }
        }
    }
}



fn read_into_vec<R: io::Read>(stream: &mut R, vec: &mut Vec<u8>) -> Poll<usize, io::Error> {
    let start_len = vec.len();
    let new_size = std::cmp::max(start_len + 1024, 4096);
    vec.resize(start_len + new_size, 0);
    match stream.read(&mut vec[start_len..]) {
        Ok(num_bytes) => {
            vec.resize(start_len + num_bytes, 0);
            Ok(Async::Ready(num_bytes))
        }
        Err(e) => {
            vec.resize(start_len, 0);
            if e.kind() == io::ErrorKind::WouldBlock {
                Ok(Async::NotReady)
            } else {
                Err(e)
            }
        }
    }
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
