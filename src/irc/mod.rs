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

//use crate::stream_fold::StreamFold;
use crate::ConnectionContext;

use tokio::io::{AsyncRead, AsyncWrite};

use futures3::future::{Future, FutureExt};
use futures3::stream::{Stream, StreamExt};
use futures3::task::Poll;

use std::boxed::Box;
use std::io;
use std::pin::Pin;

use slog::{debug, info, trace};

use std::task::Context;

pub struct IrcUserConnection<S: AsyncRead + AsyncWrite + Clone> {
    conn: Pin<Box<transport::IrcServerConnection<S>>>,
    pub user: String,
    pub nick: String,
    pub real_name: String,
    pub password: String,
    server_name: String,
    user_prefix: String,
    server_model: models::ServerModel,
}

#[derive(Debug, Clone, Default)]
struct UserNickBuilder {
    nick: Option<String>,
    user: Option<String>,
    real_name: Option<String>,
    password: Option<String>,
}

impl UserNickBuilder {
    pub(crate) fn to_user_nick(self) -> UserNick {
        UserNick {
            nick: self.nick.unwrap(),
            user: self.user.unwrap(),
            real_name: self.real_name.unwrap(),
            password: self.password.unwrap(),
        }
    }
}

impl Future for UserNickBuilder {
    type Output = UserNickBuilder;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        if self.nick.is_some()
            && self.user.is_some()
            && self.real_name.is_some()
            && self.real_name.is_some()
            && self.password.is_some()
        {
            Poll::Ready(self.clone())
        } else {
            Poll::Pending
        }
    }
}

#[derive(Debug, Clone, Default)]
struct UserNick {
    nick: String,
    user: String,
    real_name: String,
    password: String,
}

impl<S> IrcUserConnection<S> 
where
    S: AsyncRead + AsyncWrite + Clone + 'static 
{
    /// Given an IO connection, discard IRC messages until we see both a USER and NICK command.
    pub async fn await_login(
        server_name: String,
        stream: S,
        ctx: ConnectionContext,
    ) -> Result<IrcUserConnection<S>, io::Error> {
        trace!(ctx.logger, "Await login");
        let irc_conn =
            transport::IrcServerConnection::new(stream, server_name.clone(), ctx.clone());

        let ctx_clone = ctx.clone();

        let irc_user: UserNick = irc_conn.clone().fold(UserNickBuilder::default(), move |mut user_nick: UserNickBuilder, cmd: Result<IrcCommand, _> | {
            let inner_cmd = match cmd {
                Ok(good_command) => {
                trace!(ctx.logger, "Got command"; "command" => good_command.command());

                good_command
                }
                Err(e) => {
                    trace!(ctx.logger, "Error when streaming commands: "; "Error" => e.to_string());
                    return user_nick
                }

            };

            match inner_cmd {
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

            // user_nick future only completes when all fields are filled
            // TODO: figure out what happens here when the IRC connection dies
            // TODO: make sure this future doesnt hang indefinitely
            user_nick
        }).await.to_user_nick();

        info!(ctx_clone.logger, "got nick and user");
        let user_prefix = format!("{}!{}@{}", &irc_user.nick, &irc_user.user, &server_name);

        let user_conn = IrcUserConnection {
            conn: Box::pin(irc_conn),
            user: irc_user.user,
            nick: irc_user.nick,
            real_name: irc_user.real_name,
            password: irc_user.password,
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

    pub fn add_channel(
        &mut self,
        name: String,
        topic: String,
        members: &[(&String, bool)],
        cx: &mut Context,
    ) {
        self.server_model.add_channel(name.clone(), topic, members);
        self.attempt_to_write_join_response(&name, cx);
    }

    fn attempt_to_write_join_response(&mut self, name: &str, cx: &mut Context) -> bool {
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
            conn.write_join(&self.user_prefix, &channel.name, cx);
            conn.write_topic(&self.server_name, &channel.name, &channel.topic, cx);
            conn.write_names(&self.nick, name, &names[..], cx);
            conn.write_numeric(
                Numeric::RplChannelmodeis,
                &self.nick,
                &format!("{} +n", &channel.name),
                cx,
            );
            true
        } else {
            false
        }
    }

    pub fn send_message(&mut self, channel: &str, sender: &str, body: &str, cx: &mut Context) {
        for line in body.split('\n') {
            self.conn
                .write_line(&format!(":{} PRIVMSG {} :{}", sender, channel, line), cx);
        }
    }

    pub fn send_action(&mut self, channel: &str, sender: &str, body: &str, cx: &mut Context) {
        for line in body.split('\n') {
            self.conn.write_line(
                &format!(
                    ":{} PRIVMSG {} :\u{0001}ACTION {}\u{0001}",
                    sender, channel, line
                ),
                cx,
            );
        }
    }

    pub fn write_invalid_password(&mut self, cx: &mut Context) {
        self.conn.write_invalid_password(&self.nick, cx);
    }

    pub fn write_redirect_join(&mut self, old_channel: &str, new_channel: &str, cx: &mut Context) {
        self.conn.write_numeric(
            Numeric::RplForwardedChannel,
            &self.nick,
            &format!(
                "{} {} :Forwarding to another channel",
                old_channel, new_channel,
            ),
            cx,
        );
    }

    pub fn welcome(&mut self, cx: &mut Context) {
        self.conn.welcome(&self.nick, cx);
    }

    pub fn send_ping(&mut self, data: &str, cx: &mut Context) {
        let line = format!("PING {}", data);
        self.write_line(&line, cx);
    }

    pub fn write_line(&mut self, line: &str, cx: &mut Context) {
        self.conn.write_line(line, cx);
    }

    fn handle_who_channel_cmd(&mut self, channel: String, cx: &mut Context) {
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
                    cx,
                );
            }
            conn.write_numeric(
                Numeric::RplEndofwho,
                &self.nick,
                &format!("{} :End of /WHO", &channel),
                cx,
            );
        } else {
            // TODO: No such room
        }
    }

    fn handle_mode_channel_cmd(&mut self, channel: String, cx: &mut Context) {
        self.conn.write_numeric(
            Numeric::RplChannelmodeis,
            &self.nick,
            &format!("{} +n", &channel),
            cx,
        );
    }

    pub fn poll(&mut self, cx: &mut Context) -> Poll<Result<Option<IrcCommand>, io::Error>> {
        loop {
            let next = match self.conn.as_mut().poll_next(cx)? {
                Poll::Ready(optional_next) => optional_next,
                Poll::Pending => return Poll::Pending,
            };

            match next {
                Some(cmd) => match cmd {
                    IrcCommand::Ping { data } => {
                        let line = format!(":{} PONG {}", &self.server_name, data);
                        self.write_line(&line, cx);
                        continue;
                    }
                    IrcCommand::Join { channel } => {
                        if !self.attempt_to_write_join_response(&channel, cx) {
                            return Poll::Ready(Ok(Some(IrcCommand::Join { channel })));
                        }
                    }
                    IrcCommand::Who { matches } => {
                        if matches.starts_with('#') {
                            self.handle_who_channel_cmd(matches, cx);
                        } else {
                            // TODO
                        }
                    }
                    IrcCommand::Mode { target, mask } => {
                        if target.starts_with('#') && mask.is_none() {
                            self.handle_mode_channel_cmd(target, cx);
                        } else {
                            // TODO
                        }
                    }
                    IrcCommand::Pong { .. } => {}
                    c => return Poll::Ready(Ok(Some(c))),
                },
                None => return Poll::Ready(Ok(None)),
            }
        }
    }
}
