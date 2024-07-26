mod protocol;

use anyhow::{Context, Result};
use clap::Parser;
use common::{
    create_kademlia_behavior, read_or_create_identity, GOSSIPSUB_CHAT_FILE_TOPIC,
    GOSSIPSUB_CHAT_TOPIC, IDENTIFY_PROTO,
};
use core::panic;
use futures::future::{select, Either};
use futures::StreamExt;
use libp2p::request_response::{self, ProtocolSupport};
use libp2p::{
    dcutr, gossipsub, identify, identity, kad,
    kad::record::store::MemoryStore,
    memory_connection_limits,
    multiaddr::{Multiaddr, Protocol},
    relay,
    swarm::{NetworkBehaviour, Swarm, SwarmEvent},
    PeerId, StreamProtocol,
};
use libp2p::{noise, ping, swarm, tcp, yamux};
use log::{debug, error, info, warn};
use protocol::FileExchangeCodec;
use std::iter;
use std::net::IpAddr;
use std::path::Path;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    time::Duration,
};

use crate::protocol::FileRequest;

const TICK_INTERVAL: Duration = Duration::from_secs(5);
const FILE_EXCHANGE_PROTOCOL: StreamProtocol = StreamProtocol::new("/test-app-file/1");
const PORT_QUIC: u16 = 9091;
const PORT_TCP: u16 = 9092;
const LOCAL_KEY_PATH: &str = "./local_key";

#[derive(Debug, Parser)]
#[clap(name = "universal connectivity rust peer")]
struct Opt {
    /// Address to listen on.
    #[clap(long, default_value = "0.0.0.0")]
    listen_address: IpAddr,

    /// If known, the external address of this node. Will be used to correctly advertise our external address across all transports.
    #[clap(long, env)]
    external_address: Option<IpAddr>,

    #[clap(long, env, required = true)]
    coordinator_address: Multiaddr,
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    gossipsub: gossipsub::Behaviour,
    identify: identify::Behaviour,
    kademlia: kad::Behaviour<MemoryStore>,
    relay_client: relay::client::Behaviour,
    request_response: request_response::Behaviour<FileExchangeCodec>,
    connection_limits: memory_connection_limits::Behaviour,
    dcutr: dcutr::Behaviour,
    ping: ping::Behaviour,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let opt = Opt::parse();
    let local_key = read_or_create_identity(Path::new(LOCAL_KEY_PATH))
        .await
        .context("Failed to read identity")?;

    let mut swarm = create_swarm(local_key)?;

    swarm.listen_on(
        Multiaddr::from(opt.listen_address)
            .with(Protocol::Udp(PORT_QUIC))
            .with(Protocol::QuicV1),
    )?;

    swarm.listen_on(Multiaddr::from(opt.listen_address).with(Protocol::Tcp(PORT_TCP)))?;

    info!("connecting to the coordinator...");
    if let Err(e) = swarm.dial(opt.coordinator_address.clone()) {
        panic!(
            "Failed to dial coordinator {0}: {e}",
            opt.coordinator_address
        );
    }

