//! The sans-I/O translation core of airmate.
//!
//! This crate performs no I/O: no sockets, no clock, no randomness and no filesystem. Everything
//! here is a pure function or a state machine driven by the caller.

#![forbid(unsafe_code)]

pub mod cache;
pub mod error;
pub mod estimate;
pub mod facts;
pub mod flavor;
pub mod inbound;
pub mod ir;
pub mod outbound;
pub mod policy;
pub mod signature;
pub mod sse;
pub mod stream;
pub mod toolname;
