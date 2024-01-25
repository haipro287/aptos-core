// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{PeerMonitorState, PeerMonitoringServiceClient};
// use aptos_channels::{aptos_channel, aptos_channel::Receiver, message_queues::QueueStyle};
use aptos_config::{
    config::PeerRole,
    network_id::{NetworkId, PeerNetworkId},
};
use aptos_netcore::transport::ConnectionOrigin;
use aptos_network2::{
    application::{interface::NetworkClient, metadata::ConnectionState, storage::PeersAndMetadata},
    // peer_manager::{ConnectionRequestSender, PeerManagerRequest, PeerManagerRequestSender},
    protocols::{
        network::{NetworkSender, NewNetworkSender},
        wire::handshake::v1::ProtocolId,
    },
    transport::ConnectionMetadata,
};
use aptos_peer_monitoring_service_server::network::NetworkRequest;
use aptos_peer_monitoring_service_types::PeerMonitoringServiceMessage;
use aptos_time_service::TimeService;
use aptos_types::account_address::{AccountAddress as PeerId};
use std::{collections::HashMap, sync::Arc};
use std::collections::BTreeMap;
use futures::StreamExt;
use futures::stream::{Stream,SelectAll};
use aptos_config::config::RoleType;
use aptos_network2::protocols::network::OutboundPeerConnections;
use aptos_network2::protocols::wire::messaging::v1::NetworkMessage;

/// A simple mock of the peer monitoring server for test purposes
pub struct MockMonitoringServer {
    // peer_manager_request_receivers:
    //     HashMap<NetworkId, aptos_channel::Receiver<(PeerId, ProtocolId), PeerManagerRequest>>,
    peers_and_metadata: Arc<PeersAndMetadata>,
    peer_senders: Arc<OutboundPeerConnections>,
    // peer_receivers: HashMap<PeerNetworkId, tokio::sync::mpsc::Receiver<NetworkMessage>>,
    peer_receivers: BTreeMap<NetworkId, SelectAll<NetworkMessage>>,
}

impl MockMonitoringServer {
    pub fn new(
        all_network_ids: Vec<NetworkId>,
    ) -> (
        PeerMonitoringServiceClient<NetworkClient<PeerMonitoringServiceMessage>>,
        Self,
        PeerMonitorState,
        TimeService,
    ) {
        // Setup the test logger (if it hasn't already been initialized)
        ::aptos_logger::Logger::init_for_testing();

        // Setup the request channels and the network sender for each network
        let mut network_senders = HashMap::new();
        // let mut peer_manager_request_receivers = HashMap::new();
        let peer_senders = Arc::new(OutboundPeerConnections::new());
        for network_id in &all_network_ids {
            // Create the channels and network sender
            // let queue_config = aptos_channel::Config::new(10).queue_style(QueueStyle::FIFO);
            // let (peer_manager_request_sender, peer_manager_request_receiver) = queue_config.build();
            // let (connection_request_sender, _connection_request_receiver) = queue_config.build();
            let network_sender = NetworkSender::new(
                *network_id,
                peer_senders.clone(),
                if network_id == NetworkId::Validator {RoleType::Validator} else {RoleType::FullNode},
                // PeerManagerRequestSender::new(peer_manager_request_sender),
                // ConnectionRequestSender::new(connection_request_sender),
            );

            // Store the channels and network sender
            // peer_manager_request_receivers.insert(*network_id, peer_manager_request_receiver);
            network_senders.insert(*network_id, network_sender);
        }

        // Setup the network client
        let peers_and_metadata = PeersAndMetadata::new(&all_network_ids);
        let network_client = NetworkClient::new(
            vec![], // The peer monitoring service doesn't use direct send
            vec![ProtocolId::PeerMonitoringServiceRpc],
            network_senders,
            peers_and_metadata.clone(),
        );

        // Create the mock server
        let mock_monitoring_server = Self {
            // peer_manager_request_receivers,
            peers_and_metadata,
            peer_senders,
            peer_receivers: BTreeMap::new(),
        };

        (
            PeerMonitoringServiceClient::new(network_client),
            mock_monitoring_server,
            PeerMonitorState::new(),
            TimeService::mock(),
        )
    }

