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

use gm::MatrixClient;
use gm::request::MatrixRequestable;
use gm::sync::SyncStream;
use gm::errors::MatrixError;
use gm::room::{Room, RoomExt, NewRoom};
use gm::types::messages::Message;
use gm::types::events::Event;
use gm::types::content::Content;
use gm::types::sync::{JoinedRoom, SyncReply};
use failure;

use std::collections::BTreeMap;

use tokio_io::{AsyncRead, AsyncWrite};
use tokio_core::reactor::Handle;
use regex::Regex;

use tasked_futures::{TaskExecutorQueue, TaskExecutor, FutureTaskedExt, TaskedFuture};

pub fn media_url<R: MatrixRequestable>(mx: &R, url: &str) -> String {
    let re = Regex::new("^mxc://([^/]+/[^/]+)$").unwrap();
    if let Some(captures) = re.captures(url) {
        // FIXME: needs to worry about URL escaping
        format!("{}/_matrix/media/v1/download/{}", mx.get_url(), &captures[1])
    } else {
        url.to_owned()
    }
}
/// Bridges a single IRC connection with a matrix session.
///
/// The `Bridge` object is a future that resolves when the IRC connection closes the session (or
/// on unrecoverable error).
pub struct Bridge<IS: AsyncRead + AsyncWrite + 'static> {
    irc_conn: IrcUserConnection<IS>,
    matrix_client: MatrixClient,
    sync: SyncStream<MatrixClient>,
    ctx: ConnectionContext,
    closed: bool,
    mappings: MappingStore,
    handle: Handle,
    is_first_sync: bool,
    executor_queue: TaskExecutorQueue<Bridge<IS>, failure::Error>,
    joining_map: BTreeMap<String, String>,
}

