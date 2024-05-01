//! Contains the pipeline implementation.

use std::sync::{mpsc, Arc, RwLock};
use alloy_primitives::Bytes;
use eyre::Result;

use crate::{config::Config, engine::PayloadAttributes};

use crate::derive::{
    stages::{
        attributes::Attributes,
        batcher_transactions::{BatcherTransactionMessage, BatcherTransactions},
        batches::Batches,
        channels::Channels,
    },
    state::State,
    purgeable::PurgeableIterator,
};

/// The derivation pipeline is iterated on to update attributes for new blocks.
pub struct Pipeline {
    /// A channel sender to send a `BatcherTransactionMessage`
    batcher_transaction_sender: mpsc::Sender<BatcherTransactionMessage>,
    /// An `Attributes` object
    attributes: Attributes,
    /// Pending `PayloadAttributes`
    pending_attributes: Option<PayloadAttributes>,
}

impl Iterator for Pipeline {
    type Item = PayloadAttributes;

    /// Returns the pending [PayloadAttributes].
    /// If none exist it will call `Attributes::next()` to advance to the next block and return those attributes instead.
    fn next(&mut self) -> Option<Self::Item> {
        if self.pending_attributes.is_some() {
            self.pending_attributes.take()
        } else {
            self.attributes.next()
        }
    }
}

impl Pipeline {
    /// Creates a new [Pipeline] and initializes [BatcherTransactions], [Channels], [Batches], and [Attributes]
    pub fn new(state: Arc<RwLock<State>>, config: Arc<Config>, seq: u64) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let batcher_transactions = BatcherTransactions::new(rx);
        let channels = Channels::new(batcher_transactions, config.clone());
        let batches = Batches::new(channels, state.clone(), config.clone());
        let attributes = Attributes::new(Box::new(batches), state, config, seq);

        Ok(Self {
            batcher_transaction_sender: tx,
            attributes,
            pending_attributes: None,
        })
    }

    /// Sends [BatcherTransactions] & the L1 block they were received in to the [BatcherTransactions] receiver.
    pub fn push_batcher_transactions(
        &self,
        txs: Vec<Bytes>,
        l1_origin: u64,
    ) -> Result<()> {
        let txs = txs.into_iter().map(Bytes::from).collect();
        self.batcher_transaction_sender
            .send(BatcherTransactionMessage { txs, l1_origin })?;
        Ok(())
    }

    /// Returns a reference to the pending [PayloadAttributes].
    /// If none are pending, it will call `self.next()` to advance to the next block and return those attributes instead.
    pub fn peek(&mut self) -> Option<&PayloadAttributes> {
        if self.pending_attributes.is_none() {
            let next_attributes = self.next();
            self.pending_attributes = next_attributes;
        }

        self.pending_attributes.as_ref()
    }

    /// Resets the state of `self.attributes` by calling `Attributes::purge()`
    pub fn purge(&mut self) -> Result<()> {
        self.attributes.purge();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        sync::{Arc, RwLock},
    };

    use ethers::{
        providers::{Middleware, Provider},
        types::H256,
        utils::keccak256,
    };

    use crate::{
        common::RawTransaction,
        config::{ChainConfig, Config},
        derive::*,
        l1::{BlockUpdate, ChainWatcher},
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn test_attributes_match() {
        if std::env::var("L1_TEST_RPC_URL").is_ok() && std::env::var("L2_TEST_RPC_URL").is_ok() {
            let rpc = env::var("L1_TEST_RPC_URL").unwrap();
            let l2_rpc = env::var("L2_TEST_RPC_URL").unwrap();

            let config = Arc::new(Config {
                l1_rpc_url: rpc.to_string(),
                l1_beacon_url: String::new(),
                l2_rpc_url: l2_rpc.to_string(),
                chain: ChainConfig::optimism_goerli(),
                l2_engine_url: String::new(),
                jwt_secret: String::new(),
                checkpoint_sync_url: None,
                rpc_port: 9545,
                rpc_addr: "127.0.0.1".to_string(),
                devnet: false,
            });

            let mut chain_watcher = ChainWatcher::new(
                config.chain.l1_start_epoch.number,
                config.chain.l2_genesis.number,
                config.clone(),
            )
            .unwrap();

            chain_watcher.start().unwrap();

            let provider = Provider::try_from(env::var("L2_TEST_RPC_URL").unwrap()).unwrap();
            let state = Arc::new(RwLock::new(
                State::new(
                    config.chain.l2_genesis,
                    config.chain.l1_start_epoch,
                    &provider,
                    config.clone(),
                )
                .await,
            ));

            let mut pipeline = Pipeline::new(state.clone(), config.clone(), 0).unwrap();

            chain_watcher.recv_from_channel().await.unwrap();
            let update = chain_watcher.recv_from_channel().await.unwrap();

            let l1_info = match update {
                BlockUpdate::NewBlock(block) => *block,
                _ => panic!("wrong update type"),
            };

            pipeline
                .push_batcher_transactions(
                    l1_info.batcher_transactions.clone(),
                    l1_info.block_info.number,
                )
                .unwrap();

            state.write().unwrap().update_l1_info(l1_info);

            if let Some(payload) = pipeline.next() {
                let hashes = get_tx_hashes(&payload.transactions.unwrap());
                let expected_hashes = get_expected_hashes(config.chain.l2_genesis.number + 1).await;

                assert_eq!(hashes, expected_hashes);
            }
        }
    }

    async fn get_expected_hashes(block_num: u64) -> Vec<H256> {
        let provider = Provider::try_from(env::var("L2_TEST_RPC_URL").unwrap()).unwrap();

        provider
            .get_block(block_num)
            .await
            .unwrap()
            .unwrap()
            .transactions
    }

    fn get_tx_hashes(txs: &[RawTransaction]) -> Vec<H256> {
        txs.iter()
            .map(|tx| H256::from_slice(&keccak256(&tx.0)))
            .collect()
    }
}
