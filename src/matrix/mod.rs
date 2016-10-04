use curl::easy::{Easy, List};

use futures::{Async, Future, Poll};
use futures::stream::Stream;

use std::io::{self, Read};
use std::mem;
use std::sync::{Arc, Mutex};

use tokio_core::reactor::Handle;
use tokio_curl::{Perform, Session};

use url::Url;

use serde_json;
use serde::{Serialize, Deserialize};

pub mod models;
mod sync;


pub struct MatrixClient {
    url: Url,
    access_token: String,
    sync_client: sync::MatrixSyncClient,
}

impl MatrixClient {
    pub fn new(handle: Handle, base_url: &Url, access_token: String) -> MatrixClient {
        MatrixClient {
            url: base_url.clone(),
            access_token: access_token.clone(),
            sync_client: sync::MatrixSyncClient::new(handle, base_url, access_token),
        }
    }

    pub fn login(handle: Handle, base_url: Url, user: String, password: String) -> impl Future<Item=MatrixClient, Error=LoginError> {
        let session = Session::new(handle.clone());
        do_json_post(&session, &base_url.join("/_matrix/client/r0/login").unwrap(), &models::LoginPasswordInput {
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
        }).map(move |response: models::LoginResponse| {
            MatrixClient::new(handle, &base_url, response.access_token)
        })
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
    type Item = models::SyncResponse;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<models::SyncResponse>, io::Error> {
        self.sync_client.poll()
    }
}
