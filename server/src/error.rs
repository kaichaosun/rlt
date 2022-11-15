use thiserror::Error;

#[derive(Error, Debug)]
pub enum ServerError {
    #[error("Must contain host header")]
    NoHostHeader,
    #[error("Proxy connection is not setup")]
    ProxyNotReady,
    #[error("Client connection is empty")]
    EmptyConnection,
    #[error("Must contain an upgrade extension")]
    NoUpgradeExtension,
    #[error("Must contain host header")]
    NoUpgradeHeader,
    #[error("Host name is invalid")]
    InvalidHostName,
    #[error("Server config is not valid")]
    InvalidConfig,
}
