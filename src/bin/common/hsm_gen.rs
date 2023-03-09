//! Creates HSMs and their agents.
//!
//! This module exists in part to encapsulate the secret shared between the HSMs.

use futures::future::join_all;
use http::Uri;
use rand::rngs::OsRng;
use rand::RngCore;
use reqwest::Url;
use std::fmt::Write;
use std::iter;
use std::net::SocketAddr;
use std::ops::RangeFrom;
use std::process::Command;
use std::time::Duration;
use tokio::time::sleep;

use super::process_group::ProcessGroup;
use loam_mvp::http_client;
use loam_mvp::realm::agent::types::{AgentService, StatusRequest};

type AgentClient = http_client::Client<AgentService>;

pub struct HsmGenerator {
    secret: String,
    port: RangeFrom<u16>,
    entrust: bool,
}

impl HsmGenerator {
    pub fn new(entrust: bool, start_port: u16) -> Self {
        let buf = if entrust {
            "010203".to_string()
        } else {
            let mut v = vec![0; 32];
            OsRng.fill_bytes(&mut v);
            let mut buf = String::new();
            for byte in v {
                write!(buf, "{byte:02x}").unwrap();
            }
            buf
        };
        Self {
            secret: buf,
            port: start_port..,
            entrust,
        }
    }

    pub async fn create_hsms(
        &mut self,
        mut count: usize,
        process_group: &mut ProcessGroup,
        bigtable: &Uri,
    ) -> Vec<Url> {
        let mut agent_urls = Vec::with_capacity(count);
        if self.entrust {
            let agent_port = self.port.next().unwrap();
            let agent_address = SocketAddr::from(([127, 0, 0, 1], agent_port)).to_string();
            let agent_url = Url::parse(&format!("http://{agent_address}")).unwrap();
            process_group.spawn(
                Command::new(format!(
                    "target/{}/entrust-agent",
                    if cfg!(debug_assertions) {
                        "debug"
                    } else {
                        "release"
                    }
                ))
                .arg("--listen")
                .arg(agent_address)
                .arg("--bigtable")
                .arg(bigtable.to_string()),
            );
            agent_urls.push(agent_url);
            count -= 1;
        }
        iter::repeat_with(|| {
            let hsm_port = self.port.next().unwrap();
            let agent_port = self.port.next().unwrap();
            let hsm_address = SocketAddr::from(([127, 0, 0, 1], hsm_port));
            let hsm_url = Url::parse(&format!("http://{hsm_address}")).unwrap();
            process_group.spawn(
                Command::new(format!(
                    "target/{}/http_hsm",
                    if cfg!(debug_assertions) {
                        "debug"
                    } else {
                        "release"
                    }
                ))
                .arg("--listen")
                .arg(hsm_address.to_string())
                .arg("--key")
                .arg(&self.secret),
            );
            let agent_address = SocketAddr::from(([127, 0, 0, 1], agent_port)).to_string();
            let agent_url = Url::parse(&format!("http://{agent_address}")).unwrap();
            process_group.spawn(
                Command::new(format!(
                    "target/{}/agent",
                    if cfg!(debug_assertions) {
                        "debug"
                    } else {
                        "release"
                    }
                ))
                .arg("--listen")
                .arg(agent_address)
                .arg("--bigtable")
                .arg(bigtable.to_string())
                .arg("--hsm")
                .arg(hsm_url.to_string()),
            );
            agent_url
        })
        .take(count)
        .for_each(|url| agent_urls.push(url));

        self.wait_for_agents(&agent_urls).await;
        agent_urls
    }

    async fn wait_for_agents(&self, agents: &[Url]) {
        // Wait for the agent to be up, which in turn waits for the HSM
        // to be up.
        //
        // TODO: we shouldn't wait here. Other code needs to handle
        // failures, since servers can go down at any later point.
        let waiters = agents.iter().map(|agent_url| async move {
            let agent_client = AgentClient::new();
            for attempt in 1.. {
                if let Ok(response) = agent_client.send(agent_url, StatusRequest {}).await {
                    if response.hsm.is_some() {
                        break;
                    }
                }
                if attempt >= 1000 {
                    panic!("Failed to connect to agent/HSM at {agent_url}");
                }
                sleep(Duration::from_millis(1)).await;
            }
            agent_url
        });
        join_all(waiters).await;
    }
}
