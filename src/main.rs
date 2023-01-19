use actix::prelude::*;
use bitvec::prelude::*;
use futures::future::{join_all, try_join_all};
use std::iter;
use std::ops::RangeFrom;
use std::str::FromStr;
use tracing::{debug, info, Level};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::FmtSubscriber;

mod client;
mod realm;
mod server;
mod types;

use client::{Client, Configuration, Pin, Realm, RecoverError, UserSecret};
use server::Server;
use types::{AuthToken, Policy};

use realm::agent::Agent;
use realm::hsm::types::{SecretsRequest, SecretsResponse, UserId};
use realm::hsm::{Hsm, RealmKey};
use realm::load_balancer::types::{ClientRequest, ClientResponse};
use realm::load_balancer::LoadBalancer;
use realm::store::Store;

/// Creates HSMs and their agents.
///
/// This module exists to encapsulate the secret shared between the HSMs.
mod hsm_gen {
    use super::*;

    pub struct HsmGenerator {
        secret: RealmKey,
        counter: RangeFrom<usize>,
    }

    impl HsmGenerator {
        pub fn new() -> Self {
            Self {
                secret: RealmKey::random(),
                counter: 1..,
            }
        }

        pub fn create_hsm(&mut self, store: Addr<Store>) -> Addr<Agent> {
            let i = self.counter.next().unwrap();
            let hsm = Hsm::new(format!("hsm{i:02}"), self.secret.clone()).start();
            Agent::new(format!("agent{i:02}"), hsm, store).start()
        }
    }
}
use hsm_gen::HsmGenerator;

