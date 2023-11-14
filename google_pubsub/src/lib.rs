use async_trait::async_trait;
use gcp_auth::AuthenticationManager;
use google::auth::AuthMiddleware;
use google::pubsub::v1::publisher_client::PublisherClient;
use google::pubsub::v1::subscriber_client::SubscriberClient;
use google::pubsub::v1::{
    ExpirationPolicy, PublishRequest, PublishResponse, PubsubMessage, Subscription, Topic,
};
use google::GrpcConnectionOptions;
use std::collections::HashMap;
use std::error::Error;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tonic::transport::{Endpoint, Uri};
use tonic::{Code, Status};
use tracing::{info, instrument, warn};

use juicebox_realm_api::types::RealmId;
use observability::{metrics, metrics_tag as tag};
use pubsub_api::Message;

pub struct Publisher {
    project: String,
    pub_client: PublisherClient<AuthMiddleware>,
    sub_client: SubscriberClient<AuthMiddleware>,
    metrics: metrics::Client,
}

impl std::fmt::Debug for Publisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher")
            .field("project", &self.project)
            .finish_non_exhaustive()
    }
}

impl Publisher {
    pub async fn new(
        service_url: Option<Uri>,
        project: String,
        auth: Option<Arc<AuthenticationManager>>,
        metrics: metrics::Client,
        options: GrpcConnectionOptions,
    ) -> Result<Self, tonic::transport::Error> {
        let url = service_url.unwrap_or(Uri::from_static("https://pubsub.googleapis.com"));
        let endpoint = options.apply(Endpoint::from(url.clone())).connect().await?;
        let channel =
            AuthMiddleware::new(endpoint, auth, &["https://www.googleapis.com/auth/pubsub"]);

        let pub_client = PublisherClient::new(channel.clone());
        let sub_client = SubscriberClient::new(channel);
        Ok(Publisher {
            project,
            pub_client,
            sub_client,
            metrics,
        })
    }
}

#[async_trait]
impl pubsub_api::Publisher for Publisher {
    #[instrument(level = "trace", skip(self, m))]
    async fn publish(
        &self,
        realm: RealmId,
        tenant: &str,
        m: Message,
    ) -> Result<(), Box<dyn Error>> {
        let pub_req = PublishRequest {
            topic: topic_name(&self.project, realm, tenant),
            messages: vec![PubsubMessage {
                data: m.0.to_string().into_bytes(),
                attributes: HashMap::new(),
                message_id: String::from(""),
                publish_time: None,
                ordering_key: String::from(""),
            }],
        };
        self.metrics
            .async_time("pubsub.publish.time", [tag!(?realm)], || async {
                match self.publish_msg(pub_req.clone()).await {
                    Err(err) if err.code() == Code::NotFound => {
                        warn!(
                            ?realm,
                            ?tenant,
                            "tenant topic not found, attempting to create it"
                        );
                        self.create_topic_and_sub(realm, tenant).await?;
                        self.publish_msg(pub_req).await
                    }
                    Err(err) => Err(err),
                    Ok(res) => Ok(res),
                }
            })
            .await?;
        Ok(())
    }
}

pub fn topic_name(project: &str, realm: RealmId, tenant: &str) -> String {
    format!("projects/{}/topics/tenant-{}-{:?}", project, tenant, realm)
}

pub fn subscription_name(project: &str, realm: RealmId, tenant: &str) -> String {
    format!(
        "projects/{}/subscriptions/tenant-{}-{:?}-sub",
        project, tenant, realm
    )
}

impl Publisher {
    async fn publish_msg(
        &self,
        req: PublishRequest,
    ) -> Result<tonic::Response<PublishResponse>, Status> {
        retry_op(
            || async {
                let mut pc = self.pub_client.clone();
                pc.publish(req.clone()).await
            },
            retry_bad_gateway,
            3,
        )
        .await
    }

    async fn create_topic(&self, topic: Topic) -> Result<tonic::Response<Topic>, Status> {
        retry_op(
            || async {
                let mut pc = self.pub_client.clone();
                pc.create_topic(topic.clone()).await
            },
            retry_bad_gateway,
            3,
        )
        .await
    }

    async fn create_subscription(
        &self,
        sub: Subscription,
    ) -> Result<tonic::Response<Subscription>, Status> {
        retry_op(
            || async {
                let mut sc = self.sub_client.clone();
                sc.create_subscription(sub.clone()).await
            },
            retry_bad_gateway,
            3,
        )
        .await
    }

    #[instrument(level = "trace", skip(self))]
    async fn create_topic_and_sub(
        &self,
        realm: RealmId,
        tenant: &str,
    ) -> Result<(), tonic::Status> {
        let labels = HashMap::from([
            (String::from("realm"), format!("{realm:?}")),
            (String::from("tenant"), tenant.to_owned()),
        ]);
        match self
            .create_topic(Topic {
                name: topic_name(&self.project, realm, tenant),
                labels: labels.clone(),
                message_storage_policy: None,
                kms_key_name: String::from(""),
                schema_settings: None,
                satisfies_pzs: false,
                message_retention_duration: None,
            })
            .await
        {
            Err(err) if err.code() == Code::AlreadyExists => {
                // We can end up concurrently trying to create the same topic, that's ok.
                info!(?realm, ?tenant, "topic for tenant already exists");
            }
            Err(err) => {
                warn!(?realm, ?tenant, ?err, "failed to create topic for tenant");
                return Err(err);
            }
            Ok(_) => {
                info!(?realm, ?tenant, "created topic for tenant");
            }
        }

        match self
            .create_subscription(Subscription {
                name: subscription_name(&self.project, realm, tenant),
                topic: topic_name(&self.project, realm, tenant),
                push_config: None,
                bigquery_config: None,
                cloud_storage_config: None,
                ack_deadline_seconds: 10,
                retain_acked_messages: false,
                message_retention_duration: None,
                labels,
                enable_message_ordering: false,
                expiration_policy: Some(ExpirationPolicy { ttl: None }),
                filter: String::from(""),
                dead_letter_policy: None,
                retry_policy: None,
                detached: false,
                enable_exactly_once_delivery: true,
                // These 2 fields are output only, it doesn't matter what
                // they're set to here.
                topic_message_retention_duration: None,
                state: 0,
            })
            .await
        {
            Err(err) if err.code() == Code::AlreadyExists => {
                // We can end up concurrently trying to create the same
                // subscription, that's ok.
                info!(?realm, ?tenant, "subscription for tenant already exists");
                Ok(())
            }
            Err(err) => {
                warn!(?realm, ?tenant, ?err, "failed to create topic subscription");
                Err(err)
            }
            Ok(_) => {
                info!(?realm, ?tenant, "created subscription for tenant");
                Ok(())
            }
        }
    }
}

// Bad Gateway is reported as
//"err":"Status { code: Unavailable, message: \"502:Bad Gateway\" // bunch of useless stuff }
fn retry_bad_gateway(s: &Status) -> bool {
    // the Pub/Sub docs explictly say to retry 502 errors.
    s.code() == Code::Unavailable && s.message().starts_with("502:")
}

async fn retry_op<F, Fut, R, T>(
    mut op: F,
    should_retry: R,
    mut attempts_left: isize,
) -> Result<T, Status>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Status>>,
    R: Fn(&Status) -> bool,
{
    loop {
        match op().await {
            Ok(r) => return Ok(r),
            Err(err) if should_retry(&err) && attempts_left > 0 => {
                sleep(Duration::from_secs(1)).await;
                attempts_left -= 1;
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}
