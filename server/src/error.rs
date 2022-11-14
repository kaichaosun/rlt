use thiserror::Error;

#[derive(Error, Debug)]
pub enum ServerError {
    #[error("Must contain host header")]
    NoHostHeader,
    #[error("Client connection is not setup")]
    EmptyConnection,
    #[error("Must contain an upgrade extension")]
    NoUpgradeExtension,
    #[error("Must contain host header")]
    NoUpgradeHeader,
    #[error("Host name is invalid")]
    InvalidHostName,
}
