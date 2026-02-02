mod presence;
mod read_receipt;

use crate::{database::KeyValueDatabase, service};

impl service::rooms::edus::Data for KeyValueDatabase {}
