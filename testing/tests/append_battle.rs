use futures::future::join_all;
use once_cell::sync::Lazy;
use rand_core::{OsRng, RngCore};
use std::path::PathBuf;
use std::time::Duration;

use agent_api::{AgentService, AppResponse};
use hsm_api::RecordId;
use juicebox_sdk::Policy;
use juicebox_sdk_core::requests::{
    ClientRequestKind, DeleteResponse, NoiseRequest, NoiseResponse, Register1Response,
    SecretsRequest, SecretsResponse,
};
use juicebox_sdk_core::types::SessionId;
use juicebox_sdk_marshalling as marshalling;
use juicebox_sdk_networking::reqwest::{self, Client, ClientOptions};
use juicebox_sdk_networking::rpc::{self, RpcError};
use juicebox_sdk_noise::client::Handshake;
use juicebox_sdk_process_group::ProcessGroup;
use testing::exec::bigtable::emulator;
use testing::exec::cluster_gen::{create_cluster, ClusterConfig, RealmConfig, RealmResult};
use testing::exec::hsm_gen::{Entrust, MetricsParticipants};
use testing::exec::PortIssuer;

// rust runs the tests in parallel, so we need each test to get its own port.
static PORT: Lazy<PortIssuer> = Lazy::new(|| PortIssuer::new(8777));

#[tokio::test]
async fn leader_battle() {
    let bt_args = emulator(PORT.next());
    let mut processes = ProcessGroup::new();

    let cluster_args = ClusterConfig {
        load_balancers: 1,
        realms: vec![RealmConfig {
            hsms: 3,
            groups: 1,
            metrics: MetricsParticipants::All,
            state_dir: None,
        }],
        bigtable: bt_args,
        secrets_file: Some(PathBuf::from("../secrets-demo.json")),
        entrust: Entrust(false),
        path_to_target: PathBuf::from(".."),
    };

    let cluster = create_cluster(cluster_args, &mut processes, PORT.clone())
        .await
        .unwrap();

    // sanity check the cluster health.
    cluster
        .client_for_user("presso".into())
        .register(
            &b"1234".to_vec().into(),
            &b"secret".to_vec().into(),
            &b"info".to_vec().into(),
            Policy { num_guesses: 4 },
        )
        .await
        .unwrap();

    let opts = ClientOptions {
        timeout: Duration::from_secs(3),
        ..ClientOptions::default()
    };
    let agent_client: Client<AgentService> = reqwest::Client::new(opts);

    make_all_agents_leader(&agent_client, &cluster.realms[0]).await;

    // Make a request to all the agents. We have to do this directly to the
    // agent as the load balancer will stop iterating the potential agents once
    // one successfully handles the request.
    let (successes, errors) =
        make_app_request_to_agents(&agent_client, &cluster.realms[0], SecretsRequest::Register1)
            .await;
    // Register1 doesn't write to the tree. Because hsmId is part of the log
    // entry each HSM will still generate a unique log entry. Only one of them
    // should get written/committed.
    assert_eq!(1, successes.len());
    assert!(matches!(
        successes[0],
        SecretsResponse::Register1(Register1Response::Ok)
    ));
    assert_eq!(2, errors.len());
    // The HSMs that lost the write battle will never commit there log entry
    // and the request will eventually hit the HTTP timeout.
    for err in errors {
        assert!(
            matches!(err, AgentAppRequestError::Rpc(RpcError::Network)),
            "{err:?}"
        )
    }
    assert_eq!(1, num_leaders(&agent_client, &cluster.realms[0]).await);

    // Should still be able to make a request via the LB fine.
    make_all_agents_leader(&agent_client, &cluster.realms[0]).await;
    cluster
        .client_for_user("presso".into())
        .register(
            &b"1234".to_vec().into(),
            &b"secret".to_vec().into(),
            &b"info".to_vec().into(),
            Policy { num_guesses: 4 },
        )
        .await
        .unwrap();

    // The LB sent the request to the original leader, the other agents didn't see anything
    // so they'll still think they're a leader.
    assert_eq!(3, num_leaders(&agent_client, &cluster.realms[0]).await);

    let (successes, errors) =
        make_app_request_to_agents(&agent_client, &cluster.realms[0], SecretsRequest::Register1)
            .await;
    // All the agents except the one the LB sent the request to don't know
    // about the new log entry that was generated. When they see the next
    // request they'll spot that and stand down.
    assert_eq!(1, successes.len());
    assert!(matches!(
        successes[0],
        SecretsResponse::Register1(Register1Response::Ok)
    ));
    assert_eq!(2, errors.len());
    for err in errors {
        assert!(matches!(
            err,
            AgentAppRequestError::NotOk(AppResponse::NotLeader)
        ));
    }
    assert_eq!(1, num_leaders(&agent_client, &cluster.realms[0]).await);

    // Do the same thing but with Delete this time.
    make_all_agents_leader(&agent_client, &cluster.realms[0]).await;

    let (successes, errors) =
        make_app_request_to_agents(&agent_client, &cluster.realms[0], SecretsRequest::Delete).await;
    // Delete always updates the tree, and so each agent should generate a
    // unique log entry (due to the nonce in the leaf encryption). In this event
    // there should be one success and the rest are errors. The HSM/Agent will
    // stand down on the log precondition check but otherwise does nothing to
    // resolve the outcome of the pending requests. These requests will block
    // waiting for a commit that will never arrive. Eventually the http client
    // times out.
    //
    // These agents that stepped down also end up stuck trying to stepdown.
    // Because their log has diverged, they'll never get to the commit index to
    // the stepdown index and the agent will keep trying to commit and not
    // getting anywhere. This also means that they can't become leader again
    // so this is a bad state to get the system into.
    assert_eq!(1, successes.len());
    assert!(matches!(
        successes[0],
        SecretsResponse::Delete(DeleteResponse::Ok)
    ));
    for error in errors {
        assert!(matches!(
            error,
            AgentAppRequestError::Rpc(RpcError::Network)
        ));
    }

    // Should still be able to make a request via the LB fine.
    cluster
        .client_for_user("presso".into())
        .register(
            &b"1234".to_vec().into(),
            &b"secret".to_vec().into(),
            &b"info".to_vec().into(),
            Policy { num_guesses: 4 },
        )
        .await
        .unwrap();
}

