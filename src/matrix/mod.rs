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

use curl::easy::{Easy, List};

use futures::{Async, Future, Poll};
use futures::stream::Stream;

use std::collections::BTreeMap;
use std::io;
use std::mem;
use std::sync::{Arc, Mutex};

use tokio_core::reactor::Handle;
use tokio_curl::Session;

use url::Url;

use serde_json;
use serde::{Serialize, Deserialize};

pub mod protocol;
mod models;
mod sync;

pub use self::models::{Room, Member};


/// A single Matrix session.
///
/// A MatrixClient both send requests and outputs a Stream of SyncResponse's. It also keeps track
/// of vaious
pub struct MatrixClient {
    url: Url,
    user_id: String,
    access_token: String,
    handle: Handle,
    sync_client: sync::MatrixSyncClient,
    rooms: BTreeMap<String, Room>,
}

impl MatrixClient {
    pub fn new(handle: Handle, base_url: &Url, user_id: String, access_token: String) -> MatrixClient {
        MatrixClient {
            url: base_url.clone(),
            user_id: user_id,
            access_token: access_token.clone(),
            handle: handle.clone(),
            sync_client: sync::MatrixSyncClient::new(handle, base_url, access_token),
            rooms: BTreeMap::new(),
        }
    }

    /// The user ID associated with this session.
    pub fn get_user_id(&self) -> &str {
        &self.user_id
    }

    /// Create a session by logging in with a user name and password.
    pub fn login(handle: Handle, base_url: Url, user: String, password: String) -> impl Future<Item=MatrixClient, Error=LoginError> {
        let session = Session::new(handle.clone());
        do_json_post(&session, &base_url.join("/_matrix/client/r0/login").unwrap(), &protocol::LoginPasswordInput {
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
        }).map(move |response: protocol::LoginResponse| {
            MatrixClient::new(handle, &base_url, response.user_id, response.access_token)
        })
    }

    pub fn send_text_message(&mut self, room_id: &str, body: String) -> impl Future<Item=protocol::RoomSendResponse, Error=io::Error> {
        let session = Session::new(self.handle.clone());
        let mut url = self.url.join(&format!("/_matrix/client/r0/rooms/{}/send/m.room.message", room_id)).unwrap();
        url
            .query_pairs_mut()
            .clear()
            .append_pair("access_token", &self.access_token);

        do_json_post(&session, &url, &protocol::RoomSendInput {
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


fn do_json_post<I: Serialize, O: Deserialize>(session: &Session, url: &Url, input: &I)
    -> impl Future<Item=O, Error=JsonPostError>
{
    let mut req = Easy::new();

    let mut list = List::new();
    list.append("Content-Type: application/json").unwrap();
    list.append("Connection: keep-alive").unwrap();

    let input_vec = serde_json::to_vec(input).expect("input to be valid json");

    req.post(true).unwrap();
    req.url(url.as_str()).unwrap();
    req.post_field_size(input_vec.len() as u64).unwrap();
    req.http_headers(list).unwrap();

    req.post_fields_copy(&input_vec).unwrap();

    let output_vec: Arc<Mutex<Vec<u8>>> = Arc::default();
    let output_vec_ptr = output_vec.clone();
    req.write_function(move |data| {
        output_vec.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }).unwrap();

    session.perform(req)
    .map_err(|err| err.into_error().into())
    .and_then(move |mut req| {

        let resp_code = req.response_code().unwrap();

        mem::drop(req);  // get rid of the strong reference to out vec.

        if resp_code != 200 {
            return Err(JsonPostError::ErrorRepsonse(resp_code));
        }

        let out_vec = Arc::try_unwrap(output_vec_ptr).expect("expected nothing to have strong reference")
            .into_inner().expect("Lock to not be poisoned");

        serde_json::from_slice(&out_vec)
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
        ErrorRepsonse(code: u32) {
            description("received non 200 response")
            display("Received response: {}", code)
        }
    }
}

impl JsonPostError {
    pub fn into_io_error(self) -> io::Error {
        match self {
            JsonPostError::Io(err) => err,
            JsonPostError::ErrorRepsonse(_) => {
                io::Error::new(io::ErrorKind::Other, "Received non-200 response")
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
        self.poll_sync()
    }
}
