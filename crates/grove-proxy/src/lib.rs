//! Grove's built-in HTTPS reverse proxy: a local certificate authority that
//! mints leaf certificates on demand, host-based routing to cluster
//! processes, websocket upgrades, and wake-on-request for idle clusters.

mod backend;
mod body;
mod ca;
mod command;
mod error;
mod forward;
mod handler;
mod pages;
mod routes;
mod server;
mod tls;
mod trust;

pub use backend::{Availability, Backend, IndexEntry};
pub use ca::CertAuthority;
pub use error::{CaError, ProxyError};
pub use routes::Route;
pub use server::{Proxy, ProxyConfig};
pub use tls::SniResolver;
