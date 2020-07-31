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
        &mut self,
        request: Request<hyper::Body>,
    ) -> hyper::client::ResponseFuture {
        self.inner.request(request)
    }
}

#[cfg(test)]
mod send_tests {
    fn is_send<T: Send>(_: T) {}
    #[test]
    fn http_client() {
        let http = crate::http::ClientWrapper::new();
        is_send(http);
    }
}
