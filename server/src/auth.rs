use crate::CONFIG;

pub trait Auth {
    fn credential_is_valid(&self, credential: &str, value: &str) -> bool;
}

impl Auth for () {
    fn credential_is_valid(&self, _credential: &str, _value: &str) -> bool {
        true
    }
}

pub struct CfWorkerStore;

impl Auth for CfWorkerStore {
    fn credential_is_valid(&self, credential: &str, value: &str) -> bool {
        let account = CONFIG.cloudflare_account.clone().unwrap();
        let namespace = CONFIG.cloudflare_namespace.clone().unwrap();
        let email = CONFIG.cloudflare_auth_email.clone().unwrap();
        let key = CONFIG.cloudflare_auth_key.clone().unwrap();

        let client = reqwest::blocking::Client::new();
        let resp = client.get(
            format!(
                "https://api.cloudflare.com/client/v4/accounts/{}/storage/kv/namespaces/{}/values/{}",
                account, namespace, value
            ))
            .header("X-Auth-Email", email)
            .header("X-Auth-Key", key)
            .send()
            .unwrap()
            .text()
            .unwrap();
        log::info!("{:#?}", resp);

        credential == resp
    }
}
