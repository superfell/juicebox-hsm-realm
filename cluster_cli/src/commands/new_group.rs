use reqwest::Url;

use agent_api::AgentService;
use juicebox_sdk_core::types::RealmId;
use juicebox_sdk_networking::reqwest::Client;

pub async fn new_group(
    realm: RealmId,
    agent_addresses: &[Url],
    agents_client: &Client<AgentService>,
) -> anyhow::Result<()> {
    println!("Creating new group in realm {realm:?}");
    let group = cluster_core::new_group(agents_client, realm, agent_addresses).await?;
    println!("Created group {group:?}");
    Ok(())
}