use actix::prelude::*;
use hmac::Hmac;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fmt;

use super::super::super::types::{
    AuthToken, DeleteRequest, DeleteResponse, Recover1Request, Recover1Response, Recover2Request,
    Recover2Response, Register1Request, Register1Response, Register2Request, Register2Response,
};
use super::super::merkle::{agent::StoreDelta, HashOutput, ReadProof};

#[derive(Copy, Clone, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct RealmId(pub [u8; 16]);

impl fmt::Debug for RealmId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Eq, Hash, PartialEq)]
pub struct GroupId(pub [u8; 16]);

impl fmt::Debug for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HsmId(pub [u8; 16]);

impl fmt::Debug for HsmId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct RecordId(pub [u8; 32]);
impl RecordId {
    pub fn num_bits() -> usize {
        256 // TODO: There's probably some generics gymnastics that could be done here.
    }
    pub fn min_id() -> Self {
        RecordId([0; 32])
    }
    pub fn max_id() -> Self {
        RecordId([255; 32])
    }
    pub fn next(&self) -> Option<RecordId> {
        let mut r = RecordId(self.0);
        for i in (0..r.0.len()).rev() {
            if r.0[i] < 255 {
                r.0[i] += 1;
                return Some(r);
            } else {
                r.0[i] = 0;
            }
        }
        None
    }
    pub fn prev(&self) -> Option<RecordId> {
        let mut r = RecordId(self.0);
        for i in (0..r.0.len()).rev() {
            if r.0[i] > 0 {
                r.0[i] -= 1;
                return Some(r);
            } else {
                r.0[i] = 255;
            }
        }
        None
    }
}
impl fmt::Debug for RecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x")?;
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LogIndex(pub u64);

impl LogIndex {
    pub fn next(&self) -> Self {
        Self(self.0.checked_add(1).unwrap())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partition {
    pub range: OwnedRange,
    pub root_hash: DataHash,
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub index: LogIndex,
    pub partition: Option<Partition>,
    pub transferring_out: Option<TransferringOut>,
    pub prev_hmac: EntryHmac,
    pub entry_hmac: EntryHmac,
    // TODO:
    // pub committed: LogIndex,
    // pub committed_statement: CommittedStatement,
}

#[derive(Clone, Debug)]
pub struct TransferringOut {
    pub destination: GroupId,
    pub partition: Partition,
    /// This is the first log index when this struct was placed in the source
    /// group's log. It's used by the source group to determine whether
    /// transferring out has committed.
    pub at: LogIndex,
}

/// See [super::EntryHmacBuilder].
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct EntryHmac(pub digest::Output<Hmac<Sha256>>);

impl EntryHmac {
    pub fn zero() -> Self {
        Self(digest::Output::<Hmac<Sha256>>::default())
    }
}

impl fmt::Debug for EntryHmac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct OwnedRange {
    pub start: RecordId, // inclusive
    pub end: RecordId,   // inclusive
}
impl fmt::Debug for OwnedRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{:?}-{:?}]", &self.start, &self.end)
    }
}
impl OwnedRange {
    pub fn full() -> OwnedRange {
        OwnedRange {
            start: RecordId::min_id(),
            end: RecordId::max_id(),
        }
    }
    pub fn contains(&self, rid: &RecordId) -> bool {
        rid >= &self.start && rid <= &self.end
    }
    pub fn contains_range(&self, rng: &OwnedRange) -> bool {
        self.contains(&rng.start) && self.contains(&rng.end)
    }
    pub fn join(&self, other: &OwnedRange) -> Option<Self> {
        match self.end.next() {
            Some(r) if r == other.start => Some(OwnedRange {
                start: self.start.clone(),
                end: other.end.clone(),
            }),
            None | Some(_) => match other.end.next() {
                Some(r) if r == self.start => Some(OwnedRange {
                    start: other.start.clone(),
                    end: self.end.clone(),
                }),
                None | Some(_) => None,
            },
        }
    }
    pub fn split_at(&self, other: &OwnedRange) -> Result<RecordId, ()> {
        assert!(self.contains_range(other));
        if self.start == other.start {
            Ok(other.end.next().unwrap())
        } else if self.end == other.end {
            Ok(other.start.clone())
        } else {
            Err(())
        }
    }
}

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub struct DataHash(pub digest::Output<Sha256>);

impl fmt::Debug for DataHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
impl HashOutput for DataHash {
    fn as_u8(&self) -> &[u8] {
        &self.0
    }
}

/// Set of HSMs forming a group.
///
/// The vector must be sorted by HSM ID, must not contain duplicates, and must
/// contain at least 1 HSM.
#[derive(Clone, Debug)]
pub struct Configuration(pub Vec<HsmId>);

/// See [super::GroupConfigurationStatementBuilder].
#[derive(Clone)]
pub struct GroupConfigurationStatement(pub digest::Output<Hmac<Sha256>>);

