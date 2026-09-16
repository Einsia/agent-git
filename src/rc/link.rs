//! Requests admitted by the local and authenticated peer endpoints.

pub enum LinkEvent {
    Frame {
        epoch: u64,
        frame: Box<crate::protocol::Frame>,
    },
}
