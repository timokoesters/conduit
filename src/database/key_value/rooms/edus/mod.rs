mod presence;
mod typing;
mod read_receipt;

use crate::{service, database::KeyValueDatabase};

impl service::rooms::edus::Data for KeyValueDatabase {}
