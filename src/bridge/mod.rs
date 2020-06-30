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

//! The module responsible for mapping IRC and Matrix onto each other.

use crate::ConnectionContext;

//use futures::stream::Stream;
//use futures::{Async, Future, Poll};
use futures3::compat::Compat;
use futures3::future::TryFutureExt;
use futures3::prelude::{Future, Stream};
use futures3::stream::StreamExt;
use futures3::task::Poll;

use crate::irc::{IrcCommand, IrcUserConnection};

use crate::matrix::protocol::{JoinedRoomSyncResponse, SyncResponse};
use crate::matrix::Room as MatrixRoom;
use crate::matrix::{self, MatrixClient};

use quick_error::quick_error;

use std::boxed::Box;
use std::collections::BTreeMap;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;

use serde_json::Value;

//use tokio_core::reactor::Handle;
use tokio::io::{AsyncRead, AsyncWrite};
use url::Url;

/// Bridges a single IRC connection with a matrix session.
///
/// The `Bridge` object is a future that resolves when the IRC connection closes the session (or
/// on unrecoverable error).
pub struct Bridge<IS: AsyncRead + AsyncWrite + 'static> {
    irc_conn: Pin<Box<IrcUserConnection<IS>>>,
    matrix_client: Pin<Box<MatrixClient>>,
    ctx: ConnectionContext,
    closed: bool,
    mappings: MappingStore,
    is_first_sync: bool,
    joining_map: BTreeMap<String, String>,
}

