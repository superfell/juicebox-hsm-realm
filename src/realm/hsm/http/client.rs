use async_trait::async_trait;
use reqwest::Url;
use std::fmt::Debug;

use super::super::{super::super::http_client::ClientError, client::Transport};

#[derive(Clone)]
pub struct HsmHttpClient {
    hsm: Url,
    http: reqwest::Client,
}

impl HsmHttpClient {
    pub fn new(url: Url) -> Self {
        Self {
            hsm: url.join("/req").unwrap(),
            http: reqwest::Client::builder().build().expect("TODO"),
        }
    }
}

impl Debug for HsmHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HsmHttpClient for {}", self.hsm)
    }
}

#[async_trait]
impl Transport for HsmHttpClient {
    type Error = ClientError;

    async fn send_rpc_msg(&self, _msg_name: &str, msg: Vec<u8>) -> Result<Vec<u8>, Self::Error> {
        match self.http.post(self.hsm.clone()).body(msg).send().await {
            Err(err) => Err(ClientError::Network(err)),
            Ok(response) if response.status().is_success() => {
                let resp_body = response.bytes().await.map_err(ClientError::Network)?;
                Ok(resp_body.to_vec())
            }
            Ok(response) => Err(ClientError::HttpStatus(response.status())),
        }
    }
}
