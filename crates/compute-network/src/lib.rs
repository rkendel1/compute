//! Compute's data plane and network providers.
//!
//! - [`endpoints`]: stable host ports that forward to whichever service
//!   instance serves them. Switching a release retargets an endpoint; the
//!   listener never closes, so a client never sees a refused connection.
//! - [`ingress`]: the node's public HTTP and HTTPS entry. It answers ACME
//!   HTTP-01 challenges, redirects to HTTPS, terminates TLS by SNI, and
//!   forwards to endpoints.
//! - [`dns`]: DNS providers behind one small trait.
//! - [`acme`]: certificate issuance with HTTP-01.
//! - [`secrets`]: node-local secret storage. Private keys and provider
//!   credentials live here or in the environment, never in control state.
//!
//! Nothing here knows about control state: the daemon decides, this crate
//! carries it out.

pub mod acme;
pub mod dns;
pub mod endpoints;
pub mod ingress;
pub mod secrets;
pub mod tls;

pub use endpoints::{Endpoints, Route};
pub use ingress::{Ingress, IngressRoute};
pub use secrets::SecretStore;
