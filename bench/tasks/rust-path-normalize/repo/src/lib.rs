//! Lexical normalization of slash-separated paths.

pub mod components;
pub mod normalize;

pub use normalize::normalize;
