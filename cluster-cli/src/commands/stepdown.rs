use anyhow::Context;
use reqwest::Url;

use hsmcore::hsm::types::HsmId;
use juicebox_hsm::http_client::{Client, ClientOptions};
use juicebox_hsm::realm::cluster::types::{ClusterService, StepDownRequest, StepDownResponse};
use juicebox_sdk_networking::rpc;

pub async fn stepdown(cluster_url: &Url, hsm: HsmId) -> anyhow::Result<()> {
    let c = Client::<ClusterService>::new(ClientOptions::default());
    let r = rpc::send(&c, cluster_url, StepDownRequest::Hsm(hsm)).await;
    match r.context("error while asking cluster manager to perform leadership stepdown")? {
        StepDownResponse::Ok => {
            println!("Leader stepdown successfully completed");
        }
        s => {
            println!("Leader stepdown had error: {s:?}");
        }
    }
    Ok(())
}
