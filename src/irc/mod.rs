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

//! The main module responsible for keeping track of IRC Matrix side of the world.
//!
//! It knows nothing about Matrix.

mod models;
mod protocol;
pub mod transport;

pub use self::protocol::{Command, IrcCommand, IrcLine, Numeric};

use crate::stream_fold::StreamFold;
use crate::ConnectionContext;

use tokio_io::{AsyncRead, AsyncWrite};

use futures::stream::Stream;
use futures::{Async, Future, Poll};

use std::io;

use slog::{debug, info, trace};

pub struct IrcUserConnection<S: AsyncRead + AsyncWrite> {
    conn: transport::IrcServerConnection<S>,
    pub user: String,
    pub nick: String,
    pub real_name: String,
    pub password: String,
    server_name: String,
    user_prefix: String,
    server_model: models::ServerModel,
}

#[derive(Debug, Clone, Default)]
struct UserNick {
    nick: Option<String>,
    user: Option<String>,
    real_name: Option<String>,
    password: Option<String>,
}

impl<S: AsyncRead + AsyncWrite + 'static> IrcUserConnection<S> {
    /// Given an IO connection, discard IRC messages until we see both a USER and NICK command.
    pub fn await_login(
        server_name: String,
        stream: S,
        ctx: ConnectionContext,
    ) -> Box<dyn Future<Item = IrcUserConnection<S>, Error = io::Error>> {
        trace!(ctx.logger, "Await login");
        let irc_conn =
            transport::IrcServerConnection::new(stream, server_name.clone(), ctx.clone());

        let ctx_clone = ctx.clone();

        let f = StreamFold::new(irc_conn, UserNick::default(), move |cmd, mut user_nick| {
            trace!(ctx.logger, "Got command"; "command" => cmd.command());
            match cmd {
                IrcCommand::Nick { nick } => user_nick.nick = Some(nick),
                IrcCommand::User { user, real_name } => {
                    user_nick.user = Some(user);
                    user_nick.real_name = Some(real_name);
                }
                IrcCommand::Pass { password } => user_nick.password = Some(password),
                c => {
                    debug!(ctx.logger, "Ignore command during login"; "cmd" => c.command());
                }
            }

            (
                user_nick.nick.is_some()
                    && user_nick.user.is_some()
                    && user_nick.real_name.is_some(),
                user_nick,
            )
        })
        .and_then(move |opt| {
            if let Some((user_nick, mut irc_conn)) = opt {
                info!(ctx_clone.logger, "Got nick and user");

                let nick = user_nick.nick.expect("nick");

                let password = if let Some(p) = user_nick.password {
                    p
                } else {
                    irc_conn.write_password_required(&nick);
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "IRC user did not supply password",
                    ));
                };

                let user = user_nick.user.expect("user");
                let user_prefix = format!("{}!{}@{}", &nick, &user, &server_name);

                let user_conn = IrcUserConnection {
                    conn: irc_conn,
                    user,
                    nick,
                    real_name: user_nick.real_name.expect("real_name"),
                    password,
                    server_name,
                    user_prefix,
                    server_model: models::ServerModel::new(),
                };

                trace!(ctx_clone.logger, "IRC conn values";
                    "nick" => user_conn.nick.clone(),
                    "user" => user_conn.user.clone(),
                    "password" => user_conn.password.clone(),
                );

                Ok(user_conn)
            } else {
                info!(ctx_clone.logger, "IRC conn died during login");
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "IRC stream ended during login",
                ))
            }
        });

        Box::new(f)
    }

    pub fn nick_exists(&self, nick: &str) -> bool {
        self.server_model.nick_exists(nick)
    }

    pub fn channel_exists(&self, channel: &str) -> bool {
        self.server_model.channel_exists(channel)
    }

    pub fn create_user(&mut self, nick: String, user: String) {
        self.server_model.create_user(nick, user);
    }

    pub fn add_channel(&mut self, name: String, topic: String, members: &[(&String, bool)]) {
        self.server_model.add_channel(name.clone(), topic, members);
        self.attempt_to_write_join_response(&name);
    }

    fn attempt_to_write_join_response(&mut self, name: &str) -> bool {
        let IrcUserConnection {
            ref mut server_model,
            ref mut conn,
            ..
        } = *self;

        if let (Some(channel), Some(members)) = (
            server_model.get_channel(name),
            server_model.get_members(name),
        ) {
            let names: Vec<_> = members
                .iter()
                .map(|&(ref user, ref entry)| (&user.nick, entry.operator))
                .collect();
            conn.write_join(&self.user_prefix, &channel.name);
            conn.write_topic(&self.server_name, &channel.name, &channel.topic);
            conn.write_names(&self.nick, name, &names[..]);
            conn.write_numeric(
                Numeric::RplChannelmodeis,
                &self.nick,
                &format!("{} +n", &channel.name),
            );
            true
        } else {
            false
        }
    }

    pub fn send_message(&mut self, channel: &str, sender: &str, body: &str) {
        for line in body.split('\n') {
            self.conn
                .write_line(&format!(":{} PRIVMSG {} :{}", sender, channel, line));
        }
    }

    pub fn send_action(&mut self, channel: &str, sender: &str, body: &str) {
        for line in body.split('\n') {
            self.conn.write_line(&format!(
                ":{} PRIVMSG {} :\u{0001}ACTION {}\u{0001}",
                sender, channel, line
            ));
        }
    }

    pub fn write_invalid_password(&mut self) {
        self.conn.write_invalid_password(&self.nick);
    }

    pub fn write_redirect_join(&mut self, old_channel: &str, new_channel: &str) {
        self.conn.write_numeric(
            Numeric::RplForwardedChannel,
            &self.nick,
            &format!(
                "{} {} :Forwarding to another channel",
                old_channel, new_channel,
            ),
        );
    }

    pub fn welcome(&mut self) {
        self.conn.welcome(&self.nick);
    }

    pub fn send_ping(&mut self, data: &str) {
        let line = format!("PING {}", data);
        self.write_line(&line);
    }

    pub fn write_line(&mut self, line: &str) {
        self.conn.write_line(line);
    }

    fn handle_who_channel_cmd(&mut self, channel: String) {
        let IrcUserConnection {
            ref mut server_model,
            ref mut conn,
            ..
        } = *self;

        if let Some(members) = server_model.get_members(&channel) {
            for (user, _) in members {
                conn.write_numeric(
                    Numeric::RplWhoreply,
                    &self.nick,
                    &format!(
                        "{} {} {} {} {} H :0 {}",
                        &channel, &user.nick, &user.mask, &self.server_name, &user.user, &user.user,
                    ),
                );
            }
            conn.write_numeric(
                Numeric::RplEndofwho,
                &self.nick,
                &format!("{} :End of /WHO", &channel),
            );
        } else {
            // TODO: No such room
        }
    }

    fn handle_mode_channel_cmd(&mut self, channel: String) {
        self.conn.write_numeric(
            Numeric::RplChannelmodeis,
            &self.nick,
            &format!("{} +n", &channel),
        );
    }

    pub fn poll(&mut self) -> Poll<Option<IrcCommand>, io::Error> {
        loop {
            match futures::try_ready!(self.conn.poll()) {
                Some(cmd) => match cmd {
                    IrcCommand::Ping { data } => {
                        let line = format!(":{} PONG {}", &self.server_name, data);
                        self.write_line(&line);
                        continue;
                    }
                    IrcCommand::Join { channel } => {
                        if !self.attempt_to_write_join_response(&channel) {
                            return Ok(Async::Ready(Some(IrcCommand::Join { channel })));
                        }
                    }
                    IrcCommand::Who { matches } => {
                        if matches.starts_with('#') {
                            self.handle_who_channel_cmd(matches);
                        } else {
                            // TODO
                        }
                    }
                    IrcCommand::Mode { target, mask } => {
                        if target.starts_with('#') && mask.is_none() {
                            self.handle_mode_channel_cmd(target);
                        } else {
                            // TODO
                        }
                    }
                    IrcCommand::Pong { .. } => {}
                    c => return Ok(Async::Ready(Some(c))),
                },
                None => return Ok(Async::Ready(None)),
            }
        }
    }
}
