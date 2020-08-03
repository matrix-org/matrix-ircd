use hyper::{self, client::HttpConnector, Request};

// TODO: can probably drop this whole struct later, just keeping things somewhat similar to how
// futures 0.1 worked to keep apis potentially similar as the port happens
pub struct ClientWrapper {
    inner: hyper::Client<HttpConnector>,
}

impl ClientWrapper {
    pub(crate) fn new() -> Self {
        Self {
            inner: hyper::Client::new(),
        }
    }
    pub(crate) fn send_request(
        &self,
        request: Request<hyper::Body>,
    ) -> hyper::client::ResponseFuture {
        self.inner.request(request)
    }
}
