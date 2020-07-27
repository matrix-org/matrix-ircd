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

use crate::ConnectionContext;

use tokio::io::{AsyncRead, AsyncWrite};

use futures::stream::StreamExt;
use futures::task::Poll;

use std::boxed::Box;
use std::io;
use std::pin::Pin;

use slog::{info, trace};

pub struct IrcUserConnection<S: AsyncRead + AsyncWrite> {
    conn: Pin<Box<transport::IrcServerConnection<S>>>,
    pub user: String,
    pub nick: String,
    pub real_name: String,
    pub password: String,
    server_name: String,
    user_prefix: String,
    server_model: models::ServerModel,
}

#[derive(Clone)]
struct UserNickBuilder {
    ctx: ConnectionContext,
    nick: Option<String>,
    user: Option<String>,
    real_name: Option<String>,
    password: Option<String>,
}

impl UserNickBuilder {
    fn with_context(ctx: ConnectionContext) -> Self {
        UserNickBuilder {
            ctx,
            nick: None,
            user: None,
            real_name: None,
            password: None,
        }
    }
}

impl UserNickBuilder {
    fn to_user_nick(self) -> UserNick {
        UserNick {
            nick: self.nick.unwrap(),
            user: self.user.unwrap(),
            real_name: self.real_name.unwrap(),
            password: self.password,
        }
    }

    /// Check that the user has input all fields except password.
    ///
    /// A missing password will be caught in `await_login`.
    fn is_complete(&self) -> bool {
        self.nick.is_some() && self.user.is_some() && self.real_name.is_some()
    }
}

impl crate::stream_fold::StateUpdate<Result<IrcCommand, io::Error>> for UserNickBuilder {
    fn state_update(&mut self, new_item: Result<IrcCommand, io::Error>) -> bool {
        let cmd = match new_item {
            Ok(cmd) => cmd,
            Err(_) => return false,
        };

        trace!(self.ctx.logger, "Got command"; "command" => cmd.command());

        match cmd {
            IrcCommand::Nick { nick } => self.nick = Some(nick),
            IrcCommand::User { user, real_name } => {
                self.user = Some(user);
                self.real_name = Some(real_name);
            }
            IrcCommand::Pass { password } => self.password = Some(password),
            c => {
                debug!(self.ctx.logger, "Ignore command during login"; "cmd" => c.command());
            }
        }
        self.is_complete()
    }
}

#[derive(Debug, Clone, Default)]
struct UserNick {
    nick: String,
    user: String,
    real_name: String,
    password: Option<String>,
}

impl<S> IrcUserConnection<S>
where
    S: AsyncRead + AsyncWrite + 'static,
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

        let folder =
            crate::stream_fold::StreamFold::new(irc_conn, UserNickBuilder::with_context(ctx));

        // We cant consume the future since we need to split it into the irc connection and
        // user_nick below
        (&folder).await;

        let (mut irc_conn, user_nick) = folder.into_parts();

        let irc_user = user_nick.to_user_nick();

        info!(ctx_clone.logger, "got nick and user");

        let user_prefix = format!("{}!{}@{}", &irc_user.nick, &irc_user.user, &server_name);

        // Check that the password was collected in UserNickBuilder, otherwise send an error
        // message through IRC
        let password = if let Some(password) = irc_user.password {
            password
        } else {
            irc_conn.write_password_required(&irc_user.nick).await;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IRC user did not supply password",
            ));
        };

        let user_conn = IrcUserConnection {
            conn: irc_conn,
            user: irc_user.user,
            nick: irc_user.nick,
            real_name: irc_user.real_name,
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

    pub async fn add_channel(&mut self, name: String, topic: String, members: &[(&String, bool)]) {
        self.server_model.add_channel(name.clone(), topic, members);
        self.attempt_to_write_join_response(&name).await;
    }

    async fn attempt_to_write_join_response(&mut self, name: &str) -> bool {
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
            conn.write_join(&self.user_prefix, &channel.name).await;
            conn.write_topic(&self.server_name, &channel.name, &channel.topic)
                .await;
            conn.write_names(&self.nick, name, &names[..]).await;
            conn.write_numeric(
                Numeric::RplChannelmodeis,
                &self.nick,
                &format!("{} +n", &channel.name),
            )
            .await;
            true
        } else {
            false
        }
    }

    pub async fn send_message(&mut self, channel: &str, sender: &str, body: &str) {
        for line in body.split('\n') {
            self.conn
                .write_line(&format!(":{} PRIVMSG {} :{}", sender, channel, line))
                .await;
        }
    }

    pub async fn send_action(&mut self, channel: &str, sender: &str, body: &str) {
        for line in body.split('\n') {
            self.conn
                .write_line(&format!(
                    ":{} PRIVMSG {} :\u{0001}ACTION {}\u{0001}",
                    sender, channel, line
                ))
                .await;
        }
    }

    pub async fn write_invalid_password(&mut self) {
        self.conn.write_invalid_password(&self.nick).await;
    }

    pub async fn write_redirect_join(&mut self, old_channel: &str, new_channel: &str) {
        self.conn
            .write_numeric(
                Numeric::RplForwardedChannel,
                &self.nick,
                &format!(
                    "{} {} :Forwarding to another channel",
                    old_channel, new_channel,
                ),
            )
            .await;
    }

    pub async fn welcome(&mut self) {
        self.conn.welcome(&self.nick).await;
    }

    pub async fn send_ping(&mut self, data: &str) {
        let line = format!("PING {}", data);
        self.write_line(&line).await;
    }

    pub async fn write_line(&mut self, line: &str) {
        self.conn.write_line(line).await;
    }

    async fn handle_who_channel_cmd(&mut self, channel: String) {
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
                )
                .await;
            }
            conn.write_numeric(
                Numeric::RplEndofwho,
                &self.nick,
                &format!("{} :End of /WHO", &channel),
            )
            .await;
        } else {
            // TODO: No such room
        }
    }

    async fn handle_mode_channel_cmd(&mut self, channel: String) {
        self.conn
            .write_numeric(
                Numeric::RplChannelmodeis,
                &self.nick,
                &format!("{} +n", &channel),
            )
            .await;
    }

    pub async fn poll(&mut self) -> Poll<Result<Option<IrcCommand>, io::Error>> {
        loop {
            let next = self.conn.as_mut().next().await;

            match next {
                Some(Ok(cmd)) => match cmd {
                    IrcCommand::Ping { data } => {
                        let line = format!(":{} PONG {}", &self.server_name, data);
                        self.write_line(&line).await;
                        continue;
                    }
                    IrcCommand::Join { channel } => {
                        if !self.attempt_to_write_join_response(&channel).await {
                            return Poll::Ready(Ok(Some(IrcCommand::Join { channel })));
                        }
                    }
                    IrcCommand::Who { matches } => {
                        if matches.starts_with('#') {
                            self.handle_who_channel_cmd(matches).await;
                        } else {
                            // TODO
                        }
                    }
                    IrcCommand::Mode { target, mask } => {
                        if target.starts_with('#') && mask.is_none() {
                            self.handle_mode_channel_cmd(target).await;
                        } else {
                            // TODO
                        }
                    }
                    IrcCommand::Pong { .. } => {}
                    c => return Poll::Ready(Ok(Some(c))),
                },
                Some(Err(e)) => return Poll::Ready(Err(e)),
                None => return Poll::Ready(Ok(None)),
            }
        }
    }
}
