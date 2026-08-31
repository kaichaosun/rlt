use thiserror::Error;

#[derive(Error, Debug)]
pub enum ServerError {
    // A missing Host header, an unregistered endpoint and a tunnel with no
    // connection to spare are all answered with a real HTTP status (see
    // `proxy::error_response`) rather than raised as errors.
    #[error("Must contain an upgrade extension")]
    NoUpgradeExtension,
    #[error("Must contain host header")]
    NoUpgradeHeader,
    #[error("Host name is invalid")]
    InvalidHostName,
    #[error("Server config is not valid")]
    InvalidConfig,
}
