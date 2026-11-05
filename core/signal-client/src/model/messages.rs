use libsignal_service::{prelude::Content, protocol::ServiceId};

#[derive(Debug)]
pub enum Received {
    /// when the receive loop is empty, happens when opening the websocket for the first time
    /// once you're done synchronizing all pending messages for this registered client.
    QueueEmpty,

    /// Got contacts (only applies if linked to a primary device
    /// Contacts can be later queried in the store.
    /// The contacts the primary device sent were read; `whole` when all of them were saved.
    Contacts { whole: bool },

    /// A message from this person's device, sent at `timestamp`, could not be decrypted. A new
    /// session with them may mend it, after which they can be asked for the message again.
    DecryptionError { sender: ServiceId, device: libsignal_service::protocol::DeviceId, timestamp: u64 },

    /// The sender could not decrypt the message this account sent at `timestamp`, and asks for it
    /// again.
    RetryRequest { sender: ServiceId, timestamp: u64 },

    /// Incoming decrypted message with metadata and content
    Content(Box<Content>),
}
