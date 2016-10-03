use curl::easy::Easy;

use futures::{Async, Future, Poll};
use futures::stream::Stream;

use serde_json;

use std::io;
use std::sync::{Arc, Mutex};

use super::models::SyncResponse;

use tokio_core::reactor::Handle;
use tokio_curl::{Perform, Session};

use url::Url;


pub struct MatrixSyncClient {
    url: Url,
    access_token: String,
    next_token: Option<String>,
    curl_session: Session,
    current_sync: Option<Perform>,
    read_buffer: Arc<Mutex<Vec<u8>>>,
    easy_handle: Option<Easy>,
    handle: Handle,
}

impl MatrixSyncClient {
    pub fn new(handle: Handle, base_url: &Url, access_token: String) -> MatrixSyncClient {
        MatrixSyncClient {
            url: base_url.join("/_matrix/client/r0/sync").unwrap(),
            access_token: access_token,
            next_token: None,
            curl_session: Session::new(handle.clone()),
            current_sync: None,
            read_buffer: Arc::default(),
            easy_handle: None,
            handle: handle,
        }
    }

    fn new_sync(&mut self) -> Perform {
        // TODO: Reuse sessions. We don't do this currently as last time it was tried it caused
        // tokio-curl to panic.

        self.curl_session = Session::new(self.handle.clone());
        let mut req = Easy::new();
        // let mut req = self.easy_handle.take().unwrap_or_else(Easy::new);

        self.url
            .query_pairs_mut()
            .clear()
            .append_pair("access_token", &self.access_token)
            .append_pair("timeout", "30000");
        if let Some(ref token) = self.next_token {
            self.url.query_pairs_mut().append_pair("since", token);
        }

        req.get(true).unwrap();
        req.url(self.url.as_str()).unwrap();

        let buf_arc = self.read_buffer.clone();
        req.write_function(move |data| {
                buf_arc.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            })
            .unwrap();

        self.curl_session.perform(req)
    }

    pub fn poll_sync(&mut self) -> Poll<SyncResponse, io::Error> {
        {
            let perform = if let Some(ref mut p) = self.current_sync {
                p
            } else {
                self.current_sync = Some(self.new_sync());
                self.current_sync.as_mut().expect("We just inserted a new request")
            };

            self.easy_handle = Some(try_ready!(perform.poll()));
        }
        self.current_sync.take();

        let sync_response = {
            let mut buf = self.read_buffer.lock().unwrap();
            let sync_response: SyncResponse = serde_json::from_slice(&buf).unwrap(); // TODO
            buf.clear();
            sync_response
        };

        // TODO: Handle non-200

        task_debug!("Got sync response"; "next_token" => sync_response.next_batch);

        self.next_token = Some(sync_response.next_batch.clone());

        Ok(Async::Ready(sync_response))
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
