use curl::easy::Easy;

use futures::{Async, Future, Poll};
use futures::stream::Stream;

use std::io;

use tokio_core::reactor::Handle;
use tokio_curl::{Perform, Session};

use url::Url;


pub mod models;
mod sync;


pub struct MatrixClient {
    url: Url,
    access_token: String,
    curl_session: Session,
    sync_client: sync::MatrixSyncClient,
}

impl MatrixClient {
    pub fn new(handle: Handle, base_url: &Url, access_token: String) -> MatrixClient {
        MatrixClient {
            url: base_url.clone(),
            access_token: access_token.clone(),
            curl_session: Session::new(handle.clone()),
            sync_client: sync::MatrixSyncClient::new(handle, base_url, access_token),
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
