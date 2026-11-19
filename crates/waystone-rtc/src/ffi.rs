//! C ABI used by the iOS controller once the transport is linked as a static
//! library. The layer is intentionally thin: it converts C strings to
//! `waystone_pb` types, forwards to [`crate::Engine`] and serializes events as
//! JSON so Swift can decode them with `Codable`.

pub use crate::engine::Event;