    let coordinator_peer_id = {
        let mut learned_observed_addr = false;
        let mut told_relay_observed_addr = false;
        let mut coordinator_peer_id: Option<PeerId> = None;
        loop {
            match swarm.next().await.unwrap() {
                SwarmEvent::NewListenAddr { .. } => {}
                SwarmEvent::Dialing { .. } => {}
                SwarmEvent::ConnectionEstablished {
                    endpoint, peer_id, ..
                } => {
                    info!(
                        "Connected to coordinator {peer_id} at {}",
                        endpoint.get_remote_address()
                    );
                    coordinator_peer_id = Some(peer_id);
                }
                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Sent {
                    ..
                })) => {
                    info!("Told relay its public address");
                    told_relay_observed_addr = true;
                }
                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received {
                    info: identify::Info { observed_addr, .. },
                    ..
                })) => {
                    info!("Relay told us our observed address: {observed_addr}");
                    learned_observed_addr = true;
                }
                event => debug!("{event:?}"),
            }
            if told_relay_observed_addr && learned_observed_addr {
                if let Some(coordinator_peer_id) = coordinator_peer_id {
                    info!("Nice. Disconnecting from the coordinator...");
                    swarm.disconnect_peer_id(coordinator_peer_id).unwrap();
                    loop {
                        match swarm.next().await.unwrap() {
                            e @ SwarmEvent::ConnectionClosed { .. } => {
                                info!("ConnectionClosed: {e:?}");
                                break;
                            }
                            _ => {}
                        }
                    }
                    break coordinator_peer_id;
                } else {
                    panic!("Coordinator peer id not grabbed...");
                }
            }
        }
    };

    info!("coordinator peer ID is {coordinator_peer_id}. Trying to add ourselves as relayed..");
    let p2p_circuit_listen_addr = opt.coordinator_address.clone().with(Protocol::P2pCircuit);
    swarm.listen_on(p2p_circuit_listen_addr.clone()).unwrap();

    info!("starting main loop");

    let chat_topic_hash = gossipsub::IdentTopic::new(GOSSIPSUB_CHAT_TOPIC).hash();
    let file_topic_hash = gossipsub::IdentTopic::new(GOSSIPSUB_CHAT_FILE_TOPIC).hash();

    let mut tick = futures_timer::Delay::new(TICK_INTERVAL);

    loop {
        match select(swarm.next(), &mut tick).await {
            Either::Left((event, _)) => match event.unwrap() {
                SwarmEvent::NewListenAddr { address, .. } => {
                    if let Some(external_ip) = opt.external_address {
                        let external_address = address
                            .replace(0, |_| Some(external_ip.into()))
                            .expect("address.len > 1 and we always return `Some`");

                        swarm.add_external_address(external_address);
                    }

                    let p2p_address = address.with(Protocol::P2p(*swarm.local_peer_id()));
                    info!("Listening on {p2p_address}");
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    info!("Connected to {peer_id}");
                }
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                    warn!("Failed to dial {peer_id:?}: {error}");
                }
                SwarmEvent::IncomingConnectionError { error, .. } => {
                    warn!("{:#}", anyhow::Error::from(error))
                }
                SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                    warn!("Connection to {peer_id} closed: {cause:?}");
                    swarm.behaviour_mut().kademlia.remove_peer(&peer_id);
                    info!("Removed {peer_id} from the routing table (if it was in there).");
                    // if !swarm.connected_peers().any(|p| p == &coordinator_peer_id) {
                    //     warn!("Lost connection to coordinator. Reconnecting...");
                    //     // TODO this doesn't make sense, we need to put a big loop around all this...
                    //     swarm.listen_on(p2p_circuit_listen_addr.clone()).unwrap();
                    // }
                }
                SwarmEvent::Behaviour(event) => {
                    match event {
                        BehaviourEvent::Ping(_) => {}
                        BehaviourEvent::ConnectionLimits(e) => {
                            debug!("ConnectionLimits event: {:?}", e);
                        }
                        BehaviourEvent::Dcutr(e) => {
                            info!("Dcutr event: {:?}", e);
                        }
                        BehaviourEvent::RelayClient(e) => {
                            match e {
                                relay::client::Event::ReservationReqAccepted {
                                    relay_peer_id,
                                    renewal,
                                    limit,
                                } => {
                                    info!("Relay reservation accepted from {:?}, renewal: {:?}, limit: {:?}", relay_peer_id, renewal, limit);
                                }
                                _ => {}
                            }
                            info!("{:?}", e);
                        }
                        BehaviourEvent::Gossipsub(gossip_event) => match gossip_event {
                            gossipsub::Event::Message {
                                message_id: _,
                                propagation_source: _,
                                message,
                            } => {
                                if message.topic == chat_topic_hash {
                                    debug!(
                                        "Received message from {:?}: {}",
                                        message.source,
                                        String::from_utf8(message.data).unwrap()
                                    );
                                    continue;
                                }

                                if message.topic == file_topic_hash {
                                    let file_id = String::from_utf8(message.data).unwrap();
                                    info!("Received file {} from {:?}", file_id, message.source);

                                    let request_id =
                                        swarm.behaviour_mut().request_response.send_request(
                                            &message.source.unwrap(),
                                            FileRequest {
                                                file_id: file_id.clone(),
                                            },
                                        );
                                    info!(
                                        "Requested file {} to {:?}: req_id:{:?}",
                                        file_id, message.source, request_id
                                    );
                                    continue;
                                }

                                error!("Unexpected gossipsub topic hash: {:?}", message.topic);
                            }
                            gossipsub::Event::Subscribed { peer_id, topic } => {
                                debug!("{peer_id} subscribed to {topic}");
                            }
                            _ => debug!("Other gossipsub event: {:?}", gossip_event),
                        },
                        BehaviourEvent::Identify(e) => {
                            match e {
                                identify::Event::Received { peer_id, info } => {
                                    debug!("Received identify info from {:?}", peer_id);
                                    swarm.add_external_address(info.observed_addr.clone());
                                }
                                identify::Event::Error { peer_id, error } => {
                                    match error {
                                        swarm::StreamUpgradeError::Timeout => {
                                            // When a browser tab closes, we don't get a swarm event
                                            // maybe there's a way to get this with TransportEvent
                                            // but for now remove the peer from routing table if there's an Identify timeout
                                            let was_removed = swarm
                                                .behaviour_mut()
                                                .kademlia
                                                .remove_peer(&peer_id)
                                                .is_some();
                                            if was_removed {
                                                debug!("Removed {peer_id} from the routing table.");
                                            } else {
                                                debug!("Would have removed {peer_id} from the routing table, but it wasn't in there.");
                                            }
                                        }
                                        _ => {
                                            debug!("{error}");
                                        }
                                    }
                                }
                                event => {
                                    debug!("other BehaviourEvent {:?}", event);
                                }
                            }
                        }
                        BehaviourEvent::Kademlia(e) => {
                            debug!("Kademlia event: {:?}", e);
                        }
                        BehaviourEvent::RequestResponse(r_r) => match r_r {
                            request_response::Event::Message { message, .. } => match message {
                                request_response::Message::Request { request, .. } => {
                                    //TODO: support ProtocolSupport::Full
                                    debug!(
                                        "umimplemented: request_response::Message::Request: {:?}",
                                        request
                                    );
                                }
                                request_response::Message::Response { response, .. } => {
                                    info!(
                                        "request_response::Message::Response: size:{}",
                                        response.file_body.len()
                                    );
                                    // TODO: store this file (in memory or disk) and provider it via Kademlia
                                }
                            },
                            request_response::Event::OutboundFailure {
                                request_id, error, ..
                            } => {
                                error!(
                                "request_response::Event::OutboundFailure for request {:?}: {:?}",
                                request_id, error
                            );
                            }
                            _ => debug!("Unhandled request_response event: {:?}", r_r),
                        },
                    }
                }
                event => {
                    debug!("Other type of event: {:?}", event);
                }
            },
            Either::Right(_) => {
                tick = futures_timer::Delay::new(TICK_INTERVAL);

                debug!(
                    "external addrs: {:?}",
                    swarm.external_addresses().collect::<Vec<&Multiaddr>>()
                );
            }
        }
    }
}

