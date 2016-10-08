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

//! This main module responsible for keeping track of the Matrix side of the world.
//!
//! It knows nothing about IRC.

use futures::{Async, Future, Poll};
use futures::stream::Stream;

use std::collections::BTreeMap;
use std::io;

use tokio_core::reactor::Handle;

use url::Url;

use serde_json;
use serde::{Serialize, Deserialize};

use http::{Request, HttpStream};


pub mod protocol;
mod models;
mod sync;

pub use self::models::{Room, Member};


/// A single Matrix session.
///
/// A `MatrixClient` both send requests and outputs a Stream of `SyncResponse`'s. It also keeps track
/// of vaious
pub struct MatrixClient {
    url: Url,
    user_id: String,
    access_token: String,
    sync_client: sync::MatrixSyncClient,
    rooms: BTreeMap<String, Room>,
    http_stream: HttpStream
}

impl MatrixClient {
    pub fn new(handle: Handle, http_stream: HttpStream, base_url: &Url, user_id: String, access_token: String) -> MatrixClient {
        MatrixClient {
            url: base_url.clone(),
            user_id: user_id,
            access_token: access_token.clone(),
            sync_client: sync::MatrixSyncClient::new(handle, base_url, access_token),
            rooms: BTreeMap::new(),
            http_stream: http_stream,
        }
    }

    /// The user ID associated with this session.
    pub fn get_user_id(&self) -> &str {
        &self.user_id
    }

    /// Create a session by logging in with a user name and password.
    pub fn login(handle: Handle, base_url: Url, user: String, password: String) -> impl Future<Item=MatrixClient, Error=LoginError> {
        let host = base_url.host_str().expect("expected host in base_url").to_string();
        let port = base_url.port_or_known_default().unwrap();

        let mut http_stream = HttpStream::new(host, port, handle.clone());

        let future = do_json_post(&mut http_stream, &base_url.join("/_matrix/client/r0/login").unwrap(), &protocol::LoginPasswordInput {
            user: user,
            password: password,
            login_type: "m.login.password".into(),
        }).map_err(move |err| {
            match err {
                JsonPostError::Io(e) => LoginError::Io(e),
                JsonPostError::ErrorRepsonse(code) if code == 401 || code == 403 => {
                    LoginError::InvalidPassword
                }
                JsonPostError::ErrorRepsonse(code) => LoginError::Io(
                    io::Error::new(io::ErrorKind::Other, format!("Got {} response", code))
                )
            }
        });

        WrappedHttp::new(
            future, http_stream
        ).map(move |(http_stream, response): (HttpStream, protocol::LoginResponse)| {
            MatrixClient::new(handle, http_stream, &base_url, response.user_id, response.access_token)
        })
    }

    pub fn send_text_message(&mut self, room_id: &str, body: String) -> impl Future<Item=protocol::RoomSendResponse, Error=io::Error> {
        let mut url = self.url.join(&format!("/_matrix/client/r0/rooms/{}/send/m.room.message", room_id)).unwrap();
        url.query_pairs_mut()
            .clear()
            .append_pair("access_token", &self.access_token);

        do_json_post(&mut self.http_stream, &url, &protocol::RoomSendInput {
            body: body,
            msgtype: "m.text".into(),
        })
        .map_err(JsonPostError::into_io_error)
    }

    pub fn get_room(&self, room_id: &str) -> Option<&Room> {
        self.rooms.get(room_id)
    }

    fn poll_sync(&mut self) -> Poll<Option<protocol::SyncResponse>, io::Error> {
        let resp = try_ready!(self.sync_client.poll());
        if let Some(ref sync_response) = resp {
            for (room_id, sync) in &sync_response.rooms.join {
                if let Some(mut room) = self.rooms.get_mut(room_id) {
                    room.update_from_sync(sync);
                    continue
                }
                self.rooms.insert(room_id.clone(), Room::from_sync(room_id.clone(), sync));
            }
        }

        Ok(Async::Ready(resp))
    }
}


fn do_json_post<I: Serialize, O: Deserialize>(stream: &mut HttpStream, url: &Url, input: &I)
    -> impl Future<Item=O, Error=JsonPostError>
{
    stream.send_request(&Request {
        method: "POST",
        path: &format!("{}?{}", url.path(), url.query().unwrap_or("")),
        headers: &[("Content-Type", "application/json")],
        body: &serde_json::to_vec(input).expect("input to be valid json"),
    })
    .map_err(|_| io::Error::new(io::ErrorKind::Other, "request future unexpectedly cancelled").into())
    .and_then(move |resp| {
        if resp.code != 200 {
            return Err(JsonPostError::ErrorRepsonse(resp.code));
        }

        serde_json::from_slice(&resp.data)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid json response").into())
    })
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
        ErrorRepsonse(code: u16) {
            description("received non 200 response")
            display("Received response: {}", code)
        }
    }
}

impl JsonPostError {
    pub fn into_io_error(self) -> io::Error {
        match self {
            JsonPostError::Io(err) => err,
            JsonPostError::ErrorRepsonse(code) => {
                io::Error::new(io::ErrorKind::Other, format!("Received {} response", code))
            }
        }
    }
}

quick_error! {
    #[derive(Debug)]
    pub enum LoginError {
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
    }
}


impl Stream for MatrixClient {
    type Item = protocol::SyncResponse;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<protocol::SyncResponse>, io::Error> {
        task_trace!("Polled matrix client");
        self.http_stream.poll()?;
        self.poll_sync()
    }
}



struct WrappedHttp<T, E> {
    future: Box<Future<Item=T, Error=E>>,
    http_stream: Option<HttpStream>,
}

impl<T, E> WrappedHttp<T, E> {
    pub fn new<F: Future<Item=T, Error=E> + 'static>(future: F, http_stream: HttpStream) -> WrappedHttp<T, E> {
        WrappedHttp {
            future: Box::new(future),
            http_stream: Some(http_stream),
        }
    }
}

impl<T, E: From<io::Error>> Future for WrappedHttp<T, E> {
    type Item = (HttpStream, T);
    type Error = E;

    fn poll(&mut self) -> Poll<(HttpStream, T), E> {
        task_trace!("WrappedHttp polled");

        let mut http_stream = self.http_stream.take().expect("WrappedHttp cannot be repolled");

        http_stream.poll()?;

        let res = match self.future.poll()? {
            Async::Ready(res) => res,
            Async::NotReady => {
                self.http_stream = Some(http_stream);
                return Ok(Async::NotReady);
            }
        };

        Ok(Async::Ready((http_stream, res)))
    }
}
