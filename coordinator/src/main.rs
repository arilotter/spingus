use anyhow::{Context, Result};
use clap::Parser;
use common::{
    create_kademlia_behavior, read_or_create_identity, GOSSIPSUB_CHAT_FILE_TOPIC,
    GOSSIPSUB_CHAT_TOPIC,
};
use futures::future::{select, Either};
use futures::StreamExt;
use libp2p::kad::store::RecordStore;
use libp2p::kad::Record;
use libp2p::{
    gossipsub::{self, IdentTopic},
    identify, identity,
    kad::{self, record::store::MemoryStore},
    multiaddr::{Multiaddr, Protocol},
    relay,
    swarm::{NetworkBehaviour, Swarm, SwarmEvent},
    PeerId,
};
use libp2p::{noise, tcp, yamux};
use log::{debug, info, warn};
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

const TICK_INTERVAL: Duration = Duration::from_secs(15);
const PORT_QUIC: u16 = 9091;
const PORT_TCP: u16 = 9092;
const LOCAL_KEY_PATH: &str = "./local_key";

#[derive(Debug, Parser)]
#[clap(name = "relay node")]
struct Opt {
    /// Address to listen on.
    #[clap(long, default_value = "0.0.0.0")]
    listen_address: IpAddr,

    /// If known, the external address of this node.
    #[clap(long, env)]
    external_address: Option<IpAddr>,
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    gossipsub: gossipsub::Behaviour,
    identify: identify::Behaviour,
    kademlia: kad::Behaviour<MemoryStore>,
    relay: relay::Behaviour,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let opt = Opt::parse();
    let local_key = read_or_create_identity(Path::new(LOCAL_KEY_PATH))
        .await
        .context("Failed to read identity")?;

    let mut swarm = create_swarm(local_key)?;

    info!("My local peer id: {}", swarm.local_peer_id());

    let address_quic = Multiaddr::from(opt.listen_address)
        .with(Protocol::Udp(PORT_QUIC))
        .with(Protocol::QuicV1);

    swarm
        .listen_on(address_quic.clone())
        .expect("listen on quic");

    swarm.add_external_address(address_quic);

    let address_tcp = Multiaddr::from(opt.listen_address).with(Protocol::Tcp(PORT_TCP));
    swarm.listen_on(address_tcp.clone()).expect("listen on tcp");
    swarm.add_external_address(address_tcp);

    info!("starting main loop");

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
                SwarmEvent::ConnectionEstablished {
                    peer_id, endpoint, ..
                } => {
                    let addr = endpoint.get_remote_address();
                    info!("Connected to {peer_id} thru endpoint {addr}");
                }
                SwarmEvent::ConnectionClosed {
                    peer_id,
                    cause,
                    endpoint,
                    ..
                } => {
                    let addr = endpoint.get_remote_address();
                    warn!("Connection to {peer_id} closed: {cause:?} thru endpoint {addr}");
                    swarm.behaviour_mut().kademlia.remove_peer(&peer_id);
                    info!("Removed {peer_id} from the routing table and peers list.");
                }
                SwarmEvent::Behaviour(event) => match event {
                    BehaviourEvent::Relay(e) => {
                        if let relay::Event::ReservationReqAccepted { src_peer_id, .. } = e {
                            info!("Relay reservation accepted from {:?}", src_peer_id);
                            let local_peer_id = *swarm.local_peer_id();
                            let peer_dialable_addrs: Vec<Multiaddr> = swarm
                                .external_addresses()
                                .map(|a| {
                                    let cloned = a.clone();
                                    (match a.iter().last().unwrap() {
                                        Protocol::P2p(p_id) if p_id == local_peer_id => cloned,
                                        _ => cloned.with(Protocol::P2p(local_peer_id)),
                                    })
                                    .with(Protocol::P2pCircuit)
                                    .with(Protocol::P2p(src_peer_id))
                                })
                                .collect();
                            let peer_dialable_addrs_bytes =
                                common::encode_multiaddrs(&peer_dialable_addrs);
                            swarm
                                .behaviour_mut()
                                .kademlia
                                .store_mut()
                                .put(Record::new(
                                    common::get_peer_key(&src_peer_id),
                                    peer_dialable_addrs_bytes,
                                ))
                                .unwrap();
                        } else if let relay::Event::ReservationTimedOut { src_peer_id, .. } = e {
                            info!("Relay reservation timed out from {:?}", src_peer_id);
                            let key = kad::record::Key::from(Vec::<u8>::from(src_peer_id));
                            swarm.behaviour_mut().kademlia.store_mut().remove(&key);
                        }
                        debug!("Relay event: {:?}", e);
                    }
                    BehaviourEvent::Identify(identify::Event::Received { peer_id, info }) => {
                        info!("Received identify info from {:?}", peer_id);
                        swarm.add_external_address(info.observed_addr.clone());
                    }
                    _ => debug!("Other behaviour event: {:?}", event),
                },
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

fn create_swarm(local_key: identity::Keypair) -> Result<Swarm<Behaviour>> {
    let local_peer_id = PeerId::from(local_key.public());
    debug!("Local peer id: {local_peer_id}");

    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .validation_mode(gossipsub::ValidationMode::Permissive)
        .build()
        .expect("Valid config");

    let mut gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(local_key.clone()),
        gossipsub_config,
    )
    .expect("Correct configuration");

    gossipsub.subscribe(&IdentTopic::new(GOSSIPSUB_CHAT_TOPIC))?;
    gossipsub.subscribe(&IdentTopic::new(GOSSIPSUB_CHAT_FILE_TOPIC))?;

    let identify_config = identify::Behaviour::new(
        identify::Config::new(common::IDENTIFY_PROTO.into(), local_key.public())
            .with_interval(Duration::from_secs(60)),
    );

    let kademlia = create_kademlia_behavior(local_peer_id);

    let behaviour = Behaviour {
        gossipsub,
        identify: identify_config,
        kademlia,
        relay: relay::Behaviour::new(
            local_peer_id,
            relay::Config {
                max_reservations: usize::MAX,
                max_reservations_per_peer: 100,
                reservation_rate_limiters: Vec::default(),
                circuit_src_rate_limiters: Vec::default(),
                max_circuits: usize::MAX,
                max_circuits_per_peer: 100,
                ..Default::default()
            },
        ),
    };

    let swarm = libp2p::SwarmBuilder::with_existing_identity(local_key)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().port_reuse(true).nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_dns()?
        .with_behaviour(|_| behaviour)?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    Ok(swarm)
}
