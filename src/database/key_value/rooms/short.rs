use std::sync::Arc;

use crate::{database::KeyValueDatabase, service};

impl service::rooms::short::Data for Arc<KeyValueDatabase> {
}
