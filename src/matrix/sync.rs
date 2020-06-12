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

//use futures::stream::Stream;
//use futures::{Async, Future, Poll};

use serde_json;

use std::io;

use super::protocol::SyncResponse;

use tokio_core::reactor::Handle;

use std::pin::Pin;
use url::Url;

type CompatResponseFut = hyper::client::ResponseFuture;

use futures3::prelude::{Future, Stream, TryFuture};
use futures3::task::Poll;
use std::task::Context;

use crate::http;

pub struct MatrixSyncClient {
    url: Url,
    access_token: String,
    next_token: Option<String>,
    http_client: http::ClientWrapper,
    current_sync: RequestStatus,
}

use std::boxed::Box;
enum RequestStatus {
    MadeRequest(CompatResponseFut),
    ConcatenatingResponse(Pin<Box<dyn Future<Output = Result<hyper::body::Bytes, hyper::Error>>>>),
    NoRequest,
}

impl MatrixSyncClient {
    pub fn new(handle: Handle, base_url: &Url, access_token: String) -> MatrixSyncClient {
        let host = base_url.host_str().expect("expected host in base_url");
        let port = base_url.port_or_known_default().unwrap();

        let tls = match base_url.scheme() {
            "http" => false,
            "https" => true,
            _ => panic!("Unrecognized scheme {}", base_url.scheme()),
        };

        MatrixSyncClient {
            url: base_url.join("/_matrix/client/r0/sync").unwrap(),
            access_token,
            next_token: None,
            http_client: http::ClientWrapper::new(),
            current_sync: RequestStatus::NoRequest,
        }
    }

    pub fn poll_sync(&mut self, cx: &mut Context<'_>) -> Poll<Result<SyncResponse, io::Error>> {
        task_trace!("Polled sync");
        loop {
            match &mut self.current_sync {
                // There is currently no active request to the matrix server, so we make one
                RequestStatus::NoRequest => {
                    self.make_request();
                    continue;
                }
                // We have made a request to the server, now we parse the result
                RequestStatus::MadeRequest(request) => {
                    let request = Pin::new(request);
                    // poll the response from the server to get the body
                    let response = match request.try_poll(cx) {
                        Poll::Ready(Ok(r)) => r,
                        Poll::Ready(Err(e)) => {
                            task_info!("Error doing sync"; "error" => format!("{}", e));
                            self.current_sync = RequestStatus::NoRequest;
                            continue;
                        }
                        Poll::Pending => return Poll::Pending,
                    };

                    // check response code to make sure the response was 200 Ok
                    if response.status() == hyper::StatusCode::OK {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("Sync returned {}", response.status().as_u16()),
                        )));
                    }

                    // transform the body into a future to contatenate the bytes
                    // Must pin the future to call poll() with futures 3.0
                    let bytes_fut = hyper::body::to_bytes(response.into_body());
                    let pin_bytes = Box::pin(bytes_fut);

                    self.current_sync = RequestStatus::ConcatenatingResponse(pin_bytes);
                    continue;
                }

                // We have finished making a request and parsing the response, now we parse the
                // body
                RequestStatus::ConcatenatingResponse(pin_bytes) => {
                    let bytes = match pin_bytes.as_mut().poll(cx) {
                        Poll::Ready(x) => x.unwrap(),
                        Poll::Pending => return Poll::Pending,
                    };

                    // Deserialize the body bytes into a json struct
                    let sync_response: SyncResponse =
                        serde_json::from_slice(&bytes).map_err(|e| {
                            io::Error::new(
                                io::ErrorKind::Other,
                                format!("Sync returned invalid JSON: {}", e),
                            )
                        })?;

                    task_trace!("Got sync response"; "next_token" => sync_response.next_batch.clone());
                    self.next_token = Some(sync_response.next_batch.clone());
                    return Poll::Ready(Ok(sync_response));
                }
            };
        }
    }
    fn make_request(&mut self) {
        self.url
            .query_pairs_mut()
            .clear()
            .append_pair("access_token", &self.access_token)
            .append_pair("filter", r#"{"presence":{"not_types":["m.presence"]}}"#)
            .append_pair("timeout", "30000");

        if let Some(ref token) = self.next_token {
            self.url.query_pairs_mut().append_pair("since", token);
        }

        let request = hyper::Request::builder()
            .method("GET")
            .uri(self.url.as_str().parse::<hyper::Uri>().unwrap())
            .body(hyper::Body::empty())
            .expect("request could not be built");

        let response = self.http_client.send_request(request);
        self.current_sync = RequestStatus::MadeRequest(response);
    }
}

impl Stream for MatrixSyncClient {
    type Item = Result<SyncResponse, io::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> futures3::task::Poll<Option<Self::Item>> {
        task_trace!("Matrix Sync Polled");

        match self.poll_sync(cx) {
            Poll::Ready(response) => Poll::Ready(Some(response)),
            Poll::Pending => Poll::Pending,
        }
    }
}
