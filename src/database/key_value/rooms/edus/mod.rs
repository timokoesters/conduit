mod presence;
mod read_receipt;
mod typing;

use crate::{database::KeyValueDatabase, service};

impl service::rooms::edus::Data for KeyValueDatabase {}
