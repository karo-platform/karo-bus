pub mod bus;
mod hub_connection;
pub mod peer;
mod peer_connection;
pub mod signal;
pub mod state;
mod utils;

pub use bus::Bus;

pub type Error = Box<dyn std::error::Error + Sync + Send>;
pub type Result<T> = std::result::Result<T, Error>;
