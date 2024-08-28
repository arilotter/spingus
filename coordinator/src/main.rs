use anyhow::{Context, Result};
use clap::Parser;
use common::{create_kademlia_behavior, read_or_create_identity, DisTrOAck, DisTrOData};
use futures::future::{select, Either};
use futures::StreamExt;
use libp2p::kad::store::RecordStore;
use libp2p::kad::Record;
use libp2p::request_response::ProtocolSupport;
use libp2p::StreamProtocol;
use libp2p::{
    identify, identity,
    kad::{self, record::store::MemoryStore},
    multiaddr::{Multiaddr, Protocol},
    relay,
    swarm::{NetworkBehaviour, Swarm, SwarmEvent},
    PeerId,
};
use log::{debug, info};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, List, ListItem};
use ratatui::Terminal;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::stdout;
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
const TICK_INTERVAL: Duration = Duration::from_secs(1);
const PORT_QUIC: u16 = 9091;
const LOCAL_KEY_PATH: &str = "./local_key";
const ROLLING_AVERAGE_WINDOW: usize = 30;
const BANDWIDTH_GRAPH_SIZE: usize = 60;

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
    identify: identify::Behaviour,
    kademlia: kad::Behaviour<MemoryStore>,
    relay: relay::Behaviour,
    direct_message: common::DisTrOBehavior,
}

#[tokio::main]
async fn main() -> Result<()> {
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

    info!("starting main loop");

    let mut tick = futures_timer::Delay::new(TICK_INTERVAL);
    let mut last_tick = Instant::now();

    let (tx, mut rx) = mpsc::channel(100);

    tokio::spawn(async move {
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout())).unwrap();
        terminal.clear().unwrap();
        loop {
            let stats = rx.recv().await.expect("Failed to receive stats");
            draw_tui(&mut terminal, &stats).expect("Failed to draw TUI");
        }
    });

    let connected_clients = Arc::new(Mutex::new(HashSet::new()));
    let log_messages = Arc::new(Mutex::new(VecDeque::new()));
    let mut data_received_per_tick: HashMap<PeerId, VecDeque<usize>> = Default::default();
    let mut bandwidth_history = VecDeque::with_capacity(BANDWIDTH_GRAPH_SIZE);
    let log_messages2 = log_messages.clone();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(move |_, record| {
            let output = format!(
                "[{}] {}: {}",
                record.module_path().unwrap_or("?"),
                record.level(),
                record.args()
            );
            log_messages2.lock().unwrap().push_back(output);
            Ok(())
        })
        .init();
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
                    connected_clients.lock().unwrap().insert(peer_id);
                    data_received_per_tick.entry(peer_id).or_default();
                }
                SwarmEvent::ConnectionClosed {
                    peer_id,
                    cause,
                    endpoint,
                    ..
                } => {
                    let addr = endpoint.get_remote_address();
                    info!("Connection to {peer_id} closed: {cause:?} thru endpoint {addr}");
                    swarm.behaviour_mut().kademlia.remove_peer(&peer_id);
                    connected_clients.lock().unwrap().remove(&peer_id);
                    data_received_per_tick.remove(&peer_id);
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
                    BehaviourEvent::DirectMessage(req) => {
                        if let libp2p::request_response::Event::Message {
                            peer,
                            message:
                                libp2p::request_response::Message::Request {
                                    request_id: _,
                                    request: DisTrOData::Data(data),
                                    channel,
                                },
                        } = req
                        {
                            let message_size = data.len();
                            info!("got message from {peer}, {}", convert_bytes(message_size));
                            data_received_per_tick
                                .entry(peer)
                                .or_insert_with(|| VecDeque::new())
                                .push_back(message_size);
                            let _ = swarm
                                .behaviour_mut()
                                .direct_message
                                .send_response(channel, DisTrOAck::Ack);
                        }
                    }
                    _ => debug!("Other behaviour event: {:?}", event),
                },
                event => {
                    debug!("Other type of event: {:?}", event);
                }
            },
            Either::Right(_) => {
                let mut data_per_sec_per_client = HashMap::new();
                let mut total_data_per_sec = 0.0;
                for (peer_id, data_received) in data_received_per_tick.iter_mut() {
                    while data_received.len() > ROLLING_AVERAGE_WINDOW {
                        data_received.pop_front();
                    }
                    let data_received_sum: f64 =
                        data_received.iter().map(|c| *c as f64).sum::<f64>()
                            / data_received.len() as f64;
                    let avg_data_per_tick =
                        data_received_sum as f64 / (Instant::now() - last_tick).as_secs_f64();
                    let avg_data_per_second = avg_data_per_tick * TICK_INTERVAL.as_secs_f64();
                    data_per_sec_per_client.insert(*peer_id, avg_data_per_second);
                    total_data_per_sec += avg_data_per_second;
                }

                bandwidth_history.push_back(total_data_per_sec);
                if bandwidth_history.len() > BANDWIDTH_GRAPH_SIZE {
                    bandwidth_history.pop_front();
                }
                let lm_len = log_messages.lock().unwrap().len();
                let to_remove = lm_len.saturating_sub(25);
                if to_remove > 0 {
                    log_messages.lock().unwrap().drain(0..to_remove);
                }

                let stats = Stats {
                    connected_clients: connected_clients.lock().unwrap().iter().cloned().collect(),
                    data_per_sec_per_client,
                    total_data_per_sec,
                    log_messages: log_messages.lock().unwrap().iter().cloned().collect(),
                    bandwidth_history: bandwidth_history.clone(),
                };

                tx.send(stats).await.expect("Failed to send stats");

                last_tick = Instant::now();
                tick = futures_timer::Delay::new(TICK_INTERVAL);
            }
        }
    }
}