impl<IS: AsyncRead + AsyncWrite + 'static + Send> Bridge<IS> {
    /// Given a new TCP connection wait until the IRC side logs in, and then login to the Matrix
    /// HS with the given user name and password.
    ///
    /// The bridge won't process any IRC commands until the initial sync has finished.
    pub async fn create(
        base_url: Url,
        stream: IS,
        irc_server_name: String,
        ctx: ConnectionContext,
    ) -> Result<Bridge<IS>, Error> {
        // make individual connections
        let irc_conn = IrcUserConnection::await_login(irc_server_name, stream, ctx.clone()).await?;
        let matrix_client =
            MatrixClient::login(base_url, irc_conn.user.clone(), irc_conn.password.clone()).await?;

        // setup connections to intermediate bridge
        let mut bridge = Bridge {
            irc_conn: Box::pin(irc_conn),
            matrix_client: Box::pin(matrix_client),
            ctx,
            closed: false,
            mappings: MappingStore::default(),
            is_first_sync: true,
            joining_map: BTreeMap::new(),
        };

        let Bridge {
            ref mut mappings,
            ref mut irc_conn,
            ref matrix_client,
            ..
        } = bridge;
        let own_nick = irc_conn.nick.clone();
        let own_user_id = matrix_client.get_user_id().into();
        mappings.insert_nick(irc_conn, own_nick, own_user_id);

        Ok(bridge)
    }

    async fn handle_irc_cmd(&mut self, line: IrcCommand) {
        debug!(self.ctx.logger, "Received IRC line"; "command" => line.command());

        match line {
            IrcCommand::PrivMsg { channel, text } => {
                if let Some(room_id) = self.mappings.channel_to_room_id(&channel) {
                    info!(self.ctx.logger, "Got msg"; "channel" => channel.as_str(), "room_id" => room_id.as_str());
                    self.matrix_client
                        .send_text_message(room_id, text)
                        .await
                        .map_err(move |_| task_warn!("Failed to send"));
                } else {
                    warn!(self.ctx.logger, "Unknown channel"; "channel" => channel.as_str());
                }
            }
            IrcCommand::Join { channel } => {
                info!(self.ctx.logger, "Joining channel"; "channel" => channel.clone());

                let join_future =
                    if let Ok(response) = self.matrix_client.join_room(channel.as_str()).await {
                        response
                    } else {
                        // TODO: log this
                        return ();
                    };

                let room_id = join_future.room_id;

                task_info!("Joined channel"; "channel" => channel.clone(), "room_id" => room_id.clone());

                if let Some(mapped_channel) = self.mappings.room_id_to_channel(&room_id) {
                    if mapped_channel == &channel {
                        // We've already joined this channel, most likely we got the sync
                        // response before the joined response.
                        // TODO: Do we wan to send something to IRC?
                        task_trace!("Already in IRC channel");
                    } else {
                        // We respond to the join with a redirect!
                        task_trace!("Redirecting channl"; "prev" => channel.clone(), "new" => mapped_channel.clone());
                        self.irc_conn
                            .write_redirect_join(&channel, mapped_channel)
                            .await;
                    }
                } else {
                    task_trace!("Waiting for room to come down sync"; "room_id" => room_id.clone());
                    self.joining_map.insert(room_id, channel);
                };
            }
            // TODO: Handle PART
            c => {
                warn!(self.ctx.logger, "Ignoring IRC command"; "command" => c.command());
            }
        }
    }

    async fn handle_sync_response(&mut self, sync_response: SyncResponse) {
        trace!(self.ctx.logger, "Received sync response"; "batch" => sync_response.next_batch);

        if self.is_first_sync {
            info!(self.ctx.logger, "Received initial sync response");

            self.irc_conn.welcome().await;
            self.irc_conn.send_ping("HELLO").await;
        }

        for (room_id, sync) in &sync_response.rooms.join {
            self.handle_room_sync(room_id, sync).await;
        }

        if self.is_first_sync {
            info!(self.ctx.logger, "Finished processing initial sync response");
            self.is_first_sync = false;
        }
    }

    async fn handle_room_sync(&mut self, room_id: &str, sync: &JoinedRoomSyncResponse) {
        let (channel, new) = if let Some(room) = self.matrix_client.get_room(room_id) {
            self.mappings
                .create_or_get_channel_name_from_matrix(&mut self.irc_conn, room)
                .await
        } else {
            warn!(self.ctx.logger, "Got room matrix doesn't know about"; "room_id" => room_id);
            return;
        };

        if let Some(attempt_channel) = self.joining_map.remove(room_id) {
            if &attempt_channel != &channel {
                self.irc_conn
                    .write_redirect_join(&attempt_channel, &channel)
                    .await;
            }
        }

        for ev in &sync.timeline.events {
            if ev.etype == "m.room.message" {
                let sender_nick = match self.mappings.get_nick_from_matrix(&ev.sender) {
                    Some(x) => x,
                    None => {
                        warn!(self.ctx.logger, "Sender not in room"; "room" => room_id, "sender" => &ev.sender[..]);
                        continue;
                    }
                };
                let body = match ev.content.get("body").and_then(Value::as_str) {
                    Some(x) => x,
                    None => {
                        warn!(self.ctx.logger, "Message has no body"; "room" => room_id, "message" => format!("{:?}", ev));
                        continue;
                    }
                };
                let msgtype = match ev.content.get("msgtype").and_then(Value::as_str) {
                    Some(x) => x,
                    None => {
                        warn!(self.ctx.logger, "Message has no msgtype"; "room" => room_id, "message" => format!("{:?}", ev));
                        continue;
                    }
                };
                match msgtype {
                    "m.text" => {
                        self.irc_conn
                            .send_message(&channel, sender_nick, body)
                            .await
                    }
                    "m.emote" => self.irc_conn.send_action(&channel, sender_nick, body).await,
                    "m.image" | "m.file" | "m.video" | "m.audio" => {
                        let url = ev.content.get("url").and_then(Value::as_str);
                        match url {
                            Some(url) => {
                                self.irc_conn
                                    .send_message(
                                        &channel,
                                        sender_nick,
                                        self.matrix_client.media_url(&url).as_str(),
                                    )
                                    .await
                            }
                            None => {
                                warn!(self.ctx.logger, "Media message has no url"; "room" => room_id,
                                                                                          "message" => format!("{:?}", ev));
                            }
                        }
                    }
                    _ => {
                        warn!(self.ctx.logger, "Unknown msgtype"; "room" => room_id, "msgtype" => msgtype);
                        self.irc_conn
                            .send_message(&channel, sender_nick, body)
                            .await;
                    }
                }
            }
        }

        if !new {
            // TODO: Send down new state
        }
    }

    pub async fn poll_irc(&mut self) -> Poll<Result<(), io::Error>> {
        // Don't handle more IRC messages until we have done an initial sync.
        // This is safe as we will get woken up by the sync.
        if self.is_first_sync {
            return Poll::Pending;
        }

        loop {
            let poll_response = match self.irc_conn.as_mut().poll().await? {
                Poll::Ready(x) => x,
                Poll::Pending => return Poll::Pending,
            };

            if let Some(line) = poll_response {
                self.handle_irc_cmd(line).await;
            } else {
                self.closed = true;
                // TODO: figure out how to replicate this behavior with the task
                //self.stop();
                return Poll::Ready(Ok(()));
            }
        }
    }

    pub async fn poll_matrix(&mut self) -> Result<(), Error> {
        while let Some(response) = self.matrix_client.as_mut().next().await {
            let response = response?;
            self.handle_sync_response(response).await;
        }
        Ok(())
    }
}

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Io(err: io::Error) {
            from()
            description("io error")
            display("I/O error: {}", err)
            cause(err)
        }
        InvalidPassword {
            description("password was invalid")
            display("Password is invalid")
        }
        HyperErr(err: hyper::Error) {
            from()
            description("hyper error when making http request")
            display("Hyper error: {}", err)
        }
        SerdeErr(err: serde_json::Error) {
            from()
            description("could not serialize / deserialize struct")
            display("Could not serialize request struct or deserialize response")
        }
        MatrixError(err: matrix::Error) {
            from()
            description("an error occured when polling the matrix client")
            display("An error occured when polling the matrix client: {} ", err)
        }
    }
}

