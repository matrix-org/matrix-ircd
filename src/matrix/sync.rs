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

use futures::{Async, Future, Poll};
use futures::stream::Stream;

use serde_json;

use std::{self, mem, io, str};
use std::io::{Read, Write};
use std::str::FromStr;

use super::protocol::SyncResponse;

use tokio_core::reactor::Handle;
use tokio_core::net::TcpStream;

use url::Url;

use httparse;


enum SyncState {
    Connected(HttpStream<TcpStream>),
    Connecting(Box<Future<Item=TcpStream, Error=io::Error>>),
}

impl SyncState {
    pub fn poll(&mut self) -> Poll<&mut HttpStream<TcpStream>, io::Error> {
        let socket = match *self {
            SyncState::Connecting(ref mut future)  => {
                try_ready!(future.poll())
            }
            SyncState::Connected(ref mut stream) => return Ok(Async::Ready(stream)),
        };

        *self = SyncState::Connected(HttpStream::new(socket));

        self.poll()
    }
}


pub struct MatrixSyncClient {
    url: Url,
    access_token: String,
    next_token: Option<String>,
    sync_state: SyncState,
    write_buffer: Vec<u8>,
}

impl MatrixSyncClient {
    pub fn new(handle: Handle, base_url: &Url, access_token: String) -> MatrixSyncClient {
        let mut client = MatrixSyncClient {
            url: base_url.join("/_matrix/client/r0/sync").unwrap(),
            access_token: access_token,
            next_token: None,
            sync_state: SyncState::Connecting(TcpStream::connect(&"212.71.233.145:80".parse().unwrap(), &handle).boxed()),
            write_buffer: Vec::with_capacity(4096),
        };

        client.write_request();

        client
    }

    pub fn write_request(&mut self) {
        self.url
            .query_pairs_mut()
            .clear()
            .append_pair("access_token", &self.access_token)
            .append_pair("filter", r#"{"presence":{"not_types":["m.presence"]}}"#)
            .append_pair("timeout", "30000");

        if let Some(ref token) = self.next_token {
            self.url.query_pairs_mut().append_pair("since", token);
        }

        write!(self.write_buffer, "GET {}?{} HTTP/1.1\r\n", self.url.path(), self.url.query().unwrap_or("")).unwrap();
        write!(self.write_buffer, "Content-Length: 0\r\n").unwrap();
        write!(self.write_buffer, "Host: {}\r\n", &self.url.host().unwrap()).unwrap();
        write!(self.write_buffer, "\r\n").unwrap();

    }

    pub fn poll_sync(&mut self) -> Poll<SyncResponse, io::Error> {
        loop {
            let sync_response: SyncResponse = {
                let mut http_stream = try_ready!(self.sync_state.poll());

                if !self.write_buffer.is_empty() {
                    task_trace!("Writing buffers");
                    let written = http_stream.get_inner_mut().write(&self.write_buffer)?;
                    self.write_buffer.drain(..written);
                }

                let response = try_ready!(http_stream.poll()).expect("sync stream returned None"); // TODO: Less unwrap.

                if response.code != 200 {
                    return Err(io::Error::new(io::ErrorKind::Other, format!("Sync returned {}", response.code)));
                }

                serde_json::from_slice(&response.data).map_err(|e| {
                    io::Error::new(io::ErrorKind::Other, format!("Sync returned invalid JSON: {}", e))
                })?
            };

            task_trace!("Got sync response"; "next_token" => sync_response.next_batch);

            self.next_token = Some(sync_response.next_batch.clone());

            self.write_request();

            return Ok(Async::Ready(sync_response))
        }
    }
}

impl Stream for MatrixSyncClient {
    type Item = SyncResponse;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<SyncResponse>, io::Error> {
        task_trace!("Matrix Sync Polled");

        let res = try_ready!(self.poll_sync());
        Ok(Async::Ready(Some(res)))
    }
}



#[derive(Debug, Default, PartialEq, Eq)]
pub struct Response {
    code: u16,
    content_type: Option<String>,
    data: Vec<u8>,
}


#[derive(Debug, Clone, Copy)]
enum HttpStreamState {
    Headers,
    RawData(Option<usize>),
    ChunkedData,
}


#[derive(Debug)]
pub struct HttpStream<Stream: Read> {
    stream: Stream,
    response_buffer: Vec<u8>,
    curr_state: HttpStreamState,
    partial_response: Response,
}

impl<S: Read> HttpStream<S> {
    pub fn new(stream: S) -> HttpStream<S> {
        HttpStream {
            stream: stream,
            response_buffer: Vec::with_capacity(4096),
            curr_state: HttpStreamState::Headers,
            partial_response: Response::default(),
        }
    }

    pub fn get_inner_mut(&mut self) -> &mut S {
        &mut self.stream
    }
}

impl<S: Read> Stream for HttpStream<S> {
    type Item = Response;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Response>, io::Error> {
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
                            return Ok(Async::Ready(Some(resp)));
                        }
                    }

                    let num_bytes = try_ready!(read_into_vec(stream, &mut self.response_buffer));
                    if num_bytes == 0 {
                        if let HttpStreamState::RawData(None) = self.curr_state {
                            self.curr_state = HttpStreamState::Headers;
                            let resp = mem::replace(&mut self.partial_response, Response::default());
                            return Ok(Async::Ready(Some(resp)));
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
                                return Ok(Async::Ready(Some(resp)));
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
    use futures::stream::Stream;
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

        let resp = match stream.poll().unwrap() {
            Async::Ready(Some(resp)) => resp,
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

        let resp = match stream.poll().unwrap() {
            Async::Ready(Some(resp)) => resp,
            c => panic!("Unexpected res: {:?}", c),
        };

        assert_eq!(resp.code, 200);
        assert_eq!(resp.content_type, None);
        assert_eq!(&resp.data[..], b"Hello");

        assert!(stream.poll().is_err());
    }

    #[test]
    fn basic_chunked_response() {
        let test_resp = Cursor::new(
            b"HTTP/1.1 200 Ok\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nH\r\n4\r\nello\r\n0\r\n\r\n"
            .to_vec()
        );
        let mut stream = HttpStream::new(test_resp);

        let resp = match stream.poll().unwrap() {
            Async::Ready(Some(resp)) => resp,
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

        let resp = match stream.poll().unwrap() {
            Async::Ready(Some(resp)) => resp,
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

        let resp = match stream.poll().unwrap() {
            Async::Ready(Some(resp)) => resp,
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
            match stream.poll().unwrap() {
                Async::Ready(Some(resp)) => {
                    assert_eq!(resp.code, 200);
                    assert_eq!(resp.content_type, None);
                    assert_eq!(&resp.data[..], b"Hello");
                    break;
                },
                Async::NotReady => {}
                c => panic!("Unexpected res: {:?}", c),
            };
        }
    }

    #[test]
    fn mutliple() {
        let test_resp = Cursor::new(b"HTTP/1.1 200 Ok\r\nContent-Length: 5\r\n\r\nHelloHTTP/1.1 200 Ok\r\nContent-Length: 5\r\n\r\nHello".to_vec());

        let mut stream = HttpStream::new(test_resp);

        for _ in &[0, 1] {
            let resp = match stream.poll().unwrap() {
                Async::Ready(Some(resp)) => resp,
                c => panic!("Unexpected res: {:?}", c),
            };

            assert_eq!(resp.code, 200);
            assert_eq!(resp.content_type, None);
            assert_eq!(&resp.data[..], b"Hello");
        }
    }
}
