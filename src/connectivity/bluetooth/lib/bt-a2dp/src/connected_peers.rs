// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{format_err, Error},
    bt_avdtp as avdtp,
    fidl_fuchsia_bluetooth_bredr::{ProfileDescriptor, ProfileProxy},
    fuchsia_async as fasync,
    fuchsia_bluetooth::{
        detachable_map::{DetachableMap, DetachableWeak},
        types::{Channel, PeerId},
    },
    fuchsia_cobalt::CobaltSender,
    fuchsia_inspect::{self as inspect, Property},
    fuchsia_inspect_derive::{AttachError, Inspect},
    futures::{
        channel::mpsc,
        stream::{Stream, StreamExt},
        task::{Context, Poll},
    },
    log::{info, warn},
    std::{collections::HashMap, pin::Pin, sync::Arc},
};

use crate::{codec::CodecNegotiation, peer::Peer, stream::Streams};

/// ConnectedPeers manages the set of connected peers based on discovery, new connection, and
/// peer session lifetime.
pub struct ConnectedPeers {
    /// The set of connected peers.
    connected: DetachableMap<PeerId, Peer>,
    /// ProfileDescriptors from discovering the peer, stored here before a peer connects.
    descriptors: HashMap<PeerId, ProfileDescriptor>,
    /// A set of streams which can be used as a template for each newly connected peer.
    streams: Streams,
    /// Codec Negotiation used to choose a compatible stream pair when starting streaming.
    codec_negotiation: CodecNegotiation,
    /// Profile Proxy, used to connect new transport sockets.
    profile: ProfileProxy,
    /// Cobalt logger to use and hand out to peers, if we are using one.
    cobalt_sender: Option<CobaltSender>,
    /// The 'peers' node of the inspect tree. All connected peers own a child node of this node.
    inspect: inspect::Node,
    /// Inspect node for which is the current preferred peer direction.
    inspect_peer_direction: inspect::StringProperty,
    /// Listeners for new connected peers
    connected_peer_senders: Vec<mpsc::Sender<DetachableWeak<PeerId, Peer>>>,
}

impl ConnectedPeers {
    pub fn new(
        streams: Streams,
        codec_negotiation: CodecNegotiation,
        profile: ProfileProxy,
        cobalt_sender: Option<CobaltSender>,
    ) -> Self {
        Self {
            connected: DetachableMap::new(),
            descriptors: HashMap::new(),
            streams,
            codec_negotiation,
            profile,
            inspect: inspect::Node::default(),
            inspect_peer_direction: inspect::StringProperty::default(),
            cobalt_sender,
            connected_peer_senders: Vec::new(),
        }
    }

    pub(crate) fn get_weak(&self, id: &PeerId) -> Option<DetachableWeak<PeerId, Peer>> {
        self.connected.get(id)
    }

    pub(crate) fn get(&self, id: &PeerId) -> Option<Arc<Peer>> {
        self.get_weak(id).and_then(|p| p.upgrade())
    }

    pub fn is_connected(&self, id: &PeerId) -> bool {
        self.connected.contains_key(id)
    }

    async fn start_streaming(
        peer: &DetachableWeak<PeerId, Peer>,
        negotiation: CodecNegotiation,
    ) -> Result<(), anyhow::Error> {
        let strong = peer.upgrade().ok_or(format_err!("Disconnected"))?;
        let remote_streams = strong.collect_capabilities().await?;

        let (negotiated, remote_seid) =
            negotiation.select(&remote_streams).ok_or(format_err!("No compatible stream found"))?;

        let strong = peer.upgrade().ok_or(format_err!("Disconnected"))?;
        strong.stream_start(remote_seid, negotiated).await.map_err(Into::into)
    }

    pub fn found(&mut self, id: PeerId, desc: ProfileDescriptor) {
        self.descriptors.insert(id, desc.clone());
        self.get(&id).map(|p| p.set_descriptor(desc));
    }

