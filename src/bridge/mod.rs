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


use ConnectionContext;

use futures::{Async, Future, Poll};
use futures::stream::Stream;

use irc::{IrcCommand, IrcUserConnection};

use matrix::{LoginError, MatrixClient};
use matrix::protocol::{SyncResponse, JoinedRoomSyncResponse};
use matrix::Room as MatrixRoom;

use std::io;
use std::collections::BTreeMap;

use serde_json::Value;

use tokio_core::io::Io;
use tokio_core::reactor::Handle;
use url::Url;

use tasked_futures::{TaskExecutorQueue, TaskExecutor, FutureTaskedExt, TaskedFuture};


/// Bridges a single IRC connection with a matrix session.
///
/// The `Bridge` object is a future that resolves when the IRC connection closes the session (or
/// on unrecoverable error).
pub struct Bridge<IS: Io + 'static> {
    irc_conn: IrcUserConnection<IS>,
    matrix_client: MatrixClient,
    ctx: ConnectionContext,
    closed: bool,
    mappings: MappingStore,
    handle: Handle,
    is_first_sync: bool,
    executor_queue: TaskExecutorQueue<Bridge<IS>, io::Error>,
    joining_map: BTreeMap<String, String>,
}

impl<IS: Io> Bridge<IS> {
    /// Given a new TCP connection wait until the IRC side logs in, and then login to the Matrix
    /// HS with the given user name and password.
    ///
    /// The bridge won't process any IRC commands until the initial sync has finished.
    pub fn create(handle: Handle, base_url: Url, stream: IS, irc_server_name: String, ctx: ConnectionContext) -> impl Future<Item=Bridge<IS>, Error=io::Error> {
        IrcUserConnection::await_login(irc_server_name, stream, ctx.clone())
        .and_then(move |mut user_connection| {
            MatrixClient::login(
                handle.clone(), base_url, user_connection.user.clone(), user_connection.password.clone()
            ).then(move |res| {
                match res {
                    Ok(matrix_client) => Ok(Bridge {
                        irc_conn: user_connection,
                        matrix_client: matrix_client,
                        ctx: ctx,
                        closed: false,
                        mappings: MappingStore::default(),
                        handle: handle,
                        is_first_sync: true,
                        executor_queue: TaskExecutorQueue::default(),
                        joining_map: BTreeMap::new(),
                    }),
                    Err(LoginError::InvalidPassword) => {
                        user_connection.write_invalid_password();
                        Err(io::Error::new(io::ErrorKind::Other, "Invalid password"))
                    }
                    Err(LoginError::Io(e)) => Err(e),
                }
            })
        })
        .and_then(|mut bridge| {
            {
                let Bridge { ref mut mappings, ref mut irc_conn, ref matrix_client, .. } = bridge;
                let own_nick = irc_conn.nick.clone();
                let own_user_id = matrix_client.get_user_id().into();
                mappings.insert_nick(irc_conn, own_nick, own_user_id);
            }
            bridge.spawn_fn(|bridge| bridge.poll_irc());
            bridge.spawn_fn(|bridge| bridge.poll_matrix());
            Ok(bridge)
        })
    }

    fn handle_irc_cmd(&mut self, line: IrcCommand) {
        debug!(self.ctx.logger, "Received IRC line"; "command" => line.command());

        match line {
            IrcCommand::PrivMsg { channel, text } => {
                if let Some(room_id) = self.mappings.channel_to_room_id(&channel) {
                    info!(self.ctx.logger, "Got msg"; "channel" => channel.as_str(), "room_id" => room_id.as_str());
                    self.handle.spawn(
                        self.matrix_client.send_text_message(room_id, text)
                        .map(|_| ()).map_err(move |_| task_warn!("Failed to send"))
                    );
                } else {
                    warn!(self.ctx.logger, "Unknown channel"; "channel" => channel.as_str());
                }
            }
            IrcCommand::Join { channel } => {
                info!(self.ctx.logger, "Joining channel"; "channel" => channel);

                let join_future = self.matrix_client.join_room(channel.as_str())
                    .into_tasked()
                    .map(move |room_join_response, bridge: &mut Bridge<IS>| {
                        let room_id = room_join_response.room_id;

                        task_info!("Joined channel"; "channel" => channel, "room_id" => room_id);

                        if let Some(mapped_channel) = bridge.mappings.room_id_to_channel(&room_id) {
                            if mapped_channel == &channel {
                                // We've already joined this channel, most likely we got the sync
                                // response before the joined response.
                                // TODO: Do we wan to send something to IRC?
                                task_trace!("Already in IRC channel");
                            } else {
                                // We respond to the join with a redirect!
                                task_trace!("Redirecting channl"; "prev" => channel, "new" => *mapped_channel);
                                bridge.irc_conn.write_redirect_join(&channel, mapped_channel);
                            }
                        } else {
                            task_trace!("Waiting for room to come down sync"; "room_id" => room_id);
                            bridge.joining_map.insert(room_id, channel);
                        }
                    });
                // TODO: Handle failure of join. Ensure that joining map is cleared.
                self.spawn(join_future);
            }
            // TODO: Handle PART
            c => {
                warn!(self.ctx.logger, "Ignoring IRC command"; "command" => c.command());
            }
        }
    }