impl fmt::Debug for GroupConfigurationStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// See [super::CapturedStatementBuilder].
#[derive(Clone)]
pub struct CapturedStatement(pub digest::Output<Hmac<Sha256>>);

impl fmt::Debug for CapturedStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TransferNonce(pub [u8; 16]);

impl fmt::Debug for TransferNonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// See [super::TransferStatementBuilder].
#[derive(Clone)]
pub struct TransferStatement(pub digest::Output<Hmac<Sha256>>);

impl fmt::Debug for TransferStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Message)]
#[rtype(result = "StatusResponse")]
pub struct StatusRequest {}

#[derive(Debug, MessageResponse)]
pub struct StatusResponse {
    pub id: HsmId,
    pub realm: Option<RealmStatus>,
}

#[derive(Debug)]
pub struct RealmStatus {
    pub id: RealmId,
    pub groups: Vec<GroupStatus>,
}

#[derive(Debug)]
pub struct GroupStatus {
    pub id: GroupId,
    pub configuration: Configuration,
    pub captured: Option<(LogIndex, EntryHmac)>,
    pub leader: Option<LeaderStatus>,
}

#[derive(Debug)]
pub struct LeaderStatus {
    pub committed: Option<LogIndex>,
    // Note: this might not be committed yet.
    pub owned_range: Option<OwnedRange>,
}

#[derive(Debug, Message)]
#[rtype(result = "NewRealmResponse")]
pub struct NewRealmRequest {
    pub configuration: Configuration,
}

#[derive(Debug, MessageResponse)]
#[allow(clippy::large_enum_variant)]
pub enum NewRealmResponse {
    Ok(NewGroupInfo),
    HaveRealm,
    InvalidConfiguration,
}

#[derive(Debug)]
pub struct NewGroupInfo {
    pub realm: RealmId,
    pub group: GroupId,
    pub statement: GroupConfigurationStatement,
    pub entry: LogEntry,
    pub delta: Option<StoreDelta<DataHash>>,
}

#[derive(Debug, Message)]
#[rtype(result = "JoinRealmResponse")]
pub struct JoinRealmRequest {
    pub realm: RealmId,
}

#[derive(Debug, MessageResponse)]
pub enum JoinRealmResponse {
    Ok { hsm: HsmId },
    HaveOtherRealm,
}

#[derive(Debug, Message)]
#[rtype(result = "NewGroupResponse")]
pub struct NewGroupRequest {
    pub realm: RealmId,
    pub configuration: Configuration,
}

#[derive(Debug, MessageResponse)]
#[allow(clippy::large_enum_variant)]
pub enum NewGroupResponse {
    Ok(NewGroupInfo),
    InvalidRealm,
    InvalidConfiguration,
}

#[derive(Debug, Message)]
#[rtype(result = "JoinGroupResponse")]
pub struct JoinGroupRequest {
    pub realm: RealmId,
    pub group: GroupId,
    pub configuration: Configuration,
    pub statement: GroupConfigurationStatement,
}

#[derive(Debug, MessageResponse)]
pub enum JoinGroupResponse {
    Ok,
    InvalidRealm,
    InvalidConfiguration,
    InvalidStatement,
}

#[derive(Debug, Message)]
#[rtype(result = "CaptureNextResponse")]
pub struct CaptureNextRequest {
    pub realm: RealmId,
    pub group: GroupId,
    pub entry: LogEntry,
}

#[derive(Debug, MessageResponse)]
pub enum CaptureNextResponse {
    Ok {
        hsm_id: HsmId,
        captured: CapturedStatement,
    },
    InvalidRealm,
    InvalidGroup,
    InvalidHmac,
    InvalidChain,
    MissingPrev,
}

#[derive(Debug, Message)]
#[rtype(result = "BecomeLeaderResponse")]
pub struct BecomeLeaderRequest {
    pub realm: RealmId,
    pub group: GroupId,
    pub last_entry: LogEntry,
}

#[derive(Debug, MessageResponse)]
pub enum BecomeLeaderResponse {
    Ok,
    InvalidRealm,
    InvalidGroup,
    InvalidHmac,
    NotCaptured { have: Option<LogIndex> },
}

#[derive(Debug, Message)]
#[rtype(result = "ReadCapturedResponse")]
pub struct ReadCapturedRequest {
    pub realm: RealmId,
    pub group: GroupId,
}

#[derive(Debug, MessageResponse)]
pub enum ReadCapturedResponse {
    Ok {
        hsm_id: HsmId,
        index: LogIndex,
        entry_hmac: EntryHmac,
        statement: CapturedStatement,
    },
    InvalidRealm,
    InvalidGroup,
    None,
}

#[derive(Debug, Message)]
#[rtype(result = "CommitResponse")]
pub struct CommitRequest {
    pub realm: RealmId,
    pub group: GroupId,
    pub index: LogIndex,
    pub entry_hmac: EntryHmac,
    pub captures: Vec<(HsmId, CapturedStatement)>,
}