/// Handles mapping various IRC and Matrix ID's onto each other.
#[derive(Debug, Clone, Default)]
struct MappingStore {
    channel_to_room_id: BTreeMap<String, String>,
    room_id_to_channel: BTreeMap<String, String>,

    matrix_uid_to_nick: BTreeMap<String, String>,
    nick_matrix_uid: BTreeMap<String, String>,
}

impl MappingStore {
    pub fn insert_nick<S: AsyncRead + AsyncWrite + 'static>(
        &mut self,
        irc_server: &mut IrcUserConnection<S>,
        nick: String,
        user_id: String,
    ) {
        self.matrix_uid_to_nick
            .insert(user_id.clone(), nick.clone());
        self.nick_matrix_uid.insert(nick.clone(), user_id.clone());

        irc_server.create_user(nick.clone(), user_id.into());
    }

    pub fn channel_to_room_id(&mut self, channel: &str) -> Option<&String> {
        self.channel_to_room_id.get(channel)
    }

    pub fn room_id_to_channel(&mut self, room_id: &str) -> Option<&String> {
        self.room_id_to_channel.get(room_id)
    }

    pub async fn create_or_get_channel_name_from_matrix<S: AsyncRead + AsyncWrite + 'static>(
        &mut self,
        irc_server: &mut IrcUserConnection<S>,
        room: &MatrixRoom,
    ) -> (String, bool) {
        let room_id = room.get_room_id();

        if let Some(channel) = self.room_id_to_channel.get(room_id) {
            return (channel.clone(), false);
        }

        // FIXME: Make sure it really is unique
        let mut channel = {
            if let Some(alias) = room.get_state_content_key("m.room.canonical_alias", "", "alias") {
                alias.into()
            } else if let Some(name) = room.get_name() {
                let stripped_name: String = name
                    .chars()
                    .filter(|c| match *c {
                        '\x00'..='\x20' | '@' | '"' | '+' | '#' | '\x7F' => false,
                        _ => true,
                    })
                    .collect();

                if !stripped_name.is_empty() {
                    format!("#{}", stripped_name)
                } else {
                    format!("#{}", room_id)
                }
            } else {
                format!("#{}", room_id)
            }
        };

        if irc_server.channel_exists(&channel) {
            let mut idx = 1;
            loop {
                let new_channel = format!("{}[{}]", &channel, idx);
                if !irc_server.channel_exists(&new_channel) {
                    channel = new_channel;
                    break;
                }
                idx += 1;
            }
        }

        self.room_id_to_channel
            .insert(room_id.into(), channel.clone());
        self.channel_to_room_id
            .insert(channel.clone(), room_id.into());

        let members: Vec<_> = room
            .get_members()
            .iter()
            .map(|(_, member)| {
                (
                    self.create_or_get_nick_from_matrix(
                        irc_server,
                        &member.user_id,
                        &member.display_name,
                    ),
                    member.moderator,
                )
            })
            .collect();

        irc_server
            .add_channel(
                channel.clone(),
                room.get_topic().unwrap_or("").into(),
                &members
                    .iter()
                    .map(|&(ref nick, op)| (nick, op))
                    .collect::<Vec<_>>()[..], // FIXME: To get around lifetimes
            )
            .await;

        (channel, true)
    }

    pub fn create_or_get_nick_from_matrix<S: AsyncRead + AsyncWrite + 'static>(
        &mut self,
        irc_server: &mut IrcUserConnection<S>,
        user_id: &str,
        display_name: &str,
    ) -> String {
        if let Some(nick) = self.matrix_uid_to_nick.get(user_id) {
            return nick.clone();
        }

        let mut nick: String = display_name
            .chars()
            .filter(|c| match *c {
                '\x00'..='\x20' | '@' | '"' | '+' | '#' | '\x7F' => false,
                _ => true,
            })
            .collect();

        if nick.len() < 3 {
            nick = user_id
                .chars()
                .filter(|c| match *c {
                    '\x00'..='\x20' | '@' | '"' | '+' | '#' | '\x7F' => false,
                    _ => true,
                })
                .collect();
        }

        if irc_server.nick_exists(&nick) {
            let mut idx = 1;
            loop {
                let new_nick = format!("{}[{}]", &nick, idx);
                if !irc_server.nick_exists(&new_nick) {
                    nick = new_nick;
                    break;
                }
                idx += 1;
            }
        }

        self.matrix_uid_to_nick.insert(user_id.into(), nick.clone());
        self.nick_matrix_uid.insert(nick.clone(), user_id.into());

        irc_server.create_user(nick.clone(), user_id.into());

        nick
    }

    pub fn get_nick_from_matrix(&self, user_id: &str) -> Option<&String> {
        self.matrix_uid_to_nick.get(user_id)
    }
}
