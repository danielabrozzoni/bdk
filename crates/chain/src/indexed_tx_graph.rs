use core::{convert::Infallible, ops::AddAssign};

use bitcoin::{OutPoint, Script, Transaction, TxOut};

use crate::{
    keychain::Balance,
    sparse_chain::ChainPosition,
    tx_graph::{Additions, TxGraph, TxNode},
    BlockAnchor, ChainOracle, FullTxOut, ObservedIn, TxIndex,
};

/// An outwards-facing view of a transaction that is part of the *best chain*'s history.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TxInChain<'a, T, A> {
    /// Where the transaction is observed (in a block or in mempool).
    pub observed_in: ObservedIn<&'a A>,
    /// The transaction with anchors and last seen timestamp.
    pub tx: TxNode<'a, T, A>,
}

/// A structure that represents changes to an [`IndexedTxGraph`].
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub struct IndexedAdditions<A, IA> {
    /// [`TxGraph`] additions.
    pub graph_additions: Additions<A>,
    /// [`TxIndex`] additions.
    pub index_additions: IA,
    /// Last block height witnessed (if any).
    pub last_height: Option<u32>,
}

impl<A, IA: Default> Default for IndexedAdditions<A, IA> {
    fn default() -> Self {
        Self {
            graph_additions: Default::default(),
            index_additions: Default::default(),
            last_height: None,
        }
    }
}

impl<A: BlockAnchor, IA: AddAssign> AddAssign for IndexedAdditions<A, IA> {
    fn add_assign(&mut self, rhs: Self) {
        let Self {
            graph_additions,
            index_additions: index_delta,
            last_height,
        } = rhs;
        self.graph_additions.append(graph_additions);
        self.index_additions += index_delta;
        if self.last_height < last_height {
            let last_height =
                last_height.expect("must exist as it is larger than self.last_height");
            self.last_height.replace(last_height);
        }
    }
}

pub struct IndexedTxGraph<A, I> {
    graph: TxGraph<A>,
    index: I, // [TODO] Make public
    last_height: u32,
}

impl<A, I: Default> Default for IndexedTxGraph<A, I> {
    fn default() -> Self {
        Self {
            graph: Default::default(),
            index: Default::default(),
            last_height: u32::MIN,
        }
    }
}

impl<A: BlockAnchor, I: TxIndex> IndexedTxGraph<A, I> {
    /// Get a reference of the internal transaction graph.
    pub fn graph(&self) -> &TxGraph<A> {
        &self.graph
    }

    /// Get a reference of the internal transaction index.
    pub fn index(&self) -> &I {
        &self.index
    }

    /// Get a mutable reference to the internal transaction index.
    pub fn index_mut(&mut self) -> &mut I {
        &mut self.index
    }

    /// Applies the [`IndexedAdditions`] to the [`IndexedTxGraph`].
    pub fn apply_additions(&mut self, additions: IndexedAdditions<A, I::Additions>) {
        let IndexedAdditions {
            graph_additions,
            index_additions,
            last_height,
        } = additions;

        self.index.apply_additions(index_additions);

        for tx in &graph_additions.tx {
            self.index.index_tx(tx);
        }
        for (&outpoint, txout) in &graph_additions.txout {
            self.index.index_txout(outpoint, txout);
        }

        self.graph.apply_additions(graph_additions);

        if let Some(height) = last_height {
            self.last_height = height;
        }
    }

    fn insert_height_internal(&mut self, tip: u32) -> Option<u32> {
        if self.last_height < tip {
            self.last_height = tip;
            Some(tip)
        } else {
            None
        }
    }

    /// Insert a block height that the chain source has scanned up to.
    pub fn insert_height(&mut self, tip: u32) -> IndexedAdditions<A, I::Additions>
    where
        I::Additions: Default,
    {
        IndexedAdditions {
            last_height: self.insert_height_internal(tip),
            ..Default::default()
        }
    }

