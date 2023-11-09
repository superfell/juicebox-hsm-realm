//! General-purpose mechanisms to access databases of secrets at runtime.
use async_trait::async_trait;
use google::GrpcConnectionOptions;
use juicebox_realm_api::types::SecretBytesVec;
use juicebox_realm_auth::{AuthKey, AuthKeyVersion};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

mod google_secret_manager;
mod periodic;
mod secrets_file;

pub use anyhow::Error;
pub use google_secret_manager::Client as GoogleSecretManagerClient;
pub use periodic::{BulkLoad, Periodic};
pub use secrets_file::SecretsFile;

/// A value that should remain confidential.
#[derive(Clone, Debug, Deserialize)]
pub struct Secret(pub SecretBytesVec);

impl From<Vec<u8>> for Secret {
    fn from(value: Vec<u8>) -> Self {
        Self(SecretBytesVec::from(value))
    }
}

impl From<Secret> for AuthKey {
    fn from(value: Secret) -> Self {
        Self(value.0)
    }
}

/// An identifier for a secret. Secret names are not confidential.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SecretName(pub String);

/// A version number for a secret.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SecretVersion(pub u64);

impl From<AuthKeyVersion> for SecretVersion {
    fn from(value: AuthKeyVersion) -> Self {
        Self(value.0)
    }
}

impl From<SecretVersion> for AuthKeyVersion {
    fn from(value: SecretVersion) -> Self {
        Self(value.0)
    }
}

/// A client to access a database of secrets.
#[async_trait]
pub trait SecretManager: Debug + Send + Sync {
    /// Returns a particular version of a secret.
    async fn get_secret_version(
        &self,
        name: &SecretName,
        version: SecretVersion,
    ) -> Result<Option<Secret>, Error>;

    /// Returns the newest version of a secret.
    async fn get_latest_secret_version(
        &self,
        name: &SecretName,
    ) -> Result<Option<(SecretVersion, Secret)>, Error> {
        Ok(self
            .get_secrets(name)
            .await?
            .into_iter()
            .max_by(|(a_version, _), (b_version, _)| a_version.cmp(b_version)))
    }

    /// Returns the secret versions for this named secret, or an empty map if
    /// there are none.
    ///
    /// Trying multiple active keys can be useful for key rotation even when
    /// the secret's version is unknown.
    async fn get_secrets(&self, name: &SecretName)
        -> Result<HashMap<SecretVersion, Secret>, Error>;
}

/// A [`HashMap`] is a simple way to access a static set of secrets.
#[async_trait]
impl SecretManager for HashMap<SecretName, HashMap<SecretVersion, Secret>> {
    async fn get_secret_version(
        &self,
        name: &SecretName,
        version: SecretVersion,
    ) -> Result<Option<Secret>, Error> {
        Ok(self
            .get(name)
            .and_then(|versions| versions.get(&version))
            .cloned())
    }

    async fn get_secrets(
        &self,
        name: &SecretName,
    ) -> Result<HashMap<SecretVersion, Secret>, Error> {
        Ok(self.get(name).cloned().unwrap_or_default())
    }
}

/// Constructs a new Google Cloud Secret Manager client that's limited to
/// accessing tenant auth keys.
pub async fn new_google_secret_manager(
    project: &str,
    auth_manager: Arc<gcp_auth::AuthenticationManager>,
    refresh_interval: Duration,
    options: GrpcConnectionOptions,
) -> Result<impl SecretManager, Error> {
    let client = GoogleSecretManagerClient::new(
        project,
        auth_manager,
        Some(format!(
            "({}) OR ({})",
            format_args!(
                "name:{} AND labels.kind=record_id_randomization_key",
                record_id_randomization_key_name().0
            ),
            "name:tenant- AND labels.kind=tenant_auth_key",
        )),
        options,
    )
    .await?;
    let manager = Periodic::new(client, refresh_interval).await?;
    Ok(manager)
}

/// The name of a per-realm secret Blake2sMac256 key. The key must be exactly
/// 32 bytes long.
///
/// The key is used to pseudo-randomly distribute record IDs (even with
/// adversarially-generated user IDs), so that the Merkle trees stay balanced.
/// This is similar to how hash tables are randomized.
pub fn record_id_randomization_key_name() -> SecretName {
    SecretName(String::from("record-id-randomization"))
}

pub fn tenant_secret_name(tenant: &str) -> SecretName {
    SecretName(format!("tenant-{tenant}"))
}