    pub fn set_preferred_direction(&mut self, direction: avdtp::EndpointType) {
        self.codec_negotiation.set_direction(direction);
        self.inspect_peer_direction.set(&format!("{:?}", direction));
    }

    pub fn preferred_direction(&self) -> avdtp::EndpointType {
        self.codec_negotiation.direction()
    }

    /// Accept a channel that was connected to the peer `id`.  If `initiator` is true, we initiated
    /// this connection (and should take the INT role)
    /// Returns a weak peer pointer if connected (even if it was connected before) if successful.
    pub fn connected(
        &mut self,
        id: PeerId,
        channel: Channel,
        initiator: bool,
    ) -> Result<DetachableWeak<PeerId, Peer>, Error> {
        if let Some(weak) = self.get_weak(&id) {
            let peer = weak.upgrade().ok_or(format_err!("Disconnected connecting transport"))?;
            if let Err(e) = peer.receive_channel(channel) {
                warn!("{} failed to connect channel: {}", id, e);
                return Err(e.into());
            }
            return Ok(weak);
        }

        let entry = self.connected.lazy_entry(&id);

        info!("Adding new peer {}", id);
        let avdtp_peer = avdtp::Peer::new(channel);

        let mut peer = Peer::create(
            id,
            avdtp_peer,
            self.streams.as_new(),
            self.profile.clone(),
            self.cobalt_sender.clone(),
        );

        if let Some(desc) = self.descriptors.get(&id) {
            peer.set_descriptor(desc.clone());
        }

        if let Err(e) = peer.iattach(&self.inspect, inspect::unique_name("peer_")) {
            warn!("Couldn't attach peer {} to inspect tree: {:?}", id, e);
        }

        let closed_fut = peer.closed();
        let peer = match entry.try_insert(peer) {
            Err(_peer) => {
                warn!("Peer connected while we were setting up peer: {}", id);
                return self.get_weak(&id).ok_or(format_err!("Peer missing"));
            }
            Ok(weak_peer) => weak_peer,
        };

        if initiator {
            let peer_clone = peer.clone();
            let negotiation = self.codec_negotiation.clone();
            fuchsia_async::Task::local(async move {
                if let Err(e) = ConnectedPeers::start_streaming(&peer_clone, negotiation).await {
                    info!("Peer {} start failed with error: {:?}", peer_clone.key(), e);
                    peer_clone.detach();
                }
            })
            .detach();
        }

        // Remove the peer when we disconnect.
        fasync::Task::local(async move {
            closed_fut.await;
            peer.detach();
        })
        .detach();

        let peer = self.get_weak(&id).ok_or(format_err!("Peer missing"))?;
        self.notify_connected(&peer);
        Ok(peer)
    }

