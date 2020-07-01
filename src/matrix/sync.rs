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

use serde_json;

use std::io;

use super::protocol::SyncResponse;

use tokio_core::reactor::Handle;

use url::Url;

use crate::http::{HttpClient, HttpResponseFuture, Request};

use slog::*;

pub struct MatrixSyncClient {
    url: Url,
    access_token: String,
    next_token: Option<String>,
    http_stream: HttpClient,
    current_sync: Option<HttpResponseFuture>,
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
            http_stream: HttpClient::new(host.into(), port, tls, handle),
            current_sync: None,
        }
    }

    pub fn poll_sync(&mut self) -> Poll<SyncResponse, io::Error> {
        task_trace!("Polled sync");
        loop {
            let sync_response: SyncResponse = {
                let http_stream = &mut self.http_stream;

                let mut current_sync = if let Some(current_sync) = self.current_sync.take() {
                    current_sync
                } else {
                    self.url
                        .query_pairs_mut()
                        .clear()
                        .append_pair("access_token", &self.access_token)
                        .append_pair("filter", r#"{"presence":{"not_types":["m.presence"]}}"#)
                        .append_pair("timeout", "30000");

                    if let Some(ref token) = self.next_token {
                        self.url.query_pairs_mut().append_pair("since", token);
                    }

                    self.current_sync = Some(http_stream.send_request(Request {
                        method: "GET",
                        path: format!("{}?{}", self.url.path(), self.url.query().unwrap_or("")),
                        headers: vec![],
                        body: vec![],
                    }));
                    continue;
                };

                let response = match current_sync.poll() {
                    Ok(Async::Ready(r)) => r,
                    Ok(Async::NotReady) => {
                        self.current_sync = Some(current_sync);
                        return Ok(Async::NotReady);
                    }
                    Err(e) => {
                        task_info!("Error doing sync"; "error" => format!("{}", e));
                        self.current_sync = None;
                        continue;
                    }
                };

                if response.code != 200 {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("Sync returned {}", response.code),
                    ));
                }

                serde_json::from_slice(&response.data).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::Other,
                        format!("Sync returned invalid JSON: {}", e),
                    )
                })?
            };

            task_trace!("Got sync response"; "next_token" => sync_response.next_batch.clone());

            self.next_token = Some(sync_response.next_batch.clone());

            return Ok(Async::Ready(sync_response));
        }
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

impl Stream for MatrixSyncClient {
    type Item = SyncResponse;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<SyncResponse>, io::Error> {
        task_trace!("Matrix Sync Polled");

        let res = futures::try_ready!(self.poll_sync());
        Ok(Async::Ready(Some(res)))
    }
}

#[cfg(test)]
mod tests {
    use super::MatrixSyncClient;
    use futures::Stream;
    use mockito::{mock, Matcher};

    #[test]
    fn matrix_sync_request() {
        let base_url = mockito::server_url().as_str().parse::<url::Url>().unwrap();
        let access_token = "sample_access_token";
        let mut core = tokio_core::reactor::Core::new().expect("could not create a tokio core");
        let handle = core.handle();

        let client = MatrixSyncClient::new(handle.clone(), &base_url, access_token.to_string());

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
        if let Err(e) = core.run(client.into_future()) {
            println! {"MatrixSyncClient returned an error: {:?}", e}
        }

        mock_req.assert();
    }
}
