//! Shared native Telegram persistence and Bot API client for the Desktop and profile gateway.
pub mod store;
pub mod telegram;
pub use store::{ChatMessage, DeliveryOutcome, DeliveryResolution, Inbox, Settings, Store, TurnOutcome, Work, WorkStage};
pub use telegram::{
    Client, Error, ErrorKind, Identity, Incoming, Method, PollBatch, Receipt, RequestFuture, Token,
    Transport, parse_allowlist,
};
