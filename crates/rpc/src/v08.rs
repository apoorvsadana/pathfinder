use crate::jsonrpc::{RpcRouter, RpcRouterBuilder};
use crate::method::subscribe_events::SubscribeEvents;
use crate::method::subscribe_new_heads::SubscribeNewHeads;
use crate::method::subscribe_pending_transactions::SubscribePendingTransactions;

#[rustfmt::skip]
pub fn register_routes() -> RpcRouterBuilder {
    RpcRouter::builder(crate::RpcVersion::V08)
        .register("starknet_blockHashAndNumber",                  crate::method::block_hash_and_number)
        .register("starknet_blockNumber",                         crate::method::block_number)
        .register("starknet_chainId",                             crate::method::chain_id)
        .register("starknet_getBlockTransactionCount",            crate::method::get_block_transaction_count)
        .register("starknet_getBlockWithTxHashes",                crate::method::get_block_with_tx_hashes)
        .register("starknet_getBlockWithTxs",                     crate::method::get_block_with_txs)
        .register("starknet_getClass",                            crate::method::get_class)
        .register("starknet_getClassAt",                          crate::method::get_class_at)
        .register("starknet_getClassHashAt",                      crate::method::get_class_hash_at)
        .register("starknet_getEvents",                           crate::method::get_events)
        .register("starknet_getNonce",                            crate::method::get_nonce)
        .register("starknet_getStateUpdate",                      crate::method::get_state_update)
        .register("starknet_getStorageAt",                        crate::method::get_storage_at)
        .register("starknet_getTransactionByBlockIdAndIndex",     crate::method::get_transaction_by_block_id_and_index)
        .register("starknet_getTransactionByHash",                crate::method::get_transaction_by_hash)
        .register("starknet_getTransactionStatus",                crate::method::get_transaction_status)
        .register("starknet_subscribeNewHeads",                   SubscribeNewHeads)
        .register("starknet_subscribePendingTransactions",        SubscribePendingTransactions)
        .register("starknet_subscribeEvents",                     SubscribeEvents)
        .register("starknet_specVersion",                         || "0.8.0-rc0")
        .register("starknet_syncing",                             crate::method::syncing)

        .register("pathfinder_getProof",                          crate::pathfinder::methods::get_proof)
}
