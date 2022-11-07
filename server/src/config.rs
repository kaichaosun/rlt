use serde::Deserialize;


#[derive(Deserialize, Debug)]
pub struct Config {
    pub cloudflare_account: Option<String>,
    pub cloudflare_namespace: Option<String>,
    pub cloudflare_auth_email: Option<String>,
    pub cloudflare_auth_key: Option<String>,
}
