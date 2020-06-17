use std::cmp::Ordering;
use std::collections::{hash_map::Entry, BinaryHeap, HashMap, HashSet};
use std::fmt;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use bitcoin::BlockHash;

use crate::chain::Network;
use crate::electrum::{Client, Hostname, Port, ProtocolVersion, ServerFeatures};
use crate::errors::{Result, ResultExt};
use crate::util::spawn_thread;

mod default_servers;
use default_servers::add_default_servers;

const HEALTH_CHECK_FREQ: Duration = Duration::from_secs(3600); // check servers every hour
const JOB_INTERVAL: Duration = Duration::from_secs(1); // run one health check job every second
const MAX_CONSECUTIVE_FAILURES: usize = 24; // drop servers after 24 consecutive failing attempts (~24 hours) (~24 hours)
const MAX_QUEUE_SIZE: usize = 500; // refuse accepting new servers if we have that many health check jobs
const MAX_SERVERS_PER_REQUEST: usize = 3; // maximum number of server hosts added per server.add_peer call
const MAX_SERVICES_PER_REQUEST: usize = 6; // maximum number of services added per server.add_peer call

#[derive(Default, Debug)]
pub struct DiscoveryManager {
    /// A queue of scheduled health check jobs, including for healthy, unhealthy and untested servers
    queue: RwLock<BinaryHeap<HealthCheck>>,

    /// A list of servers that were found to be healthy on their last health check
    healthy: RwLock<HashMap<ServerAddr, Server>>,

    /// Used to test for compatibility
    our_genesis_hash: BlockHash,
    our_version: ProtocolVersion,

    /// Optional, will not support onion hosts without this
    tor_proxy: Option<SocketAddr>,
}

/// A Server corresponds to a single IP address or onion hostname, with one or more services
/// exposed on different ports.
#[derive(Debug)]
struct Server {
    services: HashSet<Service>,
    hostname: Hostname,
    features: ServerFeatures,
    // the `ServerAddr` isn't kept here directly, but is also available next to `Server` as the key for
    // the `healthy` field on `DiscoveryManager`
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
enum ServerAddr {
    Clearnet(IpAddr),
    Onion(Hostname),
}

#[derive(Eq, PartialEq, Hash, Copy, Clone, Debug)]
pub enum Service {
    Tcp(Port),
    Ssl(Port),
    // unimplemented: Ws and Wss
}

/// A queued health check job, one per service/port (and not per server)
#[derive(Eq, Debug)]
struct HealthCheck {
    addr: ServerAddr,
    hostname: Hostname,
    service: Service,
    is_default: bool,
    added_by: Option<IpAddr>,
    last_check: Option<Instant>,
    last_healthy: Option<Instant>,
    consecutive_failures: usize,
}

/// The server entry format returned from server.peers.subscribe
#[derive(Serialize)]
pub struct ServerEntry(ServerAddr, Hostname, Vec<String>);

impl DiscoveryManager {
    pub fn new(
        our_network: Network,
        our_version: ProtocolVersion,
        tor_proxy: Option<SocketAddr>,
    ) -> Self {
        let discovery = Self {
            our_genesis_hash: our_network.genesis_hash(),
            our_version,
            tor_proxy,
            ..Default::default()
        };
        add_default_servers(&discovery, our_network);
        discovery
    }

