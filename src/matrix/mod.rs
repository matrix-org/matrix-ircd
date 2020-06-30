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

//! The main module responsible for keeping track of the Matrix side of the world.
//!
//! It knows nothing about IRC.

use futures3::prelude::Stream;
use futures3::task::Poll;

use std::boxed::Box;
use std::collections::BTreeMap;
use std::io;
use std::pin::Pin;
use std::task::Context;

use url::percent_encoding::{percent_encode, PATH_SEGMENT_ENCODE_SET};
use url::Url;

use serde_json;

use rand::{thread_rng, Rng};

use crate::http;
use regex::Regex;

use quick_error::quick_error;

mod models;
pub mod protocol;
mod sync;

pub use self::models::{Member, Room};


/// A single Matrix session.
///
/// A `MatrixClient` both send requests and outputs a Stream of `SyncResponse`'s. It also keeps track
/// of vaious
pub struct MatrixClient {
    url: Url,
    user_id: String,
    access_token: String,
    sync_client: Pin<Box<sync::MatrixSyncClient>>,
    rooms: BTreeMap<String, Room>,
    http_client: http::ClientWrapper,
}

impl MatrixClient {
    pub fn new(
        http_client: http::ClientWrapper,
        base_url: &Url,
        user_id: String,
        access_token: String,
    ) -> MatrixClient {
        MatrixClient {
            url: base_url.clone(),
            user_id,
            access_token: access_token.clone(),
            sync_client: Box::pin(sync::MatrixSyncClient::new(base_url, access_token)),
            rooms: BTreeMap::new(),
            http_client,
        }
    }

    /// The user ID associated with this session.
    pub fn get_user_id(&self) -> &str {
        &self.user_id
    }

    /// Create a session by logging in with a user name and password.
    pub(crate) async fn login(
        base_url: Url,
        user: String,
        password: String,
    ) -> Result<MatrixClient, Error> {
        let host = base_url
            .host_str()
            .expect("expected host in base_url")
            .to_string();
        let port = base_url.port_or_known_default().unwrap();
        let tls = match base_url.scheme() {
            "http" => false,
            "https" => true,
            _ => panic!("Unrecognized scheme {}", base_url.scheme()),
        };

        let mut http_client = http::ClientWrapper::new();

        let login = protocol::LoginPasswordInput {
            user,
            password,
            login_type: "m.login.password".to_string(),
        };
        let json_bytes = serde_json::to_vec(&login).expect("could not serialize login to json");
        let body = json_bytes.into();

        let request = hyper::Request::builder()
            .method("POST")
            .uri(
                base_url
                    .join("/_matrix/client/r0/login")
                    .expect("could not join login path")
                    .as_str()
                    .parse::<hyper::Uri>()
                    .unwrap(),
            )
            .body(body)
            .expect("request could not be built");

        let login = http_client.send_request(request).await?;

        let status_code = login.status().as_u16();
        if status_code == 401 || status_code == 403 {
            return Err(JsonPostError::ErrorResponse(status_code)
                .into_io_error()
                .into());
        }

        let body_bytes = hyper::body::to_bytes(login.into_body()).await?;
        let login_response: protocol::LoginResponse = serde_json::from_slice(&body_bytes)?;

        let matrix_client = MatrixClient::new(
            http_client,
            &base_url,
            login_response.user_id,
            login_response.access_token,
        );
        Ok(matrix_client)
    }

    pub(crate) async fn send_text_message(
        &mut self,
        room_id: &str,
        body: String,
    ) -> Result<protocol::RoomSendResponse, Error> {
        let msg_id = thread_rng().gen::<u16>();
        let mut url = self
            .url
            .join(&format!(
                "/_matrix/client/r0/rooms/{}/send/m.room.message/mircd-{}",
                room_id, msg_id
            ))
            .expect("Unable to construct a valid API url");

        url.query_pairs_mut()
            .clear()
            .append_pair("access_token", &self.access_token);

        let body = serde_json::to_vec(&protocol::RoomSendInput {
            body,
            msgtype: "m.text".to_string(),
        })?
        .into();

        let request = hyper::Request::builder()
            .method("PUT")
            .uri(url.as_str().parse::<hyper::Uri>().unwrap())
            .body(body)
            .expect("request could not be built");

        // TODO: improve this error handling
        let response = self.http_client.send_request(request).await?;
        let bytes = hyper::body::to_bytes(response.into_body()).await?;

        let json_response = serde_json::from_slice(&bytes)?;
        Ok(json_response)
    }

    pub async fn join_room(&mut self, room_id: &str) -> Result<protocol::RoomJoinResponse, Error> {
        let roomid_encoded = percent_encode(room_id.as_bytes(), PATH_SEGMENT_ENCODE_SET);
        let mut url = self
            .url
            .join(&format!("/_matrix/client/r0/join/{}", roomid_encoded))
            .expect("Unable to construct a valid API url");

        url.query_pairs_mut()
            .clear()
            .append_pair("access_token", &self.access_token);

        let json_bytes = serde_json::to_vec(&protocol::RoomJoinInput {})?;

        let request = hyper::Request::builder()
            .method("POST")
            .uri(url.as_str().parse::<hyper::Uri>().unwrap())
            .body(json_bytes.into())
            .expect("request could not be built");

        let response = self.http_client.send_request(request).await?;
        let bytes = hyper::body::to_bytes(response.into_body()).await?;
        let json_response = serde_json::from_slice(&bytes)?;
        Ok(json_response)
    }