    /// Insert a `txout` that exists in `outpoint` with the given `observation`.
    pub fn insert_txout(
        &mut self,
        outpoint: OutPoint,
        txout: &TxOut,
        observation: ObservedIn<A>,
    ) -> IndexedAdditions<A, I::Additions> {
        let last_height = match &observation {
            ObservedIn::Block(anchor) => self.insert_height_internal(anchor.anchor_block().height),
            ObservedIn::Mempool(_) => None,
        };

        IndexedAdditions {
            graph_additions: {
                let mut graph_additions = self.graph.insert_txout(outpoint, txout.clone());
                graph_additions.append(match observation {
                    ObservedIn::Block(anchor) => self.graph.insert_anchor(outpoint.txid, anchor),
                    ObservedIn::Mempool(seen_at) => {
                        self.graph.insert_seen_at(outpoint.txid, seen_at)
                    }
                });
                graph_additions
            },
            index_additions: <I as TxIndex>::index_txout(&mut self.index, outpoint, txout),
            last_height,
        }
    }

    pub fn insert_tx(
        &mut self,
        tx: &Transaction,
        observation: ObservedIn<A>,
    ) -> IndexedAdditions<A, I::Additions> {
        let txid = tx.txid();

        let last_height = match &observation {
            ObservedIn::Block(anchor) => self.insert_height_internal(anchor.anchor_block().height),
            ObservedIn::Mempool(_) => None,
        };

        IndexedAdditions {
            graph_additions: {
                let mut graph_additions = self.graph.insert_tx(tx.clone());
                graph_additions.append(match observation {
                    ObservedIn::Block(anchor) => self.graph.insert_anchor(txid, anchor),
                    ObservedIn::Mempool(seen_at) => self.graph.insert_seen_at(txid, seen_at),
                });
                graph_additions
            },
            index_additions: <I as TxIndex>::index_tx(&mut self.index, tx),
            last_height,
        }
    }

