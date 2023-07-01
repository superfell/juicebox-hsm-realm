use clap::Parser;

use ::reqwest::Certificate;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use tracing::info;

use juicebox_hsm::logging;
use juicebox_sdk::{
    AuthToken, Client, ClientBuilder, Configuration, Pin, Policy, RealmId, RecoverError,
    TokioSleeper, UserInfo, UserSecret,
};
use juicebox_sdk_networking::reqwest::{self, ClientOptions};
use juicebox_sdk_networking::rpc::LoadBalancerService;

/// A Rust demo of the SDK.
#[derive(Parser)]
struct Args {
    /// The SDK client configuration information, as a JSON string.
    #[arg(short, long)]
    configuration: String,

    /// The SDK client auth tokens, as a JSON string mapping realm ID to base64-encoded JWT.
    #[arg(short, long)]
    auth_tokens: String,

    /// DER file containing self-signed certificate for connecting to the load
    /// balancers over TLS. May be given more than once.
    #[arg(long = "tls-certificate", value_name = "PATH")]
    tls_certificates: Vec<PathBuf>,
}

#[tokio::main]
async fn main() {
    logging::configure("juicebox-demo");

    let args = Args::parse();

    let configuration =
        Configuration::from_json(&args.configuration).expect("failed to parse configuration");

    let json_auth_tokens: HashMap<String, AuthToken> =
        serde_json::from_str(&args.auth_tokens).expect("failed to parse auth tokens");

    let auth_tokens = json_auth_tokens
        .into_iter()
        .map(|(id, token)| (RealmId(hex::decode(id).unwrap().try_into().unwrap()), token))
        .collect();

    let lb_certs = args
        .tls_certificates
        .iter()
        .map(|path| {
            Certificate::from_der(&fs::read(path).expect("failed to read certificate file"))
                .expect("failed to decode certificate file")
        })
        .collect();

    let client: Client<
        TokioSleeper,
        reqwest::Client<LoadBalancerService>,
        HashMap<RealmId, AuthToken>,
    > = ClientBuilder::new()
        .configuration(configuration)
        .auth_token_manager(auth_tokens)
        .reqwest_with_options(ClientOptions {
            additional_root_certs: lb_certs,
        })
        .tokio_sleeper()
        .build();

    info!("Starting register (allowing 2 guesses)");
    client
        .register(
            &Pin::from(b"1234".to_vec()),
            &UserSecret::from(b"teyla21".to_vec()),
            &UserInfo::from(b"presso".to_vec()),
            Policy { num_guesses: 2 },
        )
        .await
        .expect("register failed");
    info!("Register succeeded");

    info!("Starting recover with wrong PIN (guess 1)");
    match client
        .recover(
            &Pin::from(b"1212".to_vec()),
            &UserInfo::from(b"presso".to_vec()),
        )
        .await
    {
        Err(RecoverError::InvalidPin { guesses_remaining }) => {
            assert_eq!(guesses_remaining, 1);
            info!("Recover expectedly unsuccessful")
        }
        result => panic!("Unexpected result from recover: {result:?}"),
    };

    info!("Starting recover with correct PIN (guess 2)");
    let secret = client
        .recover(
            &Pin::from(b"1234".to_vec()),
            &UserInfo::from(b"presso".to_vec()),
        )
        .await
        .expect("recover failed");
    info!(
        secret = String::from_utf8_lossy(secret.expose_secret()).to_string(),
        "Recovered secret"
    );

    info!("Starting recover with wrong PIN (guess 1)");
    match client
        .recover(
            &Pin::from(b"1212".to_vec()),
            &UserInfo::from(b"presso".to_vec()),
        )
        .await
    {
        Err(RecoverError::InvalidPin { guesses_remaining }) => {
            assert_eq!(guesses_remaining, 1);
            info!("Recover expectedly unsuccessful")
        }
        result => panic!("Unexpected result from recover: {result:?}"),
    };

    info!("Starting recover with wrong PIN (guess 2)");
    match client
        .recover(
            &Pin::from(b"1212".to_vec()),
            &UserInfo::from(b"presso".to_vec()),
        )
        .await
    {
        Err(RecoverError::InvalidPin { guesses_remaining }) => {
            assert_eq!(guesses_remaining, 0);
            info!("Recover expectedly unsuccessful")
        }
        result => panic!("Unexpected result from recover: {result:?}"),
    };

    info!("Starting recover with correct PIN (guess 3)");
    match client
        .recover(
            &Pin::from(b"1234".to_vec()),
            &UserInfo::from(b"presso".to_vec()),
        )
        .await
    {
        Err(RecoverError::InvalidPin { guesses_remaining }) => {
            assert_eq!(guesses_remaining, 0);
            info!("Recover expectedly unsuccessful")
        }
        result => panic!("Unexpected result from recover: {result:?}"),
    };

    info!("Starting register");
    client
        .register(
            &Pin::from(b"4321".to_vec()),
            &UserSecret::from(b"presso42".to_vec()),
            &UserInfo::from(b"teyla".to_vec()),
            Policy { num_guesses: 2 },
        )
        .await
        .expect("register failed");
    info!("register succeeded");

    info!("Starting recover with correct PIN (guess 1)");
    let secret = client
        .recover(
            &Pin::from(b"4321".to_vec()),
            &UserInfo::from(b"teyla".to_vec()),
        )
        .await
        .expect("recover failed");
    info!(
        secret = String::from_utf8_lossy(secret.expose_secret()).to_string(),
        "Recovered secret"
    );

    info!("Deleting secret");
    client.delete().await.expect("delete unexpectedly failed");
    info!("delete succeeded");

    logging::flush();
    info!(pid = std::process::id(), "exiting");
}