    fn handle_sync_response(&mut self, sync_response: SyncResponse) {
        trace!(self.ctx.logger, "Received sync response"; "batch" => sync_response.next_batch);

        if self.is_first_sync {
            info!(self.ctx.logger, "Received initial sync response");

            self.irc_conn.welcome();
            self.irc_conn.send_ping("HELLO");
        }

        for (room_id, sync) in &sync_response.rooms.join {
            self.handle_room_sync(room_id, sync);
        }

        if self.is_first_sync {
            info!(self.ctx.logger, "Finished processing initial sync response");
            self.is_first_sync = false;
        }
    }

    fn handle_room_sync(&mut self, room_id: &str, sync: &JoinedRoomSyncResponse) {
        let (channel, new) = if let Some(room) = self.matrix_client.get_room(room_id) {
            self.mappings.create_or_get_channel_name_from_matrix(&mut self.irc_conn, room)
        } else {
            warn!(self.ctx.logger, "Got room matrix doesn't know about"; "room_id" => room_id);
            return;
        };

        if let Some(attempt_channel) = self.joining_map.remove(room_id) {
            if &attempt_channel != &channel {
                self.irc_conn.write_redirect_join(&attempt_channel, &channel);
            }
        }

        for ev in &sync.timeline.events {
            if ev.etype == "m.room.message" {
                if let Some(body) = ev.content.find("body").and_then(Value::as_str) {
                    if let Some(sender_nick) = self.mappings.get_nick_from_matrix(&ev.sender) {
                        self.irc_conn.send_message(&channel, sender_nick, body);
                    } else {
                        warn!(self.ctx.logger, "Sender not in room"; "room" => room_id, "sender" => &ev.sender[..]);
                    }
                }
            }
        }

        if !new {
            // TODO: Send down new state
        }
    }

    fn poll_irc(&mut self) -> Poll<(), io::Error> {
        // Don't handle more IRC messages until we have done an initial sync.
        // This is safe as we will get woken up by the sync.
        if self.is_first_sync {
            return Ok(Async::NotReady);
        }

        loop {
            if let Some(line) = try_ready!(self.irc_conn.poll()) {
                self.handle_irc_cmd(line);
            } else {
                self.closed = true;
                self.stop();
                return Ok(Async::Ready(()));
            }
        }
    }

    fn poll_matrix(&mut self) -> Poll<(), io::Error> {
        loop {
            if let Some(sync_response) = try_ready!(self.matrix_client.poll()) {
                self.handle_sync_response(sync_response);
            } else {
                self.closed = true;
                self.stop();
                return Ok(Async::Ready(()))
            }
        }
    }
}

impl<IS: Io> TaskExecutor for Bridge<IS> {
    type Error = io::Error;

    fn task_executor_mut(&mut self) -> &mut TaskExecutorQueue<Self, io::Error> {
        &mut self.executor_queue
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
    pub fn insert_nick<S: Io>(&mut self, irc_server: &mut IrcUserConnection<S>, nick: String, user_id: String) {
        self.matrix_uid_to_nick.insert(user_id.clone(), nick.clone());
        self.nick_matrix_uid.insert(nick.clone(), user_id.clone());

        irc_server.create_user(nick.clone(), user_id.into());
    }

    pub fn channel_to_room_id(&mut self, channel: &str) -> Option<&String> {
        self.channel_to_room_id.get(channel)
    }

    pub fn room_id_to_channel(&mut self, room_id: &str) -> Option<&String> {
        self.room_id_to_channel.get(room_id)
    }

    pub fn create_or_get_channel_name_from_matrix<S: Io>(
        &mut self, irc_server: &mut IrcUserConnection<S>, room: &MatrixRoom
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
                let stripped_name: String = name.chars().filter(|c| match *c {
                    '\x00' ... '\x20' | '@' | '"' | '+' | '#' | '\x7F' => false,
                    _ => true,
                }).collect();

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

        self.room_id_to_channel.insert(room_id.into(), channel.clone());
        self.channel_to_room_id.insert(channel.clone(), room_id.into());

        let members: Vec<_> = room.get_members().iter().map(|(_, member)| {
            (self.create_or_get_nick_from_matrix(irc_server, &member.user_id, &member.display_name), member.moderator)
        }).collect();

        irc_server.add_channel(
            channel.clone(),
            room.get_topic().unwrap_or("").into(),
            &members.iter().map(|&(ref nick, op)| (nick, op)).collect::<Vec<_>>()[..],  // FIXME: To get around lifetimes
        );

        (channel, true)
    }

    pub fn create_or_get_nick_from_matrix<S: Io>(
        &mut self, irc_server: &mut IrcUserConnection<S>, user_id: &str, display_name: &str
    ) -> String {
        if let Some(nick) = self.matrix_uid_to_nick.get(user_id) {
            return nick.clone();
        }

        let mut nick: String = display_name.chars().filter(|c| match *c {
            '\x00' ... '\x20' | '@' | '"' | '+' | '#' | '\x7F' => false,
            _ => true,
        }).collect();


        if nick.len() < 3 {
            nick = user_id.chars().filter(|c| match *c {
                '\x00' ... '\x20' | '@' | '"' | '+' | '#' | '\x7F' => false,
                _ => true,
            }).collect();
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