    pub fn get_room(&self, room_id: &str) -> Option<&Room> {
        self.rooms.get(room_id)
    }

    pub fn media_url(&self, url: &str) -> String {
        lazy_static::lazy_static! {
            static ref RE: Regex = Regex::new("^mxc://([^/]+/[^/]+)$").unwrap();
        }
        if let Some(captures) = RE.captures(url) {
            self.url
                .join("/_matrix/media/v1/download/")
                .unwrap()
                .join(&captures[1])
                .unwrap()
                .into_string()
        } else {
            url.to_owned()
        }
    }

    fn poll_sync(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<Option<Result<protocol::SyncResponse, Error>>> {
        let resp = match self.sync_client.as_mut().poll_next(cx)? {
            Poll::Ready(x) => x,
            Poll::Pending => return Poll::Pending,
        };

        if let Some(mut sync_response) = resp {
            for (room_id, sync) in &mut sync_response.rooms.join {
                sync.timeline.events.retain(|ev| {
                    !ev.unsigned
                        .transaction_id
                        .as_ref()
                        .map(|txn_id| txn_id.starts_with("mircd-"))
                        .unwrap_or(false)
                });

                if let Some(room) = self.rooms.get_mut(room_id) {
                    room.update_from_sync(sync);
                    continue;
                }

                // We can't put this in an else because of the mutable borrow in the if condition.
                self.rooms
                    .insert(room_id.clone(), Room::from_sync(room_id.clone(), sync));
            }
            Poll::Ready(Some(Ok(sync_response)))
        } else {
            Poll::Ready(None)
        }
    }
}

quick_error! {
    #[derive(Debug)]
    enum JsonPostError {
        Io(err: io::Error) {
            from()
            description("io error")
            display("I/O error: {}", err)
            cause(err)
        }
        ErrorResponse(code: u16) {
            description("received non 200 response")
            display("Received response: {}", code)
        }
    }
}

impl JsonPostError {
    pub fn into_io_error(self) -> io::Error {
        match self {
            JsonPostError::Io(err) => err,
            JsonPostError::ErrorResponse(code) => {
                io::Error::new(io::ErrorKind::Other, format!("Received {} response", code))
            }
        }
    }
}

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Io(err: io::Error) {
            from()
            description("io error")
            display("I/O error: {}", err)
            cause(err)
        }
        InvalidPassword {
            description("password was invalid")
            display("Password is invalid")
        }
        HyperErr(err: hyper::Error) {
            from()
            description("hyper error when making http request")
            display("Hyper error: {}", err)
        }
        SerdeErr(err: serde_json::Error) {
            from()
            description("could not serialize / deserialize struct")
            display("Could not serialize request struct or deserialize response")
        }
    }
}


impl Stream for MatrixClient {
    type Item = Result<protocol::SyncResponse, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        task_trace!("Polled matrix client");
        self.poll_sync(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::MatrixClient;
    use mockito::{mock, Matcher, Matcher::UrlEncoded};
    #[tokio::test]
    async fn matrix_login() {
        let base_url = mockito::server_url().as_str().parse::<url::Url>().unwrap();
        let username = "sample_username".to_string();
        let password = "sample_password".to_string();

        let mock_req = mock("POST", "/_matrix/client/r0/login?")
            .with_header("content-type", "application/json")
            .with_status(200)
            .create();

        // run the future to completion. The future will error since invalid json is
        // returned, but as long as the call is correct the error is outside the scope of this
        // test. It is explicitly handled here in case the mock assert panics.
        if let Err(e) = MatrixClient::login(base_url, username, password).await {
            println! {"MatrixSyncClient returned an error: {:?}", e}
        }

        // give the executor some time to execute the http request on a thread pool
        std::thread::sleep(std::time::Duration::from_millis(200));

        mock_req.assert();
    }

    #[tokio::test]
    async fn send_text_message() {
        let base_url = mockito::server_url().as_str().parse::<url::Url>().unwrap();
        let user_id = "sample_user_id".to_string();
        let token = "sample_token".to_string();
        let room_id = "room_id";

        let mock_req = mock(
            "PUT",
            Matcher::Regex(
                r"^/_matrix/client/r0/rooms/(.+)/send/m.room.message/mircd-(\d+)$".to_string(),
            ),
        )
        .match_query(UrlEncoded("access_token".to_string(), token.clone()))
        .with_status(200)
        .create();

        let httpclient = crate::http::ClientWrapper::new();

        let mut client = MatrixClient::new(httpclient, &base_url, user_id, token);

        // run the future to completion. The future will error since invalid json is
        // returned, but as long as the call is correct the error is outside the scope of this
        // test. It is explicitly handled here in case the mock assert panics.
        if let Err(e) = client
            .send_text_message(room_id, "sample_body".to_string())
            .await
        {
            println! {"MatrixSyncClient returned an error: {:?}", e}
        }

        // give the executor some time to execute the http request on a thread pool
        std::thread::sleep(std::time::Duration::from_millis(200));

        mock_req.assert();
    }
}
