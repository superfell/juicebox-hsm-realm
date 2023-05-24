use futures::future::join_all;
use futures::FutureExt;
use std::collections::HashMap;
use std::iter::zip;
use tracing::{info, warn};
use url::Url;

use super::super::agent::types as agent_types;
use super::super::rpc::HandlerError;
use super::{types, ManagementGrant, Manager};
use hsm_types::{Configuration, GroupId, HsmId, LogIndex};
use hsmcore::hsm::types as hsm_types;
use juicebox_sdk_core::types::RealmId;
use juicebox_sdk_networking::rpc::{self, RpcError};

impl Manager {
    pub(super) async fn handle_leader_stepdown(
        &self,
        req: types::StepDownRequest,
    ) -> Result<types::StepDownResponse, HandlerError> {
        type Response = types::StepDownResponse;

        let addresses: HashMap<HsmId, Url> = match self.0.store.get_addresses().await {
            Ok(a) => a.into_iter().collect(),
            Err(_err) => return Ok(Response::NoStore),
        };

        // calculate the exact set of step downs needed.
        let stepdowns = match self.resolve_stepdowns(&req, &addresses).await {
            Err(e) => return Ok(e),
            Ok(sd) => sd,
        };
        let mut grants = Vec::with_capacity(stepdowns.len());
        for stepdown in &stepdowns {
            match self.mark_as_busy(stepdown.realm, stepdown.group) {
                None => {
                    return Ok(Response::Busy {
                        realm: stepdown.realm,
                        group: stepdown.group,
                    })
                }
                Some(grant) => grants.push(grant),
            }
        }

        for (stepdown, grant) in zip(stepdowns, grants) {
            info!(url=%stepdown.url, hsm=?stepdown.hsm, group=?stepdown.group, realm=?stepdown.realm, "Asking Agent/HSM to step down as leader");
            match rpc::send(
                &self.0.agents,
                &stepdown.url,
                agent_types::StepDownRequest {
                    realm: stepdown.realm,
                    group: stepdown.group,
                },
            )
            .await
            {
                Err(err) => return Ok(Response::RpcError(err)),
                Ok(agent_types::StepDownResponse::NoHsm) => return Ok(Response::NoHsm),
                Ok(agent_types::StepDownResponse::InvalidGroup) => {
                    return Ok(Response::InvalidGroup)
                }
                Ok(agent_types::StepDownResponse::InvalidRealm) => {
                    return Ok(Response::InvalidRealm)
                }
                Ok(agent_types::StepDownResponse::NotLeader) => return Ok(Response::NotLeader),
                Ok(agent_types::StepDownResponse::Ok { last }) => {
                    if let Err(err) = self
                        .assign_leader_post_stepdown(&addresses, &grant, stepdown, Some(last))
                        .await
                    {
                        return Ok(Response::RpcError(err));
                    }
                }
            }
        }
        Ok(Response::Ok)
    }

    /// Leader stepdown was completed, assign a new one.
    async fn assign_leader_post_stepdown(
        &self,
        addresses: &HashMap<HsmId, Url>,
        grant: &ManagementGrant<'_>,
        stepdown: Stepdown,
        last: Option<LogIndex>,
    ) -> Result<Option<HsmId>, RpcError> {
        let hsm_status = super::get_hsm_statuses(
            &self.0.agents,
            stepdown
                .config
                .0
                .iter()
                .filter_map(|hsm| addresses.get(hsm)),
        )
        .await;

        super::leader::assign_group_a_leader(
            &self.0.agents,
            grant,
            stepdown.config,
            Some(stepdown.hsm),
            &hsm_status,
            last,
        )
        .await
    }

    async fn resolve_stepdowns(
        &self,
        req: &types::StepDownRequest,
        addresses: &HashMap<HsmId, Url>,
    ) -> Result<Vec<Stepdown>, types::StepDownResponse> {
        match req {
            types::StepDownRequest::Hsm(hsm) => match addresses.get(hsm) {
                None => {
                    warn!(?hsm, "failed to find hsm in service discovery");
                    Err(types::StepDownResponse::InvalidHsm)
                }
                Some(url) => {
                    match rpc::send(&self.0.agents, url, agent_types::StatusRequest {}).await {
                        Err(err) => {
                            warn!(?err, url=%url, hsm=?hsm, "failed to get status of HSM");
                            Err(types::StepDownResponse::RpcError(err))
                        }
                        Ok(agent_types::StatusResponse {
                            hsm:
                                Some(hsm_types::StatusResponse {
                                    id,
                                    realm: Some(rs),
                                    ..
                                }),
                            ..
                        }) if id == *hsm => Ok(rs
                            .groups
                            .into_iter()
                            .filter_map(|gs| {
                                gs.leader.map(|_| Stepdown {
                                    hsm: *hsm,
                                    url: url.clone(),
                                    group: gs.id,
                                    realm: rs.id,
                                    config: gs.configuration,
                                })
                            })
                            .collect()),
                        Ok(_s) => {
                            info!(?hsm,url=%url, "hsm is not a member of a realm");
                            Ok(Vec::new())
                        }
                    }
                }
            },
            types::StepDownRequest::Group { realm, group } => {
                Ok(join_all(addresses.iter().map(|(_hsm, url)| {
                    rpc::send(&self.0.agents, url, agent_types::StatusRequest {})
                        .map(|r| (r, url.clone()))
                }))
                .await
                .into_iter()
                .filter_map(|(s, url)| {
                    if let Some((hsm_id, realm_status)) = s
                        .ok()
                        .and_then(|s| s.hsm)
                        .and_then(|hsm| hsm.realm.map(|r| (hsm.id, r)))
                    {
                        if realm_status.id == *realm {
                            for g in realm_status.groups {
                                if g.id == *group && g.leader.is_some() {
                                    return Some(Stepdown {
                                        hsm: hsm_id,
                                        url,
                                        group: *group,
                                        realm: *realm,
                                        config: g.configuration,
                                    });
                                }
                            }
                        }
                    }
                    None
                })
                .collect())
            }
        }
    }
}

struct Stepdown {
    hsm: HsmId,
    url: Url,
    group: GroupId,
    realm: RealmId,
    config: Configuration,
}
