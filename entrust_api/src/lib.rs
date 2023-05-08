//! Entrust specific types dealing with initialization and startup of the hsmcore implementation.

#![no_std]

extern crate alloc;

use alloc::{string::String, vec::Vec};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum EntrustRequest {
    Initialize(InitializeRequest),
    Start(StartRequest),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum EntrustResponse {
    Initialize(InitializeResponse),
    Start(StartResponse),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InitializeRequest {
    pub noise_private_key: Ticket,
    pub noise_public_key: Ticket,
    pub hmac_key: Ticket,
}

/// A Ticket for gaining accessing to a key, as generated by Cmd_GetTicket.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Ticket(pub Vec<u8>);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum InitializeResponse {
    Ok,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StartRequest {
    pub tree_overlay_size: u16,
    pub max_sessions: u16,
}

impl Default for StartRequest {
    fn default() -> Self {
        Self {
            tree_overlay_size: 511,
            max_sessions: 511,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum StartResponse {
    Ok,
    PersistenceError(String),
    WorldSigner(WorldSignerError),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum WorldSignerError {
    FailedToLoad {
        status: u32, // aka M_Status
    },
    /// The SEE Machine failed to find a world signer. Ensure that both the
    /// SEEMachine binary and the userdata file are signed with a `seeinteg`
    /// key.
    NoWorldSigner,
    /// The SEE Machine 2 or more world signer, there should only be 1. Ensure
    /// that both the SEEMachine binary and the userdata file are signed with
    /// the same `seeinteg` key.
    TooManyWorldSigners,
}

impl From<WorldSignerError> for StartResponse {
    fn from(value: WorldSignerError) -> Self {
        StartResponse::WorldSigner(value)
    }
}
