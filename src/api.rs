// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under the MIT license <LICENSE-MIT
// http://opensource.org/licenses/MIT> or the Modified BSD license <LICENSE-BSD
// https://opensource.org/licenses/BSD-3-Clause>, at your option. This file may not be copied,
// modified, or distributed except according to those terms. Please review the Licences for the
// specific language governing permissions and limitations relating to use of the SAFE Network
// Software.

#[cfg(feature = "upnp")]
use super::igd;
use super::{
    bootstrap_cache::BootstrapCache,
    config::{Config, SerialisableCertificate},
    connections::{Connection, IncomingConnections, SendStream},
    dirs::{Dirs, OverRide},
    error::{Error, Result},
    peer_config::{self, DEFAULT_IDLE_TIMEOUT_MSEC, DEFAULT_KEEP_ALIVE_INTERVAL_MSEC},
};
use bytes::Bytes;
use futures::future::select_ok;
use log::{error, info, trace};
use std::{
    collections::VecDeque,
    mem,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
};

/// Default maximum allowed message size. We'll error out on any bigger messages and probably
/// shutdown the connection. This value can be overridden via the `Config` option.
pub const DEFAULT_MAX_ALLOWED_MSG_SIZE: usize = 500 * 1024 * 1024; // 500MiB

/// In the absence of a port supplied by the user via the config we will first try using this
/// before using a random port.
pub const DEFAULT_PORT_TO_TRY: u16 = 12000;

/// Message received from a peer
pub enum Message {
    /// A message sent by peer on a uni-directional stream
    UniStream {
        /// Message's bytes
        bytes: Bytes,
        /// Address the message was sent from
        src: SocketAddr,
    },
    /// A message sent by peer on a bi-directional stream
    BiStream {
        /// Message's bytes
        bytes: Bytes,
        /// Address the message was sent from
        src: SocketAddr,
        /// Stream to send a message back to the initiator
        send: SendStream,
    },
}

/// Host name of the Quic communication certificate used by peers
// TODO: make it configurable
const CERT_SERVER_NAME: &str = "MaidSAFE.net";

/// Main QuicP2p instance to communicate with QuicP2p using an async API
#[derive(Clone)]
pub struct QuicP2p {
    local_addr: SocketAddr,
    allow_random_port: bool,
    max_msg_size: usize,
    bootstrap_cache: BootstrapCache,
    endpoint_cfg: quinn::ServerConfig,
    client_cfg: quinn::ClientConfig,
}

impl QuicP2p {
    /// Construct `QuicP2p` with the default config and bootstrap cache enabled
    pub fn new() -> Result<Self> {
        Self::with_config(None, Default::default(), true)
    }

    /// Construct `QuicP2p` with supplied parameters, ready to be used.
    /// If config is not specified it'll call `Config::read_or_construct_default()`
    ///
    /// `bootstrap_nodes`: takes bootstrap nodes from the user.
    ///
    /// In addition to bootstrap nodes provided, optionally use the nodes found
    /// in the bootstrap cache file (if such a file exists) or disable this feature.
    pub fn with_config(
        cfg: Option<Config>,
        bootstrap_nodes: VecDeque<SocketAddr>,
        use_bootstrap_cache: bool,
    ) -> Result<Self> {
        let cfg = unwrap_config_or_default(cfg)?;
        info!("Config passed in to QP2P: {:?}", cfg);

        let (port, allow_random_port) = cfg
            .port
            .map(|p| (p, false))
            .unwrap_or((DEFAULT_PORT_TO_TRY, true));

        let ip = cfg.ip.unwrap_or_else(|| IpAddr::V4(Ipv4Addr::UNSPECIFIED));

        let max_msg_size = cfg
            .max_msg_size_allowed
            .map(|size| size as usize)
            .unwrap_or(DEFAULT_MAX_ALLOWED_MSG_SIZE);

        let idle_timeout_msec = cfg.idle_timeout_msec.unwrap_or(DEFAULT_IDLE_TIMEOUT_MSEC);

        let keep_alive_interval_msec = cfg
            .keep_alive_interval_msec
            .unwrap_or(DEFAULT_KEEP_ALIVE_INTERVAL_MSEC);

        let (key, cert) = {
            let our_complete_cert: SerialisableCertificate = Default::default();
            our_complete_cert.obtain_priv_key_and_cert()?
        };

        let custom_dirs = cfg
            .bootstrap_cache_dir
            .clone()
            .map(|custom_dir| Dirs::Overide(OverRide::new(&custom_dir)));

        let mut bootstrap_cache =
            BootstrapCache::new(cfg.hard_coded_contacts, custom_dirs.as_ref())?;
        if use_bootstrap_cache {
            bootstrap_cache
                .peers_mut()
                .extend(bootstrap_nodes.into_iter());
        } else {
            let _ = mem::replace(bootstrap_cache.peers_mut(), bootstrap_nodes);
        }

        let endpoint_cfg =
            peer_config::new_our_cfg(idle_timeout_msec, keep_alive_interval_msec, cert, key)?;

        let client_cfg = peer_config::new_client_cfg(idle_timeout_msec, keep_alive_interval_msec);

        let quic_p2p = Self {
            local_addr: SocketAddr::new(ip, port),
            allow_random_port,
            max_msg_size,
            bootstrap_cache,
            endpoint_cfg,
            client_cfg,
        };

        Ok(quic_p2p)
    }

