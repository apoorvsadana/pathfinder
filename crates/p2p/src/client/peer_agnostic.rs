//! _High level_ client for p2p interaction.
//! Frees the caller from managing peers manually.
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc as fmpsc;
use futures::{Stream, StreamExt};
use libp2p::PeerId;
use p2p_proto::class::{ClassesRequest, ClassesResponse};
use p2p_proto::common::{Direction, Iteration};
use p2p_proto::event::{EventsRequest, EventsResponse};
use p2p_proto::header::{BlockHeadersRequest, BlockHeadersResponse};
use p2p_proto::state::{
    ContractDiff,
    ContractStoredValue,
    DeclaredClass,
    StateDiffsRequest,
    StateDiffsResponse,
};
use p2p_proto::transaction::{TransactionWithReceipt, TransactionsRequest, TransactionsResponse};
use pathfinder_common::event::Event;
use pathfinder_common::state_update::{ContractClassUpdate, StateUpdateData};
use pathfinder_common::transaction::TransactionVariant;
use pathfinder_common::{
    BlockNumber,
    CasmHash,
    ClassHash,
    ContractAddress,
    ContractNonce,
    SierraHash,
    SignedBlockHeader,
    StateDiffCommitment,
    StorageAddress,
    StorageValue,
    TransactionCommitment,
    TransactionHash,
    TransactionIndex,
};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;
pub mod traits;

use traits::{
    BlockClient,
    ClassStream,
    EventStream,
    HeaderStream,
    StateDiffStream,
    TransactionStream,
};

use crate::client::conv::{CairoDefinition, FromDto, SierraDefinition, TryFromDto};
use crate::client::peer_aware;
use crate::client::types::{
    ClassDefinition,
    ClassDefinitionsError,
    EventsForBlockByTransaction,
    IncorrectStateDiffCount,
    Receipt,
    UnverifiedStateUpdateData,
    UnverifiedTransactionData,
    UnverifiedTransactionDataWithBlockNumber,
};
use crate::peer_data::PeerData;

#[derive(Clone, Debug)]
pub struct Client {
    inner: peer_aware::Client,
    block_propagation_topic: Arc<String>,
    peers: Arc<RwLock<Decaying<HashSet<PeerId>>>>,
}

impl Client {
    pub fn new(inner: peer_aware::Client, block_propagation_topic: String) -> Self {
        Self {
            inner,
            block_propagation_topic: Arc::new(block_propagation_topic),
            peers: Default::default(),
        }
    }

    // Propagate new L2 head head
    pub async fn propagate_new_head(
        &self,
        block_id: p2p_proto::common::BlockId,
    ) -> anyhow::Result<()> {
        tracing::debug!(number=%block_id.number, hash=%block_id.hash.0, topic=%self.block_propagation_topic,
            "Propagating head"
        );

        self.inner
            .publish(
                &self.block_propagation_topic,
                p2p_proto::header::NewBlock::Id(block_id),
            )
            .await
    }

    async fn get_random_peers(&self) -> Vec<PeerId> {
        use rand::seq::SliceRandom;

        let r = self.peers.read().await;
        let mut peers = if let Some(peers) = r.get() {
            peers.iter().copied().collect::<Vec<_>>()
        } else {
            // Avoid deadlock
            drop(r);
            let mut w = self.peers.write().await;
            // Check again because the previous lock in the queue might have been a write
            // lock that has already updated the peers.
            if let Some(peers) = w.get() {
                return peers.iter().copied().collect::<Vec<_>>();
            }

            let mut peers = self
                .inner
                .get_closest_peers(PeerId::random())
                .await
                .unwrap_or_default();

            // We could be on the list
            peers.remove(self.inner.peer_id());

            let peers_vec = peers.iter().copied().collect::<Vec<_>>();

            w.update(peers);
            peers_vec
        };
        peers.shuffle(&mut rand::thread_rng());
        peers
    }
}

impl HeaderStream for Client {
    fn header_stream(
        self,
        start: BlockNumber,
        stop: BlockNumber,
        reverse: bool,
    ) -> impl Stream<Item = PeerData<SignedBlockHeader>> {
        let inner = self.inner.clone();
        let outer = self;
        header_stream::make(
            start,
            stop,
            reverse,
            move || {
                let outer = outer.clone();
                async move { outer.get_random_peers().await }
            },
            move |peer, request| {
                let inner = inner.clone();
                async move { inner.send_headers_sync_request(peer, request).await }
            },
        )
    }
}

impl TransactionStream for Client {
    fn transaction_stream(
        self,
        start: BlockNumber,
        stop: BlockNumber,
        transaction_counts_and_commitments_stream: impl Stream<Item = anyhow::Result<(usize, TransactionCommitment)>>
            + Send
            + 'static,
    ) -> impl Stream<Item = PeerData<(UnverifiedTransactionData, BlockNumber)>> {
        let inner = self.inner.clone();
        let outer = self;
        transaction_stream::make(
            start,
            stop,
            transaction_counts_and_commitments_stream,
            move || {
                let outer = outer.clone();
                async move { outer.get_random_peers().await }
            },
            move |peer, request| {
                let inner = inner.clone();
                async move { inner.send_transactions_sync_request(peer, request).await }
            },
        )
    }
}

impl StateDiffStream for Client {
    /// ### Important
    ///
    /// Contract class updates are by default set to
    /// `ContractClassUpdate::Deploy` but __the caller is responsible for
    /// determining if the class was really deployed or replaced__.
    fn state_diff_stream(
        self,
        start: BlockNumber,
        stop: BlockNumber,
        state_diff_length_and_commitment_stream: impl Stream<Item = anyhow::Result<(usize, StateDiffCommitment)>>
            + Send
            + 'static,
    ) -> impl Stream<Item = PeerData<(UnverifiedStateUpdateData, BlockNumber)>> {
        let inner = self.inner.clone();
        let outer = self;
        state_diff_stream::make(
            start,
            stop,
            state_diff_length_and_commitment_stream,
            move || {
                let outer = outer.clone();
                async move { outer.get_random_peers().await }
            },
            move |peer, request| {
                let inner = inner.clone();
                async move { inner.send_state_diffs_sync_request(peer, request).await }
            },
        )
    }
}