#[derive(Debug, MessageResponse)]
pub enum CommitResponse {
    Ok {
        committed: Option<LogIndex>,
        responses: Vec<(EntryHmac, SecretsResponse)>,
    },
    AlreadyCommitted {
        committed: LogIndex,
    },
    NoQuorum,
    InvalidRealm,
    InvalidGroup,
    NotLeader,
}

#[derive(Debug, Message)]
#[rtype(result = "TransferOutResponse")]
pub struct TransferOutRequest {
    pub realm: RealmId,
    pub source: GroupId,
    pub destination: GroupId,
    pub range: OwnedRange,
    pub index: LogIndex,
    pub proof: ReadProof<DataHash>,
}

#[derive(Debug, MessageResponse)]
#[allow(clippy::large_enum_variant)]
pub enum TransferOutResponse {
    Ok {
        entry: LogEntry,
        delta: Option<StoreDelta<DataHash>>,
    },
    InvalidRealm,
    InvalidGroup,
    NotLeader,
    /// This is also returned when asking for a split that's more than one more
    /// bit beyond the currently owned prefix.
    NotOwner,
    StaleIndex,
    StaleProof,
    InvalidProof,
}

#[derive(Debug, Message)]
#[rtype(result = "TransferNonceResponse")]
pub struct TransferNonceRequest {
    pub realm: RealmId,
    pub destination: GroupId,
}

#[derive(Debug, MessageResponse)]
pub enum TransferNonceResponse {
    Ok(TransferNonce),
    InvalidRealm,
    InvalidGroup,
    NotLeader,
}

#[derive(Debug, Message)]
#[rtype(result = "TransferStatementResponse")]
pub struct TransferStatementRequest {
    pub realm: RealmId,
    pub source: GroupId,
    pub destination: GroupId,
    pub nonce: TransferNonce,
}

#[derive(Debug, MessageResponse)]
pub enum TransferStatementResponse {
    Ok(TransferStatement),
    InvalidRealm,
    InvalidGroup,
    NotLeader,
    NotTransferring,
    Busy,
}

#[derive(Debug, Message)]
#[rtype(result = "TransferInResponse")]
pub struct TransferInRequest {
    pub realm: RealmId,
    pub destination: GroupId,
    pub transferring: Partition,
    pub nonce: TransferNonce,
    pub statement: TransferStatement,
}

#[derive(Debug, MessageResponse)]
#[allow(clippy::large_enum_variant)]
pub enum TransferInResponse {
    Ok { entry: LogEntry },
    InvalidRealm,
    InvalidGroup,
    NotLeader,
    UnacceptablePrefix,
    InvalidNonce,
    InvalidStatement,
}

#[derive(Debug, Message)]
#[rtype(result = "CompleteTransferResponse")]
pub struct CompleteTransferRequest {
    pub realm: RealmId,
    pub source: GroupId,
    pub destination: GroupId,
    pub range: OwnedRange,
}

#[derive(Debug, MessageResponse)]
#[allow(clippy::large_enum_variant)]
pub enum CompleteTransferResponse {
    Ok(LogEntry),
    NotTransferring,
    InvalidRealm,
    InvalidGroup,
    NotLeader,
}

#[derive(Debug, Message)]
#[rtype(result = "AppResponse")]
pub struct AppRequest {
    pub realm: RealmId,
    pub group: GroupId,
    pub rid: RecordId,
    pub request: SecretsRequest,
    pub index: LogIndex,
    pub proof: ReadProof<DataHash>,
}

#[derive(Debug, MessageResponse)]
#[allow(clippy::large_enum_variant)]
pub enum AppResponse {
    Ok {
        entry: LogEntry,
        delta: Option<StoreDelta<DataHash>>,
    },
    InvalidRealm,
    InvalidGroup,
    StaleProof,
    InvalidProof,
    NotOwner,
    NotLeader,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum SecretsRequest {
    Register1(Register1Request),
    Register2(Register2Request),
    Recover1(Recover1Request),
    Recover2(Recover2Request),
    Delete(DeleteRequest),
}

impl SecretsRequest {
    pub fn auth_token(&self) -> &AuthToken {
        match self {
            SecretsRequest::Register1(r) => &r.auth_token,
            SecretsRequest::Register2(r) => &r.auth_token,
            SecretsRequest::Recover1(r) => &r.auth_token,
            SecretsRequest::Recover2(r) => &r.auth_token,
            SecretsRequest::Delete(r) => &r.auth_token,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[allow(clippy::large_enum_variant)]
pub enum SecretsResponse {
    Register1(Register1Response),
    Register2(Register2Response),
    Recover1(Recover1Response),
    Recover2(Recover2Response),
    Delete(DeleteResponse),
}
