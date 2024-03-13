// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use aptos_config::{
    config::MempoolConfig,
    network_id::{NetworkId, PeerNetworkId},
};
use aptos_infallible::RwLock;
use aptos_logger::prelude::*;
use aptos_peer_monitoring_service_types::PeerMonitoringMetadata;
use aptos_time_service::{TimeService, TimeServiceTrait};
use itertools::Itertools;
use std::{
    cmp::Ordering,
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    sync::Arc,
    time::Instant,
};

/// A simple struct that offers comparisons and ordering for peer prioritization
#[derive(Clone, Debug)]
struct PrioritizedPeersComparator {
    random_state: RandomState,
}

impl PrioritizedPeersComparator {
    fn new() -> Self {
        Self {
            random_state: RandomState::new(),
        }
    }

    /// Provides ordering for peers when forwarding transactions.
    /// Higher priority peers are greater than lower priority peers.
    fn compare(
        &self,
        peer_a: &(PeerNetworkId, Option<PeerMonitoringMetadata>),
        peer_b: &(PeerNetworkId, Option<PeerMonitoringMetadata>),
    ) -> Ordering {
        // Deconstruct the peer tuples
        let (network_id_a, monitoring_metadata_a) = peer_a;
        let (network_id_b, monitoring_metadata_b) = peer_b;

        // First, compare by network ID (i.e., Validator > VFN > Public)
        let network_ordering =
            compare_network_id(&network_id_a.network_id(), &network_id_b.network_id());
        if !network_ordering.is_eq() {
            return network_ordering; // Only return if it's not equal
        }

        // Otherwise, compare by peer distance from the validators.
        // This avoids badly configured/connected peers (e.g., broken VN-VFN connections).
        let distance_ordering =
            compare_validator_distance(monitoring_metadata_a, monitoring_metadata_b);
        if !distance_ordering.is_eq() {
            return distance_ordering; // Only return if it's not equal
        }

        // Otherwise, compare by peer ping latency (the lower the better)
        let latency_ordering = compare_ping_latency(monitoring_metadata_a, monitoring_metadata_b);
        if !latency_ordering.is_eq() {
            return latency_ordering; // Only return if it's not equal
        }

        // Otherwise, simply hash the peer ID and compare the hashes.
        // In practice, this should be relatively rare.
        let hash_a = self.hash_peer_id(network_id_a);
        let hash_b = self.hash_peer_id(network_id_b);
        hash_a.cmp(&hash_b)
    }

    /// Stable within a mempool instance but random between instances
    fn hash_peer_id(&self, peer_network_id: &PeerNetworkId) -> u64 {
        let mut hasher = self.random_state.build_hasher();
        hasher.write(peer_network_id.peer_id().as_ref());
        hasher.finish()
    }
}

/// A simple struct to hold state for peer prioritization
#[derive(Clone, Debug)]
pub struct PrioritizedPeersState {
    // The current mempool configuration
    mempool_config: MempoolConfig,

    // The current list of prioritized peers
    prioritized_peers: Arc<RwLock<Vec<PeerNetworkId>>>,

    // The comparator used to prioritize peers
    peer_comparator: PrioritizedPeersComparator,

    // Whether ping latencies were observed for all peers
    observed_all_ping_latencies: bool,

    // The last time peer priorities were updated
    last_peer_priority_update: Option<Instant>,

    // The time service used to fetch timestamps
    time_service: TimeService,
}

impl PrioritizedPeersState {
    pub fn new(mempool_config: MempoolConfig, time_service: TimeService) -> Self {
        Self {
            mempool_config,
            prioritized_peers: Arc::new(RwLock::new(Vec::new())),
            peer_comparator: PrioritizedPeersComparator::new(),
            observed_all_ping_latencies: false,
            last_peer_priority_update: None,
            time_service,
        }
    }

    /// Returns the priority of the given peer. The lower the
    /// value, the higher the priority.
    pub fn get_peer_priority(&self, peer_network_id: &PeerNetworkId) -> usize {
        let prioritized_peers = self.prioritized_peers.read();
        prioritized_peers
            .iter()
            .find_position(|peer| *peer == peer_network_id)
            .map_or(usize::MAX, |(position, _)| position)
    }

