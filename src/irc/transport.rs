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

use crate::ConnectionContext;

use futures::stream::Stream;
use futures::task::Poll;

use std::boxed::Box;
use std::fmt::Write;
use std::io::{self, Cursor};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Context;

use super::protocol::{IrcCommand, Numeric};

use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncRead, AsyncWrite};

pub struct IrcServerConnection<S>
where
    S: AsyncRead + AsyncWrite + Send,
{
    conn: Pin<Box<S>>,
    read_buffer: Vec<u8>,
    inner: Arc<Mutex<IrcServerConnectionInner>>,
    closed: bool,
    ctx: ConnectionContext,
    server_name: String,
}

impl<S> IrcServerConnection<S>
where
    S: AsyncWrite + AsyncRead + Send,
{
    pub fn new(conn: S, server_name: String, context: ConnectionContext) -> IrcServerConnection<S> {
        IrcServerConnection {
            conn: Box::pin(conn),
            read_buffer: Vec::with_capacity(1024),
            inner: Arc::new(Mutex::new(IrcServerConnectionInner::new())),
            closed: false,
            ctx: context,
            server_name,
        }
    }

    pub async fn write_line(&mut self, line: &str) {
        {
            let mut inner = self.inner.lock().unwrap();

            trace!(self.ctx.logger, "Writing line"; "line" => line);

            {
                let v = inner.write_buffer.get_mut();
                v.extend_from_slice(line.as_bytes());
                v.push(b'\n');
            }

            if let Some(ref waker) = inner.waker {
                waker.wake_by_ref()
            }
        }

        let _ = self.write().await;
    }

    pub async fn write_invalid_password(&mut self, nick: &str) {
        self.write_numeric(Numeric::ErrPasswdmismatch, nick, ":Invalid password")
            .await;
    }

    pub async fn write_password_required(&mut self, nick: &str) {
        self.write_numeric(Numeric::ErrNeedmoreparams, nick, "PASS :Password required")
            .await;
    }

    pub async fn write_numeric(&mut self, numeric: Numeric, nick: &str, rest_of_line: &str) {
        let line = format!(
            ":{} {} {} {}",
            &self.server_name,
            numeric.as_str(),
            nick,
            rest_of_line
        );
        self.write_line(&line).await;
    }

    pub async fn welcome(&mut self, nick: &str) {
        self.write_numeric(
            Numeric::RplWelcome,
            nick,
            ":Welcome to the Matrix Internet Relay Network",
        )
        .await;

        let motd_start = format!(":- {} Message of the day -", self.server_name);
        self.write_numeric(Numeric::RplMotdstart, nick, &motd_start)
            .await;
        self.write_numeric(Numeric::RplMotd, nick, ":-").await;
        self.write_numeric(Numeric::RplMotd, nick, ":- This is a bridge into Matrix")
            .await;
        self.write_numeric(Numeric::RplMotd, nick, ":-").await;
        self.write_numeric(Numeric::RplEndofmotd, nick, ":End of MOTD")
            .await;
    }

    pub async fn write_join(&mut self, nick: &str, channel: &str) {
        let line = format!(":{} JOIN {}", nick, channel);
        self.write_line(&line).await;
    }

    pub async fn write_topic(&mut self, nick: &str, channel: &str, topic: &str) {
        self.write_numeric(Numeric::RplTopic, nick, &format!("{} :{}", channel, topic))
            .await;
    }

    pub async fn write_names(&mut self, nick: &str, channel: &str, names: &[(&String, bool)]) {
        for iter in names.chunks(10) {
            let mut line = format!("@ {} :", channel);
            for &(nick, op) in iter {
                write!(line, "{}{} ", if op { "@" } else { "" }, &nick).unwrap();
            }
            let line = line.trim();
            self.write_numeric(Numeric::RplNamreply, nick, &line).await;
        }
        self.write_numeric(
            Numeric::RplEndofnames,
            nick,
            &format!("{} :End of /NAMES", channel),
        )
        .await;
    }

    fn poll_read(&mut self, cx: &mut Context) -> Poll<Result<IrcCommand, io::Error>> {
        loop {
            while let Some(pos) = self.read_buffer.iter().position(|&c| c == b'\n') {
                trace!(self.ctx.logger, "Pulling an IRC line from the buffer");
                let to_return = self.read_buffer.drain(..pos + 1).collect();
                match String::from_utf8(to_return) {
                    Ok(line) => {
                        let line = line.trim_end().to_string();
                        if let Ok(irc_line) = line.parse() {
                            return Poll::Ready(Ok(irc_line));
                        } else {
                            warn!(self.ctx.logger, "Invalid IRC line"; "line" => line);
                        }
                    }
                    Err(_) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Invalid UTF-8",
                        )))
                    }
                }
            }

            let start_len = self.read_buffer.len();

            trace!(self.ctx.logger, "current buffer length: {}", start_len);

            if start_len >= 2048 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Line too long",
                )));
            }
            self.read_buffer.resize(2048, 0);

            match self
                .conn
                .as_mut()
                .poll_read(cx, &mut self.read_buffer[start_len..])
            {
                Poll::Ready(Ok(0)) => {
                    debug!(self.ctx.logger, "Recieved EOF, Closing IRC connection");
                    self.closed = true;
                    self.read_buffer.resize(start_len, 0);
                    return Poll::Pending;
                }
                Poll::Ready(Ok(bytes_read)) => {
                    trace!(
                        self.ctx.logger,
                        "Read {} bytes from irc connection",
                        bytes_read
                    );
                    self.read_buffer.resize(start_len + bytes_read, 0);
                }
                Poll::Ready(Err(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    trace!(self.ctx.logger, "IRC connection WouldBlock error");
                    self.read_buffer.resize(start_len, 0);
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => {
                    trace!(self.ctx.logger, "IRC connection other error");
                    return Poll::Ready(Err(e));
                }
                Poll::Pending => {
                    trace!(self.ctx.logger, "TCP connection pending");
                    self.read_buffer.resize(start_len, 0);
                    return Poll::Pending;
                }
            };
        }
    }

    /// poll_wite is an (almost) identical method to `write` with the exception that it is not
    /// async since Stream::poll_next (implemented below) is not async, meaning we cant use async /
    /// await there.
    fn poll_write(&mut self, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        loop {
            let mut inner = self.inner.lock().unwrap();

            if inner.waker.is_none() {
                inner.waker = Some(cx.waker().clone())
            }

            if inner.write_buffer.get_ref().is_empty() {
                return Poll::Ready(Ok(()));
            }

            let pos = inner.write_buffer.position() as usize;
            if inner.write_buffer.get_ref().len() - pos == 0 {
                inner.write_buffer.get_mut().clear();
                inner.write_buffer.set_position(0);
                return Poll::Ready(Ok(()));
            }

            let bytes_written = {
                let to_write = &inner.write_buffer.get_ref()[pos..];

                match self.conn.as_mut().poll_write(cx, to_write)? {
                    Poll::Ready(bytes_written) => bytes_written,
                    Poll::Pending => return Poll::Pending,
                }
            };

            debug!(
                self.ctx.logger,
                "Wrote {} bytes in poll_write", bytes_written
            );

            inner
                .write_buffer
                .set_position((pos + bytes_written) as u64);

            match self.conn.as_mut().poll_flush(cx)? {
                Poll::Ready(_) => (),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    /// `write` is almost identical to `poll_write`, except for the fact that is async, and can
    /// therfore take advantage of AsyncWriteExt::write. Having this method is a requirement for
    /// the `write_line` method, which we are (basically) required to make async due to it being
    /// called for all irc routines. These irc routies need to be async in order to make
    /// `bridge/mod.rs` take advantage of async / await.
    async fn write(&mut self) -> Result<(), io::Error> {
        loop {
            // A new scope is used to make sure the MutexGuard is not alive while .await is used.
            // This is because the std MutexGuard is !Send and will produce compiler errors.
            let (pos, to_write) = {
                let mut inner = self.inner.lock().unwrap();

                if inner.write_buffer.get_ref().is_empty() {
                    return Ok(());
                }

                let pos = inner.write_buffer.position() as usize;
                if inner.write_buffer.get_ref().len() - pos == 0 {
                    inner.write_buffer.get_mut().clear();
                    inner.write_buffer.set_position(0);
                    return Ok(());
                }

                // we clone the bytes and return them from the scope since the guard must be
                // dropped
                let to_write = inner.write_buffer.get_ref()[pos..].to_vec();
                (pos, to_write)
            };

            let bytes_written = self.conn.as_mut().write(&to_write).await?;

            // TODO: re-locking the mutex here gives another thread time to change the underlying
            // data. Ideally this should be a tokio mutex where MutexGuard is Send and we do not
            // need to re-lock
            let mut inner = self.inner.lock().unwrap();

            inner
                .write_buffer
                .set_position((pos + bytes_written) as u64);
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Send> Stream for IrcServerConnection<S> {
    type Item = Result<IrcCommand, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        trace!(self.ctx.logger, "IRC Polled");

        if self.closed {
            trace!(
                self.ctx.logger,
                "IRC is closed, returning None from IrcServerConnection"
            );
            return Poll::Ready(None);
        }

        trace!(self.ctx.logger, "calling poll_write");
        match self.poll_write(cx)? {
            Poll::Ready(_) => (),
            Poll::Pending => return Poll::Pending,
        };

        trace!(self.ctx.logger, "calling poll_read");

        match self.poll_read(cx)? {
            Poll::Ready(line) => return Poll::Ready(Some(Ok(line))),
            Poll::Pending => trace!(self.ctx.logger, "Poll::Pending was returned from poll_read"),
        }

        if self.closed {
            trace!(
                self.ctx.logger,
                "IRC was closed in IrcServerConnection::poll_read returning None from stream"
            );
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

#[derive(Debug, Clone)]
struct IrcServerConnectionInner {
    write_buffer: Cursor<Vec<u8>>,
    waker: Option<std::task::Waker>,
}

impl IrcServerConnectionInner {
    pub fn new() -> IrcServerConnectionInner {
        IrcServerConnectionInner {
            write_buffer: Cursor::new(Vec::with_capacity(1024)),
            waker: None,
        }
    }
}
