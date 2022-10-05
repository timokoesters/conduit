pub mod presence;
pub mod read_receipt;
pub mod typing;

pub trait Data: presence::Data + read_receipt::Data + typing::Data {}

pub struct Service<D: Data> {
    pub presence: presence::Service<D>,
    pub read_receipt: read_receipt::Service<D>,
    pub typing: typing::Service<D>,
}