#[actix_rt::main]
async fn main() {
    let log_level = std::env::var("LOGLEVEL")
        .map(|s| match Level::from_str(&s) {
            Ok(level) => level,
            Err(e) => panic!("failed to parse LOGLEVEL: {e}"),
        })
        .unwrap_or(Level::DEBUG);
    let subscriber = FmtSubscriber::builder()
        .with_file(true)
        .with_line_number(true)
        .with_max_level(log_level)
        .with_span_events(FmtSpan::ACTIVE)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    info!(
        max_level = %log_level,
        "set up tracing. you can set verbosity with env var LOGLEVEL."
    );

    info!("creating in-memory store");
    let store = Store::new().start();

    let num_load_balancers = 2;
    info!(count = num_load_balancers, "creating load balancers");
    let load_balancers: Vec<Addr<LoadBalancer>> = (1..=num_load_balancers)
        .map(|i| LoadBalancer::new(format!("lb{i}"), store.clone()).start())
        .collect();

    let mut hsm_generator = HsmGenerator::new();

    let num_hsms = 5;
    info!(count = num_hsms, "creating initial HSMs and agents");
    let group1: Vec<Addr<Agent>> = iter::repeat_with(|| hsm_generator.create_hsm(store.clone()))
        .take(num_hsms)
        .collect();
    let (realm_id, group_id1) = realm::cluster::new_realm(&group1).await.unwrap();
    info!(?realm_id, group_id = ?group_id1, "initialized cluster");

    info!("creating additional groups");
    let group2: Vec<Addr<Agent>> = iter::repeat_with(|| hsm_generator.create_hsm(store.clone()))
        .take(5)
        .collect();
    let group3: Vec<Addr<Agent>> = iter::repeat_with(|| hsm_generator.create_hsm(store.clone()))
        .take(4)
        .collect();
    let new_groups = try_join_all([
        realm::cluster::new_group(realm_id, &group2),
        realm::cluster::new_group(realm_id, &group3),
        realm::cluster::new_group(realm_id, &group1),
    ])
    .await
    .unwrap();
    info!(?realm_id, ?new_groups, "created groups");

    let num_hsms = 5;
    info!(count = num_hsms, "creating group2 on new HSMs");
    let group_id2 = realm::cluster::new_group(realm_id, &group2).await.unwrap();
    info!(?realm_id, group_id = ?group_id2, "initialized group");

    let num_hsms = 4;
    info!(count = num_hsms, "creating group3 on new HSMs");
    let group_id3 = realm::cluster::new_group(realm_id, &group2).await.unwrap();
    info!(?realm_id, group_id = ?group_id3, "initialized group");

    info!(count = num_hsms, "creating group4 on first group's HSMs");
    let group_id4 = realm::cluster::new_group(realm_id, &group1).await.unwrap();
    info!(?realm_id, group_id = ?group_id4, "initialized group");

    info!("incrementing a bunch");
    let uids = [
        UserId(bitvec::bitvec![0, 0]),
        UserId(bitvec::bitvec![0, 1]),
        UserId(bitvec::bitvec![1, 0]),
    ];
    join_all(
        iter::zip(uids.iter().cycle(), load_balancers.iter().cycle())
            .take(297)
            .map(|(uid, load_balancer)| async move {
                let result = load_balancer
                    .send(ClientRequest {
                        realm: realm_id,
                        uid: uid.clone(),
                        request: SecretsRequest::Increment,
                    })
                    .await
                    .unwrap();
                match result {
                    ClientResponse::Ok(SecretsResponse::Increment(new_value)) => {
                        debug!(?uid, new_value, "incremented")
                    }
                    ClientResponse::Unavailable => todo!(),
                }
            }),
    )
    .await;

    info!("reading counts after many parallel requests");
    join_all(uids.iter().map(|uid| {
        let load_balancer = load_balancers[0].clone();
        async move {
            let result = load_balancer
                .send(ClientRequest {
                    realm: realm_id,
                    uid: uid.clone(),
                    request: SecretsRequest::Increment,
                })
                .await
                .unwrap();
            match result {
                ClientResponse::Ok(SecretsResponse::Increment(new_value)) => {
                    info!(?uid, new_value, "incremented")
                }
                ClientResponse::Unavailable => todo!(),
            }
        }
    }))
    .await;

    println!("main: Starting 4 servers");
    let server1_addr = Server::new(String::from("server1")).start();
    let server2_addr = Server::new(String::from("server2")).start();
    let server3_addr = Server::new(String::from("server3")).start();
    let server4_addr = Server::new(String::from("dead-server4")).start();
    println!();

    let client = Client::new(
        Configuration {
            realms: vec![
                Realm {
                    address: server1_addr,
                    public_key: b"qwer".to_vec(),
                },
                Realm {
                    address: server2_addr,
                    public_key: b"asdf".to_vec(),
                },
                Realm {
                    address: server3_addr,
                    public_key: b"zxcv".to_vec(),
                },
                Realm {
                    address: server4_addr,
                    public_key: b"uiop".to_vec(),
                },
            ],
            register_threshold: 3,
            recover_threshold: 3,
        },
        AuthToken {
            user: String::from("mario"),
            signature: String::from("it's-a-me!"),
        },
    );

    println!("main: Starting register (allowing 2 guesses)");
    client
        .register(
            &Pin(b"1234".to_vec()),
            &UserSecret(b"teyla21".to_vec()),
            Policy { num_guesses: 2 },
        )
        .await
        .expect("register failed");
    println!("main: register succeeded");
    println!();

    println!("main: Starting recover with wrong PIN (guess 1)");
    match client.recover(&Pin(b"1212".to_vec())).await {
        Err(RecoverError::Unsuccessful(_)) => { /* ok */ }
        result => panic!("Unexpected result from recover: {result:?}"),
    };
    println!();

    println!("main: Starting recover with correct PIN (guess 2)");
    let secret = client
        .recover(&Pin(b"1234".to_vec()))
        .await
        .expect("recover failed");
    println!(
        "main: Recovered secret {:?}",
        String::from_utf8_lossy(&secret.0)
    );
    println!();

    println!("main: Starting recover with wrong PIN (guess 1)");
    match client.recover(&Pin(b"1212".to_vec())).await {
        Err(RecoverError::Unsuccessful(_)) => { /* ok */ }
        result => panic!("Unexpected result from recover: {result:?}"),
    };
    println!();

    println!("main: Starting recover with wrong PIN (guess 2)");
    match client.recover(&Pin(b"1212".to_vec())).await {
        Err(RecoverError::Unsuccessful(_)) => { /* ok */ }
        result => panic!("Unexpected result from recover: {result:?}"),
    };
    println!();

    println!("main: Starting recover with correct PIN (guess 3)");
    match client.recover(&Pin(b"1234".to_vec())).await {
        Err(RecoverError::Unsuccessful(_)) => { /* ok */ }
        result => panic!("Unexpected result from recover: {result:?}"),
    };
    println!();

    println!("main: Starting register");
    client
        .register(
            &Pin(b"1234".to_vec()),
            &UserSecret(b"teyla21".to_vec()),
            Policy { num_guesses: 2 },
        )
        .await
        .expect("register failed");
    println!("main: register succeeded");
    println!();

    println!("main: Deleting secret");
    match client.delete_all().await {
        Ok(()) => {
            println!("main: delete succeeded");
        }
        Err(e) => {
            println!("main: warning: delete failed: {e:?}");
        }
    }
    println!();

    println!("main: exiting");
    System::current().stop();
}