    pub fn filter_and_insert_txs<'t, T>(
        &mut self,
        txs: T,
        observation: ObservedIn<A>,
    ) -> IndexedAdditions<A, I::Additions>
    where
        T: Iterator<Item = &'t Transaction>,
        I::Additions: Default + AddAssign,
    {
        txs.filter_map(|tx| {
            if self.index.is_tx_relevant(tx) {
                Some(self.insert_tx(tx, observation.clone()))
            } else {
                None
            }
        })
        .fold(IndexedAdditions::default(), |mut acc, other| {
            acc += other;
            acc
        })
    }

    /// Get the last block height that we are synced up to.
    pub fn last_height(&self) -> u32 {
        self.last_height
    }

    // [TODO] Have to methods, one for relevant-only, and one for any. Have one in `TxGraph`.
    pub fn try_list_chain_txs<'a, C>(
        &'a self,
        chain: C,
    ) -> impl Iterator<Item = Result<TxInChain<'a, Transaction, A>, C::Error>>
    where
        C: ChainOracle + 'a,
    {
        self.graph
            .full_transactions()
            .filter(|tx| self.index.is_tx_relevant(tx))
            .filter_map(move |tx| {
                self.graph
                    .try_get_chain_position(&chain, tx.txid)
                    .map(|v| v.map(|observed_in| TxInChain { observed_in, tx }))
                    .transpose()
            })
    }

    pub fn list_chain_txs<'a, C>(
        &'a self,
        chain: C,
    ) -> impl Iterator<Item = TxInChain<'a, Transaction, A>>
    where
        C: ChainOracle<Error = Infallible> + 'a,
    {
        self.try_list_chain_txs(chain)
            .map(|r| r.expect("error is infallible"))
    }

    pub fn try_list_chain_txouts<'a, C>(
        &'a self,
        chain: C,
    ) -> impl Iterator<Item = Result<FullTxOut<ObservedIn<A>>, C::Error>> + 'a
    where
        C: ChainOracle + 'a,
        ObservedIn<A>: ChainPosition,
    {
        self.graph
            .all_txouts()
            .filter(|(_, txo)| self.index.is_spk_owned(&txo.script_pubkey))
            .filter_map(move |(op, txout)| -> Option<Result<_, C::Error>> {
                let graph_tx = self.graph.get_tx(op.txid)?;

                let is_on_coinbase = graph_tx.is_coin_base();

                let chain_position = match self.graph.try_get_chain_position(&chain, op.txid) {
                    Ok(Some(observed_at)) => observed_at.into_owned(),
                    Ok(None) => return None,
                    Err(err) => return Some(Err(err)),
                };

                let spent_by = match self.graph.try_get_spend_in_chain(&chain, op) {
                    Ok(Some((obs, txid))) => Some((obs.into_owned(), txid)),
                    Ok(None) => None,
                    Err(err) => return Some(Err(err)),
                };

                let full_txout = FullTxOut {
                    outpoint: op,
                    txout: txout.clone(),
                    chain_position,
                    spent_by,
                    is_on_coinbase,
                };

                Some(Ok(full_txout))
            })
    }

    pub fn list_chain_txouts<'a, C>(
        &'a self,
        chain: C,
    ) -> impl Iterator<Item = FullTxOut<ObservedIn<A>>> + 'a
    where
        C: ChainOracle<Error = Infallible> + 'a,
        ObservedIn<A>: ChainPosition,
    {
        self.try_list_chain_txouts(chain)
            .map(|r| r.expect("error in infallible"))
    }

    /// Return relevant unspents.
    pub fn try_list_chain_utxos<'a, C>(
        &'a self,
        chain: C,
    ) -> impl Iterator<Item = Result<FullTxOut<ObservedIn<A>>, C::Error>> + 'a
    where
        C: ChainOracle + 'a,
        ObservedIn<A>: ChainPosition,
    {
        self.try_list_chain_txouts(chain)
            .filter(|r| !matches!(r, Ok(txo) if txo.spent_by.is_none()))
    }

    pub fn list_chain_utxos<'a, C>(
        &'a self,
        chain: C,
    ) -> impl Iterator<Item = FullTxOut<ObservedIn<A>>> + 'a
    where
        C: ChainOracle<Error = Infallible> + 'a,
        ObservedIn<A>: ChainPosition,
    {
        self.try_list_chain_utxos(chain)
            .map(|r| r.expect("error is infallible"))
    }

    pub fn try_balance<C, F>(
        &self,
        chain: C,
        tip: u32,
        mut should_trust: F,
    ) -> Result<Balance, C::Error>
    where
        C: ChainOracle,
        ObservedIn<A>: ChainPosition + Clone,
        F: FnMut(&Script) -> bool,
    {
        let mut immature = 0;
        let mut trusted_pending = 0;
        let mut untrusted_pending = 0;
        let mut confirmed = 0;

        for res in self.try_list_chain_txouts(&chain) {
            let txout = res?;

            match &txout.chain_position {
                ObservedIn::Block(_) => {
                    if txout.is_on_coinbase {
                        if txout.is_mature(tip) {
                            confirmed += txout.txout.value;
                        } else {
                            immature += txout.txout.value;
                        }
                    }
                }
                ObservedIn::Mempool(_) => {
                    if should_trust(&txout.txout.script_pubkey) {
                        trusted_pending += txout.txout.value;
                    } else {
                        untrusted_pending += txout.txout.value;
                    }
                }
            }
        }

        Ok(Balance {
            immature,
            trusted_pending,
            untrusted_pending,
            confirmed,
        })
    }

    pub fn balance<C, F>(&self, chain: C, tip: u32, should_trust: F) -> Balance
    where
        C: ChainOracle<Error = Infallible>,
        ObservedIn<A>: ChainPosition + Clone,
        F: FnMut(&Script) -> bool,
    {
        self.try_balance(chain, tip, should_trust)
            .expect("error is infallible")
    }

    pub fn try_balance_at<C>(&self, chain: C, height: u32) -> Result<u64, C::Error>
    where
        C: ChainOracle,
        ObservedIn<A>: ChainPosition + Clone,
    {
        let mut sum = 0;
        for txo_res in self.try_list_chain_txouts(chain) {
            let txo = txo_res?;
            if txo.is_spendable_at(height) {
                sum += txo.txout.value;
            }
        }
        Ok(sum)
    }

    pub fn balance_at<C>(&self, chain: C, height: u32) -> u64
    where
        C: ChainOracle<Error = Infallible>,
        ObservedIn<A>: ChainPosition + Clone,
    {
        self.try_balance_at(chain, height)
            .expect("error is infallible")
    }
}