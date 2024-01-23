use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

use agent_api::{
    CancelPreparedTransferRequest, CancelPreparedTransferResponse, CompleteTransferRequest,
    CompleteTransferResponse, PrepareTransferRequest, PrepareTransferResponse, TransferInRequest,
    TransferInResponse, TransferOutRequest, TransferOutResponse,
};
pub use cluster_api::{TransferError, TransferRequest};
use juicebox_networking::reqwest::ClientOptions;
use juicebox_networking::rpc::{self};
use observability::metrics;
use service_core::http::ReqwestClientMetrics;
use store::StoreClient;

use super::leader::find_leaders;

pub async fn transfer(
    store: &StoreClient,
    metrics: metrics::Client,
    transfer: TransferRequest,
) -> Result<(), TransferError> {
    type Error = TransferError;

    info!(
        realm=?transfer.realm,
        source=?transfer.source,
        destination=?transfer.destination,
        range=?transfer.range,
        "transferring ownership"
    );

    if transfer.source == transfer.destination {
        warn!(
            group=?transfer.source,
            "cannot transfer ownership to the same group (unsupported)"
        );
        return Err(Error::InvalidGroup);
    }

    let agent_client = ReqwestClientMetrics::new(metrics, ClientOptions::default());
    // This will attempt to Cancel the prepared transfer at the destination group when dropped
    // unless cancelable gets set to false.
    let mut prepare_guard = CancelPrepareGuard {
        transfer: &transfer,
        store,
        agents: &agent_client,
        cancelable: true,
    };

    let mut state = TransferState::Transferring;
    let mut last_error: Option<Error> = None;

    let mut tries = 0;
    loop {
        tries += 1;
        if tries > 20 {
            return Err(last_error.unwrap_or(Error::TooManyRetries));
        } else if tries > 1 {
            sleep(Duration::from_millis(25)).await;
            warn!(?state, ?last_error, "retrying transfer due to error");
        }

        let leaders = find_leaders(store, &agent_client).await.unwrap_or_default();

        let Some((_, source_leader)) = leaders.get(&(transfer.realm, transfer.source)) else {
            last_error = Some(Error::NoSourceLeader);
            continue;
        };

        let Some((_, dest_leader)) = leaders.get(&(transfer.realm, transfer.destination)) else {
            last_error = Some(Error::NoDestinationLeader);
            continue;
        };

        // Once the source group commits the log entry that the range is
        // transferring out, the range must then move to the destination group.
        // Having the destination have to prepare first and subsequently reject any
        // other transfers ensures that when the process gets around to transfer_in,
        // it'll succeed. This is an issue with each group owning 0 or 1 ranges: the
        // only group that can accept a range is one that owns no range or one that
        // owns an adjacent range.

        // The Agents will not respond to these RPCs until the related log entry is
        // committed. (where protocol safety requires the log entry to commit).
        if state == TransferState::Transferring {
            let (nonce, prepared_stmt) = match rpc::send(
                &agent_client,
                dest_leader,
                PrepareTransferRequest {
                    realm: transfer.realm,
                    source: transfer.source,
                    destination: transfer.destination,
                    range: transfer.range.clone(),
                },
            )
            .await
            {
                Ok(PrepareTransferResponse::Ok { nonce, statement }) => (nonce, statement),
                Ok(PrepareTransferResponse::InvalidRealm) => {
                    // In theory you should never be able to get here, as the checks
                    // to find the leaders wouldn't find any leaders for an unknown
                    // realm/group
                    unreachable!(
                        "PrepareTransfer to group:{:?} in realm:{:?} failed with InvalidRealm",
                        transfer.destination, transfer.realm,
                    );
                }
                Ok(PrepareTransferResponse::InvalidGroup) => return Err(Error::InvalidGroup),
                Ok(PrepareTransferResponse::OtherTransferPending) => {
                    return Err(Error::OtherTransferPending)
                }
                Ok(PrepareTransferResponse::UnacceptableRange) => {
                    return Err(Error::UnacceptableRange)
                }
                Ok(PrepareTransferResponse::CommitTimeout) => return Err(Error::CommitTimeout),
                Ok(PrepareTransferResponse::NoStore) => return Err(Error::NoStore),
                Ok(PrepareTransferResponse::NoHsm) => {
                    last_error = Some(Error::NoDestinationLeader);
                    continue;
                }
                Ok(PrepareTransferResponse::NotLeader) => {
                    last_error = Some(Error::NoDestinationLeader);
                    continue;
                }
                Err(error) => {
                    warn!(%error, "RPC error with destination leader during PrepareTransfer");
                    last_error = Some(Error::RpcError(error));
                    continue;
                }
            };

            let (transferring_partition, transfer_stmt) = match rpc::send(
                &agent_client,
                source_leader,
                TransferOutRequest {
                    realm: transfer.realm,
                    source: transfer.source,
                    destination: transfer.destination,
                    range: transfer.range.clone(),
                    nonce,
                    statement: prepared_stmt.clone(),
                },
            )
            .await
            {
                Ok(TransferOutResponse::Ok {
                    transferring,
                    statement,
                }) => {
                    prepare_guard.cancelable = false;
                    (transferring, statement)
                }
                Ok(TransferOutResponse::UnacceptableRange) => return Err(Error::UnacceptableRange),
                Ok(TransferOutResponse::InvalidGroup) => {
                    return Err(Error::InvalidGroup);
                }
                Ok(TransferOutResponse::OtherTransferPending) => {
                    return Err(Error::OtherTransferPending);
                }
                Ok(TransferOutResponse::CommitTimeout) => {
                    // This might still commit, so we shouldn't cancel the prepare.
                    prepare_guard.cancelable = false;
                    return Err(Error::CommitTimeout);
                }
                Ok(TransferOutResponse::NoStore) => return Err(Error::NoStore),
                Ok(
                    TransferOutResponse::NotOwner
                    | TransferOutResponse::NoHsm
                    | TransferOutResponse::NotLeader,
                ) => {
                    last_error = Some(Error::NoSourceLeader);
                    continue;
                }
                Ok(TransferOutResponse::InvalidProof) => {
                    panic!("TransferOut reported invalid proof");
                }
                Ok(TransferOutResponse::InvalidRealm) => {
                    unreachable!(
                        "TransferOut reported invalid realm for realm:{:?}",
                        transfer.realm
                    )
                }
                Ok(TransferOutResponse::InvalidStatement) => {
                    panic!(
                    "the destination group leader provided an invalid prepared transfer statement"
                );
                }
                Err(error) => {
                    warn!(%error, "RPC error with source leader during TransferOut");
                    last_error = Some(Error::RpcError(error));
                    continue;
                }
            };

            match rpc::send(
                &agent_client,
                dest_leader,
                TransferInRequest {
                    realm: transfer.realm,
                    source: transfer.source,
                    destination: transfer.destination,
                    transferring: transferring_partition.clone(),
                    nonce,
                    statement: transfer_stmt.clone(),
                },
            )
            .await
            {
                Ok(TransferInResponse::Ok) => {
                    state = TransferState::Completing;
                }
                Ok(TransferInResponse::CommitTimeout) => return Err(Error::CommitTimeout),
                Ok(TransferInResponse::NotPrepared) => {
                    unreachable!(
                        "TransferIn reported Not Prepared, but we just called prepareTransfer"
                    );
                }
                Ok(TransferInResponse::InvalidStatement) => {
                    panic!("TransferIn reported an invalid transfer statement");
                }
                Ok(r @ TransferInResponse::InvalidGroup | r @ TransferInResponse::InvalidRealm) => {
                    unreachable!("Only a buggy coordinator can get these errors by this point in the process. Got {r:?}");
                }
                Ok(TransferInResponse::InvalidNonce) => {
                    last_error = Some(Error::InvalidNonce);
                    continue;
                }
                Ok(TransferInResponse::NoStore) => {
                    last_error = Some(Error::NoStore);
                    continue;
                }
                Ok(
                    TransferInResponse::NoHsm
                    | TransferInResponse::NotLeader
                    | TransferInResponse::NotOwner,
                ) => {
                    last_error = Some(Error::NoDestinationLeader);
                    continue;
                }
                Err(error) => {
                    warn!(%error, "RPC Error reported while calling TransferIn");
                    last_error = Some(Error::RpcError(error));
                    continue;
                }
            };
        }

        if state == TransferState::Completing {
            // the TransferIn agent RPC waits for the log entry to commit, so
            // its safe to call CompleteTransfer now.
            match rpc::send(
                &agent_client,
                source_leader,
                CompleteTransferRequest {
                    realm: transfer.realm,
                    source: transfer.source,
                    destination: transfer.destination,
                    range: transfer.range.clone(),
                },
            )
            .await
            {
                Ok(CompleteTransferResponse::Ok) => return Ok(()),
                Ok(CompleteTransferResponse::CommitTimeout) => return Err(Error::CommitTimeout),
                Ok(CompleteTransferResponse::NoHsm | CompleteTransferResponse::NotLeader) => {
                    last_error = Some(Error::NoSourceLeader);
                    continue;
                }
                Ok(
                    CompleteTransferResponse::InvalidRealm | CompleteTransferResponse::InvalidGroup,
                ) => {
                    unreachable!();
                }
                Ok(CompleteTransferResponse::NotTransferring) => {
                    warn!("got NotTransferring during complete transfer");
                    // This could happen if retried for a transient error but the request
                    // had actually succeeded (e.g. commit timeout)
                    return Ok(());
                }
                Err(error) => {
                    warn!(%error, "RPC error during CompleteTransfer request");
                    last_error = Some(Error::RpcError(error));
                    continue;
                }
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum TransferState {
    Transferring,
    Completing,
}

struct CancelPrepareGuard<'a> {
    transfer: &'a TransferRequest,
    store: &'a StoreClient,
    agents: &'a ReqwestClientMetrics,
    cancelable: bool,
}

impl<'a> Drop for CancelPrepareGuard<'a> {
    fn drop(&mut self) {
        if self.cancelable {
            tokio::runtime::Handle::current().block_on(cancel_prepared_transfer(
                self.agents,
                self.store,
                self.transfer,
            ));
        }
    }
}

async fn cancel_prepared_transfer(
    client: &ReqwestClientMetrics,
    store: &StoreClient,
    t: &TransferRequest,
) {
    let leaders = find_leaders(store, client).await.unwrap_or_default();

    let Some((_, dest_leader)) = leaders.get(&(t.realm, t.destination)) else {
        warn!(group=?t.destination, "couldn't find a leader for the group");
        return;
    };

    match rpc::send(
        client,
        dest_leader,
        CancelPreparedTransferRequest {
            realm: t.realm,
            source: t.source,
            destination: t.destination,
            range: t.range.clone(),
        },
    )
    .await
    {
        Ok(CancelPreparedTransferResponse::Ok) => {
            info!(destination=?t.destination, "canceled previously prepared transfer");
        }
        Ok(other) => {
            warn!(?other, "CancelPreparedTransfer failed");
        }
        Err(err) => {
            warn!(%err, "RPC error while trying to cancel prepared transfer");
        }
    }
}