impl ClassStream for Client {
    fn class_stream(
        self,
        start: BlockNumber,
        stop: BlockNumber,
        declared_class_counts_stream: impl Stream<Item = anyhow::Result<usize>> + Send + 'static,
    ) -> impl Stream<Item = PeerData<ClassDefinition>> {
        let inner = self.inner.clone();
        let outer = self;
        class_definition_stream::make(
            start,
            stop,
            declared_class_counts_stream,
            move || {
                let outer = outer.clone();
                async move { outer.get_random_peers().await }
            },
            move |peer, request| {
                let inner = inner.clone();
                async move { inner.send_classes_sync_request(peer, request).await }
            },
        )
    }
}

impl EventStream for Client {
    /// ### Important
    ///
    /// Events are grouped by block and by transaction. The order of flattened
    /// events in a block is guaranteed to be correct because the event
    /// commitment is part of block hash. However the number of events per
    /// transaction for __pre 0.13.2__ Starknet blocks is __TRUSTED__
    /// because neither signature nor block hash contain this information.
    fn event_stream(
        self,
        start: BlockNumber,
        stop: BlockNumber,
        event_counts_stream: impl Stream<Item = anyhow::Result<usize>> + Send + 'static,
    ) -> impl Stream<Item = PeerData<EventsForBlockByTransaction>> {
        let inner = self.inner.clone();
        let outer = self;
        event_stream::make(
            start,
            stop,
            event_counts_stream,
            move || {
                let outer = outer.clone();
                async move { outer.get_random_peers().await }
            },
            move |peer, request| {
                let inner = inner.clone();
                async move { inner.send_events_sync_request(peer, request).await }
            },
        )
    }
}

impl BlockClient for Client {
    async fn transactions_for_block(
        self,
        block: BlockNumber,
    ) -> Option<(
        PeerId,
        impl Stream<Item = anyhow::Result<(TransactionVariant, Receipt)>>,
    )> {
        let request = TransactionsRequest {
            iteration: Iteration {
                start: block.get().into(),
                direction: Direction::Forward,
                limit: 1,
                step: 1.into(),
            },
        };

        let peers = self.get_random_peers().await;

        for peer in peers {
            let Ok(stream) = self
                .inner
                .send_transactions_sync_request(peer, request)
                .await
                .inspect_err(|error| tracing::debug!(%peer, %error, "Transactions request failed"))
            else {
                continue;
            };

            let stream = stream
                .take_while(|x| std::future::ready(!matches!(x, &TransactionsResponse::Fin)))
                .enumerate()
                .map(|(i, x)| -> anyhow::Result<_> {
                    match x {
                        TransactionsResponse::Fin => unreachable!("Already handled Fin above"),
                        TransactionsResponse::TransactionWithReceipt(tx_with_receipt) => Ok((
                            TransactionVariant::try_from_dto(tx_with_receipt.transaction)?,
                            Receipt::try_from((
                                tx_with_receipt.receipt,
                                TransactionIndex::new(i.try_into().unwrap())
                                    .ok_or_else(|| anyhow::anyhow!("Invalid transaction index"))?,
                            ))?,
                        )),
                    }
                });

            return Some((peer, stream));
        }

        None
    }

    async fn state_diff_for_block(
        self,
        block: BlockNumber,
        state_diff_length: u64,
    ) -> Result<Option<(PeerId, StateUpdateData)>, IncorrectStateDiffCount> {
        let request = StateDiffsRequest {
            iteration: Iteration {
                start: block.get().into(),
                direction: Direction::Forward,
                limit: 1,
                step: 1.into(),
            },
        };

        let peers = self.get_random_peers().await;

        for peer in peers {
            let Ok(mut stream) = self
                .inner
                .send_state_diffs_sync_request(peer, request)
                .await
                .inspect_err(|error| tracing::debug!(%peer, %error, "State diffs request failed"))
            else {
                continue;
            };

            let mut current_count = state_diff_length;
            let mut state_diff = StateUpdateData::default();

            while let Some(resp) = stream.next().await {
                match resp {
                    StateDiffsResponse::ContractDiff(ContractDiff {
                        address,
                        nonce,
                        class_hash,
                        values,
                        domain: _,
                    }) => {
                        match current_count.checked_sub(values.len().try_into().unwrap()) {
                            Some(x) => current_count = x,
                            None => {
                                tracing::debug!(%peer, "Too many storage diffs: {} > {}", values.len(), current_count);
                                return Err(IncorrectStateDiffCount(peer));
                            }
                        }
                        let address = ContractAddress(address.0);
                        if address == ContractAddress::ONE {
                            let storage = &mut state_diff
                                .system_contract_updates
                                .entry(address)
                                .or_default()
                                .storage;
                            values
                                .into_iter()
                                .for_each(|ContractStoredValue { key, value }| {
                                    storage.insert(StorageAddress(key), StorageValue(value));
                                });
                        } else {
                            let update =
                                &mut state_diff.contract_updates.entry(address).or_default();
                            values
                                .into_iter()
                                .for_each(|ContractStoredValue { key, value }| {
                                    update
                                        .storage
                                        .insert(StorageAddress(key), StorageValue(value));
                                });

                            if let Some(nonce) = nonce {
                                match current_count.checked_sub(1) {
                                    Some(x) => current_count = x,
                                    None => {
                                        tracing::debug!(%peer, "Too many nonce updates");
                                        return Err(IncorrectStateDiffCount(peer));
                                    }
                                }
                                update.nonce = Some(ContractNonce(nonce));
                            }

                            if let Some(class_hash) = class_hash.map(|x| ClassHash(x.0)) {
                                match current_count.checked_sub(1) {
                                    Some(x) => current_count = x,
                                    None => {
                                        tracing::debug!(%peer, "Too many deployed contracts");
                                        return Err(IncorrectStateDiffCount(peer));
                                    }
                                }
                                update.class = Some(ContractClassUpdate::Deploy(class_hash));
                            }
                        }
                    }
                    StateDiffsResponse::DeclaredClass(DeclaredClass {
                        class_hash,
                        compiled_class_hash,
                    }) => {
                        match current_count.checked_sub(1) {
                            Some(x) => current_count = x,
                            None => {
                                tracing::debug!(%peer, "Too many declared classes");
                                return Err(IncorrectStateDiffCount(peer));
                            }
                        }
                        if let Some(compiled_class_hash) = compiled_class_hash {
                            state_diff
                                .declared_sierra_classes
                                .insert(SierraHash(class_hash.0), CasmHash(compiled_class_hash.0));
                        } else {
                            state_diff
                                .declared_cairo_classes
                                .insert(ClassHash(class_hash.0));
                        }
                    }
                    StateDiffsResponse::Fin => {
                        if current_count != 0 {
                            tracing::debug!(%peer, "Too few storage diffs");
                            return Err(IncorrectStateDiffCount(peer));
                        }
                        return Ok(Some((peer, state_diff)));
                    }
                }
            }
        }

        Ok(None)
    }