#[derive(Debug)]
enum AgentAppRequestError {
    NotOk(AppResponse),
    Rpc(RpcError),
}

async fn make_app_request_to_agents(
    agent_client: &Client<AgentService>,
    realm: &RealmResult,
    req: SecretsRequest,
) -> (Vec<SecretsResponse>, Vec<AgentAppRequestError>) {
    let group = *realm.groups.last().unwrap();

    let mut pk_bytes = [0u8; 32];
    pk_bytes.copy_from_slice(&realm.communication_public_key.0);
    let pk = x25519_dalek::PublicKey::from(pk_bytes);

    let req = marshalling::to_vec(&req).unwrap();
    let mut successes = Vec::new();
    let mut errors = Vec::new();
    for result in join_all(realm.agents.iter().map(|agent| async {
        let (handshake, req) = Handshake::start(&pk, &req, &mut OsRng).unwrap();
        let mut record_id = RecordId::max_id();
        OsRng.fill_bytes(&mut record_id.0);
        let r = agent_api::AppRequest {
            realm: realm.realm,
            group,
            record_id,
            session_id: SessionId(OsRng.next_u32()),
            kind: ClientRequestKind::SecretsRequest,
            encrypted: NoiseRequest::Handshake { handshake: req },
            tenant: "Bob".into(),
        };
        match rpc::send(agent_client, agent, r).await {
            Ok(AppResponse::Ok(NoiseResponse::Handshake {
                handshake: result, ..
            })) => {
                let app_res = handshake.finish(&result).unwrap();
                let secret_response: SecretsResponse = marshalling::from_slice(&app_res.1).unwrap();
                Ok(secret_response)
            }
            Ok(other_response) => Err(AgentAppRequestError::NotOk(other_response)),
            Err(err) => Err(AgentAppRequestError::Rpc(err)),
        }
    }))
    .await
    {
        match result {
            Ok(res) => successes.push(res),
            Err(err) => errors.push(err),
        }
    }
    (successes, errors)
}

// Asks all the agents in the realm to become leader and verifies that they all think they're leader.
async fn make_all_agents_leader(agent_client: &Client<AgentService>, realm: &RealmResult) {
    let group = *realm.groups.last().unwrap();
    let res = join_all(realm.agents.iter().map(|agent| {
        rpc::send(
            agent_client,
            agent,
            agent_api::BecomeLeaderRequest {
                realm: realm.realm,
                group,
                last: None,
            },
        )
    }))
    .await;
    assert!(res.into_iter().all(|r| r.is_ok()));

    // check that they all think they're leader.
    assert_eq!(realm.agents.len(), num_leaders(agent_client, realm).await);
}

async fn num_leaders(agent_client: &Client<AgentService>, realm: &RealmResult) -> usize {
    let group = *realm.groups.last().unwrap();
    join_all(
        realm
            .agents
            .iter()
            .map(|agent| rpc::send(agent_client, agent, agent_api::StatusRequest {})),
    )
    .await
    .into_iter()
    .filter_map(|r| r.ok())
    .filter_map(|s| s.hsm)
    .filter_map(|hsm| hsm.realm)
    .filter(|r| r.groups.iter().any(|g| g.id == group && g.leader.is_some()))
    .count()
}