fn create_swarm(local_key: identity::Keypair) -> Result<Swarm<Behaviour>> {
    let local_peer_id = PeerId::from(local_key.public());
    debug!("Local peer id: {local_peer_id}");

    let identify_config = identify::Behaviour::new(
        identify::Config::new(common::IDENTIFY_PROTO.into(), local_key.public())
            .with_interval(Duration::from_secs(60)),
    );

    let kademlia = create_kademlia_behavior(local_peer_id);

    let behaviour = Behaviour {
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
        direct_message: common::DisTrOBehavior::new(
            [(
                StreamProtocol::new(common::REQ_RES_PROTOCOL),
                ProtocolSupport::Full,
            )],
            Default::default(),
        ),
    };

    let swarm = libp2p::SwarmBuilder::with_existing_identity(local_key)
        .with_tokio()
        .with_quic()
        .with_dns()?
        .with_behaviour(|_| behaviour)?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    Ok(swarm)
}

struct Stats {
    connected_clients: Vec<PeerId>,
    data_per_sec_per_client: HashMap<PeerId, f64>,
    total_data_per_sec: f64,
    log_messages: Vec<String>,
    bandwidth_history: VecDeque<f64>,
}

fn draw_tui(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    stats: &Stats,
) -> Result<()> {
    terminal.draw(|f| {
        let size = f.area();

        let block = Block::default()
            .title("Relay Node Stats")
            .borders(Borders::ALL);
        f.render_widget(block, size);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(
                [
                    Constraint::Percentage(20),
                    Constraint::Percentage(20),
                    Constraint::Percentage(20),
                    Constraint::Percentage(20),
                    Constraint::Percentage(20),
                ]
                .as_ref(),
            )
            .split(size);

        let connected_clients = List::new(
            stats
                .connected_clients
                .iter()
                .map(|peer_id| ListItem::new(format!("{}", peer_id))),
        )
        .block(
            Block::default()
                .title("Connected Clients")
                .borders(Borders::ALL),
        );
        f.render_widget(connected_clients, chunks[0]);

        let data_per_sec_per_client = List::new(stats.data_per_sec_per_client.iter().map(
            |(peer_id, data_per_sec)| {
                ListItem::new(format!("{}: {}/s", peer_id, convert_bytes(*data_per_sec)))
            },
        ))
        .block(
            Block::default()
                .title("Data Received per Second per Client")
                .borders(Borders::ALL),
        );
        f.render_widget(data_per_sec_per_client, chunks[1]);

        let bw_history = stats
            .bandwidth_history
            .iter()
            .enumerate()
            .map(|(x, y)| (x as f64, *y))
            .collect::<Vec<_>>();
        let bandwidth_graph = Chart::new(vec![Dataset::default()
            .name("Bandwidth")
            .graph_type(GraphType::Line)
            .data(&bw_history)])
        .block(
            Block::default()
                .title(format!(
                    "Bandwidth {}",
                    convert_bytes(stats.total_data_per_sec)
                ))
                .borders(Borders::ALL),
        )
        .x_axis(Axis::default().title("Time").labels(vec!["0", "30", "60"]))
        .y_axis(Axis::default().title("Bandwidth (bytes/s)").labels(vec![
            convert_bytes(0.0),
            convert_bytes(5.0 * 1024.0 * 1024.0),
        ]));
        f.render_widget(bandwidth_graph, chunks[2]);

        let log_messages = List::new(
            stats
                .log_messages
                .iter()
                .map(|msg| ListItem::new(msg.clone())),
        )
        .block(Block::default().title("Log Messages").borders(Borders::ALL));
        f.render_widget(log_messages, chunks[3]);
    })?;
    Ok(())
}

fn convert_bytes(bytes: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    const PB: f64 = TB * 1024.0;

    if bytes < KB {
        format!("{} B", bytes)
    } else if bytes < MB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes < TB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes < PB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else {
        format!("{:.2} PB", bytes as f64 / PB as f64)
    }
}
