pub mod presence;
pub mod read_receipt;
pub mod typing;

pub trait Data: presence::Data + read_receipt::Data + typing::Data {}

pub struct Service<D: Data> {
    presence: presence::Service<D>,
    read_receipt: read_receipt::Service<D>,
    typing: typing::Service<D>,
}