    async fn class_definitions_for_block(
        self,
        block: BlockNumber,
        declared_classes_count: u64,
    ) -> Result<Option<(PeerId, Vec<ClassDefinition>)>, ClassDefinitionsError> {
        let request = ClassesRequest {
            iteration: Iteration {
                start: block.get().into(),
                direction: Direction::Forward,
                limit: 1,
                step: 1.into(),
            },
        };

        let peers = self.get_random_peers().await;

        for peer in peers {
            let Ok(mut stream) = self
                .inner
                .send_classes_sync_request(peer, request)
                .await
                .inspect_err(|error| tracing::debug!(%peer, %error, "State diffs request failed"))
            else {
                continue;
            };

            let mut current_count = declared_classes_count;
            let mut class_definitions = Vec::new();

            while let Some(resp) = stream.next().await {
                match resp {
                    ClassesResponse::Class(p2p_proto::class::Class::Cairo0 {
                        class,
                        domain: _,
                    }) => {
                        let definition = CairoDefinition::try_from_dto(class)
                            .map_err(|_| ClassDefinitionsError::CairoDefinitionError(peer))?;
                        class_definitions.push(ClassDefinition::Cairo {
                            block_number: block,
                            definition: definition.0,
                        });
                    }
                    ClassesResponse::Class(p2p_proto::class::Class::Cairo1 {
                        class,
                        domain: _,
                    }) => {
                        let definition = SierraDefinition::try_from_dto(class)
                            .map_err(|_| ClassDefinitionsError::SierraDefinitionError(peer))?;
                        class_definitions.push(ClassDefinition::Sierra {
                            block_number: block,
                            sierra_definition: definition.0,
                        });
                    }
                    ClassesResponse::Fin => {
                        tracing::debug!(%peer, "Received FIN in class definitions source");
                        break;
                    }
                }

                current_count = match current_count.checked_sub(1) {
                    Some(x) => x,
                    None => {
                        tracing::debug!(%peer, "Too many class definitions");
                        return Err(ClassDefinitionsError::IncorrectClassDefinitionCount(peer));
                    }
                };
            }

            if current_count != 0 {
                tracing::debug!(%peer, "Too few class definitions");
                return Err(ClassDefinitionsError::IncorrectClassDefinitionCount(peer));
            }

            return Ok(Some((peer, class_definitions)));
        }

        Ok(None)
    }

    async fn events_for_block(
        self,
        block: BlockNumber,
    ) -> Option<(PeerId, impl Stream<Item = (TransactionHash, Event)>)> {
        let request = EventsRequest {
            iteration: Iteration {
                start: block.get().into(),
                direction: Direction::Forward,
                limit: 1,
                step: 1.into(),
            },
        };

        let peers = self.get_random_peers().await;

        for peer in peers {
            let Ok(stream) = self
                .inner
                .send_events_sync_request(peer, request)
                .await
                .inspect_err(|error| tracing::debug!(%peer, %error, "Events request failed"))
            else {
                continue;
            };

            let stream = stream
                .take_while(|x| std::future::ready(!matches!(x, &EventsResponse::Fin)))
                .map(|x| match x {
                    EventsResponse::Fin => unreachable!("Already handled Fin above"),
                    EventsResponse::Event(event) => (
                        TransactionHash(event.transaction_hash.0),
                        Event::from_dto(event),
                    ),
                });

            return Some((peer, stream));
        }

        None
    }
}

mod header_stream {
    use super::*;