    /// Bootstrap to the network.
    ///
    /// Bootstrap concept is different from "connect" in several ways: `bootstrap()` will try to
    /// connect to all peers which are specified in the config (`hard_coded_contacts`) or were
    /// previously cached.
    /// Once a connection with a peer succeeds, a `Connection` for such peer will be returned
    /// and all other connections will be dropped.
    pub async fn bootstrap(&mut self) -> Result<Connection> {
        // TODO: refactor bootstrap_cache so we can simply get the list of nodes
        let bootstrap_nodes: Vec<SocketAddr> = self
            .bootstrap_cache
            .peers()
            .iter()
            .rev()
            .chain(self.bootstrap_cache.hard_coded_contacts().iter())
            .cloned()
            .collect();

        trace!("Bootstrapping with nodes {:?}", bootstrap_nodes);
        // Attempt to connect to all nodes and return the first one to succeed
        let mut tasks = Vec::default();
        for node_addr in bootstrap_nodes {
            let endpoint_cfg = self.endpoint_cfg.clone();
            let client_cfg = self.client_cfg.clone();
            let max_msg_size = self.max_msg_size;
            let local_addr = self.local_addr;
            let allow_random_port = self.allow_random_port;
            let task_handle = tokio::spawn(async move {
                new_connection_to(
                    &node_addr,
                    endpoint_cfg,
                    client_cfg,
                    max_msg_size,
                    local_addr,
                    allow_random_port,
                )
                .await
            });
            tasks.push(task_handle);
        }

        let (conn_info, _) = select_ok(tasks).await.map_err(|err| {
            error!("Failed to botstrap to the network: {}", err);
            Error::BootstrapFailure
        })?;

        let (connection, addr) = conn_info?;
        self.local_addr = addr;

        Ok(connection)
    }

    /// Connect to the given peer and return a `Connection` object if it succeeds,
    /// which can then be used to send messages to the connected peer.
    pub async fn connect_to(&mut self, node_addr: &SocketAddr) -> Result<Connection> {
        let (connection, addr) = new_connection_to(
            node_addr,
            self.endpoint_cfg.clone(),
            self.client_cfg.clone(),
            self.max_msg_size,
            self.local_addr,
            self.allow_random_port,
        )
        .await?;

        Ok(connection)
    }

    /// Obtain stream of incoming QUIC connections
    pub fn listen(&self) -> Result<IncomingConnections> {
        let (_, quinn_incoming) = bind(
            self.endpoint_cfg.clone(),
            self.local_addr,
            self.allow_random_port,
        )?;
        IncomingConnections::new(quinn_incoming, self.max_msg_size)
    }

    /// Get our connection adddress to give to others for them to connect to us.
    ///
    /// Attempts to use UPnP to automatically find the public endpoint and forward a port.
    /// Will use hard coded contacts to ask for our endpoint. If no contact is given then we'll
    /// simply build our connection info by querying the underlying bound socket for our address.
    /// Note that if such an obtained address is of unspecified category we will ignore that as
    /// such an address cannot be reached and hence not useful.
    #[cfg(feature = "upnp")]
    pub fn our_endpoint(&self) -> Result<SocketAddr> {
        // TODO: make use of IGD and echo services
        Ok(self.local_addr)
    }
}

// Creates a new Connection
async fn new_connection_to(
    node_addr: &SocketAddr,
    endpoint_cfg: quinn::ServerConfig,
    client_cfg: quinn::ClientConfig,
    max_msg_size: usize,
    local_addr: SocketAddr,
    allow_random_port: bool,
) -> Result<(Connection, SocketAddr)> {
    trace!("Attempting to connect to peer: {}", node_addr);
    let (quinn_endpoint, _) = bind(endpoint_cfg, local_addr, allow_random_port)?;

    let quinn_connecting = quinn_endpoint.connect_with(client_cfg, &node_addr, CERT_SERVER_NAME)?;

    let quinn::NewConnection {
        connection: quic_conn,
        ..
    } = quinn_connecting.await?;

    trace!("Successfully connected to peer: {}", node_addr);

    Ok((
        Connection::new(quic_conn, max_msg_size).await?,
        quinn_endpoint.local_addr()?,
    ))
}

// Bind a new socket with a local address
fn bind(
    endpoint_cfg: quinn::ServerConfig,
    local_addr: SocketAddr,
    allow_random_port: bool,
) -> Result<(quinn::Endpoint, quinn::Incoming)> {
    let mut endpoint_builder = quinn::Endpoint::builder();
    let _ = endpoint_builder.listen(endpoint_cfg);

    match UdpSocket::bind(&local_addr) {
        Ok(udp) => endpoint_builder.with_socket(udp).map_err(Error::Endpoint),
        Err(err) if allow_random_port => {
            info!(
                "Failed to bind to port: {} - Error: {}. Trying random port instead.",
                DEFAULT_PORT_TO_TRY, err
            );
            let bind_addr = SocketAddr::new(local_addr.ip(), 0);

            endpoint_builder.bind(&bind_addr).map_err(|e| {
                error!("Failed to bind to random port {:?}", e);
                Error::Endpoint(e)
            })
        }
        Err(err) => Err(Error::Configuration {
            e: format!(
                "Could not bind to the user supplied port: {}! Error: {}",
                local_addr.port(),
                err
            ),
        }),
    }
}

// Private helpers

// Unwrap the conffig if provided by the user, otherwise construct the default one
#[cfg(not(feature = "upnp"))]
fn unwrap_config_or_default(cfg: Option<Config>) -> Result<Config> {
    cfg.map_or(Config::read_or_construct_default(None), |cfg| Ok(cfg))
}

#[cfg(feature = "upnp")]
fn unwrap_config_or_default(cfg: Option<Config>) -> Result<Config> {
    let mut cfg = cfg.map_or(Config::read_or_construct_default(None)?, |cfg| cfg);
    if cfg.ip.is_none() {
        cfg.ip = igd::get_local_ip().ok();
    };

    Ok(cfg)
}
