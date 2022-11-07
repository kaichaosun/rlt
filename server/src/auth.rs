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
        
        false
    }
}