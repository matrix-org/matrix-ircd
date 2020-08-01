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

use serde_json;

use std::boxed::Box;
use std::io;
use std::ops::DerefMut;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::Context;

use super::protocol::SyncResponse;

use url::Url;

use futures::prelude::{Future, TryFuture};
use futures::task::Poll;

use crate::{http, ConnectionContext};

pub struct MatrixSyncClient {
    url: Url,
    access_token: String,
    next_token: Option<String>,
    http_client: http::ClientWrapper,
    current_sync: Mutex<RequestStatus>,
    ctx: ConnectionContext,
}

enum RequestStatus {
    MadeRequest(hyper::client::ResponseFuture),
    ConcatenatingResponse(
        Pin<Box<dyn Future<Output = Result<hyper::body::Bytes, hyper::Error>> + Send>>,
    ),
    NoRequest,
}

impl MatrixSyncClient {
    pub fn new(base_url: &Url, access_token: String, ctx: ConnectionContext) -> MatrixSyncClient {
        MatrixSyncClient {
            url: base_url.join("/_matrix/client/r0/sync").unwrap(),
            access_token,
            next_token: None,
            http_client: http::ClientWrapper::new(),
            current_sync: Mutex::new(RequestStatus::NoRequest),
            ctx,
        }
    }

    pub fn poll_sync(&mut self, cx: &mut Context<'_>) -> Poll<Result<SyncResponse, io::Error>> {
        task_trace!(self.ctx, "Polled sync");
        loop {
            let mut current_sync = self.current_sync.lock().unwrap();

            match &mut current_sync.deref_mut() {
                // There is currently no active request to the matrix server, so we make one
                RequestStatus::NoRequest => {
                    let new_request_status = self.make_request();
                    *current_sync = new_request_status;
                    continue;
                }
                // We have made a request to the server, now we parse the result
                RequestStatus::MadeRequest(request) => {
                    let request = Pin::new(request);
                    // poll the response from the server to get the body
                    let response = match request.try_poll(cx) {
                        Poll::Ready(Ok(r)) => r,
                        Poll::Ready(Err(e)) => {
                            task_info!(self.ctx, "Error doing sync"; "error" => format!("{}", e));
                            *current_sync = RequestStatus::NoRequest;
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

                    *current_sync = RequestStatus::ConcatenatingResponse(pin_bytes);
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

                    task_trace!(self.ctx, "Got sync response"; "next_token" => sync_response.next_batch.clone());
                    self.next_token = Some(sync_response.next_batch.clone());
                    return Poll::Ready(Ok(sync_response));
                }
            };
        }
    }
    fn make_request(&self) -> RequestStatus {
        let mut url = self.url.clone();
        url.query_pairs_mut()
            .clear()
            .append_pair("access_token", &self.access_token)
            .append_pair("filter", r#"{"presence":{"not_types":["m.presence"]}}"#)
            .append_pair("timeout", "30000");

        if let Some(ref token) = self.next_token {
            url.query_pairs_mut().append_pair("since", token);
        }

        let request = hyper::Request::builder()
            .method("GET")
            .uri(url.as_str().parse::<hyper::Uri>().unwrap())
            .body(hyper::Body::empty())
            .expect("request could not be built");

        let response = self.http_client.send_request(request);
        RequestStatus::MadeRequest(response)
    }
}

impl std::fmt::Debug for MatrixSyncClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixSyncClient")
            .field("url", &self.url)
            .field("access_token", &self.access_token)
            .field("next_token", &self.next_token)
            .finish()
    }
}

impl futures::prelude::Stream for MatrixSyncClient {
    type Item = Result<SyncResponse, io::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> futures::task::Poll<Option<Self::Item>> {
        task_trace!(self.ctx, "Matrix Sync Polled");

        match self.poll_sync(cx) {
            Poll::Ready(response) => Poll::Ready(Some(response)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MatrixSyncClient;
    use futures::stream::StreamExt;
    use mockito::{mock, Matcher};

    #[tokio::test]
    async fn matrix_sync_request() {
        let base_url = mockito::server_url().as_str().parse::<url::Url>().unwrap();
        let access_token = "sample_access_token";

        let ctx = crate::ConnectionContext::testing_context();

        let client = MatrixSyncClient::new(&base_url, access_token.to_string(), ctx);

        let mock_req = mock("GET", "/_matrix/client/r0/sync")
            .with_status(200)
            // check queries added to the http request in MatrixSyncClient::poll_sync()
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("access_token".to_string(), access_token.to_string()),
                Matcher::UrlEncoded(
                    "filter".to_string(),
                    r#"{"presence":{"not_types":["m.presence"]}}"#.to_string(),
                ),
                Matcher::UrlEncoded("timeout".to_string(), "30000".to_string()),
            ]))
            .create();

        // run the future to completion. The future will error since invalid json is
        // returned, but as long as the call is correct, the error is outside the scope of this
        // test
        client.into_future().await;

        mock_req.assert();
    }
}
