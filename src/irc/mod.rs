pub mod transport;
mod protocol;

pub use self::protocol::{Command, IrcCommand, IrcLine, Numeric};


use ConnectionContext;

use tokio_core::io::Io;

use futures::{Async, Future, Poll};
use futures::stream::Stream;

use itertools::Itertools;

use std::borrow::Borrow;
use std::io;
use std::mem;
use std::fmt::Write;


pub struct IrcUserConnection<S: Io> {
    conn: transport::IrcServerConnection<S>,
    pub user: String,
    pub nick: String,
    pub real_name: String,
    pub password: String,
    server_name: String,
}


#[derive(Debug, Clone, Default)]
struct UserNick {
    nick: Option<String>,
    user: Option<String>,
    real_name: Option<String>,
    password: Option<String>,
}

impl<S: Io> IrcUserConnection<S> {
    pub fn await_login(server_name: String, stream: S, ctx: ConnectionContext) -> impl Future<Item=IrcUserConnection<S>, Error=io::Error> {
        trace!(ctx.logger, "Await login");
        let irc_conn = transport::IrcServerConnection::new(stream, ctx.clone());

        let ctx_clone = ctx.clone();

        StreamFold::new(irc_conn, UserNick::default(), move |cmd, mut user_nick| {
            trace!(ctx.logger, "Got command"; "command" => cmd.command());
            match cmd {
                IrcCommand::Nick { nick } => user_nick.nick = Some(nick),
                IrcCommand::User { user, real_name } => {
                    user_nick.user = Some(user);
                    user_nick.real_name = Some(real_name);
                }
                IrcCommand::Pass { password } => {
                    user_nick.password = Some(password)
                }
                c => {
                    debug!(ctx.logger, "Ignore command during login"; "cmd" => c.command());
                }
            }

            (user_nick.nick.is_some() && user_nick.user.is_some() && user_nick.real_name.is_some(), user_nick)
        }).and_then(move |opt| {
            if let Some((user_nick, mut irc_conn)) = opt {
                info!(ctx_clone.logger, "Got nick and user");

                let password = if let Some(p) = user_nick.password {
                    p
                } else {
                    irc_conn.write_line(&format!("{} PASS :Password required", Numeric::ERR_NEEDMOREPARAMS.as_str()));
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "IRC user did not supply password"));
                };

                let user_conn = IrcUserConnection {
                    conn: irc_conn,
                    user: user_nick.user.expect("user"),
                    nick: user_nick.nick.expect("nick"),
                    real_name: user_nick.real_name.expect("real_name"),
                    password: password,
                    server_name: server_name,
                };

                trace!(ctx_clone.logger, "IRC conn values";
                    "nick" => user_conn.nick,
                    "user" => user_conn.user,
                    "password" => user_conn.password,
                );

                Ok(user_conn)
            } else {
                info!(ctx_clone.logger, "IRC conn died during login");
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "IRC stream ended during login"))
            }
        })
    }

    pub fn conn_mut(&mut self) -> &mut transport::IrcServerConnection<S> {
        &mut self.conn
    }

    pub fn write_invalid_password(&mut self) {
        self.write_numeric(Numeric::ERR_PASSWDMISMATCH, ":Invalid password");
    }

    pub fn write_password_required(&mut self) {
        self.write_numeric(Numeric::ERR_NEEDMOREPARAMS, ":Password required");
    }

    pub fn write_numeric(&mut self, numeric: Numeric, rest_of_line: &str) {
        let line = format!(":{} {} {} {}", &self.server_name, numeric.as_str(), &self.nick, rest_of_line);
        self.write_line(&line);
    }

    pub fn welcome(&mut self) {
        self.write_numeric(Numeric::RPL_WELCOME, ":Welcome to the Matrix Internet Relay Network");

        let motd_start = format!(":- {} Message of the day -", self.server_name);
        self.write_numeric(Numeric::RPL_MOTDSTART, &motd_start);
        self.write_numeric(Numeric::RPL_MOTD, ":-");
        self.write_numeric(Numeric::RPL_MOTD, ":- This is a bridge into Matrix");
        self.write_numeric(Numeric::RPL_MOTD, ":-");
        self.write_numeric(Numeric::RPL_ENDOFMOTD, ":End of MOTD");
    }

    pub fn write_join(&mut self, channel: &str) {
        let line = format!(":{} JOIN {}", &self.nick, channel);
        self.write_line(&line);
    }

    pub fn write_topic(&mut self, channel: &str, topic: &str) {
        self.write_numeric(Numeric::RPL_TOPIC, &format!("{} :{}", channel, topic));
    }

    pub fn write_names(&mut self, channel: &str, names: &[String]) {
        let line = format!("@ {} :{}", channel, &self.nick);
        self.write_numeric(Numeric::RPL_NAMREPLY, &line);

        for iter in names.chunks(10).into_iter() {
            let mut line = format!("@ {} :", channel);
            for val in iter {
                write!(line, "{} ", &val);
            }
            line.trim();
            self.write_numeric(Numeric::RPL_NAMREPLY, &line);
        }
        self.write_numeric(Numeric::RPL_ENDOFNAMES, &format!("{} :End of /NAMES", channel));
    }

    pub fn write_line(&mut self, line: &str) {
        self.conn.write_line(line);
    }

    pub fn poll(&mut self) -> Poll<Option<IrcCommand>, io::Error> {
        loop {
            match try_ready!(self.conn.poll()) {
                Some(cmd) => match cmd {
                    IrcCommand::Ping { data } => {
                        let line = format!(":{} PONG {}", &self.server_name, data);
                        self.write_line(&line);
                        continue
                    }
                    c => return Ok(Async::Ready(Some(c)))
                },
                None => return Ok(Async::Ready(None))
            }
        }
    }
}


#[must_use = "futures do nothing unless polled"]
struct StreamFold<I, E, S: Stream<Item=I, Error=E>, V, F: FnMut(I, V) -> (bool, V)> {
    func: F,
    state: StreamFoldState<S, V>
}

impl<I, E, S: Stream<Item=I, Error=E>, V, F: FnMut(I, V) -> (bool, V)> StreamFold<I, E, S, V, F> {
    pub fn new(stream: S, value: V, func: F) -> StreamFold<I, E, S, V, F> {
        StreamFold {
            func: func,
            state: StreamFoldState::Full {
                stream: stream,
                value: value,
            }
        }
    }
}

enum StreamFoldState<S, V> {
    Empty,
    Full {
        stream: S,
        value: V,
    }
}

impl<I, E, S: Stream<Item=I, Error=E>, V, F: FnMut(I, V) -> (bool, V)> Future for StreamFold<I, E, S, V, F> {
    type Item = Option<(V, S)>;
    type Error = E;

    fn poll(&mut self) -> Poll<Option<(V, S)>, E> {
        let (mut stream, mut value) = match mem::replace(&mut self.state, StreamFoldState::Empty) {
            StreamFoldState::Empty => panic!("cannot poll Fold twice"),
            StreamFoldState::Full { stream, value } => (stream, value),
        };

        loop {
            match stream.poll()? {
                Async::Ready(Some(item)) => {
                    let (done, val) = (self.func)(item, value);
                    value = val;
                    if done {
                        return Ok(Async::Ready(Some((value, stream))))
                    }
                }
                Async::Ready(None) => {
                    return Ok(Async::Ready(None))
                }
                Async::NotReady => {
                    self.state = StreamFoldState::Full { stream: stream, value: value };
                    return Ok(Async::NotReady)
                }
            }
        }
    }
}
