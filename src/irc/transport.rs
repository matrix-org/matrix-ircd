use ConnectionContext;

use futures::{Async, Poll, task};
use futures::stream::Stream;

use std::io::{self, Cursor};
use std::sync::{Arc, Mutex};

use super::protocol::IrcCommand;

use tokio_core::io::Io;


pub struct IrcServerConnection<S: Io> {
    conn: S,
    read_buffer: Vec<u8>,
    inner: Arc<Mutex<IrcServerConnectionInner>>,
    closed: bool,
    ctx: ConnectionContext,
}

impl<S: Io> IrcServerConnection<S> {
    pub fn new(conn: S, context: ConnectionContext) -> IrcServerConnection<S> {
        IrcServerConnection {
            conn: conn,
            read_buffer: Vec::with_capacity(1024),
            inner: Arc::new(Mutex::new(IrcServerConnectionInner::new())),
            closed: false,
            ctx: context,
        }
    }

    pub fn get_handle(&self) -> IrcServerConnectionHandle {
        IrcServerConnectionHandle { inner: self.inner.clone() }
    }

    pub fn write_line(&mut self, line: &str) {
        {
            let mut inner = self.inner.lock().unwrap();

            trace!(self.ctx.logger, "Writting line"; "line" => line);

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


#[derive(Debug, Clone)]
pub struct IrcServerConnectionHandle {
    inner: Arc<Mutex<IrcServerConnectionInner>>,
}

impl IrcServerConnectionHandle {
    pub fn write_line(&mut self, line: &str) {
        let mut inner = self.inner.lock().unwrap();

        task_trace!("Writting line"; "line" => line);

        {
            let mut v = inner.write_buffer.get_mut();
            v.extend_from_slice(line.as_bytes());
            v.push(b'\n');
        }

        if let Some(ref t) = inner.write_notify {
            t.unpark();
        }
    }
}
