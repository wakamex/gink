/// Whether to trust or reject new identities
#[derive(Debug, Clone)]
pub enum OnNewIdentity {
    Reject,
    Trust,
    /// Trust a changed identity that a message arrives under, and refuse to send to one until the
    /// store has been told to, with its `approve_identity`: a message from someone shows itself
    /// to be from them or does not, while a message to a key that changed may go to someone else.
    AskToSend,
}