    /// Returns true iff the prioritized peers list is ready for another update.
    /// This is based on the last time the prioritized peers were updated, and if
    /// ping latencies were observed for all peers in the last update.
    pub fn ready_for_update(&self, peers_changed: bool) -> bool {
        // If our peers have changed, or we haven't observed ping latencies
        // for all peers yet, we should update the prioritized peers again.
        // This is necessary because ping latencies are only populated sometime
        // after the peer connects, so it is necessary to continuously update the
        // prioritized peers list until we have observed ping latencies for all peers.
        if peers_changed || !self.observed_all_ping_latencies {
            return true;
        }

        // Otherwise, we should only update if enough time has passed since the last update
        match self.last_peer_priority_update {
            None => true, // We haven't updated yet
            Some(last_update) => {
                let duration_since_update = self.time_service.now().duration_since(last_update);
                let update_interval_secs = self
                    .mempool_config
                    .shared_mempool_priority_update_interval_secs;
                duration_since_update.as_secs() > update_interval_secs
            },
        }
    }

    /// Sorts the given peers by priority using the prioritized peer comparator.
    /// The peers are sorted in descending order (i.e., higher values are prioritized).
    fn sort_peers_by_priority(
        &self,
        peers_and_metadata: &[(PeerNetworkId, Option<PeerMonitoringMetadata>)],
    ) -> Vec<PeerNetworkId> {
        peers_and_metadata
            .iter()
            .sorted_by(|peer_a, peer_b| {
                let ordering = &self.peer_comparator.compare(peer_a, peer_b);
                ordering.reverse() // Prioritize higher values (i.e., sorted by descending order)
            })
            .map(|(peer, _)| *peer)
            .collect()
    }

    /// Updates the prioritized peers list
    pub fn update_prioritized_peers(
        &mut self,
        peers_and_metadata: Vec<(PeerNetworkId, Option<PeerMonitoringMetadata>)>,
    ) {
        // Calculate the new set of prioritized peers
        let new_prioritized_peers = self.sort_peers_by_priority(&peers_and_metadata);

        // Update the prioritized peers
        let mut prioritized_peers = self.prioritized_peers.write();
        if new_prioritized_peers != *prioritized_peers {
            info!(
                "Updating mempool peer priority list: {:?}",
                new_prioritized_peers
            );
        }
        *prioritized_peers = new_prioritized_peers;

        // Check if we've now observed ping latencies for all peers
        if !self.observed_all_ping_latencies {
            self.observed_all_ping_latencies = peers_and_metadata
                .iter()
                .all(|(_, metadata)| get_peer_ping_latency(metadata).is_some());
        }

        // Set the last peer priority update time
        self.last_peer_priority_update = Some(self.time_service.now());
    }
}

/// Returns the distance from the validators for the
/// given monitoring metadata (if one exists).
fn get_distance_from_validators(
    monitoring_metadata: &Option<PeerMonitoringMetadata>,
) -> Option<u64> {
    monitoring_metadata.as_ref().and_then(|metadata| {
        metadata
            .latest_network_info_response
            .as_ref()
            .map(|network_info_response| network_info_response.distance_from_validators)
    })
}

/// Returns the ping latency for the given monitoring
/// metadata (if one exists).
fn get_peer_ping_latency(monitoring_metadata: &Option<PeerMonitoringMetadata>) -> Option<f64> {
    monitoring_metadata
        .as_ref()
        .and_then(|metadata| metadata.average_ping_latency_secs)
}

/// Compares the network ID for the given pair of peers.
/// The peer with the highest network is prioritized.
fn compare_network_id(network_id_a: &NetworkId, network_id_b: &NetworkId) -> Ordering {
    // We need to reverse the default ordering to ensure that: Validator > VFN > Public
    network_id_a.cmp(network_id_b).reverse()
}

/// Compares the ping latency for the given pair of monitoring metadata.
/// The peer with the lowest ping latency is prioritized.
fn compare_ping_latency(
    monitoring_metadata_a: &Option<PeerMonitoringMetadata>,
    monitoring_metadata_b: &Option<PeerMonitoringMetadata>,
) -> Ordering {
    // Get the ping latency from the monitoring metadata
    let ping_latency_a = get_peer_ping_latency(monitoring_metadata_a);
    let ping_latency_b = get_peer_ping_latency(monitoring_metadata_b);

    // Compare the ping latencies
    match (ping_latency_a, ping_latency_b) {
        (Some(ping_latency_a), Some(ping_latency_b)) => {
            // Prioritize the peer with the lowest ping latency
            ping_latency_a.total_cmp(&ping_latency_b).reverse()
        },
        (Some(_), None) => {
            Ordering::Greater // Prioritize the peer with a ping latency
        },
        (None, Some(_)) => {
            Ordering::Less // Prioritize the peer with a ping latency
        },
        (None, None) => {
            Ordering::Equal // Neither peer has a ping latency
        },
    }
}