    /// Add a server requested via `server.add_peer`
    pub fn add_server_request(&self, added_by: IpAddr, features: ServerFeatures) -> Result<()> {
        self.verify_compatibility(&features)?;

        let mut queue = self.queue.write().unwrap();
        ensure!(queue.len() < MAX_QUEUE_SIZE, "queue size exceeded");

        // TODO optimize
        let mut existing_services: HashMap<ServerAddr, HashSet<Service>> = HashMap::new();
        for health_check in queue.iter() {
            existing_services
                .entry(health_check.addr.clone())
                .or_default()
                .insert(health_check.service);
        }

        // collect HealthChecks for candidate services
        let jobs = features
            .hosts
            .iter()
            .take(MAX_SERVERS_PER_REQUEST)
            .filter_map(|(hostname, ports)| {
                let hostname = hostname.to_lowercase();

                if hostname.len() > 100 {
                    warn!("skipping invalid hostname");
                    return None;
                }
                let addr = match ServerAddr::resolve(&hostname) {
                    Ok(addr) => addr,
                    Err(e) => {
                        warn!("failed resolving {}: {:?}", hostname, e);
                        return None;
                    }
                };
                // ensure the server address matches the ip that advertised it to us.
                // onion hosts are exempt.
                if let ServerAddr::Clearnet(ip) = addr {
                    if ip != added_by {
                        warn!(
                            "server ip does not match source ip ({}, {} != {})",
                            hostname, ip, added_by
                        );
                        return None;
                    }
                }
                Some((addr, hostname, ports))
            })
            .flat_map(|(addr, hostname, ports)| {
                let tcp_service = ports.tcp_port.into_iter().map(Service::Tcp);
                let ssl_service = ports.ssl_port.into_iter().map(Service::Ssl);
                let services = tcp_service.chain(ssl_service).collect::<HashSet<Service>>();

                services
                    .into_iter()
                    .filter(|service| {
                        existing_services
                            .get(&addr)
                            .map_or(true, |s| !s.contains(service))
                    })
                    .map(|service| {
                        HealthCheck::new(addr.clone(), hostname.clone(), service, Some(added_by))
                    })
                    .collect::<Vec<_>>()
            })
            .take(MAX_SERVICES_PER_REQUEST)
            .collect::<Vec<_>>();

        ensure!(!jobs.is_empty(), "no new valid entries");

        ensure!(
            queue.len() + jobs.len() <= MAX_QUEUE_SIZE,
            "queue size exceeded"
        );

        queue.extend(jobs);
        Ok(())
    }

    /// Add a default server. Default servers are exempt from limits and given more leniency
    /// before being removed due to unavailability.
    pub fn add_default_server(&self, hostname: Hostname, services: Vec<Service>) -> Result<()> {
        let addr = ServerAddr::resolve(&hostname)?;
        let mut queue = self.queue.write().unwrap();
        queue.extend(
            services
                .into_iter()
                .map(|service| HealthCheck::new(addr.clone(), hostname.clone(), service, None)),
        );
        Ok(())
    }

    /// Get the list of healthy servers formatted for `servers.peers.subscribe`
    pub fn get_servers(&self) -> Vec<ServerEntry> {
        // XXX return a random sample instead of everything?
        self.healthy
            .read()
            .unwrap()
            .iter()
            .map(|(addr, server)| {
                ServerEntry(addr.clone(), server.hostname.clone(), server.feature_strs())
            })
            .collect()
    }

    /// Run the next health check in the queue (a single one)
    fn run_health_check(&self) -> Result<()> {
        // abort if there are no entries in the queue, or its still too early for the next one up
        if self.queue.read().unwrap().peek().map_or(true, |next| {
            next.last_check
                .map_or(false, |t| t.elapsed() < HEALTH_CHECK_FREQ)
        }) {
            return Ok(());
        }

        let mut health_check = self.queue.write().unwrap().pop().unwrap();
        debug!("processing {:?}", health_check);

        let was_healthy = health_check.is_healthy();

        match self.check_server(
            &health_check.addr,
            &health_check.hostname,
            health_check.service,
        ) {
            Ok(features) => {
                debug!(
                    "{} {:?} is available",
                    health_check.hostname, health_check.service
                );

                if !was_healthy {
                    self.save_healthy_service(&health_check, features);
                }
                // XXX update features?

                health_check.last_check = Some(Instant::now());
                health_check.last_healthy = health_check.last_check;
                health_check.consecutive_failures = 0;
                // schedule the next health check
                self.queue.write().unwrap().push(health_check);

                Ok(())
            }
            Err(e) => {
                debug!(
                    "{} {:?} is unavailable: {:?}",
                    health_check.hostname, health_check.service, e
                );

                if was_healthy {
                    // XXX should we assume the server's other services are down too?
                    self.remove_unhealthy_service(&health_check);
                }

                health_check.last_check = Some(Instant::now());
                health_check.consecutive_failures += 1;

                if health_check.should_retry() {
                    self.queue.write().unwrap().push(health_check);
                } else {
                    debug!("giving up on {:?}", health_check);
                }

                Err(e)
            }
        }
    }

