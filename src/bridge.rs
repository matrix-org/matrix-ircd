use ConnectionContext;

use futures::{Async, Future, Poll};
use futures::stream::Stream;

use irc;
use irc::IrcLine;

use matrix::MatrixClient;
use matrix::models::SyncResponse;

use std::io;

use tokio_core::io::Io;


pub struct Bridge<IS: Io> {
    irc_conn: irc::transport::IrcServerConnection<IS>,
    matrix_client: MatrixClient,
    ctx: ConnectionContext,
    closed: bool,
}

impl<IS: Io> Bridge<IS> {
    pub fn new(stream: IS, ctx: ConnectionContext, matrix_client: MatrixClient) -> Bridge<IS> {
        let transport = irc::transport::IrcServerConnection::new(stream, ctx.clone());

        Bridge {
            irc_conn: transport,
            matrix_client: matrix_client,
            ctx: ctx,
            closed: false,
        }
    }

    fn handle_irc_cmd(&mut self, line: IrcLine) {
        debug!(self.ctx.logger, "Received IRC line"; "command" => line.command);
        self.irc_conn.write_line("Test response");
    }

    fn handle_sync_response(&mut self, sync_response: SyncResponse) {
        trace!(self.ctx.logger, "Received sync response"; "batch" => sync_response.next_batch);
        self.irc_conn.write_line(&format!("Batch: {}", sync_response.next_batch));
    }

    fn poll_irc(&mut self) -> Poll<(), io::Error> {
        if let Some(line) = try_ready!(self.irc_conn.poll()) {
            self.handle_irc_cmd(line);
        } else {
            self.closed = true;
        }
        Ok(Async::Ready(()))
    }

    fn poll_matrix(&mut self) -> Poll<(), io::Error> {
        if let Some(sync_response) = try_ready!(self.matrix_client.poll()) {
            self.handle_sync_response(sync_response);
        } else {
            self.closed = true;
        }
        Ok(Async::Ready(()))
    }
}

impl<IS: Io> Future for Bridge<IS> {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        trace!(self.ctx.logger, "Bridge Polled");

        loop {
            if self.closed {
                return Ok(Async::Ready(()));
            }

            if let (Async::NotReady, Async::NotReady) = (self.poll_irc()?, self.poll_matrix()?) {
                break;
            }
        }

        Ok(Async::NotReady)
    }
}
