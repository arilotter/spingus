use anyhow::Result;
use libp2p::{
    identity,
    kad::{self, store::MemoryStore},
    Multiaddr, PeerId, StreamProtocol,
};
use log::info;
use std::path::Path;
use tokio::fs;

pub const GOSSIPSUB_CHAT_TOPIC: &str = "test-app";
pub const GOSSIPSUB_CHAT_FILE_TOPIC: &str = "test-app-file";
pub const IDENTIFY_PROTO: &str = &"/test-app/0.1.0";

pub async fn read_or_create_identity(path: &Path) -> Result<identity::Keypair> {
    if path.exists() {
        let bytes = fs::read(&path).await?;

        info!("Using existing identity from {}", path.display());

        return Ok(identity::Keypair::from_protobuf_encoding(&bytes)?); // This only works for ed25519 but that is what we are using.
    }

    let identity = identity::Keypair::generate_ed25519();

    fs::write(&path, &identity.to_protobuf_encoding()?).await?;

    info!("Generated new identity and wrote it to {}", path.display());

    Ok(identity)
}

const KADEMLIA_PROTOCOL_NAME: StreamProtocol = StreamProtocol::new("/ipfs/kad/1.0.0");

pub fn create_kademlia_behavior(local_peer_id: PeerId) -> kad::Behaviour<MemoryStore> {
    let mut cfg = kad::Config::default();
    cfg.set_protocol_names(vec![KADEMLIA_PROTOCOL_NAME]);
    cfg.set_caching(kad::Caching::Enabled { max_peers: 100 });

    let store = MemoryStore::new(local_peer_id);
    let kademlia = kad::Behaviour::with_config(local_peer_id, store, cfg);
    kademlia
}

const PEER_ADDR_KEY_PREFIX: &[u8] = b"p_addrs";

pub fn get_peer_key(peer_id: &PeerId) -> kad::record::Key {
    let bytes: Vec<u8> = PEER_ADDR_KEY_PREFIX
        .into_iter()
        .copied()
        .chain(peer_id.to_bytes())
        .collect();
    kad::record::Key::from(bytes)
}

pub fn get_peer_id_from_key(key: &kad::record::Key) -> PeerId {
    PeerId::from_bytes(&key.as_ref()[PEER_ADDR_KEY_PREFIX.len()..]).unwrap()
}

pub fn encode_multiaddrs(addrs: &[Multiaddr]) -> Vec<u8> {
    let addrs_as_bytes = addrs.iter().map(|addr| addr.to_vec()).collect::<Vec<_>>();
    bincode::serialize(&addrs_as_bytes).unwrap()
}

pub fn decode_multiaddrs(bytes: &[u8]) -> Vec<Multiaddr> {
    let addrs_as_bytes: Vec<Vec<u8>> = bincode::deserialize(bytes).unwrap();
    addrs_as_bytes
        .into_iter()
        .map(|addr| Multiaddr::try_from(addr).unwrap())
        .collect::<Vec<_>>()
}
