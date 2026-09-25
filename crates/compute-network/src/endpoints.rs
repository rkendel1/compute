//! Stable endpoints.
//!
//! A service's endpoint is a host port that outlives any one instance of
//! the service. Each instance binds its own port; the endpoint forwards
//! every new connection to the instance its [`Route`] names. Retargeting
//! is atomic for new connections, and connections already open finish on
//! the instance they started on. That is what lets a release move traffic
//! without a refused connection, and what lets it drain the old instance.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Where an endpoint sends new connections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The serving instance.
    pub instance_id: String,
    /// The instance's own port on this node.
    pub target_port: u16,
}

struct Listener {
    route: Arc<RwLock<Route>>,
    task: JoinHandle<()>,
}

/// Open connections per instance.
#[derive(Default)]
struct Connections {
    open: Mutex<BTreeMap<String, usize>>,
    served: Mutex<BTreeMap<String, u64>>,
}

impl Connections {
    fn opened(self: &Arc<Self>, instance_id: &str) -> ConnectionGuard {
        *self
            .open
            .lock()
            .expect("connections")
            .entry(instance_id.to_string())
            .or_default() += 1;
        *self
            .served
            .lock()
            .expect("connections")
            .entry(instance_id.to_string())
            .or_default() += 1;
        ConnectionGuard {
            connections: self.clone(),
            instance_id: instance_id.to_string(),
        }
    }
}

struct ConnectionGuard {
    connections: Arc<Connections>,
    instance_id: String,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut open = self.connections.open.lock().expect("connections");
        if let Some(count) = open.get_mut(&self.instance_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                open.remove(&self.instance_id);
            }
        }
    }
}

/// The node's endpoints.
pub struct Endpoints {
    bind: IpAddr,
    listeners: Mutex<BTreeMap<u16, Listener>>,
    connections: Arc<Connections>,
}

impl Endpoints {
    /// Endpoints listen on `bind` (loopback unless the node exposes them).
    pub fn new(bind: IpAddr) -> Self {
        Self {
            bind,
            listeners: Mutex::new(BTreeMap::new()),
            connections: Arc::default(),
        }
    }

    pub fn loopback() -> Self {
        Self::new(IpAddr::V4(Ipv4Addr::LOCALHOST))
    }

    /// Send `host_port`'s new connections to `route`, listening first if
    /// the endpoint is new.
    pub async fn assign(&self, host_port: u16, route: Route) -> io::Result<()> {
        {
            let listeners = self.listeners.lock().expect("listeners");
            if let Some(listener) = listeners.get(&host_port)
                && !listener.task.is_finished()
            {
                *listener.route.write().expect("route") = route;
                return Ok(());
            }
        }
        let listener = TcpListener::bind(SocketAddr::new(self.bind, host_port)).await?;
        let shared = Arc::new(RwLock::new(route));
        let task = tokio::spawn(serve(listener, shared.clone(), self.connections.clone()));
        self.listeners.lock().expect("listeners").insert(
            host_port,
            Listener {
                route: shared,
                task,
            },
        );
        Ok(())
    }

    /// Stop listening on every endpoint not in `keep`.
    pub fn retain(&self, keep: &BTreeSet<u16>) {
        self.listeners
            .lock()
            .expect("listeners")
            .retain(|port, listener| {
                let kept = keep.contains(port);
                if !kept {
                    listener.task.abort();
                }
                kept
            });
    }

    /// Where `host_port` sends new connections, if it listens.
    pub fn route(&self, host_port: u16) -> Option<Route> {
        self.listeners
            .lock()
            .expect("listeners")
            .get(&host_port)
            .filter(|listener| !listener.task.is_finished())
            .map(|listener| listener.route.read().expect("route").clone())
    }

    pub fn ports(&self) -> Vec<u16> {
        self.listeners
            .lock()
            .expect("listeners")
            .keys()
            .copied()
            .collect()
    }

    /// Connections open to an instance through any endpoint.
    pub fn open_connections(&self, instance_id: &str) -> usize {
        self.connections
            .open
            .lock()
            .expect("connections")
            .get(instance_id)
            .copied()
            .unwrap_or(0)
    }

    /// Connections an instance has accepted through endpoints since this
    /// node started.
    pub fn served_connections(&self, instance_id: &str) -> u64 {
        self.connections
            .served
            .lock()
            .expect("connections")
            .get(instance_id)
            .copied()
            .unwrap_or(0)
    }
}

impl Drop for Endpoints {
    fn drop(&mut self) {
        for listener in self.listeners.lock().expect("listeners").values() {
            listener.task.abort();
        }
    }
}

async fn serve(listener: TcpListener, route: Arc<RwLock<Route>>, connections: Arc<Connections>) {
    loop {
        let Ok((client, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let target = route.read().expect("route").clone();
        let guard = connections.opened(&target.instance_id);
        tokio::spawn(async move {
            let _guard = guard;
            let _ = forward(client, target.target_port).await;
        });
    }
}

/// Connect to a local port, with a bound on how long that may take.
pub async fn connect_local(port: u16) -> io::Result<TcpStream> {
    tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))?
}

async fn forward(mut client: TcpStream, port: u16) -> io::Result<()> {
    let mut upstream = connect_local(port).await?;
    let _ = client.set_nodelay(true);
    let _ = upstream.set_nodelay(true);
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn backend(reply: &'static str) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 64];
                    let _ = stream.read(&mut buffer).await;
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        port
    }

    async fn ask(port: u16) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(b"hi").await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        reply
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[tokio::test]
    async fn retargeting_moves_new_connections_and_keeps_the_listener() {
        let one = backend("one").await;
        let two = backend("two").await;
        let endpoints = Endpoints::loopback();
        let port = free_port();
        endpoints
            .assign(
                port,
                Route {
                    instance_id: "a".into(),
                    target_port: one,
                },
            )
            .await
            .unwrap();
        assert_eq!(ask(port).await, "one");
        endpoints
            .assign(
                port,
                Route {
                    instance_id: "b".into(),
                    target_port: two,
                },
            )
            .await
            .unwrap();
        assert_eq!(ask(port).await, "two");
        assert_eq!(endpoints.route(port).unwrap().instance_id, "b");
        assert_eq!(endpoints.served_connections("a"), 1);
        assert_eq!(endpoints.served_connections("b"), 1);
        endpoints.retain(&BTreeSet::new());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(endpoints.route(port).is_none());
    }

    #[tokio::test]
    async fn open_connections_are_counted_until_they_close() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let target = listener.local_addr().unwrap().port();
        let endpoints = Endpoints::loopback();
        let port = free_port();
        endpoints
            .assign(
                port,
                Route {
                    instance_id: "a".into(),
                    target_port: target,
                },
            )
            .await
            .unwrap();
        let client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (held, _) = listener.accept().await.unwrap();
        assert_eq!(endpoints.open_connections("a"), 1);
        drop(client);
        drop(held);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while endpoints.open_connections("a") != 0 {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