    pub fn make<PF, RF>(
        start: BlockNumber,
        stop: BlockNumber,
        reverse: bool,
        get_peers: impl Fn() -> PF + Send + 'static,
        send_request: impl Fn(PeerId, BlockHeadersRequest) -> RF + Send + 'static,
    ) -> impl Stream<Item = PeerData<SignedBlockHeader>>
    where
        PF: Future<Output = Vec<PeerId>> + Send,
        RF: Future<Output = anyhow::Result<fmpsc::Receiver<BlockHeadersResponse>>> + Send,
    {
        let start: i64 = start.get().try_into().expect("block number <= i64::MAX");
        let stop: i64 = stop.get().try_into().expect("block number <= i64::MAX");

        let (mut start, stop, dir) = match reverse {
            true => (stop, start, Direction::Backward),
            false => (start, stop, Direction::Forward),
        };

        tracing::trace!(?start, ?stop, ?dir, "Streaming headers");

        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            // Loop which refreshes peer set once we exhaust it.
            'outer: loop {
                'next_peer: for peer in get_peers().await {
                    let limit = start.max(stop) - start.min(stop) + 1;

                    let request = BlockHeadersRequest {
                        iteration: Iteration {
                            start: u64::try_from(start).expect("start >= 0").into(),
                            direction: dir,
                            limit: limit.try_into().expect("limit >= 0"),
                            step: 1.into(),
                        },
                    };

                    let mut responses = match send_request(peer, request).await {
                        Ok(x) => x,
                        Err(error) => {
                            tracing::debug!(%peer, reason=%error, "Headers request failed");
                            continue 'next_peer;
                        }
                    };

                    while let Some(r) = responses.next().await {
                        match handle_response(peer, r, dir, &mut start, stop, tx.clone()).await {
                            Action::NextResponse => {}
                            Action::NextPeer => continue 'next_peer,
                            Action::TerminateStream => break 'outer,
                        }
                    }

                    if done(dir, start, stop) {
                        tracing::debug!(%peer, "Header stream Fin missing");
                        break 'outer;
                    }

                    // TODO: track how much and how fast this peer responded
                    // with i.e. don't let them drip feed us etc.
                }
            }
        });

        ReceiverStream::new(rx)
    }

    async fn handle_response(
        peer: PeerId,
        signed_header: BlockHeadersResponse,
        direction: Direction,
        start: &mut i64,
        stop: i64,
        tx: mpsc::Sender<PeerData<SignedBlockHeader>>,
    ) -> Action {
        match signed_header {
            BlockHeadersResponse::Header(hdr) => match SignedBlockHeader::try_from_dto(*hdr) {
                Ok(hdr) => {
                    if done(direction, *start, stop) {
                        tracing::debug!(%peer, "Header stream Fin missing, got extra header instead");
                        return Action::TerminateStream;
                    }

                    _ = tx.send(PeerData::new(peer, hdr)).await;

                    *start = match direction {
                        Direction::Forward => *start + 1,
                        Direction::Backward => *start - 1,
                    };

                    Action::NextResponse
                }
                Err(error) => {
                    tracing::debug!(%peer, %error, "Header stream failed");
                    if done(direction, *start, stop) {
                        return Action::TerminateStream;
                    }

                    Action::NextPeer
                }
            },
            BlockHeadersResponse::Fin => {
                tracing::debug!(%peer, "Header stream Fin");
                if done(direction, *start, stop) {
                    return Action::TerminateStream;
                }

                Action::NextPeer
            }
        }
    }

    enum Action {
        NextResponse,
        NextPeer,
        TerminateStream,
    }

    fn done(direction: Direction, start: i64, stop: i64) -> bool {
        match direction {
            Direction::Forward => start > stop,
            Direction::Backward => start < stop,
        }
    }
}

mod transaction_stream {
    use super::*;

    /// ### Important
    ///
    /// Caller must guarantee `start <= stop`
    pub fn make<PF, RF>(
        mut start: BlockNumber,
        stop: BlockNumber,
        counts_and_commitments_stream: impl Stream<Item = anyhow::Result<(usize, TransactionCommitment)>>
            + Send
            + 'static,
        get_peers: impl Fn() -> PF + Send + 'static,
        send_request: impl Fn(PeerId, TransactionsRequest) -> RF + Send + 'static,
    ) -> impl Stream<Item = PeerData<UnverifiedTransactionDataWithBlockNumber>>
    where
        PF: Future<Output = Vec<PeerId>> + Send,
        RF: Future<Output = anyhow::Result<fmpsc::Receiver<TransactionsResponse>>> + Send,
    {
        tracing::trace!(?start, ?stop, "Streaming Transactions");

        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            let mut counts_and_commitments_stream = Box::pin(counts_and_commitments_stream);

            let Some(Ok(cnt)) = counts_and_commitments_stream.next().await else {
                tracing::debug!("Transaction counts and commitments stream terminated prematurely");
                return;
            };

            // Transaction counter for the currently received block
            let mut progress = TransactionStreamProgress::new(cnt);

            // Loop which refreshes peer set once we exhaust it.
            'outer: loop {
                'next_peer: for peer in get_peers().await {
                    let mut responses = match send_request(peer, make_request(start, stop)).await {
                        Ok(x) => x,
                        Err(error) => {
                            tracing::debug!(%peer, reason=%error, "Transactions request failed");
                            continue 'next_peer;
                        }
                    };

                    let mut transactions = Vec::new();
                    // If the previous peer failed to provide the entire block we need to start over
                    progress.rollback();
                    tracing::trace!(block_number=%start, num_responses=%progress.count(), "Expecting");

                    while let Some(resp) = responses.next().await {
                        let txn_idx = into_idx(transactions.len());
                        match handle_response(peer, resp, &mut progress, txn_idx, start == stop)
                            .await
                        {
                            Action::NextPeer => continue 'next_peer,
                            Action::TerminateStream => break 'outer,
                            Action::TryYield(txn) => {
                                transactions.push(*txn);
                                if try_yield(
                                    peer,
                                    &mut progress,
                                    &mut counts_and_commitments_stream,
                                    &mut transactions,
                                    &mut start,
                                    stop,
                                    tx.clone(),
                                )
                                .await
                                {
                                    break 'outer;
                                }
                                // Move to the next response
                            }
                        }
                    }

                    // The current peer is not giving us a Fin
                    // TODO punish the peer
                    tracing::debug!(%peer, "Fin missing");

                    if progress.count() == 0 && start == stop {
                        // The last block we were looking for was not followed by a Fin
                        break 'outer;
                    }
                }
            }
        });

        ReceiverStream::new(rx)
    }

    async fn handle_response(
        peer: PeerId,
        response: TransactionsResponse,
        progress: &mut TransactionStreamProgress,
        txn_idx: TransactionIndex,
        is_last_block: bool,
    ) -> Action {
        match response {
            TransactionsResponse::TransactionWithReceipt(TransactionWithReceipt {
                transaction,
                receipt,
            }) => {
                let (Ok(t), Ok(r)) = (
                    TransactionVariant::try_from_dto(transaction),
                    Receipt::try_from((receipt, txn_idx)),
                ) else {
                    // TODO punish the peer
                    tracing::debug!(%peer, "Transaction or receipt failed to parse");
                    return Action::NextPeer;
                };

                match progress.count_mut().checked_sub(1) {
                    Some(x) => {
                        *progress.count_mut() = x;
                        Action::TryYield(Box::new((t, r)))
                    }
                    None => {
                        // TODO punish the peer
                        tracing::debug!(%peer, "Too many transactions");
                        // We can only get here in case of the last block, which means that the
                        // stream should be terminated
                        debug_assert!(is_last_block);
                        Action::TerminateStream
                    }
                }
            }
            TransactionsResponse::Fin => {
                if progress.count() > 0 {
                    // TODO punish the peer
                    tracing::debug!(%peer, "Premature transaction stream Fin");
                    return Action::NextPeer;
                }

                if is_last_block {
                    return Action::TerminateStream;
                }

                // This peer will not give us more blocks, move to the next peer
                Action::NextPeer
            }
        }
    }

    fn make_request(start: BlockNumber, stop: BlockNumber) -> TransactionsRequest {
        TransactionsRequest {
            iteration: Iteration {
                start: start.get().into(),
                direction: Direction::Forward,
                limit: stop.get() - start.get() + 1,
                step: 1.into(),
            },
        }
    }

    fn into_idx(len: usize) -> TransactionIndex {
        TransactionIndex::new_or_panic(len.try_into().expect("ptr size is 64bits"))
    }

    /// ### Important
    ///
    /// Returns true if the stream should be terminated
    async fn try_yield(
        peer: PeerId,
        progress: &mut TransactionStreamProgress,
        counts_and_commitments_stream: &mut (impl Stream<Item = anyhow::Result<(usize, TransactionCommitment)>>
                  + Unpin
                  + Send
                  + 'static),
        transactions: &mut Vec<(TransactionVariant, Receipt)>,
        start: &mut BlockNumber,
        stop: BlockNumber,
        tx: mpsc::Sender<PeerData<UnverifiedTransactionDataWithBlockNumber>>,
    ) -> bool {
        if progress.count() == 0 {
            tracing::trace!(block_number=%start, "All transactions received for block");

            _ = tx
                .send(PeerData::new(
                    peer,
                    (
                        UnverifiedTransactionData {
                            expected_commitment: progress.commitment(),
                            transactions: std::mem::take(transactions),
                        },
                        *start,
                    ),
                ))
                .await;

            if *start < stop {
                tracing::trace!(next_block=%start, "Moving to next block");
                *start += 1;

                if let Some(Ok(x)) = counts_and_commitments_stream.next().await {
                    *progress = TransactionStreamProgress::new(x);
                } else {
                    tracing::debug!(%peer, "Transaction counts and commitments stream terminated prematurely");
                    return true;
                };
            }
        }

        false
    }

    enum Action {
        NextPeer,
        TerminateStream,
        TryYield(Box<(TransactionVariant, Receipt)>),
    }

    #[derive(Clone, Copy, Debug)]
    struct TransactionStreamProgress {
        count: usize,
        commitment: TransactionCommitment,
        count_backup: usize,
    }

    impl TransactionStreamProgress {
        fn new((count, commitment): (usize, TransactionCommitment)) -> Self {
            Self {
                count,
                commitment,
                count_backup: count,
            }
        }

        fn count(&self) -> usize {
            self.count
        }

        fn count_mut(&mut self) -> &mut usize {
            &mut self.count
        }

        fn commitment(&self) -> TransactionCommitment {
            self.commitment
        }

        fn rollback(&mut self) -> Self {
            self.count = self.count_backup;
            *self
        }
    }
}

