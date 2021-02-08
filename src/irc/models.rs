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

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub struct Channel {
    pub name: String,
    pub topic: String,
    pub modes: Vec<u8>,
    pub users: BTreeMap<String, UserRoomEntry>,
}

#[derive(Debug, Clone)]
pub struct UserRoomEntry {
    pub operator: bool,
    pub voice: bool,
}

#[derive(Debug, Clone)]
pub struct User {
    pub nick: String,
    pub user: String,
    pub mask: String,
    channels: BTreeSet<String>,
}

pub struct ServerModel {
    channels: BTreeMap<String, Channel>,
    users: BTreeMap<String, User>,
}

impl ServerModel {
    pub fn new() -> ServerModel {
        ServerModel {
            channels: BTreeMap::new(),
            users: BTreeMap::new(),
        }
    }

    pub fn nick_exists(&self, nick: &str) -> bool {
        self.users.contains_key(nick)
    }

    pub fn channel_exists(&self, channel: &str) -> bool {
        self.channels.contains_key(channel)
    }

    pub fn create_user(&mut self, nick: String, user: String) {
        self.users.insert(
            nick.clone(),
            User {
                nick,
                user,
                mask: "/matrix/user".into(),
                channels: BTreeSet::new(),
            },
        );
    }

    pub fn add_channel(
        &mut self,
        name: String,
        topic: String,
        members: &[(&String, bool)],
    ) -> &Channel {
        if self.channels.contains_key(&name) {
            panic!("Trying to add channel that already exists");
        }

        let channel = Channel {
            name: name.clone(),
            topic,
            modes: Vec::from("+n"),
            users: members
                .iter()
                .map(|&(nick, is_operator)| {
                    (
                        nick.to_string(),
                        UserRoomEntry {
                            operator: is_operator,
                            voice: false,
                        },
                    )
                })
                .collect(),
        };

        self.channels.insert(name.clone(), channel);

        for &(member, _) in members {
            if let Some(user) = self.users.get_mut(member) {
                user.channels.insert(name.clone());
            }
        }

        &self.channels[&name]
    }

    pub fn get_channel(&self, channel: &str) -> Option<&Channel> {
        self.channels.get(channel)
    }

    pub fn get_members(&self, channel: &str) -> Option<Vec<(User, UserRoomEntry)>> {
        self.channels.get(channel).map(|entry| {
            entry
                .users
                .iter()
                .map(|(nick, user_entry)| {
                    (
                        self.users
                            .get(nick)
                            .expect("expected user to exist")
                            .clone(),
                        user_entry.clone(),
                    )
                })
                .collect()
        })
    }
}