impl<IS: AsyncRead + AsyncWrite + 'static> Bridge<IS> {
    /// Given a new TCP connection wait until the IRC side logs in, and then login to the Matrix
    /// HS with the given user name and password.
    ///
    /// The bridge won't process any IRC commands until the initial sync has finished.
    pub fn create(handle: Handle, base_url: String, stream: IS, irc_server_name: String, ctx: ConnectionContext) -> impl Future<Item = Bridge<IS>, Error = failure::Error> {
        IrcUserConnection::await_login(irc_server_name, stream, ctx.clone())
            .map_err(|e| e.into())
        .and_then(move |mut user_connection| {
            // FIXME: provide a way to login via access token? Perhaps by setting the
            // username to 'ACCESS_TOKEN_LOGIN' and password to access token or similar?
            MatrixClient::login_password(
                &user_connection.user, &user_connection.password, &base_url.to_string(), &handle
            ).then(move |res| {
                match res {
                    Ok(matrix_client) => Ok(Bridge {
                        irc_conn: user_connection,
                        sync: SyncStream::new(matrix_client.clone()),
                        matrix_client: matrix_client,
                        ctx: ctx,
                        closed: false,
                        mappings: MappingStore::default(),
                        handle: handle,
                        is_first_sync: true,
                        executor_queue: TaskExecutorQueue::default(),
                        joining_map: BTreeMap::new(),
                    }),
                    Err(MatrixError::BadRequest(ref e)) if e.errcode == "M_FORBIDDEN" => {
                        user_connection.write_invalid_password();
                        Err(format_err!("Invalid password."))
                    }
                    Err(e) => Err(e.into()),
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
        .map_err(|e| e.into())
    }

    fn handle_irc_cmd(&mut self, line: IrcCommand) {
        debug!(self.ctx.logger, "Received IRC line"; "command" => line.command());

        match line {
            IrcCommand::PrivMsg { channel, text } => {
                if let Some(room_id) = self.mappings.channel_to_room_id(&channel) {
                    info!(self.ctx.logger, "Got msg"; "channel" => channel.as_str(), "room_id" => room_id.as_str());
                    let room = Room::from_id(room_id);
                    // FIXME: Ideally, we should have some form of message processor here,
                    // to handle things like ACTIONs, colours, bold, etc.
                    //
                    // https://modern.ircdocs.horse/formatting.html might prove helpful.
                    let fut = room.cli(&mut self.matrix_client)
                        .send(Message::Text { body: text, formatted_body: None, format: None });
                    self.handle.spawn(
                        fut
                        .map(|_| ()).map_err(move |_| task_warn!("Failed to send"))
                    );
                } else {
                    warn!(self.ctx.logger, "Unknown channel"; "channel" => channel.as_str());
                }
            }
            IrcCommand::Join { channel } => {
                info!(self.ctx.logger, "Joining channel"; "channel" => channel.clone());
                let join_future = NewRoom::join(&mut self.matrix_client, channel.as_str())
                    .map_err(|e| e.into())
                    .into_tasked()
                    .map(move |room, bridge: &mut Bridge<IS>| {
                        let room_id = room.id;
                        task_info!("Joined channel"; "channel" => channel.clone(), "room_id" => room_id.to_string());

                        if let Some(mapped_channel) = bridge.mappings.room_id_to_channel(&room_id) {
                            if mapped_channel == &channel {
                                // We've already joined this channel, most likely we got the sync
                                // response before the joined response.
                                // TODO: Do we wan to send something to IRC?
                                task_trace!("Already in IRC channel");
                            } else {
                                // We respond to the join with a redirect!
                                task_trace!("Redirecting channl"; "prev" => channel.clone(), "new" => mapped_channel.clone());
                                bridge.irc_conn.write_redirect_join(&channel, mapped_channel);
                            }
                        } else {
                            task_trace!("Waiting for room to come down sync"; "room_id" => room_id.to_string());
                            bridge.joining_map.insert(room_id.into(), channel);
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

    fn handle_sync_response(&mut self, sync_response: SyncReply) {
        trace!(self.ctx.logger, "Received sync response"; "batch" => sync_response.next_batch);

        if self.is_first_sync {
            info!(self.ctx.logger, "Received initial sync response");

            self.irc_conn.welcome();
            self.irc_conn.send_ping("HELLO");
        }

        for (room, sync) in sync_response.rooms.join {
            self.handle_room_sync(room, sync);
        }

        if self.is_first_sync {
            info!(self.ctx.logger, "Finished processing initial sync response");
            self.is_first_sync = false;
        }
    }

    fn handle_room_sync(&mut self, room: Room<'static>, mut sync: JoinedRoom) {
        if self.mappings.room_id_to_channel(&room.id).is_none() {
            self.mappings.create_mapping(&mut self.irc_conn, room.clone(), ::std::mem::replace(&mut sync.state.events, vec![]));
        }
        let channel = self.mappings.room_id_to_channel(&room.id).unwrap();

        if let Some(attempt_channel) = self.joining_map.remove(&room.id as &str) {
            if &attempt_channel as &str != &channel as &str {
                self.irc_conn.write_redirect_join(&attempt_channel, &channel);
            }
        }

        for ev in &sync.timeline.events {
            // FIXME: handle extra state events
            if let Some(ref rd) = ev.room_data {
                if let Some(ref unsigned) = rd.unsigned {
                    if let Some(ref txid) = unsigned.transaction_id {
                        /* we sent this event; ignore */
                        trace!(self.ctx.logger, "Ignoring self-event"; "txid" => txid.to_owned(), "room" => room.id.to_string(), "evtid" => rd.event_id.to_owned());
                        continue;
                    }
                }
                let sender_nick = match self.mappings.get_nick_from_matrix(&rd.sender) {
                    Some(x) => x,
                    None    => {
                        warn!(self.ctx.logger, "Sender not in room"; "room" => room.id.to_string(), "sender" => rd.sender.to_string());
                        continue;
                    }
                };
                match ev.content {
                    Content::RoomMessage(ref msg) => {
                        match msg {
                            Message::Text { ref body, .. } => self.irc_conn.send_message(&channel, sender_nick, body),
                            // FIXME: this should actually send a NOTICE on the IRC side...
                            Message::Notice { ref body, .. } => self.irc_conn.send_message(&channel, sender_nick, body),
                            Message::Emote { ref body } => self.irc_conn.send_action(&channel, sender_nick, body),
                            Message::Image { ref url, .. } | Message::File { ref url, .. } | Message::Video { ref url, .. } | Message::Audio { ref url, .. } => {
                                self.irc_conn.send_message(&channel, sender_nick, media_url(&self.matrix_client, url).as_str())
                            },
                            Message::Location { ref body, ref geo_uri } => self.irc_conn.send_message(&channel, sender_nick, &format!("Location '{}': {}", body, geo_uri))
                        }
                    },
                    // TODO: handle other room event types as well?
                    _ => {}
                }
            }
        }
    }

    fn poll_irc(&mut self) -> Poll<(), failure::Error> {
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

    fn poll_matrix(&mut self) -> Poll<(), failure::Error> {
        loop {
            if let Some(sync_response) = try_ready!(self.sync.poll()) {
                self.handle_sync_response(sync_response);
            } else {
                self.closed = true;
                self.stop();
                return Ok(Async::Ready(()))
            }
        }
    }
}

impl<IS: AsyncRead + AsyncWrite> TaskExecutor for Bridge<IS> {
    type Error = failure::Error;

    fn task_executor_mut(&mut self) -> &mut TaskExecutorQueue<Self, failure::Error> {
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
    pub fn insert_nick<S: AsyncRead + AsyncWrite + 'static>(&mut self, irc_server: &mut IrcUserConnection<S>, nick: String, user_id: String) {
        self.matrix_uid_to_nick.insert(user_id.clone(), nick.clone());
        self.nick_matrix_uid.insert(nick.clone(), user_id.clone());

        irc_server.create_user(nick.clone(), user_id.into());
    }

    pub fn channel_to_room_id(&self, channel: &str) -> Option<&String> {
        self.channel_to_room_id.get(channel)
    }

    pub fn room_id_to_channel(&self, room_id: &str) -> Option<&String> {
        self.room_id_to_channel.get(room_id)
    }

    pub fn create_mapping<S>(&mut self, irc_server: &mut IrcUserConnection<S>, room: Room<'static>, state: Vec<Event>)
        where S: AsyncRead + AsyncWrite + 'static {
        let mut channel = room.id.to_string();
        // "Channel name level" - i.e. how desirable the current value of 
        // 'channel' is, on a scale from 0 (best) to u32::MAX (worst).
        // Used to prevent the room name from trampling over the canonical
        // alias, for example.
        let mut channel_level = ::std::u32::MAX;
        let mut members = BTreeMap::new();
        let mut topic = String::new();
        let mut ops = BTreeMap::new();

        // FIXME: also calculate a room name based on the members in the room.
        for evt in state {
            match evt.content {
                Content::RoomCanonicalAlias(ca) => {
                    channel = ca.alias;
                    channel_level = 0;
                },
                Content::RoomName(name) => {
                    if channel_level > 1 {
                        let stripped_name: String = name.name.chars().filter(|c| match *c {
                            '\x00' ... '\x20' | '@' | '"' | '+' | '#' | '\x7F' => false,
                            _ => true,
                        }).collect();

                        if !stripped_name.is_empty() {
                            channel = format!("&{}", stripped_name);
                            channel_level = 1;
                        }
                    }
                },
                Content::RoomTopic(t) => {
                    topic = t.topic;
                },
                Content::RoomMember(member) => {
                    if let Some(sd) = evt.state_data {
                        // FIXME: This assumes that the state_key is always something useful (i.e.
                        // never blank)
                        // FIXME: This also doesn't do per-room displaynames well.
                        let nick = self.create_or_get_nick_from_matrix(irc_server, &sd.state_key, member.displayname.as_ref().unwrap_or(&sd.state_key));
                        members.insert(sd.state_key, nick);
                    }
                },
                Content::RoomPowerLevels(ple) => {
                    for (uid, pl) in ple.users {
                        // FIXME: this calculation is a trifle inflexible
                        // and could also use things like halfops as well
                        ops.insert(uid, pl >= 50);
                    }
                },
                _ => {}
            }
        }
        self.room_id_to_channel.insert(room.id.to_string(), channel.clone());
        self.channel_to_room_id.insert(channel.clone(), room.id.to_string());
        let members = members.into_iter()
            .map(|(uid, nick)| {
                (nick, ops.get(&uid).map(|x| *x).unwrap_or(false))
            })
            .collect::<Vec<_>>();

        irc_server.add_channel(
            channel.clone(),
            topic,
            &members.iter().map(|&(ref nick, op)| (nick, op)).collect::<Vec<_>>(),  // FIXME: To get around lifetimes
        );
    }

    pub fn create_or_get_nick_from_matrix<S: AsyncRead + AsyncWrite + 'static>(
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
