pub mod presence;
pub mod read_receipt;
pub mod typing;

pub trait Data: presence::Data + read_receipt::Data + typing::Data + 'static {}

pub struct Service {
    pub presence: presence::Service,
    pub read_receipt: read_receipt::Service,
    pub typing: typing::Service,
}
