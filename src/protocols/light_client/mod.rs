//! Client-side implementation for CKB light client protocol.
//!
//! TODO(light-client) More documentation.

use std::collections::HashMap;
use std::sync::Arc;

use ckb_chain_spec::consensus::Consensus;
use ckb_network::{bytes::Bytes, CKBProtocolContext, CKBProtocolHandler, PeerIndex};
use ckb_pow::PowEngine;
use ckb_types::{
    core::BlockNumber, packed, prelude::*, utilities::merkle_mountain_range::VerifiableHeader, U256,
};
use log::{debug, error, info, trace, warn};

mod components;
pub mod constant;
mod peers;
mod prelude;
mod sampling;

use prelude::*;

pub(crate) use self::peers::{LastState, PeerState, Peers, ProveState};
use super::{
    status::{Status, StatusCode},
    BAD_MESSAGE_BAN_TIME,
};
use crate::protocols::LAST_N_BLOCKS;
use crate::storage::Storage;

pub struct LightClientProtocol {
    storage: Storage,
    peers: Arc<Peers>,
    consensus: Consensus,
    best: Option<(PeerIndex, ProveState)>,
}

impl CKBProtocolHandler for LightClientProtocol {
    fn init(&mut self, nc: Arc<dyn CKBProtocolContext + Sync>) {
        info!("LightClient.protocol initialized");
        nc.set_notify(
            constant::REFRESH_PEERS_DURATION,
            constant::REFRESH_PEERS_TOKEN,
        )
        .expect("set_notify should be ok");
    }

    fn connected(
        &mut self,
        nc: Arc<dyn CKBProtocolContext + Sync>,
        peer: PeerIndex,
        version: &str,
    ) {
        info!("LightClient({}).connected peer={}", version, peer);
        self.peers().add_peer(peer);
        self.get_last_state(nc.as_ref(), peer);
    }

    fn disconnected(&mut self, _nc: Arc<dyn CKBProtocolContext + Sync>, peer: PeerIndex) {
        info!("LightClient.disconnected peer={}", peer);
        self.peers().remove_peer(peer);
    }

    fn received(&mut self, nc: Arc<dyn CKBProtocolContext + Sync>, peer: PeerIndex, data: Bytes) {
        trace!("LightClient.received peer={}", peer);

        let msg = match packed::LightClientMessageReader::from_slice(&data) {
            Ok(msg) => msg.to_enum(),
            _ => {
                warn!(
                    "LightClient.received a malformed message from Peer({})",
                    peer
                );
                nc.ban_peer(
                    peer,
                    BAD_MESSAGE_BAN_TIME,
                    String::from("send us a malformed message"),
                );
                return;
            }
        };

        let item_name = msg.item_name();
        let status = self.try_process(nc.as_ref(), peer, msg.clone());
        if status.is_ok()
            && matches!(
                msg,
                packed::LightClientMessageUnionReader::SendBlockSamples(_)
            )
        {
            self.update_best_state();
        }

        if let Some(ban_time) = status.should_ban() {
            error!(
                "process {} from {}, ban {:?} since result is {}",
                item_name, peer, ban_time, status
            );
            nc.ban_peer(peer, ban_time, status.to_string());
        } else if status.should_warn() {
            warn!("process {} from {}, result is {}", item_name, peer, status);
        } else if !status.is_ok() {
            debug!("process {} from {}, result is {}", item_name, peer, status);
        }
    }

    fn notify(&mut self, nc: Arc<dyn CKBProtocolContext + Sync>, token: u64) {
        match token {
            constant::REFRESH_PEERS_TOKEN => {
                self.refresh_all_peers(nc.as_ref());
            }
            _ => unreachable!(),
        }
    }
}

impl LightClientProtocol {
    fn try_process(
        &mut self,
        nc: &dyn CKBProtocolContext,
        peer: PeerIndex,
        message: packed::LightClientMessageUnionReader<'_>,
    ) -> Status {
        match message {
            packed::LightClientMessageUnionReader::SendLastState(reader) => {
                components::SendLastStateProcess::new(reader, self, peer, nc).execute()
            }
            packed::LightClientMessageUnionReader::SendBlockSamples(reader) => {
                components::SendBlockSamplesProcess::new(reader, self, peer, nc).execute()
            }
            _ => StatusCode::UnexpectedProtocolMessage.into(),
        }
    }

    fn get_last_state(&self, nc: &dyn CKBProtocolContext, peer: PeerIndex) {
        let content = packed::GetLastState::new_builder().build();
        let message = packed::LightClientMessage::new_builder()
            .set(content)
            .build();
        nc.reply(peer, &message);
    }

