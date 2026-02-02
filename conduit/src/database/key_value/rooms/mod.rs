mod alias;
mod auth_chain;
mod directory;
mod edus;
mod lazy_load;
mod metadata;
mod outlier;
mod pdu_metadata;
mod search;
mod short;
mod state;
mod state_accessor;
mod state_cache;
mod state_compressor;
mod threads;
mod timeline;
mod user;

use ruma::{RoomId, UserId};

use crate::{database::KeyValueDatabase, service};

impl service::rooms::Data for KeyValueDatabase {}

/// Constructs roomuser_id and userroom_id respectively in byte form
fn get_room_and_user_byte_ids(room_id: &RoomId, user_id: &UserId) -> (Vec<u8>, Vec<u8>) {
    (
        get_roomuser_id_bytes(room_id, user_id),
        get_userroom_id_bytes(user_id, room_id),
    )
}

fn get_roomuser_id_bytes(room_id: &RoomId, user_id: &UserId) -> Vec<u8> {
    let mut roomuser_id = room_id.as_bytes().to_vec();
    roomuser_id.push(0xff);
    roomuser_id.extend_from_slice(user_id.as_bytes());
    roomuser_id
}

fn get_userroom_id_bytes(user_id: &UserId, room_id: &RoomId) -> Vec<u8> {
    let mut userroom_id = user_id.as_bytes().to_vec();
    userroom_id.push(0xff);
    userroom_id.extend_from_slice(room_id.as_bytes());
    userroom_id
}