fn make_gossipsub_behavior(keypair: identity::Keypair) -> Result<gossipsub::Behaviour> {
    // To content-address message, we can take the hash of message and use it as an ID.
    let message_id_fn = |message: &gossipsub::Message| {
        let mut s = DefaultHasher::new();
        message.data.hash(&mut s);
        gossipsub::MessageId::from(s.finish().to_string())
    };

    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .validation_mode(gossipsub::ValidationMode::Permissive) // This sets the kind of message validation. The default is Strict (enforce message signing)
        .message_id_fn(message_id_fn) // content-address messages. No two messages of the same content will be propagated.
        .mesh_outbound_min(1)
        .mesh_n_low(1)
        .flood_publish(true)
        .build()
        .expect("Valid config");

    let mut gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(keypair),
        gossipsub_config,
    )
    .expect("Correct configuration");

    gossipsub.subscribe(&gossipsub::IdentTopic::new(GOSSIPSUB_CHAT_TOPIC))?;
    gossipsub.subscribe(&gossipsub::IdentTopic::new(GOSSIPSUB_CHAT_FILE_TOPIC))?;

    Ok(gossipsub)
}

fn create_swarm(local_key: identity::Keypair) -> Result<Swarm<Behaviour>> {
    let local_peer_id = PeerId::from(local_key.public());
    debug!("Local peer id: {local_peer_id}");

    let identify_config = identify::Behaviour::new(
        identify::Config::new(IDENTIFY_PROTO.into(), local_key.public())
            .with_interval(Duration::from_secs(60)), // do this so we can get timeouts for dropped connections
    );

    let kademlia = create_kademlia_behavior(local_peer_id);

    let gossipsub_behaviour = make_gossipsub_behavior(local_key.clone())?;

    let swarm = libp2p::SwarmBuilder::with_existing_identity(local_key)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().port_reuse(true).nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_dns()?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|keypair, relay_behaviour| Behaviour {
            relay_client: relay_behaviour,
            gossipsub: gossipsub_behaviour,
            kademlia,
            ping: ping::Behaviour::new(ping::Config::new()),
            dcutr: dcutr::Behaviour::new(keypair.public().to_peer_id()),
            identify: identify_config,
            request_response: request_response::Behaviour::new(
                // TODO: support ProtocolSupport::Full
                iter::once((FILE_EXCHANGE_PROTOCOL, ProtocolSupport::Outbound)),
                Default::default(),
            ),
            connection_limits: memory_connection_limits::Behaviour::with_max_percentage(0.9),
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    Ok(swarm)
}
