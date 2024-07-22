use anyhow::Result;
use libp2p::{
    identity,
    kad::{self, store::MemoryStore},
    PeerId, StreamProtocol,
};
use log::info;
use std::{collections::HashSet, path::Path};
use tokio::fs;

pub const GOSSIPSUB_RELAYED_PEERS_TOPIC: &str = "psyche-relayed-peers";
pub const GOSSIPSUB_CHAT_TOPIC: &str = "test-app";
pub const GOSSIPSUB_CHAT_FILE_TOPIC: &str = "test-app-file";
pub const IDENTIFY_PROTO: &str = &"/test-app/0.1.0";

pub fn create_relayed_peers_message(peers: &HashSet<PeerId>) -> Vec<u8> {
    let num_peers = peers.len();
    let mut msg = Vec::new();
    msg.extend_from_slice(&(num_peers as u32).to_le_bytes());
    for peer in peers.iter() {
        let peer_bytes = peer.to_bytes();
        msg.extend_from_slice(&(peer_bytes.len() as u32).to_le_bytes());
        msg.extend_from_slice(&peer_bytes);
        println!("encoding multiaddr: {}, {:?}", peer_bytes.len(), peer_bytes);
    }
    msg
}

pub fn parse_relayed_peers_message(data: &[u8]) -> Vec<PeerId> {
    let num_multiaddrs = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let mut multiaddrs = Vec::with_capacity(num_multiaddrs);
    let mut offset = 4;
    while offset < data.len() {
        let multiaddr_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        offset += 4;
        let multiaddr_end = offset + multiaddr_len as usize;
        let multiaddr_bytes = data[offset..multiaddr_end].to_vec();
        offset = multiaddr_end;
        offset += multiaddr_end;
        println!(
            "decoding multiaddr: {}, {:?}",
            multiaddr_len, multiaddr_bytes
        );
        let multiaddr = PeerId::try_from(multiaddr_bytes).unwrap();
        multiaddrs.push(multiaddr);
    }
    multiaddrs
}

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

    let store = MemoryStore::new(local_peer_id);
    let kademlia = kad::Behaviour::with_config(local_peer_id, store, cfg);
    kademlia
}
