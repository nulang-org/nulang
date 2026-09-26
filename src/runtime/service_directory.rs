use super::cluster::NodeId;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ServiceProtocol {
    Nul0,
    Http,
    Grpc,
    Tcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceHealth {
    Serving,
    Draining,
    Unhealthy,
}

impl ServiceHealth {
    pub fn is_routable(self) -> bool {
        self == Self::Serving
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceAdvertisement {
    pub node_id: NodeId,
    pub service: String,
    pub deployment_id: String,
    pub replica: u32,
    pub allocation_epoch: u64,
    pub host: String,
    pub port: u16,
    pub protocol: ServiceProtocol,
    pub health: ServiceHealth,
}

impl ServiceAdvertisement {
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier("service name", &self.service)?;
        validate_identifier("deployment id", &self.deployment_id)?;
        if self.allocation_epoch == 0 {
            return Err("service allocation epoch must be greater than zero".into());
        }
        if self.host.trim().is_empty() {
            return Err("service host must not be empty".into());
        }
        if self.port == 0 {
            return Err("service port must be greater than zero".into());
        }
        Ok(())
    }

    fn same_endpoint_identity(&self, other: &Self) -> bool {
        self.node_id == other.node_id
            && self.service == other.service
            && self.deployment_id == other.deployment_id
            && self.replica == other.replica
            && self.allocation_epoch == other.allocation_epoch
            && self.host == other.host
            && self.port == other.port
            && self.protocol == other.protocol
    }

    fn same_workload_identity(&self, other: &Self) -> bool {
        self.node_id == other.node_id
            && self.deployment_id == other.deployment_id
            && self.replica == other.replica
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceAdvertisementSnapshot {
    pub node_id: NodeId,
    pub generation: u64,
    pub services: Vec<ServiceAdvertisement>,
}

#[derive(Default)]
pub(crate) struct ServiceDirectory {
    advertisements: Vec<ServiceAdvertisement>,
    remote_generations: HashMap<NodeId, u64>,
    // Highest allocation epoch ever accepted for one node/deployment/replica
    // during the node's current lifetime. Endpoint withdrawal must not erase
    // this fence or an old workload could later resurrect through service
    // discovery. Confirmed node removal clears the node's watermarks so a
    // genuine same-NodeId restart can begin a fresh lifetime.
    allocation_epochs: HashMap<(NodeId, String, u32), u64>,
}

impl ServiceDirectory {
    pub(crate) fn upsert_local(
        &mut self,
        advertisement: ServiceAdvertisement,
    ) -> Result<bool, String> {
        advertisement.validate()?;

        let allocation_key = (
            advertisement.node_id,
            advertisement.deployment_id.clone(),
            advertisement.replica,
        );
        let max_epoch = self
            .allocation_epochs
            .get(&allocation_key)
            .copied()
            .unwrap_or(0);

        if advertisement.allocation_epoch < max_epoch {
            return Err(format!(
                "stale service advertisement for {} replica {} epoch {}; current epoch is {}",
                advertisement.deployment_id,
                advertisement.replica,
                advertisement.allocation_epoch,
                max_epoch
            ));
        }

        if advertisement.allocation_epoch > max_epoch {
            self.advertisements.retain(|existing| {
                !existing.same_workload_identity(&advertisement)
                    || existing.allocation_epoch >= advertisement.allocation_epoch
            });
            self.allocation_epochs
                .insert(allocation_key, advertisement.allocation_epoch);
        } else if max_epoch == 0 {
            self.allocation_epochs
                .insert(allocation_key, advertisement.allocation_epoch);
        }

        if let Some(existing) = self
            .advertisements
            .iter_mut()
            .find(|existing| existing.same_endpoint_identity(&advertisement))
        {
            if existing.health == advertisement.health {
                return Ok(false);
            }
            existing.health = advertisement.health;
            return Ok(true);
        }

        self.advertisements.push(advertisement);
        Ok(true)
    }

    pub(crate) fn remove_local_allocation(
        &mut self,
        node_id: NodeId,
        deployment_id: &str,
        replica: u32,
        allocation_epoch: u64,
    ) -> usize {
        let before = self.advertisements.len();
        self.advertisements.retain(|advertisement| {
            advertisement.node_id != node_id
                || advertisement.deployment_id != deployment_id
                || advertisement.replica != replica
                || advertisement.allocation_epoch != allocation_epoch
        });
        before - self.advertisements.len()
    }

    pub(crate) fn local_snapshot(
        &self,
        node_id: NodeId,
        generation: u64,
        limit: usize,
    ) -> Result<ServiceAdvertisementSnapshot, String> {
        let services: Vec<_> = self
            .advertisements
            .iter()
            .filter(|advertisement| advertisement.node_id == node_id)
            .cloned()
            .collect();

        if services.len() > limit {
            return Err(format!(
                "service directory snapshot has {} local endpoints, exceeding limit {}; refusing partial advertisement",
                services.len(),
                limit
            ));
        }

        Ok(ServiceAdvertisementSnapshot {
            node_id,
            generation,
            services,
        })
    }

    pub(crate) fn remote_generation(&self, node_id: NodeId) -> Option<u64> {
        self.remote_generations.get(&node_id).copied()
    }

    pub(crate) fn replace_remote_node(
        &mut self,
        snapshot: ServiceAdvertisementSnapshot,
    ) -> Result<usize, String> {
        if self
            .remote_generations
            .get(&snapshot.node_id)
            .is_some_and(|generation| snapshot.generation <= *generation)
        {
            return Ok(0);
        }

        validate_snapshot(&snapshot)?;

        // A newer metadata generation is not permission to move allocation
        // ownership backwards. Keep an epoch watermark independently of the
        // visible endpoint set so omission/withdrawal cannot erase fencing.
        for advertisement in &snapshot.services {
            let key = (
                snapshot.node_id,
                advertisement.deployment_id.clone(),
                advertisement.replica,
            );
            if let Some(current) = self.allocation_epochs.get(&key) {
                if advertisement.allocation_epoch < *current {
                    return Err(format!(
                        "stale service snapshot for {} replica {} epoch {}; current epoch is {}",
                        advertisement.deployment_id,
                        advertisement.replica,
                        advertisement.allocation_epoch,
                        current
                    ));
                }
            }
        }

        let before = self.advertisements.len();
        self.advertisements
            .retain(|advertisement| advertisement.node_id != snapshot.node_id);
        let removed = before - self.advertisements.len();
        let inserted = snapshot.services.len();
        for advertisement in &snapshot.services {
            let key = (
                snapshot.node_id,
                advertisement.deployment_id.clone(),
                advertisement.replica,
            );
            self.allocation_epochs
                .entry(key)
                .and_modify(|current| *current = (*current).max(advertisement.allocation_epoch))
                .or_insert(advertisement.allocation_epoch);
        }
        self.advertisements.extend(snapshot.services);
        self.remote_generations
            .insert(snapshot.node_id, snapshot.generation);

        Ok(removed + inserted)
    }

    pub(crate) fn remove_remote_node(&mut self, node_id: NodeId) -> (usize, bool) {
        let before = self.advertisements.len();
        self.advertisements
            .retain(|advertisement| advertisement.node_id != node_id);
        let generation_removed = self.remote_generations.remove(&node_id).is_some();
        self.allocation_epochs
            .retain(|(owner, _, _), _| *owner != node_id);
        (before - self.advertisements.len(), generation_removed)
    }

    pub(crate) fn resolve(&self, service: &str) -> Result<Vec<ServiceAdvertisement>, String> {
        validate_identifier("service name", service)?;
        let mut endpoints: Vec<_> = self
            .advertisements
            .iter()
            .filter(|advertisement| {
                advertisement.service == service && advertisement.health.is_routable()
            })
            .cloned()
            .collect();

        endpoints.sort_by(|left, right| {
            left.node_id
                .0
                .cmp(&right.node_id.0)
                .then_with(|| left.deployment_id.cmp(&right.deployment_id))
                .then_with(|| left.replica.cmp(&right.replica))
                .then_with(|| left.allocation_epoch.cmp(&right.allocation_epoch))
                .then_with(|| left.protocol.cmp(&right.protocol))
                .then_with(|| left.host.cmp(&right.host))
                .then_with(|| left.port.cmp(&right.port))
        });
        Ok(endpoints)
    }

    pub(crate) fn len(&self) -> usize {
        self.advertisements.len()
    }

    pub(crate) fn remote_len(&self, local_node_id: Option<NodeId>) -> usize {
        self.advertisements
            .iter()
            .filter(|advertisement| Some(advertisement.node_id) != local_node_id)
            .count()
    }
}

fn validate_identifier(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.trim() != value {
        return Err(format!(
            "{label} must be non-empty and must not have surrounding whitespace"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{label} must not contain control characters"));
    }
    Ok(())
}

fn validate_snapshot(snapshot: &ServiceAdvertisementSnapshot) -> Result<(), String> {
    let mut endpoint_keys = HashSet::new();
    let mut allocation_epochs = HashMap::<(String, u32), u64>::new();

    for advertisement in &snapshot.services {
        advertisement.validate()?;
        if advertisement.node_id != snapshot.node_id {
            return Err(format!(
                "service advertisement node mismatch: snapshot owner {:?}, endpoint claims {:?}",
                snapshot.node_id, advertisement.node_id
            ));
        }

        let endpoint_key = (
            advertisement.service.clone(),
            advertisement.deployment_id.clone(),
            advertisement.replica,
            advertisement.allocation_epoch,
            advertisement.host.clone(),
            advertisement.port,
            advertisement.protocol,
        );
        if !endpoint_keys.insert(endpoint_key) {
            return Err(format!(
                "duplicate service endpoint in snapshot for {} replica {} epoch {}",
                advertisement.deployment_id, advertisement.replica, advertisement.allocation_epoch
            ));
        }

        let allocation_key = (advertisement.deployment_id.clone(), advertisement.replica);
        if let Some(existing_epoch) = allocation_epochs.get(&allocation_key) {
            if *existing_epoch != advertisement.allocation_epoch {
                return Err(format!(
                    "service snapshot advertises multiple epochs for {} replica {}: {} and {}",
                    advertisement.deployment_id,
                    advertisement.replica,
                    existing_epoch,
                    advertisement.allocation_epoch
                ));
            }
        } else {
            allocation_epochs.insert(allocation_key, advertisement.allocation_epoch);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(
        node: u64,
        service: &str,
        deployment: &str,
        replica: u32,
        epoch: u64,
        port: u16,
        health: ServiceHealth,
    ) -> ServiceAdvertisement {
        ServiceAdvertisement {
            node_id: NodeId(node),
            service: service.into(),
            deployment_id: deployment.into(),
            replica,
            allocation_epoch: epoch,
            host: format!("10.0.0.{node}"),
            port,
            protocol: ServiceProtocol::Http,
            health,
        }
    }

    #[test]
    fn local_health_update_does_not_duplicate_endpoint() {
        let mut directory = ServiceDirectory::default();
        let serving = endpoint(1, "api", "api-deploy", 0, 1, 8080, ServiceHealth::Serving);
        assert!(directory.upsert_local(serving.clone()).unwrap());
        assert!(!directory.upsert_local(serving.clone()).unwrap());

        let mut unhealthy = serving;
        unhealthy.health = ServiceHealth::Unhealthy;
        assert!(directory.upsert_local(unhealthy).unwrap());

        assert_eq!(directory.len(), 1);
        assert!(directory.resolve("api").unwrap().is_empty());
    }

    #[test]
    fn newer_local_epoch_replaces_older_and_stale_epoch_is_rejected() {
        let mut directory = ServiceDirectory::default();
        let first = endpoint(1, "api", "api-deploy", 0, 3, 8080, ServiceHealth::Serving);
        let newer = endpoint(1, "api", "api-deploy", 0, 4, 8080, ServiceHealth::Serving);

        directory.upsert_local(first.clone()).unwrap();
        directory.upsert_local(newer.clone()).unwrap();
        assert_eq!(directory.resolve("api").unwrap(), vec![newer]);

        assert!(directory.upsert_local(first).is_err());
    }

    #[test]
    fn remote_generation_rejects_stale_snapshot_and_newer_snapshot_replaces() {
        let mut directory = ServiceDirectory::default();
        let first = ServiceAdvertisementSnapshot {
            node_id: NodeId(7),
            generation: 2,
            services: vec![endpoint(
                7,
                "api",
                "api-deploy",
                0,
                1,
                8080,
                ServiceHealth::Serving,
            )],
        };
        directory.replace_remote_node(first).unwrap();

        let stale = ServiceAdvertisementSnapshot {
            node_id: NodeId(7),
            generation: 1,
            services: vec![endpoint(
                7,
                "api",
                "api-deploy",
                0,
                2,
                9090,
                ServiceHealth::Serving,
            )],
        };
        assert_eq!(directory.replace_remote_node(stale).unwrap(), 0);
        assert_eq!(directory.resolve("api").unwrap()[0].port, 8080);

        let newer = ServiceAdvertisementSnapshot {
            node_id: NodeId(7),
            generation: 3,
            services: vec![endpoint(
                7,
                "api",
                "api-deploy",
                0,
                2,
                9090,
                ServiceHealth::Serving,
            )],
        };
        directory.replace_remote_node(newer).unwrap();
        let resolved = directory.resolve("api").unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].allocation_epoch, 2);
        assert_eq!(resolved[0].port, 9090);
    }

    #[test]
    fn withdrawn_local_endpoint_cannot_resurrect_at_lower_epoch() {
        let mut directory = ServiceDirectory::default();
        directory
            .upsert_local(endpoint(
                1,
                "api",
                "api-deploy",
                0,
                5,
                8080,
                ServiceHealth::Serving,
            ))
            .unwrap();
        assert_eq!(
            directory.remove_local_allocation(NodeId(1), "api-deploy", 0, 5),
            1
        );
        assert!(directory
            .upsert_local(endpoint(
                1,
                "api",
                "api-deploy",
                0,
                4,
                8080,
                ServiceHealth::Serving,
            ))
            .is_err());
    }

    #[test]
    fn newer_remote_generation_cannot_regress_allocation_epoch_after_omission() {
        let mut directory = ServiceDirectory::default();
        directory
            .replace_remote_node(ServiceAdvertisementSnapshot {
                node_id: NodeId(7),
                generation: 1,
                services: vec![endpoint(
                    7,
                    "api",
                    "api-deploy",
                    0,
                    5,
                    8080,
                    ServiceHealth::Serving,
                )],
            })
            .unwrap();
        directory
            .replace_remote_node(ServiceAdvertisementSnapshot {
                node_id: NodeId(7),
                generation: 2,
                services: Vec::new(),
            })
            .unwrap();

        assert!(directory
            .replace_remote_node(ServiceAdvertisementSnapshot {
                node_id: NodeId(7),
                generation: 3,
                services: vec![endpoint(
                    7,
                    "api",
                    "api-deploy",
                    0,
                    4,
                    8080,
                    ServiceHealth::Serving,
                )],
            })
            .is_err());
        assert!(directory.resolve("api").unwrap().is_empty());
    }

    #[test]
    fn remote_cleanup_allows_same_node_generation_to_restart() {
        let mut directory = ServiceDirectory::default();
        directory
            .replace_remote_node(ServiceAdvertisementSnapshot {
                node_id: NodeId(7),
                generation: 8,
                services: vec![endpoint(
                    7,
                    "api",
                    "api-deploy",
                    0,
                    1,
                    8080,
                    ServiceHealth::Serving,
                )],
            })
            .unwrap();

        assert_eq!(directory.remove_remote_node(NodeId(7)), (1, true));
        directory
            .replace_remote_node(ServiceAdvertisementSnapshot {
                node_id: NodeId(7),
                generation: 1,
                services: vec![endpoint(
                    7,
                    "api",
                    "api-deploy",
                    0,
                    2,
                    9090,
                    ServiceHealth::Serving,
                )],
            })
            .unwrap();
        assert_eq!(directory.resolve("api").unwrap()[0].allocation_epoch, 2);
    }

    #[test]
    fn local_snapshot_refuses_partial_replacement() {
        let mut directory = ServiceDirectory::default();
        directory
            .upsert_local(endpoint(
                1,
                "api",
                "api-deploy",
                0,
                1,
                8080,
                ServiceHealth::Serving,
            ))
            .unwrap();
        directory
            .upsert_local(endpoint(
                1,
                "metrics",
                "api-deploy",
                0,
                1,
                9090,
                ServiceHealth::Serving,
            ))
            .unwrap();

        assert!(directory.local_snapshot(NodeId(1), 5, 1).is_err());
        let snapshot = directory.local_snapshot(NodeId(1), 5, 2).unwrap();
        assert_eq!(snapshot.generation, 5);
        assert_eq!(snapshot.services.len(), 2);
    }

    #[test]
    fn remote_snapshot_rejects_multiple_epochs_for_same_replica() {
        let mut directory = ServiceDirectory::default();
        let snapshot = ServiceAdvertisementSnapshot {
            node_id: NodeId(7),
            generation: 1,
            services: vec![
                endpoint(7, "api", "api-deploy", 0, 1, 8080, ServiceHealth::Serving),
                endpoint(
                    7,
                    "metrics",
                    "api-deploy",
                    0,
                    2,
                    9090,
                    ServiceHealth::Serving,
                ),
            ],
        };

        assert!(directory.replace_remote_node(snapshot).is_err());
        assert_eq!(directory.len(), 0);
    }
}
