use anyhow::{Context, Result};
use bitcoin::{BlockHash, Transaction, Txid};
use bitcoin_slices::{
    bsl::{self, FindTransaction},
    Error::VisitBreak,
    Visit,
};
use std::collections::HashMap;

use crate::{
    cache::Cache,
    chain::Chain,
    config::Config,
    daemon::Daemon,
    db::DBStore,
    index::Index,
    mempool::{FeeHistogram, Mempool},
    metrics::Metrics,
    signals::ExitFlag,
    status::{Balance, ScriptHashStatus, UnspentEntry},
};

/// Electrum protocol subscriptions' tracker
pub struct Tracker {
    index: Index,
    mempool: Mempool,
    metrics: Metrics,
    ignore_mempool: bool,
    pub silent_payments_index: bool,
}

pub(crate) enum Error {
    NotReady,
}

impl Tracker {
    pub fn new(config: &Config, metrics: Metrics) -> Result<Self> {
        let store = DBStore::open(
            &config.db_path,
            config.db_log_dir.as_deref(),
            config.auto_reindex,
        )?;
        let chain = Chain::new(config.network);
        Ok(Self {
            index: Index::load(
                store,
                chain,
                &metrics,
                config.index_batch_size,
                config.index_lookup_limit,
                config.reindex_last_blocks,
            )
            .context("failed to open index")?,
            mempool: Mempool::new(&metrics),
            metrics,
            ignore_mempool: config.ignore_mempool,
            silent_payments_index: config.silent_payments_index,
        })
    }

    pub(crate) fn chain(&self) -> &Chain {
        self.index.chain()
    }

    pub(crate) fn fees_histogram(&self) -> &FeeHistogram {
        self.mempool.fees_histogram()
    }

    pub(crate) fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub(crate) fn get_unspent(&self, status: &ScriptHashStatus) -> Vec<UnspentEntry> {
        status.get_unspent(self.index.chain())
    }

    pub(crate) fn sync(&mut self, daemon: &Daemon, exit_flag: &ExitFlag) -> Result<bool> {
        let mut done = self.index.sync(daemon, exit_flag)?;
        if done && self.silent_payments_index {
            done = self.index.silent_payments_sync(daemon, exit_flag)?;
        }
        if done && !self.ignore_mempool {
            self.mempool.sync(daemon, exit_flag);
            // TODO: double check tip - and retry on diff
        }
        Ok(done)
    }

    pub(crate) fn status(&self) -> Result<(), Error> {
        if self.index.is_ready() {
            return Ok(());
        }
        Err(Error::NotReady)
    }

    pub(crate) fn sp_status(&self) -> Result<(), Error> {
        if self.index.is_sp_ready() {
            return Ok(());
        }
        Err(Error::NotReady)
    }

    pub(crate) fn update_scripthash_status(
        &self,
        status: &mut ScriptHashStatus,
        daemon: &Daemon,
        cache: &Cache,
    ) -> Result<bool> {
        let prev_statushash = status.statushash();
        status.sync(&self.index, &self.mempool, daemon, cache)?;
        Ok(prev_statushash != status.statushash())
    }

    pub(crate) fn get_balance(&self, status: &ScriptHashStatus) -> Balance {
        status.get_balance(self.chain())
    }

    pub(crate) fn lookup_transaction(
        &self,
        daemon: &Daemon,
        txid: Txid,
    ) -> Result<Option<(BlockHash, Transaction)>> {
        // Note: there are two blocks with coinbase transactions having same txid (see BIP-30)
        let blockhashes = self.index.filter_by_txid(txid);
        let mut result = None;
        daemon.for_blocks(blockhashes, |blockhash, block| {
            if result.is_some() {
                return; // keep first matching transaction
            }
            let mut visitor = FindTransaction::new(txid);
            result = match bsl::Block::visit(&block, &mut visitor) {
                Ok(_) | Err(VisitBreak) => visitor.tx_found().map(|tx| (blockhash, tx)),
                Err(e) => panic!("core returned invalid block: {:?}", e),
            };
        })?;
        Ok(result)
    }

    pub(crate) fn get_tweaks(&self, height: usize) -> Result<HashMap<u64, Vec<String>>> {
        let tweaks: Vec<(u64, Vec<String>)> = self.index.get_tweaks(height as u64).collect();
        let mut res: HashMap<u64, Vec<String>> = HashMap::new();
        for (height, tweaks) in tweaks {
            res.entry(height).or_insert_with(Vec::new).extend(tweaks)
        }
        Ok(res)
    }
}