mod state_diff_stream {
    use super::*;

    /// ### Important
    ///
    /// Caller must guarantee `start <= stop`
    pub fn make<PF, RF>(
        mut start: BlockNumber,
        stop: BlockNumber,
        len_and_commitment_stream: impl Stream<Item = anyhow::Result<(usize, StateDiffCommitment)>>
            + Send
            + 'static,
        get_peers: impl Fn() -> PF + Send + 'static,
        send_request: impl Fn(PeerId, StateDiffsRequest) -> RF + Send + 'static,
    ) -> impl Stream<Item = PeerData<(UnverifiedStateUpdateData, BlockNumber)>>
    where
        PF: Future<Output = Vec<PeerId>> + Send,
        RF: Future<Output = anyhow::Result<fmpsc::Receiver<StateDiffsResponse>>> + Send,
    {
        tracing::trace!(?start, ?stop, "Streaming state diffs");

        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            let mut len_and_commitment_stream = Box::pin(len_and_commitment_stream);

            let Some(Ok(cnt)) = len_and_commitment_stream.next().await else {
                tracing::debug!("Transaction counts and commitments stream terminated prematurely");
                return;
            };

            let mut progress = StateDiffStreamProgress::new(cnt);

            // Loop which refreshes peer set once we exhaust it.
            'outer: loop {
                'next_peer: for peer in get_peers().await {
                    let mut responses = match send_request(peer, make_request(start, stop)).await {
                        Ok(x) => x,
                        Err(error) => {
                            tracing::debug!(%peer, reason=%error, "State diff request failed");
                            continue 'next_peer;
                        }
                    };

                    // If the previous peer failed to provide the entire block we need to start over
                    progress.rollback();
                    tracing::trace!(block_number=%start, num_responses=%progress.count(), "Expecting");

                    let mut state_diff = StateUpdateData::default();

                    while let Some(state_diff_response) = responses.next().await {
                        match handle_response(
                            peer,
                            state_diff_response,
                            &mut state_diff,
                            &mut progress,
                            start == stop,
                        ) {
                            Action::NextPeer => continue 'next_peer,
                            Action::TerminateStream => break 'outer,
                            Action::TryYield => {
                                if try_yield(
                                    peer,
                                    &mut progress,
                                    &mut len_and_commitment_stream,
                                    &mut state_diff,
                                    &mut start,
                                    stop,
                                    tx.clone(),
                                )
                                .await
                                {
                                    break 'outer;
                                }
                                // Move to the next response
                            }
                        }
                    }

                    // The current peer is not giving us a Fin
                    // TODO punish the peer
                    tracing::debug!(%peer, "Fin missing");

                    if progress.count() == 0 && start == stop {
                        // The last block we were looking for was not followed by a Fin
                        break 'outer;
                    }
                }
            }
        });

        ReceiverStream::new(rx)
    }

    fn handle_response(
        peer: PeerId,
        response: StateDiffsResponse,
        state_diff: &mut StateUpdateData,
        progress: &mut StateDiffStreamProgress,
        is_last_block: bool,
    ) -> Action {
        match response {
            StateDiffsResponse::ContractDiff(ContractDiff {
                address,
                nonce,
                class_hash,
                values,
                domain: _,
            }) => {
                let address = ContractAddress(address.0);

                match progress.count_mut().checked_sub(values.len()) {
                    Some(x) => *progress.count_mut() = x,
                    None => {
                        // TODO punish the peer
                        tracing::debug!(%peer, "Too many storage diffs: {} > {}", values.len(), progress.count());
                        // We can only get here in case of the last block, which means that the
                        // stream should be terminated
                        debug_assert!(is_last_block);
                        return Action::TerminateStream;
                    }
                }

                if address == ContractAddress::ONE {
                    let storage = &mut state_diff
                        .system_contract_updates
                        .entry(address)
                        .or_default()
                        .storage;
                    values
                        .into_iter()
                        .for_each(|ContractStoredValue { key, value }| {
                            storage.insert(StorageAddress(key), StorageValue(value));
                        });

                    Action::TryYield
                } else {
                    let update = &mut state_diff.contract_updates.entry(address).or_default();
                    values
                        .into_iter()
                        .for_each(|ContractStoredValue { key, value }| {
                            update
                                .storage
                                .insert(StorageAddress(key), StorageValue(value));
                        });

                    if let Some(nonce) = nonce {
                        match progress.count_mut().checked_sub(1) {
                            Some(x) => *progress.count_mut() = x,
                            None => {
                                // TODO punish the peer
                                tracing::debug!(%peer, "Too many nonce updates");
                                // We can only get here in case of the last block, which means that
                                // the stream should be terminated
                                debug_assert!(is_last_block);
                                return Action::TerminateStream;
                            }
                        }

                        update.nonce = Some(ContractNonce(nonce));
                    }

                    if let Some(class_hash) = class_hash.map(|x| ClassHash(x.0)) {
                        match progress.count_mut().checked_sub(1) {
                            Some(x) => *progress.count_mut() = x,
                            None => {
                                // TODO punish the peer
                                tracing::debug!(%peer, "Too many deployed contracts");
                                // We can only get here in case of the last block, which means that
                                // the stream should be terminated
                                debug_assert!(is_last_block);
                                return Action::TerminateStream;
                            }
                        }

                        update.class = Some(ContractClassUpdate::Deploy(class_hash));
                    }

                    Action::TryYield
                }
            }
            StateDiffsResponse::DeclaredClass(DeclaredClass {
                class_hash,
                compiled_class_hash,
            }) => {
                if let Some(compiled_class_hash) = compiled_class_hash {
                    state_diff
                        .declared_sierra_classes
                        .insert(SierraHash(class_hash.0), CasmHash(compiled_class_hash.0));
                } else {
                    state_diff
                        .declared_cairo_classes
                        .insert(ClassHash(class_hash.0));
                }

                match progress.count_mut().checked_sub(1) {
                    Some(x) => {
                        *progress.count_mut() = x;
                        Action::TryYield
                    }
                    None => {
                        // TODO punish the peer
                        tracing::debug!(%peer, "Too many declared classes");

                        // We can only get here in case of the last block, which
                        // means that the stream should be terminated
                        debug_assert!(is_last_block);
                        Action::TerminateStream
                    }
                }
            }
            StateDiffsResponse::Fin => {
                if progress.count() > 0 {
                    // TODO punish the peer
                    tracing::debug!(%peer, "Premature state diff stream Fin");
                    return Action::NextPeer;
                }

                if is_last_block {
                    return Action::TerminateStream;
                }

                // This peer will not give us more blocks
                Action::TryYield
            }
        }
    }

    fn make_request(start: BlockNumber, stop: BlockNumber) -> StateDiffsRequest {
        StateDiffsRequest {
            iteration: Iteration {
                start: start.get().into(),
                direction: Direction::Forward,
                limit: stop.get() - start.get() + 1,
                step: 1.into(),
            },
        }
    }

    enum Action {
        NextPeer,
        TerminateStream,
        TryYield,
    }

    /// ### Important
    ///
    /// Returns true if the stream should be terminated
    async fn try_yield(
        peer: PeerId,
        progress: &mut StateDiffStreamProgress,
        state_diff_length_and_commitment_stream: &mut (impl Stream<Item = anyhow::Result<(usize, StateDiffCommitment)>>
                  + Unpin
                  + Send
                  + 'static),
        state_diff: &mut StateUpdateData,
        start: &mut BlockNumber,
        stop: BlockNumber,
        tx: mpsc::Sender<PeerData<(UnverifiedStateUpdateData, BlockNumber)>>,
    ) -> bool {
        if progress.count() == 0 {
            // All the counters for this block have been exhausted which means
            // that the state update for this block is complete.
            tracing::trace!(block_number=%start, "State diff received for block");

            _ = tx
                .send(PeerData::new(
                    peer,
                    (
                        UnverifiedStateUpdateData {
                            expected_commitment: progress.commitment(),
                            state_diff: std::mem::take(state_diff),
                        },
                        *start,
                    ),
                ))
                .await;

            if *start < stop {
                // Move to the next block
                *start += 1;
                tracing::trace!(next_block=%start, "Moving to next block");

                let Some(Ok(cnt)) = state_diff_length_and_commitment_stream.next().await else {
                    tracing::debug!(
                        "Transaction counts and commitments stream terminated prematurely"
                    );

                    return true;
                };

                *progress = StateDiffStreamProgress::new(cnt);

                tracing::trace!(block_number=%start, num_responses=%progress.count(), "Expecting");
            }
        }

        false
    }

    #[derive(Clone, Copy, Debug)]
    struct StateDiffStreamProgress {
        count: usize,
        commitment: StateDiffCommitment,
        count_backup: usize,
    }

    impl StateDiffStreamProgress {
        fn new((count, commitment): (usize, StateDiffCommitment)) -> Self {
            Self {
                count,
                commitment,
                count_backup: count,
            }
        }

        fn count(&self) -> usize {
            self.count
        }

        fn count_mut(&mut self) -> &mut usize {
            &mut self.count
        }

        fn commitment(&self) -> StateDiffCommitment {
            self.commitment
        }

        fn rollback(&mut self) -> Self {
            self.count = self.count_backup;
            *self
        }
    }
}

