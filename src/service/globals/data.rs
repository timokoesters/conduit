use ruma::signatures::Ed25519KeyPair;

use crate::Result;

pub trait Data {
    fn load_keypair(&self) -> Result<Ed25519KeyPair>;
    fn remove_keypair(&self) -> Result<()>;
}