    fn get_block_samples(&self, nc: &dyn CKBProtocolContext, peer: PeerIndex) {
        if let Some(content) = self
            .peers()
            .get_state(&peer)
            .expect("checked: should have state")
            .get_prove_request()
            .map(|request| request.get_request().to_owned())
        {
            let message = packed::LightClientMessage::new_builder()
                .set(content)
                .build();
            nc.reply(peer, &message);
        }
    }
}

impl LightClientProtocol {
    pub(crate) fn new(storage: Storage, peers: Arc<Peers>, consensus: Consensus) -> Self {
        Self {
            storage,
            peers,
            consensus,
            best: None,
        }
    }

    pub(crate) fn mmr_activated_number(&self) -> BlockNumber {
        self.consensus.hardfork_switch().mmr_activated_number()
    }
    pub(crate) fn pow_engine(&self) -> Arc<dyn PowEngine> {
        self.consensus.pow_engine()
    }

    pub(crate) fn peers(&self) -> &Peers {
        &self.peers
    }

    fn refresh_all_peers(&mut self, nc: &dyn CKBProtocolContext) {
        let now = faketime::unix_time_as_millis();
        for peer in self.peers().get_peers_which_require_updating() {
            self.get_block_samples(nc, peer);
            self.peers().update_timestamp(peer, now);
        }
        self.update_best_state();
    }

    fn update_best_state(&mut self) {
        let last_best = self.best.take();
        let mut best: Option<(PeerIndex, ProveState)> = None;
        for (curr_peer, curr_state) in self.peers().get_peers_which_are_proved() {
            if best.is_none() {
                best = Some((curr_peer, curr_state));
            } else {
                let best_total_difficulty = best
                    .as_ref()
                    .map(|(_, state)| state.get_total_difficulty())
                    .expect("checkd: best is not None");
                let curr_total_difficulty = curr_state.get_total_difficulty();
                if curr_total_difficulty > best_total_difficulty {
                    best = Some((curr_peer, curr_state));
                }
            }
        }
        if let Some((_, prove_state)) = best.as_ref() {
            let reorg_last_headers = prove_state.get_reorg_last_headers();
            if !reorg_last_headers.is_empty() {
                if let Some((_, last_prove_state)) = last_best.as_ref() {
                    let last_headers: HashMap<_, _> = last_prove_state
                        .get_last_headers()
                        .into_iter()
                        .map(|header| (header.number(), header))
                        .collect();
                    for reorg_header in reorg_last_headers.into_iter().rev() {
                        if last_headers
                            .get(&reorg_header.number())
                            .map(|header| *header != reorg_header)
                            .unwrap_or(true)
                        {
                            trace!("rollback block#{}", reorg_header.number());
                            self.storage
                                .rollback_filtered_transactions(reorg_header.number());
                        } else {
                            break;
                        }
                    }
                } else {
                    let tip_number: u64 = self.storage.get_tip_header().raw().number().unpack();
                    for i in 0..LAST_N_BLOCKS {
                        if tip_number < 1 + i {
                            break;
                        }
                        trace!("rollback block#{}", tip_number - i);
                        self.storage.rollback_filtered_transactions(tip_number - i);
                    }
                }
            }
            self.storage.update_last_state(
                prove_state.get_total_difficulty(),
                &prove_state.get_last_header().header().data(),
            );
        }
        self.best = best;
    }

    fn build_prove_request_content(
        &self,
        peer_state: &PeerState,
        last_header: &VerifiableHeader,
        last_total_difficulty: &U256,
    ) -> packed::GetBlockSamples {
        let (start_hash, start_number, start_total_difficulty) = peer_state
            .get_prove_state()
            .map(|inner| {
                (
                    inner.get_last_header().header().hash(),
                    inner.get_last_header().header().number(),
                    inner.get_total_difficulty().to_owned(),
                )
            })
            .unwrap_or_else(|| {
                let (total_difficulty, last_tip) = self.storage.get_last_state();
                (
                    last_tip.calc_header_hash(),
                    last_tip.raw().number().unpack(),
                    total_difficulty,
                )
            });
        let last_number = last_header.header().number();
        let (difficulty_boundary, difficulties) = sampling::sample_blocks(
            start_number,
            &start_total_difficulty,
            last_number,
            last_total_difficulty,
        );
        packed::GetBlockSamples::new_builder()
            .last_hash(last_header.header().hash())
            .start_hash(start_hash)
            .start_number(start_number.pack())
            .difficulty_boundary(difficulty_boundary.pack())
            .difficulties(difficulties.into_iter().map(|inner| inner.pack()).pack())
            .build()
    }
}
