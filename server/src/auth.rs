use anyhow::Result;

use crate::CONFIG;
use crate::error::ServerError;

pub trait Auth {
    fn credential_is_valid(&self, credential: &str, value: &str) -> Result<bool>;
}

impl Auth for () {
    fn credential_is_valid(&self, _credential: &str, _value: &str) -> Result<bool> {
        Ok(true)
    }
}

pub struct CfWorkerStore;

impl Auth for CfWorkerStore {
    fn credential_is_valid(&self, credential: &str, value: &str) -> Result<bool> {
        let account = CONFIG.cloudflare_account.clone().ok_or(ServerError::InvalidConfig)?;
        let namespace = CONFIG.cloudflare_namespace.clone().ok_or(ServerError::InvalidConfig)?;
        let email = CONFIG.cloudflare_auth_email.clone().ok_or(ServerError::InvalidConfig)?;
        let key = CONFIG.cloudflare_auth_key.clone().ok_or(ServerError::InvalidConfig)?;

        let client = reqwest::blocking::Client::new();
        let resp = client.get(
            format!(
                "https://api.cloudflare.com/client/v4/accounts/{}/storage/kv/namespaces/{}/values/{}",
                account, namespace, value
            ))
            .header("X-Auth-Email", email)
            .header("X-Auth-Key", key)
            .send()?
            .text()?;
        log::info!("{:#?}", resp);

        Ok(credential == resp)
    }
}