    /// Upsert the server/service into the healthy set
    fn save_healthy_service(&self, health_check: &HealthCheck, features: ServerFeatures) {
        let addr = health_check.addr.clone();
        let mut healthy = self.healthy.write().unwrap();
        assert!(healthy
            .entry(addr)
            .or_insert_with(|| Server::new(health_check.hostname.clone(), features))
            .services
            .insert(health_check.service));
    }

    /// Remove the service, and remove the server entirely if it has no other reamining healthy services
    fn remove_unhealthy_service(&self, health_check: &HealthCheck) {
        let addr = health_check.addr.clone();
        let mut healthy = self.healthy.write().unwrap();
        if let Entry::Occupied(mut entry) = healthy.entry(addr) {
            let server = entry.get_mut();
            assert!(server.services.remove(&health_check.service));
            if server.services.is_empty() {
                entry.remove_entry();
            }
        } else {
            unreachable!("missing expected server, corrupted state");
        }
    }

    fn check_server(
        &self,
        addr: &ServerAddr,
        hostname: &Hostname,
        service: Service,
    ) -> Result<ServerFeatures> {
        debug!("checking service {:?} {:?}", addr, service);

        let mut client: Client = match (addr, service) {
            (ServerAddr::Clearnet(ip), Service::Tcp(port)) => Client::new((*ip, port))?,
            (ServerAddr::Clearnet(_), Service::Ssl(port)) => Client::new_ssl((hostname, port))?,
            (ServerAddr::Onion(hostname), Service::Tcp(port)) => {
                let tor_proxy = self
                    .tor_proxy
                    .chain_err(|| "no tor proxy configured, onion hosts are unsupported")?;
                Client::new_proxy((hostname, port), tor_proxy)?
            }
            (ServerAddr::Onion(_), Service::Ssl(_)) => bail!("ssl over onion is unsupported"),
        };

        let features = client.server_features()?;
        self.verify_compatibility(&features)?;

        // TODO register ourselves with add_peer
        // XXX should we require this to succeed?
        //ensure!(client.add_peer(self.our_features)?, "server does not reciprocate");

        Ok(features)
    }

    fn verify_compatibility(&self, features: &ServerFeatures) -> Result<()> {
        ensure!(
            features.genesis_hash == self.our_genesis_hash,
            "incompatible networks"
        );

        ensure!(
            features.protocol_min <= self.our_version && features.protocol_max >= self.our_version,
            "incompatible protocol versions"
        );

        ensure!(
            features.hash_function == "sha256",
            "incompatible hash function"
        );

        Ok(())
    }

    pub fn spawn_jobs_thread(manager: Arc<DiscoveryManager>) {
        spawn_thread("discovery-jobs", move || loop {
            if let Err(e) = manager.run_health_check() {
                debug!("health check failed: {:?}", e);
            }
            // XXX use a dynamic JOB_INTERVAL, adjusted according to the queue size and HEALTH_CHECK_FREQ?
            thread::sleep(JOB_INTERVAL);
        });
    }
}

impl Server {
    fn new(hostname: Hostname, features: ServerFeatures) -> Self {
        Server {
            hostname,
            features,
            services: HashSet::new(),
        }
    }

