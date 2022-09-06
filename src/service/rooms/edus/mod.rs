pub mod presence;
pub mod read_receipt;
pub mod typing;

pub struct Service<D> {
    presence: presence::Service<D>,
    read_receipt: read_receipt::Service<D>,
    typing: typing::Service<D>,
}