/// Compares the validator distance for the given pair of monitoring metadata.
/// The peer with the lowest validator distance is prioritized.
fn compare_validator_distance(
    monitoring_metadata_a: &Option<PeerMonitoringMetadata>,
    monitoring_metadata_b: &Option<PeerMonitoringMetadata>,
) -> Ordering {
    // Get the validator distance from the monitoring metadata
    let validator_distance_a = get_distance_from_validators(monitoring_metadata_a);
    let validator_distance_b = get_distance_from_validators(monitoring_metadata_b);

    // Compare the distances
    match (validator_distance_a, validator_distance_b) {
        (Some(validator_distance_a), Some(validator_distance_b)) => {
            // Prioritize the peer with the lowest validator distance
            validator_distance_a.cmp(&validator_distance_b).reverse()
        },
        (Some(_), None) => {
            Ordering::Greater // Prioritize the peer with a validator distance
        },
        (None, Some(_)) => {
            Ordering::Less // Prioritize the peer with a validator distance
        },
        (None, None) => {
            Ordering::Equal // Neither peer has a validator distance
        },
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use aptos_config::{
        config::MempoolConfig,
        network_id::{NetworkId, PeerNetworkId},
    };
    use aptos_peer_monitoring_service_types::{
        response::NetworkInformationResponse, PeerMonitoringMetadata,
    };
    use aptos_types::PeerId;
    use core::cmp::Ordering;
    use std::collections::BTreeMap;

    #[test]
    fn test_compare_network_id() {
        // Create different network types
        let validator_network = NetworkId::Validator;
        let vfn_network = NetworkId::Vfn;
        let public_network = NetworkId::Public;

        // Compare the validator and VFN networks
        assert_eq!(
            Ordering::Greater,
            compare_network_id(&validator_network, &vfn_network)
        );

        // Compare the VFN and public networks
        assert_eq!(
            Ordering::Greater,
            compare_network_id(&validator_network, &public_network)
        );

        // Compare the validator and public networks
        assert_eq!(
            Ordering::Greater,
            compare_network_id(&vfn_network, &public_network)
        );
    }

    #[test]
    fn test_compare_validator_distance() {
        // Create monitoring metadata with the same distance
        let monitoring_metadata_1 = create_metadata_with_distance(Some(1));
        let monitoring_metadata_2 = create_metadata_with_distance(Some(1));

        // Verify that the metadata is equal
        assert_eq!(
            Ordering::Equal,
            compare_validator_distance(&Some(monitoring_metadata_1), &Some(monitoring_metadata_2))
        );

        // Create monitoring metadata with different distances
        let monitoring_metadata_1 = create_metadata_with_distance(Some(0));
        let monitoring_metadata_2 = create_metadata_with_distance(Some(4));

        // Verify that the metadata has different ordering
        assert_eq!(
            Ordering::Greater,
            compare_validator_distance(
                &Some(monitoring_metadata_1.clone()),
                &Some(monitoring_metadata_2.clone())
            )
        );
        assert_eq!(
            Ordering::Less,
            compare_validator_distance(&Some(monitoring_metadata_2), &Some(monitoring_metadata_1))
        );

        // Create monitoring metadata with and without distances
        let monitoring_metadata_1 = create_metadata_with_distance(Some(0));
        let monitoring_metadata_2 = create_metadata_with_distance(None);

        // Verify that the metadata with a distance has a higher ordering
        assert_eq!(
            Ordering::Greater,
            compare_validator_distance(
                &Some(monitoring_metadata_1.clone()),
                &Some(monitoring_metadata_2.clone())
            )
        );
        assert_eq!(
            Ordering::Less,
            compare_validator_distance(
                &Some(monitoring_metadata_2.clone()),
                &Some(monitoring_metadata_1.clone())
            )
        );

        // Compare monitoring metadata that is missing entirely
        assert_eq!(
            Ordering::Greater,
            compare_validator_distance(&Some(monitoring_metadata_1.clone()), &None)
        );
        assert_eq!(
            Ordering::Less,
            compare_validator_distance(&None, &Some(monitoring_metadata_1))
        );
    }

    #[test]
    fn test_compare_ping_latency() {
        // Create monitoring metadata with the same ping latency
        let monitoring_metadata_1 = create_metadata_with_latency(Some(1.0));
        let monitoring_metadata_2 = create_metadata_with_latency(Some(1.0));

        // Verify that the metadata is equal
        assert_eq!(
            Ordering::Equal,
            compare_ping_latency(&Some(monitoring_metadata_1), &Some(monitoring_metadata_2))
        );

        // Create monitoring metadata with different ping latencies
        let monitoring_metadata_1 = create_metadata_with_latency(Some(0.5));
        let monitoring_metadata_2 = create_metadata_with_latency(Some(2.0));

        // Verify that the metadata has different ordering
        assert_eq!(
            Ordering::Greater,
            compare_ping_latency(
                &Some(monitoring_metadata_1.clone()),
                &Some(monitoring_metadata_2.clone())
            )
        );
        assert_eq!(
            Ordering::Less,
            compare_ping_latency(&Some(monitoring_metadata_2), &Some(monitoring_metadata_1))
        );

        // Create monitoring metadata with and without ping latencies
        let monitoring_metadata_1 = create_metadata_with_latency(Some(0.5));
        let monitoring_metadata_2 = create_metadata_with_latency(None);

        // Verify that the metadata with a ping latency has a higher ordering
        assert_eq!(
            Ordering::Greater,
            compare_ping_latency(
                &Some(monitoring_metadata_1.clone()),
                &Some(monitoring_metadata_2.clone())
            )
        );
        assert_eq!(
            Ordering::Less,
            compare_ping_latency(
                &Some(monitoring_metadata_2.clone()),
                &Some(monitoring_metadata_1.clone())
            )
        );

        // Compare monitoring metadata that is missing entirely
        assert_eq!(
            Ordering::Greater,
            compare_ping_latency(&Some(monitoring_metadata_1.clone()), &None)
        );
        assert_eq!(
            Ordering::Less,
            compare_ping_latency(&None, &Some(monitoring_metadata_1))
        );
    }

    #[test]
    fn test_get_peer_priority() {
        // Create a prioritized peer state
        let prioritized_peers_state =
            PrioritizedPeersState::new(MempoolConfig::default(), TimeService::mock());

        // Create a list of peers
        let validator_peer = create_validator_peer();
        let vfn_peer = create_vfn_peer();
        let public_peer = create_public_peer();

        // Set the prioritized peers
        let prioritized_peers = vec![validator_peer, vfn_peer, public_peer];
        *prioritized_peers_state.prioritized_peers.write() = prioritized_peers.clone();

        // Verify that the peer priorities are correct
        for (index, peer) in prioritized_peers.iter().enumerate() {
            let expected_priority = index;
            let actual_priority = prioritized_peers_state.get_peer_priority(peer);
            assert_eq!(actual_priority, expected_priority);
        }
    }

    #[test]
    fn test_ready_for_update() {
        // Create a mempool configuration
        let shared_mempool_priority_update_interval_secs = 10;
        let mempool_config = MempoolConfig {
            shared_mempool_priority_update_interval_secs,
            ..MempoolConfig::default()
        };

        // Create a prioritized peer state
        let time_service = TimeService::mock();
        let mut prioritized_peers_state =
            PrioritizedPeersState::new(mempool_config.clone(), time_service.clone());

        // Verify that the prioritized peers should be updated (no prior update time)
        let peers_changed = false;
        assert!(prioritized_peers_state.ready_for_update(peers_changed));

        // Set the last peer priority update time
        prioritized_peers_state.last_peer_priority_update = Some(Instant::now());

        // Verify that the prioritized peers should still be updated (not all ping latencies were observed)
        assert!(prioritized_peers_state.ready_for_update(peers_changed));

        // Set the ping latencies observed flag
        prioritized_peers_state.observed_all_ping_latencies = true;

        // Verify that the prioritized peers should not be updated (not enough time has passed)
        assert!(!prioritized_peers_state.ready_for_update(peers_changed));

        // Emulate a change in peers and verify the prioritized peers should be updated
        assert!(prioritized_peers_state.ready_for_update(true));

        // Elapse some time (but not enough for the prioritized peers to be updated)
        let time_service = time_service.into_mock();
        time_service.advance_secs(shared_mempool_priority_update_interval_secs / 2);

        // Verify that the prioritized peers should not be updated (not enough time has passed)
        assert!(!prioritized_peers_state.ready_for_update(peers_changed));

        // Elapse enough time for the prioritized peers to be updated
        time_service.advance_secs(shared_mempool_priority_update_interval_secs + 1);

        // Verify that the prioritized peers should be updated (enough time has passed)
        assert!(prioritized_peers_state.ready_for_update(peers_changed));
    }

    #[test]
    fn test_sort_peers_by_priority() {
        // Create a prioritized peer state
        let prioritized_peers_state =
            PrioritizedPeersState::new(MempoolConfig::default(), TimeService::mock());

        // Create a list of peers (without metadata)
        let validator_peer = (create_validator_peer(), None);
        let vfn_peer = (create_vfn_peer(), None);
        let public_peer = (create_public_peer(), None);

        // Verify that peers are prioritized by network ID first
        let all_peers = vec![
            vfn_peer.clone(),
            public_peer.clone(),
            validator_peer.clone(),
        ];
        let prioritized_peers = prioritized_peers_state.sort_peers_by_priority(&all_peers);
        let expected_peers = vec![validator_peer.0, vfn_peer.0, public_peer.0];
        assert_eq!(prioritized_peers, expected_peers);

        // Create a list of peers with the same network ID, but different validator distances
        let public_peer_1 = (
            create_public_peer(),
            Some(create_metadata_with_distance(Some(1))),
        );
        let public_peer_2 = (
            create_public_peer(),
            Some(create_metadata_with_distance(None)), // No validator distance
        );
        let public_peer_3 = (
            create_public_peer(),
            Some(create_metadata_with_distance(Some(0))),
        );
        let public_peer_4 = (
            create_public_peer(),
            Some(create_metadata_with_distance(Some(2))),
        );

        // Verify that peers on the same network ID are prioritized by validator distance
        let all_peers = vec![
            public_peer_1.clone(),
            public_peer_2.clone(),
            public_peer_3.clone(),
            public_peer_4.clone(),
        ];
        let prioritized_peers = prioritized_peers_state.sort_peers_by_priority(&all_peers);
        let expected_peers = vec![
            public_peer_3.0,
            public_peer_1.0,
            public_peer_4.0,
            public_peer_2.0,
        ];
        assert_eq!(prioritized_peers, expected_peers);

        // Create a list of peers with the same network ID and validator distance, but different ping latencies
        let public_peer_1 = (
            create_public_peer(),
            Some(create_metadata_with_distance_and_latency(1, 0.5)),
        );
        let public_peer_2 = (
            create_public_peer(),
            Some(create_metadata_with_distance_and_latency(1, 2.0)),
        );
        let public_peer_3 = (
            create_public_peer(),
            Some(create_metadata_with_distance_and_latency(1, 0.4)),
        );
        let public_peer_4 = (
            create_public_peer(),
            Some(create_metadata_with_distance(Some(1))), // No ping latency
        );

        // Verify that peers on the same network ID and validator distance are prioritized by ping latency
        let all_peers = vec![
            public_peer_1.clone(),
            public_peer_2.clone(),
            public_peer_3.clone(),
            public_peer_4.clone(),
        ];
        let prioritized_peers = prioritized_peers_state.sort_peers_by_priority(&all_peers);
        let expected_peers = vec![
            public_peer_3.0,
            public_peer_1.0,
            public_peer_2.0,
            public_peer_4.0,
        ];
        assert_eq!(prioritized_peers, expected_peers);
    }

    #[test]
    fn test_update_prioritized_peers() {
        // Create a prioritized peer state
        let time_service = TimeService::mock();
        let mut prioritized_peers_state =
            PrioritizedPeersState::new(MempoolConfig::default(), time_service.clone());

        // Verify that the last peer priority update time is not set
        assert!(prioritized_peers_state.last_peer_priority_update.is_none());

        // Create a list of peers with and without ping latencies
        let public_peer_1 = (
            create_public_peer(),
            Some(create_metadata_with_distance_and_latency(1, 0.5)),
        );
        let public_peer_2 = (
            create_public_peer(),
            Some(create_metadata_with_distance_and_latency(1, 2.0)),
        );
        let public_peer_3 = (
            create_public_peer(),
            Some(create_metadata_with_distance_and_latency(1, 0.4)),
        );
        let public_peer_4 = (
            create_public_peer(),
            Some(create_metadata_with_distance(Some(1))), // No ping latency
        );

        // Update the prioritized peers
        let all_peers = vec![
            public_peer_1.clone(),
            public_peer_2.clone(),
            public_peer_3.clone(),
            public_peer_4.clone(),
        ];
        prioritized_peers_state.update_prioritized_peers(all_peers);

        // Verify that the prioritized peers were updated correctly
        let expected_peers = vec![
            public_peer_3.0,
            public_peer_1.0,
            public_peer_2.0,
            public_peer_4.0,
        ];
        let prioritized_peers = prioritized_peers_state.prioritized_peers.read().clone();
        assert_eq!(prioritized_peers, expected_peers);

        // Verify that the last peer priority update time was set correctly
        assert_eq!(
            prioritized_peers_state.last_peer_priority_update,
            Some(time_service.now())
        );

        // Verify that the observed ping latencies flag was not set
        assert!(!prioritized_peers_state.observed_all_ping_latencies);

        // Elapse some time
        let time_service = time_service.into_mock();
        time_service.advance_secs(100);

        // Update the prioritized peers for only peers with ping latencies
        let all_peers = vec![
            public_peer_1.clone(),
            public_peer_2.clone(),
            public_peer_3.clone(),
        ];
        prioritized_peers_state.update_prioritized_peers(all_peers);

        // Verify that the prioritized peers were updated correctly
        let expected_peers = vec![public_peer_3.0, public_peer_1.0, public_peer_2.0];
        let prioritized_peers = prioritized_peers_state.prioritized_peers.read().clone();
        assert_eq!(prioritized_peers, expected_peers);

        // Verify that the last peer priority update time was set correctly
        assert_eq!(
            prioritized_peers_state.last_peer_priority_update,
            Some(time_service.now())
        );

        // Verify that the observed ping latencies flag was set
        assert!(prioritized_peers_state.observed_all_ping_latencies);
    }

    /// Creates a peer monitoring metadata with the given distance
    fn create_metadata_with_distance(
        distance_from_validators: Option<u64>,
    ) -> PeerMonitoringMetadata {
        // Create a network info response with the given distance
        let network_info_response =
            distance_from_validators.map(|distance_from_validators| NetworkInformationResponse {
                connected_peers: BTreeMap::new(),
                distance_from_validators,
            });

        // Create the peer monitoring metadata
        PeerMonitoringMetadata::new(None, network_info_response, None, None)
    }

    /// Creates a peer monitoring metadata with the given distance and latency
    fn create_metadata_with_distance_and_latency(
        distance_from_validators: u64,
        average_ping_latency_secs: f64,
    ) -> PeerMonitoringMetadata {
        let mut monitoring_metadata = create_metadata_with_distance(Some(distance_from_validators));
        monitoring_metadata.average_ping_latency_secs = Some(average_ping_latency_secs);
        monitoring_metadata
    }

    /// Creates a peer monitoring metadata with the given ping latency
    fn create_metadata_with_latency(
        average_ping_latency_secs: Option<f64>,
    ) -> PeerMonitoringMetadata {
        // Create the peer monitoring metadata
        PeerMonitoringMetadata::new(average_ping_latency_secs, None, None, None)
    }

    /// Creates a validator peer with a random peer ID
    fn create_validator_peer() -> PeerNetworkId {
        PeerNetworkId::new(NetworkId::Validator, PeerId::random())
    }

    /// Creates a VFN peer with a random peer ID
    fn create_vfn_peer() -> PeerNetworkId {
        PeerNetworkId::new(NetworkId::Vfn, PeerId::random())
    }

    /// Creates a public peer with a random peer ID
    fn create_public_peer() -> PeerNetworkId {
        PeerNetworkId::new(NetworkId::Public, PeerId::random())
    }
}