    /// Notify the listeners that a new peer has been connected to.
    fn notify_connected(&mut self, peer: &DetachableWeak<PeerId, Peer>) {
        let mut i = 0;
        while i != self.connected_peer_senders.len() {
            if let Err(_) = self.connected_peer_senders[i].try_send(peer.clone()) {
                let _ = self.connected_peer_senders.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// Get a stream that produces peers that have been connected.
    pub fn connected_stream(&mut self) -> PeerConnections {
        let (sender, receiver) = mpsc::channel(0);
        self.connected_peer_senders.push(sender);
        PeerConnections { stream: receiver }
    }
}

impl Inspect for &mut ConnectedPeers {
    fn iattach(self, parent: &inspect::Node, name: impl AsRef<str>) -> Result<(), AttachError> {
        self.inspect = parent.create_child(name);
        let peer_dir_str = format!("{:?}", self.preferred_direction());
        self.inspect_peer_direction =
            self.inspect.create_string("preferred_peer_direction", peer_dir_str);
        self.streams.iattach(&self.inspect, "local_streams")
    }
}

/// Provides a stream of peers that have been connected to. This stream produces an item whenever
/// an A2DP peer has been connected.  It will produce None when no more peers will be connected.
pub struct PeerConnections {
    stream: mpsc::Receiver<DetachableWeak<PeerId, Peer>>,
}

impl Stream for PeerConnections {
    type Item = DetachableWeak<PeerId, Peer>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.poll_next_unpin(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bt_avdtp::Request;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_bluetooth_bredr::{ProfileMarker, ProfileRequestStream};
    use fidl_fuchsia_cobalt::CobaltEvent;
    use fuchsia_inspect::assert_inspect_tree;
    use futures::channel::mpsc;
    use futures::{self, task::Poll, StreamExt};
    use std::convert::{TryFrom, TryInto};

    use crate::{media_task::tests::TestMediaTaskBuilder, media_types::*, stream::Stream};

    fn fake_cobalt_sender() -> (CobaltSender, mpsc::Receiver<CobaltEvent>) {
        const BUFFER_SIZE: usize = 100;
        let (sender, receiver) = mpsc::channel(BUFFER_SIZE);
        (CobaltSender::new(sender), receiver)
    }

    fn run_to_stalled(exec: &mut fasync::Executor) {
        let _ = exec.run_until_stalled(&mut futures::future::pending::<()>());
    }

    fn exercise_avdtp(exec: &mut fasync::Executor, remote: Channel, peer: &Peer) {
        let remote_avdtp = avdtp::Peer::new(remote);
        let mut remote_requests = remote_avdtp.take_request_stream();

        // Should be able to actually communicate via the peer.
        let avdtp = peer.avdtp();
        let discover_fut = avdtp.discover();

        futures::pin_mut!(discover_fut);

        assert!(exec.run_until_stalled(&mut discover_fut).is_pending());

        let responder = match exec.run_until_stalled(&mut remote_requests.next()) {
            Poll::Ready(Some(Ok(Request::Discover { responder }))) => responder,
            x => panic!("Expected a Ready Discovery request but got {:?}", x),
        };

        let endpoint_id = avdtp::StreamEndpointId::try_from(1).expect("endpointid creation");

        let information = avdtp::StreamInformation::new(
            endpoint_id,
            false,
            avdtp::MediaType::Audio,
            avdtp::EndpointType::Source,
        );

        responder.send(&[information]).expect("Sending response should have worked");

        let _stream_infos = match exec.run_until_stalled(&mut discover_fut) {
            Poll::Ready(Ok(infos)) => infos,
            x => panic!("Expected a Ready response but got {:?}", x),
        };
    }

    fn setup_connected_peer_test(
    ) -> (fasync::Executor, PeerId, ConnectedPeers, ProfileRequestStream) {
        let exec = fasync::Executor::new().expect("executor should build");
        let (proxy, stream) =
            create_proxy_and_stream::<ProfileMarker>().expect("Profile proxy should be created");
        let id = PeerId(1);
        let (cobalt_sender, _) = fake_cobalt_sender();

        let peers = ConnectedPeers::new(
            Streams::new(),
            CodecNegotiation::build(vec![], avdtp::EndpointType::Sink).unwrap(),
            proxy,
            Some(cobalt_sender),
        );

        (exec, id, peers, stream)
    }

    #[test]
    fn connect_creates_peer() {
        let (mut exec, id, mut peers, _stream) = setup_connected_peer_test();

        let (remote, channel) = Channel::create();

        let peer = peers.connected(id, channel, false).expect("peer should connect");
        let peer = peer.upgrade().expect("peer should be connected");

        exercise_avdtp(&mut exec, remote, &peer);
    }

    #[test]
    fn connect_notifies_streams() {
        let (mut exec, id, mut peers, _stream) = setup_connected_peer_test();

        let (remote, channel) = Channel::create();

        let mut peer_stream = peers.connected_stream();
        let mut peer_stream_two = peers.connected_stream();

        let peer = peers.connected(id, channel, false).expect("peer should connect");
        let peer = peer.upgrade().expect("peer should be connected");

        // Peers should have been notified of the new peer
        let weak = exec.run_singlethreaded(peer_stream.next()).expect("peer stream to produce");
        assert_eq!(weak.key(), &id);
        let weak = exec.run_singlethreaded(peer_stream_two.next()).expect("peer stream to produce");
        assert_eq!(weak.key(), &id);

        exercise_avdtp(&mut exec, remote, &peer);

        // If you drop one stream, the other one should still produce.
        drop(peer_stream);

        let id2 = PeerId(2);
        let (remote2, channel2) = Channel::create();
        let peer2 = peers.connected(id2, channel2, false).expect("peer should connect");
        let peer2 = peer2.upgrade().expect("peer two should be connected");

        let weak = exec.run_singlethreaded(peer_stream_two.next()).expect("peer stream to produce");
        assert_eq!(weak.key(), &id2);

        exercise_avdtp(&mut exec, remote2, &peer2);
    }

    // Arbitrarily chosen ID for the SBC stream endpoint.
    const SBC_SEID: u8 = 9;

    // Arbitrarily chosen ID for the AAC stream endpoint.
    const AAC_SEID: u8 = 10;

    fn build_test_stream(id: u8, codec_cap: avdtp::ServiceCapability) -> Stream {
        let endpoint = avdtp::StreamEndpoint::new(
            id,
            avdtp::MediaType::Audio,
            avdtp::EndpointType::Sink,
            vec![avdtp::ServiceCapability::MediaTransport, codec_cap],
        )
        .expect("endpoint builds");
        let task_builder = TestMediaTaskBuilder::new();

        Stream::build(endpoint, task_builder.builder())
    }

    #[test]
    fn connect_initiation_uses_negotiation() {
        let mut exec = fasync::Executor::new().expect("executor should build");
        let (proxy, _stream) =
            create_proxy_and_stream::<ProfileMarker>().expect("Profile proxy should be created");
        let id = PeerId(1);
        let (cobalt_sender, _) = fake_cobalt_sender();

        let (remote, channel) = Channel::create();
        let remote = avdtp::Peer::new(remote);

        let aac_codec: avdtp::ServiceCapability = AacCodecInfo::new(
            AacObjectType::MANDATORY_SNK,
            AacSamplingFrequency::MANDATORY_SNK,
            AacChannels::MANDATORY_SNK,
            true,
            0, // 0 = Unknown constant bitrate support (A2DP Sec. 4.5.2.4)
        )
        .unwrap()
        .into();
        let remote_aac_seid: avdtp::StreamEndpointId = 2u8.try_into().unwrap();

        let sbc_codec: avdtp::ServiceCapability = SbcCodecInfo::new(
            SbcSamplingFrequency::MANDATORY_SNK,
            SbcChannelMode::MANDATORY_SNK,
            SbcBlockCount::MANDATORY_SNK,
            SbcSubBands::MANDATORY_SNK,
            SbcAllocation::MANDATORY_SNK,
            SbcCodecInfo::BITPOOL_MIN,
            SbcCodecInfo::BITPOOL_MAX,
        )
        .unwrap()
        .into();
        let remote_sbc_seid: avdtp::StreamEndpointId = 1u8.try_into().unwrap();

        let negotiation = CodecNegotiation::build(
            vec![aac_codec.clone(), sbc_codec.clone()],
            avdtp::EndpointType::Sink,
        )
        .unwrap();

        let mut streams = Streams::new();
        streams.insert(build_test_stream(SBC_SEID, sbc_codec.clone()));
        streams.insert(build_test_stream(AAC_SEID, aac_codec.clone()));

        let mut peers =
            ConnectedPeers::new(streams, negotiation.clone(), proxy, Some(cobalt_sender));

        assert!(peers.connected(id, channel, true).is_ok());

        // Should discover remote streams, negotiate, and start.

        let mut remote_requests = remote.take_request_stream();

        match exec.run_singlethreaded(&mut remote_requests.next()) {
            Some(Ok(avdtp::Request::Discover { responder })) => {
                let endpoints = vec![
                    avdtp::StreamInformation::new(
                        remote_sbc_seid.clone(),
                        false,
                        avdtp::MediaType::Audio,
                        avdtp::EndpointType::Source,
                    ),
                    avdtp::StreamInformation::new(
                        remote_aac_seid.clone(),
                        false,
                        avdtp::MediaType::Audio,
                        avdtp::EndpointType::Source,
                    ),
                ];
                responder.send(&endpoints).expect("response succeeds");
            }
            x => panic!("Expected a discovery request, got {:?}", x),
        };

        for _twice in 1..=2 {
            match exec.run_singlethreaded(&mut remote_requests.next()) {
                Some(Ok(avdtp::Request::GetCapabilities { stream_id, responder })) => {
                    if stream_id == remote_sbc_seid {
                        responder.send(&vec![
                            avdtp::ServiceCapability::MediaTransport,
                            sbc_codec.clone(),
                        ])
                    } else if stream_id == remote_aac_seid {
                        responder.send(&vec![
                            avdtp::ServiceCapability::MediaTransport,
                            aac_codec.clone(),
                        ])
                    } else {
                        responder.reject(avdtp::ErrorCode::BadAcpSeid)
                    }
                    .expect("respond succeeds");
                }
                x => panic!("Expected a get capabilities request, got {:?}", x),
            };
        }

        match exec.run_singlethreaded(&mut remote_requests.next()) {
            Some(Ok(avdtp::Request::SetConfiguration {
                local_stream_id,
                remote_stream_id,
                capabilities: _,
                responder,
            })) => {
                // Should set the aac stream, matched with local AAC seid.
                assert_eq!(remote_aac_seid, local_stream_id);
                let local_aac_seid: avdtp::StreamEndpointId = AAC_SEID.try_into().unwrap();
                assert_eq!(local_aac_seid, remote_stream_id);
                responder.send().expect("response sends");
            }
            x => panic!("Expected a set configuration request, got {:?}", x),
        };
    }

    #[test]
    fn connected_peers_inspect() {
        let (_exec, id, mut peers, _stream) = setup_connected_peer_test();

        let inspect = inspect::Inspector::new();
        peers.iattach(inspect.root(), "peers").expect("should attach to inspect tree");

        assert_inspect_tree!(inspect, root: {
            peers: { local_streams: contains {}, preferred_peer_direction: "Sink" }});

        peers.set_preferred_direction(avdtp::EndpointType::Source);

        assert_inspect_tree!(inspect, root: {
            peers: { local_streams: contains {}, preferred_peer_direction: "Source" }});

        // Connect a peer, it should show up in the tree.
        let (_remote, channel) = Channel::create();
        assert!(peers.connected(id, channel, false).is_ok());

        assert_inspect_tree!(inspect, root: {
            peers: {
                preferred_peer_direction: "Source",
                local_streams: contains {},
                peer_0: { id: "0000000000000001", local_streams: contains {} }
            }
        });
    }

    #[test]
    fn connected_peers_peer_disconnect_removes_peer() {
        let (mut exec, id, mut peers, _stream) = setup_connected_peer_test();

        let (remote, channel) = Channel::create();

        assert!(peers.connected(id, channel, false).is_ok());
        run_to_stalled(&mut exec);

        // Disconnect the signaling channel, peer should be gone.
        drop(remote);

        run_to_stalled(&mut exec);

        assert!(peers.get(&id).is_none());
    }

    #[test]
    fn connected_peers_reconnect_works() {
        let (mut exec, id, mut peers, _stream) = setup_connected_peer_test();

        let (remote, channel) = Channel::create();
        assert!(peers.connected(id, channel, false).is_ok());
        run_to_stalled(&mut exec);

        // Disconnect the signaling channel, peer should be gone.
        drop(remote);

        run_to_stalled(&mut exec);

        assert!(peers.get(&id).is_none());

        // Connect another peer with the same ID
        let (_remote, channel) = Channel::create();

        assert!(peers.connected(id, channel, false).is_ok());
        run_to_stalled(&mut exec);

        // Should be connected.
        assert!(peers.get(&id).is_some());
    }
}