    /// Get server features and services in the compact string array format used for `servers.peers.subscribe`
    fn feature_strs(&self) -> Vec<String> {
        let mut strs = Vec::with_capacity(self.services.len() + 1);
        strs.push(format!("v{}", self.features.protocol_max));
        if let Some(pruning) = self.features.pruning {
            strs.push(format!("p{}", pruning));
        }
        strs.extend(self.services.iter().map(|s| s.to_string()));
        strs
    }
}

impl ServerAddr {
    fn resolve(host: &str) -> Result<Self> {
        Ok(if host.ends_with(".onion") {
            ServerAddr::Onion(host.into())
        } else if let Ok(ip) = IpAddr::from_str(host) {
            ServerAddr::Clearnet(ip)
        } else {
            let ip = format!("{}:1", host)
                .to_socket_addrs()
                .chain_err(|| "hostname resolution failed")?
                .next()
                .chain_err(|| "hostname resolution failed")?
                .ip();
            ServerAddr::Clearnet(ip)
        })
    }
}

impl fmt::Display for ServerAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerAddr::Clearnet(ip) => write!(f, "{}", ip),
            ServerAddr::Onion(hostname) => write!(f, "{}", hostname),
        }
    }
}

impl serde::Serialize for ServerAddr {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl HealthCheck {
    fn new(
        addr: ServerAddr,
        hostname: Hostname,
        service: Service,
        added_by: Option<IpAddr>,
    ) -> Self {
        HealthCheck {
            addr,
            hostname,
            service,
            is_default: added_by.is_none(),
            added_by,
            last_check: None,
            last_healthy: None,
            consecutive_failures: 0,
        }
    }

    fn is_healthy(&self) -> bool {
        match (self.last_check, self.last_healthy) {
            (Some(last_check), Some(last_healthy)) => last_check == last_healthy,
            _ => false,
        }
    }

    // allow the server to fail up to MAX_CONSECTIVE_FAILURES time before giving up on it.
    // if its a non-default server and the very first attempt fails, give up immediatly.
    fn should_retry(&self) -> bool {
        (self.last_healthy.is_some() || self.is_default)
            && self.consecutive_failures < MAX_CONSECUTIVE_FAILURES
    }
}

impl PartialEq for HealthCheck {
    fn eq(&self, other: &Self) -> bool {
        self.hostname == other.hostname && self.service == other.service
    }
}

impl Ord for HealthCheck {
    fn cmp(&self, other: &Self) -> Ordering {
        self.last_check.cmp(&other.last_check).reverse()
    }
}

impl PartialOrd for HealthCheck {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Service::Tcp(port) => write!(f, "t{}", port),
            Service::Ssl(port) => write!(f, "s{}", port),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::Network;
    use std::time;

    #[test]
    fn test() -> Result<()> {
        stderrlog::new().verbosity(4).init().unwrap();

        let discovery = DiscoveryManager::new(
            Network::Testnet,
            "1.4".parse().unwrap(),
            Some("127.0.0.1:9150".parse().unwrap()),
        );

        discovery.add_default_server(
            "electrum.blockstream.info".into(),
            vec![Service::Tcp(60001)],
        );
        discovery.add_default_server("testnet.hsmiths.com".into(), vec![Service::Ssl(53012)]);
        discovery.add_default_server(
            "tn.not.fyi".into(),
            vec![Service::Tcp(55001), Service::Ssl(55002)],
        );
        discovery.add_default_server(
            "electrum.blockstream.info".into(),
            vec![Service::Tcp(60001), Service::Ssl(60002)],
        );
        discovery.add_default_server(
            "explorerzydxu5ecjrkwceayqybizmpjjznk5izmitf2modhcusuqlid.onion".into(),
            vec![Service::Tcp(143)],
        );

        debug!("{:#?}", discovery);

        for _ in 0..12 {
            discovery
                .run_health_check()
                .map_err(|e| warn!("{:?}", e))
                .ok();
            thread::sleep(time::Duration::from_secs(1));
        }

        debug!("{:#?}", discovery);

        info!("{}", json!(discovery.get_servers()));

        Ok(())
    }
}