mod class_definition_stream {
    use super::*;

    pub fn make<PF, RF>(
        mut start: BlockNumber,
        stop: BlockNumber,
        counts_stream: impl Stream<Item = anyhow::Result<usize>> + Send + 'static,
        get_peers: impl Fn() -> PF + Send + 'static,
        send_request: impl Fn(PeerId, ClassesRequest) -> RF + Send + 'static,
    ) -> impl Stream<Item = PeerData<ClassDefinition>>
    where
        PF: Future<Output = Vec<PeerId>> + Send,
        RF: Future<Output = anyhow::Result<fmpsc::Receiver<ClassesResponse>>> + Send,
    {
        tracing::trace!(?start, ?stop, "Streaming classes");

        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            let mut declared_class_counts_stream = Box::pin(counts_stream);

            let Some(Ok(cnt)) = declared_class_counts_stream.next().await else {
                tracing::debug!("Declared class counts stream terminated prematurely");
                return;
            };

            let mut progress = BlockProgress::new(cnt);

            // Loop which refreshes peer set once we exhaust it.
            'outer: loop {
                'next_peer: for peer in get_peers().await {
                    let mut responses = match send_request(peer, make_request(start, stop)).await {
                        Ok(x) => x,
                        Err(error) => {
                            // Failed to establish connection, try next peer.
                            tracing::debug!(%peer, reason=%error, "Classes request failed");
                            continue 'next_peer;
                        }
                    };
                    // If the previous peer failed to provide the entire block we need to start over
                    progress.rollback();

                    while start <= stop {
                        tracing::trace!(block_number=%start, expected_classes=%progress.get(), "Expecting class definition responses");
                        let mut class_definitions = Vec::new();

                        while progress.get() > 0 {
                            if let Some(response) = responses.next().await {
                                match handle_response(peer, response, start) {
                                    Some(x) => class_definitions.push(x),
                                    None => continue 'next_peer,
                                }
                                *progress.as_mut() -= 1;
                            } else {
                                tracing::debug!(%peer, "Premature class definition stream termination");
                                // TODO punish the peer
                                continue 'next_peer;
                            }
                        }

                        if yield_block(
                            peer,
                            &mut progress,
                            &mut declared_class_counts_stream,
                            class_definitions,
                            &mut start,
                            stop,
                            tx.clone(),
                        )
                        .await
                        {
                            break 'outer;
                        }
                    }

                    break 'outer;
                }
            }
        });

        ReceiverStream::new(rx)
    }

    fn make_request(start: BlockNumber, stop: BlockNumber) -> ClassesRequest {
        ClassesRequest {
            iteration: Iteration {
                start: start.get().into(),
                direction: Direction::Forward,
                limit: stop.get() - start.get() + 1,
                step: 1.into(),
            },
        }
    }

    /// ### Important
    ///
    /// Returns `None` if the caller should move to the next peer
    fn handle_response(
        peer: PeerId,
        response: ClassesResponse,
        block_number: BlockNumber,
    ) -> Option<ClassDefinition> {
        match response {
            ClassesResponse::Class(p2p_proto::class::Class::Cairo0 { class, domain: _ }) => {
                let Ok(CairoDefinition(definition)) = CairoDefinition::try_from_dto(class) else {
                    // TODO punish the peer
                    tracing::debug!(%peer, "Cairo definition failed to parse");
                    return None;
                };

                Some(ClassDefinition::Cairo {
                    block_number,
                    definition,
                })
            }
            ClassesResponse::Class(p2p_proto::class::Class::Cairo1 { class, domain: _ }) => {
                let Ok(SierraDefinition(definition)) = SierraDefinition::try_from_dto(class) else {
                    // TODO punish the peer
                    tracing::debug!(%peer, "Sierra definition failed to parse");
                    return None;
                };

                Some(ClassDefinition::Sierra {
                    block_number,
                    sierra_definition: definition,
                })
            }
            ClassesResponse::Fin => {
                tracing::debug!(%peer, "Received FIN, continuing with next peer");
                None
            }
        }
    }

    /// ### Important
    ///
    /// Returns true if the stream should be terminated
    async fn yield_block(
        peer: PeerId,
        progress: &mut BlockProgress,
        counts_stream: &mut (impl Stream<Item = anyhow::Result<usize>> + Unpin + Send + 'static),
        class_definitions: Vec<ClassDefinition>,
        start: &mut BlockNumber,
        stop: BlockNumber,
        tx: mpsc::Sender<PeerData<ClassDefinition>>,
    ) -> bool {
        tracing::trace!(block_number=%start, "All classes received for block");

        for class_definition in class_definitions {
            _ = tx.send(PeerData::new(peer, class_definition)).await;
        }

        if *start == stop {
            return true;
        }

        *start += 1;

        let Some(Ok(cnt)) = counts_stream.next().await else {
            tracing::debug!("Declared class counts stream terminated prematurely");
            return true;
        };

        *progress = BlockProgress::new(cnt);

        tracing::trace!(block_number=%start, expected_classes=%progress.get(), "Expecting class definition responses");

        false
    }
}

