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


use ConnectionContext;

use futures::{Async, Poll, task};
use futures::stream::Stream;

use std::io::{self, Cursor};
use std::sync::{Arc, Mutex};
use std::fmt::Write;

use super::protocol::{Numeric, IrcCommand};

use tokio_core::io::Io;


pub struct IrcServerConnection<S: Io> {
    conn: S,
    read_buffer: Vec<u8>,
    inner: Arc<Mutex<IrcServerConnectionInner>>,
    closed: bool,
    ctx: ConnectionContext,
    server_name: String,
}

impl<S: Io> IrcServerConnection<S> {
    pub fn new(conn: S, server_name: String, context: ConnectionContext) -> IrcServerConnection<S> {
        IrcServerConnection {
            conn: conn,
            read_buffer: Vec::with_capacity(1024),
            inner: Arc::new(Mutex::new(IrcServerConnectionInner::new())),
            closed: false,
            ctx: context,
            server_name: server_name,
        }
    }

    pub fn write_line(&mut self, line: &str) {
        {
            let mut inner = self.inner.lock().unwrap();

            trace!(self.ctx.logger, "Writing line"; "line" => line);

            {
                let mut v = inner.write_buffer.get_mut();
                v.extend_from_slice(line.as_bytes());
                v.push(b'\n');
            }

            if let Some(ref t) = inner.write_notify {
                t.unpark();
            }
        }
        self.poll_write().ok();
    }

    pub fn write_invalid_password(&mut self, nick: &str) {
        self.write_numeric(Numeric::ErrPasswdmismatch, nick, ":Invalid password");
    }

    pub fn write_password_required(&mut self, nick: &str) {
        self.write_numeric(Numeric::ErrNeedmoreparams, nick, "PASS :Password required");
    }

    pub fn write_numeric(&mut self, numeric: Numeric, nick: &str, rest_of_line: &str) {
        let line = format!(":{} {} {} {}", &self.server_name, numeric.as_str(), nick, rest_of_line);
        self.write_line(&line);
    }

    pub fn welcome(&mut self, nick: &str) {
        self.write_numeric(Numeric::RplWelcome, nick, ":Welcome to the Matrix Internet Relay Network");

        let motd_start = format!(":- {} Message of the day -", self.server_name);
        self.write_numeric(Numeric::RplMotdstart, nick, &motd_start);
        self.write_numeric(Numeric::RplMotd, nick, ":-");
        self.write_numeric(Numeric::RplMotd, nick, ":- This is a bridge into Matrix");
        self.write_numeric(Numeric::RplMotd, nick, ":-");
        self.write_numeric(Numeric::RplEndofmotd, nick, ":End of MOTD");
    }

    pub fn write_join(&mut self, nick: &str, channel: &str) {
        let line = format!(":{} JOIN {}", nick, channel);
        self.write_line(&line);
    }

    pub fn write_topic(&mut self, nick: &str, channel: &str, topic: &str) {
        self.write_numeric(Numeric::RplTopic, nick, &format!("{} :{}", channel, topic));
    }

    pub fn write_names(&mut self, nick: &str, channel: &str, names: &[(&String, bool)]) {
        for iter in names.chunks(10) {
            let mut line = format!("@ {} :", channel);
            for &(nick, op) in iter {
                write!(line, "{}{} ", if op {"@"} else {""}, &nick).unwrap();
            }
            line.trim();
            self.write_numeric(Numeric::RplNamreply, nick, &line);
        }
        self.write_numeric(Numeric::RplEndofnames, nick, &format!("{} :End of /NAMES", channel));
    }

    fn poll_read(&mut self) -> Poll<IrcCommand, io::Error> {
        loop {
            while let Some(pos) = self.read_buffer.iter().position(|&c| c == b'\n') {
                let to_return = self.read_buffer.drain(..pos + 1).collect();
                match String::from_utf8(to_return) {
                    Ok(line) => {
                        let line = line.trim_right().to_string();
                        if let Ok(irc_line) = line.parse() {
                            trace!(self.ctx.logger, "Got IRC line"; "line" => line);
                            return Ok(Async::Ready(irc_line));
                        } else {
                            warn!(self.ctx.logger, "Invalid IRC line"; "line" => line);
                        }
                    }
                    Err(_) => {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid UTF-8"))
                    }
                }
            }

            let start_len = self.read_buffer.len();
            if start_len >= 2048 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Line too long"));
            }
            self.read_buffer.resize(2048, 0);
            match self.conn.read(&mut self.read_buffer[start_len..]) {
                Ok(0) => {
                    debug!(self.ctx.logger, "Closed");
                    self.closed = true;
                    self.read_buffer.resize(start_len, 0);
                    return Ok(Async::NotReady);
                }
                Ok(bytes_read) => {
                    self.read_buffer.resize(start_len + bytes_read, 0);
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.read_buffer.resize(start_len, 0);
                    return Ok(Async::NotReady);
                }
                Err(e) => {
                    return Err(e);
                }
            };
        }
    }

    fn poll_write(&mut self) -> Poll<(), io::Error> {
        loop {
            let mut inner = self.inner.lock().unwrap();

            if inner.write_notify.is_none() {
                inner.write_notify = Some(task::park());
            }

            if inner.write_buffer.get_ref().is_empty() {
                return Ok(Async::Ready(()));
            }

            let pos = inner.write_buffer.position() as usize;
            if inner.write_buffer.get_ref().len() - pos == 0 {
                inner.write_buffer.get_mut().clear();
                inner.write_buffer.set_position(0);
                return Ok(Async::Ready(()));
            }

            let bytes_written = {
                let to_write = &inner.write_buffer.get_ref()[pos..];
                try_nb!(self.conn.write(to_write))
            };

            inner.write_buffer.set_position((pos + bytes_written) as u64);

            try_nb!(self.conn.flush());
        }
    }
}

impl<S: Io> Stream for IrcServerConnection<S> {
    type Item = IrcCommand;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<IrcCommand>, io::Error> {
        trace!(self.ctx.logger, "IRC Polled");

        if self.closed {
            return Ok(Async::Ready(None));
        }

        self.poll_write()?;

        if let Async::Ready(line) = self.poll_read()? {
            return Ok(Async::Ready(Some(line)));
        }

        if self.closed {
            Ok(Async::Ready(None))
        } else {
            Ok(Async::NotReady)
        }
    }
}


#[derive(Debug, Clone)]
struct IrcServerConnectionInner {
    write_buffer: Cursor<Vec<u8>>,
    write_notify: Option<task::Task>,
}

impl IrcServerConnectionInner {
    pub fn new() -> IrcServerConnectionInner {
        IrcServerConnectionInner {
            write_buffer: Cursor::new(Vec::with_capacity(1024)),
            write_notify: None,
        }
    }
}