    /// Add a new peer to the peers and metadata struct
    pub fn add_new_peer(&mut self, network_id: NetworkId, role: PeerRole) -> PeerNetworkId {
        // Create a new peer
        let peer_id = PeerId::random();
        let peer_network_id = PeerNetworkId::new(network_id, peer_id);

        // Create and save a new connection metadata
        let mut connection_metadata = ConnectionMetadata::mock_with_role_and_origin(
            peer_id,
            role,
            ConnectionOrigin::Outbound,
        );
        connection_metadata
            .application_protocols
            .insert(ProtocolId::PeerMonitoringServiceRpc);
        self.peers_and_metadata
            .insert_connection_metadata(peer_network_id, connection_metadata)
            .unwrap();

        // Return the new peer
        peer_network_id
    }

    /// Disconnects the peer in the peers and metadata struct
    pub fn disconnect_peer(&mut self, peer: PeerNetworkId) {
        self.update_peer_state(peer, ConnectionState::Disconnected);
    }

    /// Reconnects the peer in the peers and metadata struct
    pub fn reconnected_peer(&mut self, peer: PeerNetworkId) {
        self.update_peer_state(peer, ConnectionState::Connected);
    }

    /// Updates the state of the given peer in the peers and metadata struct
    fn update_peer_state(&mut self, peer: PeerNetworkId, state: ConnectionState) {
        self.peers_and_metadata
            .update_connection_state(peer, state)
            .unwrap();
    }

    /// Get the next request sent from the client
    pub async fn next_request(&mut self, network_id: &NetworkId) -> Option<NetworkRequest> {
        match self.peer_receivers.get(network_id) {
            Some(nchan) => {
                nchan.next().await
            }
            None => None,
        }
        //  if let Some(nchan) = self.peer_receivers.get(network_id) {
        //     nchan.next().await
        // } else {
        //     return None;
        // }
        // // Get the request receiver
        // let peer_manager_request_receiver = self.get_request_receiver(network_id);
        //
        // // Wait for the next request
        // match peer_manager_request_receiver.next().await {
        //     Some(PeerManagerRequest::SendRpc(peer_id, network_request)) => {
        //         // Deconstruct the network request
        //         let peer_network_id = PeerNetworkId::new(*network_id, peer_id);
        //         let protocol_id = network_request.protocol_id;
        //         let request_data = network_request.data;
        //         let response_sender = network_request.res_tx;
        //
        //         // Deserialize the network message
        //         let peer_monitoring_message: PeerMonitoringServiceMessage =
        //             bcs::from_bytes(request_data.as_ref()).unwrap();
        //         let peer_monitoring_service_request = match peer_monitoring_message {
        //             PeerMonitoringServiceMessage::Request(request) => request,
        //             _ => panic!("Unexpected message received: {:?}", peer_monitoring_message),
        //         };
        //
        //         // Return the network request
        //         Some(NetworkRequest {
        //             peer_network_id,
        //             protocol_id,
        //             peer_monitoring_service_request,
        //             response_sender: ResponseSender::new(response_sender),
        //         })
        //     },
        //     Some(PeerManagerRequest::SendDirectSend(_, _)) => {
        //         panic!("Unexpected direct send message received!")
        //     },
        //     None => None,
        // }
    }

    /// Verifies that there are no pending requests on the network
    pub async fn verify_no_pending_requests(&mut self, network_id: &NetworkId) {
        // Get the request receiver
        // let peer_manager_request_receiver = self.get_request_receiver(network_id);
        let pending_request = match self.peer_receivers.get(network_id) {
            Some(nchan) => {
                match nchan.try_next() {
                    Ok(xo) => match xo {
                        Some(xr) => Some(xr),
                        None => None,
                    }
                    Err(_) => {
                        None
                    }
                }
            }
            None => { None }
        };

        if let Some(pending_request) = pending_request {
            panic!("Unexpected pending request: {:?}", pending_request);
        }
    }

    // /// Gets the request receiver for the specified network
    // fn get_request_receiver(
    //     &mut self,
    //     network_id: &NetworkId,
    // ) -> &mut Receiver<(AccountAddress, ProtocolId), PeerManagerRequest> {
    //     self.peer_manager_request_receivers
    //         .get_mut(network_id)
    //         .unwrap()
    // }
}