mod event_stream {
    use super::*;

    pub fn make<PF, RF>(
        mut start: BlockNumber,
        stop: BlockNumber,
        counts_stream: impl Stream<Item = anyhow::Result<usize>> + Send + 'static,
        get_peers: impl Fn() -> PF + Send + 'static,
        send_request: impl Fn(PeerId, EventsRequest) -> RF + Send + 'static,
    ) -> impl Stream<Item = PeerData<EventsForBlockByTransaction>>
    where
        PF: Future<Output = Vec<PeerId>> + Send,
        RF: Future<Output = anyhow::Result<fmpsc::Receiver<EventsResponse>>> + Send,
    {
        tracing::trace!(?start, ?stop, "Streaming events");

        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            let mut counts_stream = Box::pin(counts_stream);

            let Some(Ok(cnt)) = counts_stream.next().await else {
                tracing::debug!("Event counts stream terminated prematurely");
                return;
            };

            let mut progress = BlockProgress::new(cnt);

            // Loop which refreshes peer set once we exhaust it.
            'outer: loop {
                'next_peer: for peer in get_peers().await {
                    let mut responses = match send_request(peer, make_request(start, stop)).await {
                        Ok(x) => x,
                        Err(error) => {
                            // TODO punish the peer
                            tracing::debug!(%peer, reason=%error, "Events request failed");
                            continue 'next_peer;
                        }
                    };

                    // Maintain the current transaction hash to group events by transaction
                    // This grouping is TRUSTED for pre 0.13.2 Starknet blocks.
                    let mut txn = None;
                    // If the previous peer failed to provide the entire block we need to start
                    // over
                    progress.rollback();

                    while start <= stop {
                        tracing::trace!(block_number=%start, expected_responses=%progress.get(), "Expecting event responses");
                        let mut events: Vec<(TransactionHash, Vec<Event>)> = Vec::new();

                        while progress.get() > 0 {
                            if let Some(response) = responses.next().await {
                                if handle_response(peer, response, &mut txn, &mut events) {
                                    continue 'next_peer;
                                }

                                *progress.as_mut() -= 1;
                            } else {
                                // TODO punish the peer
                                tracing::debug!(%peer, block_number=%start, "Premature event stream termination");
                                continue 'next_peer;
                            }
                        }

                        if yield_block(
                            peer,
                            &mut progress,
                            &mut counts_stream,
                            events,
                            &mut start,
                            stop,
                            tx.clone(),
                        )
                        .await
                        {
                            break 'outer;
                        }
                    }

                    break 'outer;
                }
            }
        });

        ReceiverStream::new(rx)
    }

    fn make_request(start: BlockNumber, stop: BlockNumber) -> EventsRequest {
        EventsRequest {
            iteration: Iteration {
                start: start.get().into(),
                direction: Direction::Forward,
                limit: stop.get() - start.get() + 1,
                step: 1.into(),
            },
        }
    }

    /// ### Important
    ///
    /// Returns true if the caller should move to the next peer
    fn handle_response(
        peer: PeerId,
        response: EventsResponse,
        current_txn: &mut Option<TransactionHash>,
        events: &mut Vec<(TransactionHash, Vec<Event>)>,
    ) -> bool {
        match response {
            EventsResponse::Event(event) => {
                let txn_hash = TransactionHash(event.transaction_hash.0);
                let Ok(event) = Event::try_from_dto(event) else {
                    // TODO punish the peer
                    tracing::debug!(%peer, "Event failed to parse");
                    return true;
                };

                match current_txn {
                    Some(x) if *x == txn_hash => {
                        // Same transaction
                        events.last_mut().expect("not empty").1.push(event);
                    }
                    None | Some(_) => {
                        // New transaction
                        events.push((txn_hash, vec![event]));
                        *current_txn = Some(txn_hash);
                    }
                }

                false
            }
            EventsResponse::Fin => {
                tracing::debug!(%peer, "Received FIN, continuing with next peer");
                true
            }
        }
    }

    /// ### Important
    ///
    /// Returns true if the stream should be terminated
    async fn yield_block(
        peer: PeerId,
        progress: &mut BlockProgress,
        counts_stream: &mut (impl Stream<Item = anyhow::Result<usize>> + Unpin + Send + 'static),
        events: Vec<(TransactionHash, Vec<Event>)>,
        start: &mut BlockNumber,
        stop: BlockNumber,
        tx: mpsc::Sender<PeerData<EventsForBlockByTransaction>>,
    ) -> bool {
        tracing::trace!(block_number=%start, "All events received for block");

        _ = tx.send(PeerData::new(peer, (*start, events))).await;

        if *start == stop {
            return true;
        }

        *start += 1;

        let Some(Ok(cnt)) = counts_stream.next().await else {
            tracing::debug!("Event counts stream terminated prematurely");
            return true;
        };

        *progress = BlockProgress::new(cnt);

        tracing::trace!(next_block=%start, expected_responses=%cnt, "Moving to next block");

        false
    }
}

