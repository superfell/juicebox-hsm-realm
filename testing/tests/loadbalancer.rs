use http::StatusCode;
use once_cell::sync::Lazy;
use std::path::PathBuf;
use std::time::Duration;

use juicebox_networking::rpc::Rpc;
use juicebox_process_group::ProcessGroup;
use juicebox_realm_api::requests::{SecretsRequest, BODY_SIZE_LIMIT};
use testing::exec::bigtable::emulator;
use testing::exec::cluster_gen::{create_cluster, ClusterConfig, RealmConfig};
use testing::exec::hsm_gen::{Entrust, MetricsParticipants};
use testing::exec::PortIssuer;

// rust runs the tests in parallel, so we need each test to get its own port.
static PORT: Lazy<PortIssuer> = Lazy::new(|| PortIssuer::new(8444));

#[tokio::test]
async fn request_bodysize_check() {
    let bt_args = emulator(PORT.next());
    let mut processes = ProcessGroup::new();

    let cluster_args = ClusterConfig {
        load_balancers: 1,
        realms: vec![RealmConfig {
            hsms: 1,
            groups: 1,
            metrics: MetricsParticipants::None,
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

    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .use_rustls_tls();
    b = b.add_root_certificate(cluster.lb_cert());

    let http = b.build().unwrap();
    let req = vec![1; BODY_SIZE_LIMIT + 1];
    let res = http
        .post(
            cluster.load_balancers[0]
                .join(SecretsRequest::PATH)
                .unwrap(),
        )
        .body(req)
        .send()
        .await
        .unwrap();
    assert_eq!(StatusCode::PAYLOAD_TOO_LARGE, res.status());

    let req = vec![1; BODY_SIZE_LIMIT];
    let res = http
        .post(
            cluster.load_balancers[0]
                .join(SecretsRequest::PATH)
                .unwrap(),
        )
        .body(req)
        .send()
        .await
        .unwrap();
    assert_eq!(StatusCode::BAD_REQUEST, res.status());
}
