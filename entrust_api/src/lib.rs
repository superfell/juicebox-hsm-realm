//! Entrust specific types dealing with initialization and startup of the hsmcore implementation.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::ops::{Add, AddAssign};
use serde::{Deserialize, Serialize};

/// A Ticket for gaining accessing to a key, as generated by Cmd_GetTicket.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Ticket(pub Vec<u8>);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NvRamState {
    LastWritten,
    Reinitialize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StartRequest {
    pub tree_overlay_size: u16,
    pub max_sessions: u16,
    pub comm_private_key: Ticket,
    pub comm_public_key: Ticket,
    pub mac_key: Ticket,
    pub record_key: Ticket,
    pub nvram: NvRamState,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum StartResponse {
    Ok,
    WorldSigner(WorldSignerError),
    InvalidTicket(KeyRole, u32), //M_Status
    InvalidKeyLength {
        role: KeyRole,
        expecting: usize,
        actual: usize,
    },
    PersistenceError(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum KeyRole {
    CommunicationPrivateKey,
    CommunicationPublicKey,
    MacKey,
    RecordKey,
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
    /// The SEE Machine found 2 or more world signers, there should only be 1.
    /// Ensure that both the SEEMachine binary and the userdata file are signed
    /// with the same `seeinteg` key.
    TooManyWorldSigners,
}

impl From<WorldSignerError> for StartResponse {
    fn from(value: WorldSignerError) -> Self {
        StartResponse::WorldSigner(value)
    }
}

// SEEJob marshalling.
//
// Responses from the HSM larger than ~8k work but can be incredibly slow,
// anywhere up to 2 seconds! To deal with this we chunk large responses
// ourselves. Oddly this doesn't appear to impact messages going to the HSM,
// only the response.
//
// We add an 8 byte trailer to the SEEJob request & responses that we use to
// manage this. The 8 byte trailer consists of
// * type (1 byte),
// * unused (1 byte),
// * number of chunks (2 bytes),
// * chunk number (4 bytes)
//
// Numbers are big endian encoded.
//
// Number of chunks & chunk number are only used for some types.
//
// Entrust support say this is fixed in firmware v13.3 and is Defect NSE-35968.
// We have not tested that yet. See support ticket #163368

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SEEJobRequestType {
    // Execute the SEEJob contained in the body.
    ExecuteSEEJob,

    // Return this chunk of a prior SEEJobs paged results.
    ReadResponseChunk(ChunkNumber),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SEEJobResponseType {
    // A complete SEEJob result from the executed SEEJob,
    SEEJobSingleResult,

    // A SEEJob result that is split into chunks. This response includes the
    // first chunk, there are `ChunkCount` more chunks to fetch starting at
    // ChunkNumber.
    SEEJobPagedResult(ChunkCount, ChunkNumber),

    // A Chunk of a previous SEEJob result, a response to a ReadResponseChunk request.
    ResultChunk(ChunkNumber),
}

/// Chunk numbers are assigned globally. Large results are split into
/// consecutively numbered chunks, modulo 2^32.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ChunkNumber(pub u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChunkCount(pub u16);

impl Add<u16> for ChunkNumber {
    type Output = ChunkNumber;

    fn add(self, rhs: u16) -> Self::Output {
        // ChunkNumber can wrap, that's okay
        ChunkNumber(self.0.wrapping_add(rhs as u32))
    }
}

impl AddAssign<u16> for ChunkNumber {
    fn add_assign(&mut self, rhs: u16) {
        // ChunkNumber can wrap, that's okay
        self.0 = self.0.wrapping_add(rhs as u32);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Trailer {
    pub type_: u8,
    pub count: ChunkCount,
    pub chunk: ChunkNumber,
}

impl Trailer {
    /// Size of the trailer in bytes.
    pub const LEN: usize = 8;

    pub fn serialize(&self) -> [u8; Self::LEN] {
        let mut r = [0; Self::LEN];
        r[0] = self.type_;
        r[2..4].copy_from_slice(&self.count.0.to_be_bytes());
        r[4..8].copy_from_slice(&self.chunk.0.to_be_bytes());
        r
    }

    pub fn deserialize(b: &[u8]) -> Result<Trailer, TrailerError> {
        if b.len() < Self::LEN {
            return Err(TrailerError::TooSmall);
        }
        Ok(Trailer {
            type_: b[0],
            count: ChunkCount(u16::from_be_bytes(b[2..4].try_into().unwrap())),
            chunk: ChunkNumber(u32::from_be_bytes(b[4..8].try_into().unwrap())),
        })
    }
}

impl TryFrom<Trailer> for SEEJobRequestType {
    type Error = TrailerError;

    fn try_from(t: Trailer) -> Result<SEEJobRequestType, TrailerError> {
        match t.type_ {
            1 => Ok(SEEJobRequestType::ExecuteSEEJob),
            2 => Ok(SEEJobRequestType::ReadResponseChunk(t.chunk)),
            t => Err(TrailerError::InvalidType(t)),
        }
    }
}

impl SEEJobRequestType {
    pub fn parse(bytes: &[u8]) -> Result<Self, TrailerError> {
        let t = Trailer::deserialize(bytes)?;
        Self::try_from(t)
    }

    pub fn as_trailer(self) -> Trailer {
        match self {
            SEEJobRequestType::ExecuteSEEJob => Trailer {
                type_: 1,
                count: ChunkCount(0),
                chunk: ChunkNumber(0),
            },
            SEEJobRequestType::ReadResponseChunk(chunk_num) => Trailer {
                type_: 2,
                count: ChunkCount(0),
                chunk: chunk_num,
            },
        }
    }
}

impl TryFrom<Trailer> for SEEJobResponseType {
    type Error = TrailerError;

    fn try_from(t: Trailer) -> Result<SEEJobResponseType, TrailerError> {
        match t.type_ {
            1 => Ok(SEEJobResponseType::SEEJobSingleResult),
            2 => Ok(SEEJobResponseType::SEEJobPagedResult(t.count, t.chunk)),
            3 => Ok(SEEJobResponseType::ResultChunk(t.chunk)),
            t => Err(TrailerError::InvalidType(t)),
        }
    }
}

impl SEEJobResponseType {
    pub fn parse(bytes: &[u8]) -> Result<Self, TrailerError> {
        let t = Trailer::deserialize(bytes)?;
        Self::try_from(t)
    }

    pub fn as_trailer(self) -> Trailer {
        match self {
            SEEJobResponseType::SEEJobSingleResult => Trailer {
                type_: 1,
                count: ChunkCount(0),
                chunk: ChunkNumber(0),
            },
            SEEJobResponseType::SEEJobPagedResult(count, starting) => Trailer {
                type_: 2,
                count,
                chunk: starting,
            },
            SEEJobResponseType::ResultChunk(chunk_num) => Trailer {
                type_: 3,
                count: ChunkCount(0),
                chunk: chunk_num,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrailerError {
    InvalidType(u8),
    TooSmall,
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn trailer_too_small() {
        assert_eq!(
            Err(TrailerError::TooSmall),
            Trailer::deserialize(&[1, 2, 3, 4, 5, 6, 7])
        );
    }

    #[test]
    fn trailer_roundtrip() {
        let start = Trailer {
            type_: 44,
            count: ChunkCount(13),
            chunk: ChunkNumber(u32::MAX),
        };
        let rt = Trailer::deserialize(&start.serialize()).unwrap();
        assert_eq!(start, rt);
    }

    #[test]
    fn seejob_req_type_roundtrip() {
        let start = SEEJobRequestType::ExecuteSEEJob;
        let rt = SEEJobRequestType::parse(&start.as_trailer().serialize()).unwrap();
        assert_eq!(start, rt);

        let start = SEEJobRequestType::ReadResponseChunk(ChunkNumber(u32::MAX - 42));
        let rt = SEEJobRequestType::parse(&start.as_trailer().serialize()).unwrap();
        assert_eq!(start, rt);
    }

    #[test]
    fn seejob_req_type_parse_error() {
        assert_eq!(
            Err(TrailerError::InvalidType(42)),
            SEEJobRequestType::try_from(
                Trailer::deserialize(&[42, 43, 44, 45, 46, 47, 48, 49]).unwrap()
            )
        );
    }

    #[test]
    fn seejob_res_type_roundtrip() {
        let start = SEEJobResponseType::SEEJobSingleResult;
        let rt = SEEJobResponseType::parse(&start.as_trailer().serialize()).unwrap();
        assert_eq!(start, rt);

        let start = SEEJobResponseType::SEEJobPagedResult(ChunkCount(100), ChunkNumber(12343));
        let rt = SEEJobResponseType::parse(&start.as_trailer().serialize()).unwrap();
        assert_eq!(start, rt);

        let start = SEEJobResponseType::ResultChunk(ChunkNumber(432));
        let rt = SEEJobResponseType::parse(&start.as_trailer().serialize()).unwrap();
        assert_eq!(start, rt);
    }

    #[test]
    fn seejob_res_type_parse_error() {
        assert_eq!(
            Err(TrailerError::InvalidType(255)),
            SEEJobResponseType::try_from(
                Trailer::deserialize(&[255, 0, 0, 0, 0, 0, 0, 0]).unwrap()
            )
        );
    }

    #[test]
    fn chunknumber_rollover() {
        let n = ChunkNumber(u32::MAX - 1);
        assert_eq!(u32::MAX, (n + 1).0);
        assert_eq!(0, (n + 2).0);
        assert_eq!(1, (n + 3).0);
    }
}