#[derive(Clone, Copy, Debug)]
struct BlockProgress {
    count: usize,
    count_backup: usize,
}

impl BlockProgress {
    fn new(count: usize) -> Self {
        Self {
            count,
            count_backup: count,
        }
    }

    fn get(&self) -> usize {
        self.count
    }

    fn rollback(&mut self) -> Self {
        self.count = self.count_backup;
        *self
    }
}

impl AsMut<usize> for BlockProgress {
    fn as_mut(&mut self) -> &mut usize {
        &mut self.count
    }
}

#[derive(Clone, Debug)]
struct Decaying<T> {
    data: T,
    last_update: Instant,
    timeout: Duration,
}

impl<T: Default> Decaying<T> {
    pub fn new(timeout: Duration) -> Self {
        Self {
            data: Default::default(),
            last_update: Instant::now(),
            timeout,
        }
    }

    /// Does not clear if elapsed, instead the caller is expected to call
    /// [`Self::update`]
    pub fn get(&self) -> Option<&T> {
        if self.last_update.elapsed() > self.timeout {
            None
        } else {
            Some(&self.data)
        }
    }

    pub fn update(&mut self, data: T) {
        self.last_update = Instant::now();
        self.data = data;
    }
}

impl<T: Default> Default for Decaying<T> {
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}
