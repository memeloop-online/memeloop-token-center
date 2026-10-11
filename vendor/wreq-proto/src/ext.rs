//! Extensions for HTTP messages in wreq_proto.

mod h1_reason_phrase;
mod h2_attempt_observer;
mod informational;
mod preserve_header;

pub use self::{
    h2_attempt_observer::{H2AttemptObserver, H2AttemptSnapshot},
    h1_reason_phrase::ReasonPhrase,
    informational::on_informational,
    preserve_header::{on_preserve_header, OnPreserveHeaderCallback},
};
pub(crate) use self::{informational::OnInformational, preserve_header::OnPreserveHeader};
pub(crate) use h2_attempt_observer::H2Connection;
