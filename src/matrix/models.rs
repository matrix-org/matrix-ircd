use std::collections::BTreeMap;

use serde_json;


#[derive(Debug, Clone, Deserialize)]
pub struct SyncResponse {
    pub next_batch: String,
    pub rooms: RoomsSyncResponse,
}


#[derive(Debug, Clone, Deserialize)]
pub struct RoomsSyncResponse {
    pub join: BTreeMap<String, JoinedRoomSyncResponse>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JoinedRoomSyncResponse {
    pub timeline: RoomTimelineSyncResponse,
    pub state: RoomStateSyncResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoomTimelineSyncResponse {
    pub limited: bool,
    pub prev_batch: String,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoomStateSyncResponse {
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub sender: String,
    pub event_id: String,
    #[serde(rename = "type")]
    pub etype: String,
    pub state_key: Option<String>,
    pub content: serde_json::Value,
}


#[derive(Debug, Clone, Serialize)]
pub struct LoginPasswordInput {
    pub user: String,
    pub password: String,
    #[serde(rename = "type")]
    pub login_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub device_id: String,
    pub home_server: String,
    pub user_id: String,
}


#[cfg(test)]
mod tests {
    use serde_json;
    use super::*;

    #[test]
    fn sync_response() {
        let sync_response_str = r#"
        {
          "next_batch": "s2240646_7037295_67564_1482345_530_40_551",
          "rooms": {
            "leave": {},
            "join": {
              "!SDFsdfqsf24SB:matrix.org": {
                "unread_notifications": {
                  "highlight_count": 0,
                  "notification_count": 0
                },
                "timeline": {
                  "limited": false,
                  "prev_batch": "s2240646_7037295_67564_1482345_530_40_551",
                  "events": [
                    {
                      "origin_server_ts": 1475512030637,
                      "sender": "@wibble:matrix.org",
                      "event_id": "$147sdfsdfsdfKBLVL:matrix.org",
                      "unsigned": {
                        "age": 1008
                      },
                      "content": {
                        "body": "test test test test",
                        "msgtype": "m.text"
                      },
                      "type": "m.room.message"
                    }
                  ]
                },
                "state": {
                  "events": []
                },
                "ephemeral": {
                  "events": []
                },
                "account_data": {
                  "events": []
                }
              }
            },
            "invite": {}
          },
          "account_data": {
            "events": []
          },
          "to_device": {
            "events": []
          },
          "presence": {}
        }
        "#;

        let _parsed: SyncResponse = serde_json::from_str(sync_response_str).unwrap();
    }
}
