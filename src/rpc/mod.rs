use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use reth::consensus_common::validation::full_validation;
use reth::primitives::{
    revm_primitives::AccountInfo, Address, ChainSpec, Receipts, SealedBlock, TransactionSigned,
    U256,
};
use reth::providers::{
    AccountReader, BlockExecutor, BlockReaderIdExt, BundleStateWithReceipts, ChainSpecProvider,
    HeaderProvider, StateProviderFactory, WithdrawalsProvider,
};
use reth::revm::{database::StateProviderDatabase, db::BundleState, processor::EVMProcessor};
use reth::rpc::compat::engine::payload::try_into_sealed_block;
use reth::rpc::result::ToRpcResult;

use std::sync::Arc;

use crate::ValidationApi;

mod types;
pub use types::*;

mod result;
use result::internal_rpc_err;

/// trait interface for a custom rpc namespace: `validation`
///
/// This defines an additional namespace where all methods are configured as trait functions.
#[rpc(client, server, namespace = "flashbots")]
#[async_trait]
pub trait ValidationApi {
    /// Validates a block submitted to the relay
    #[method(name = "validateBuilderSubmissionV2")]
    async fn validate_builder_submission_v2(
        &self,
        request_body: ValidationRequestBody,
    ) -> RpcResult<()>;
}

impl<Provider> ValidationApi<Provider>
where
    Provider: BlockReaderIdExt
        + ChainSpecProvider
        + StateProviderFactory
        + HeaderProvider
        + AccountReader
        + WithdrawalsProvider
        + 'static,
{
    /// The provider that can interact with the chain.
    pub fn provider(&self) -> &Provider {
        &self.inner.provider
    }

    /// Create a new instance of the [ValidationApi]
    pub fn new(provider: Provider) -> Self {
        let inner = Arc::new(ValidationApiInner { provider });
        Self { inner }
    }

    fn execute_and_verify_block(
        &self,
        block: &SealedBlock,
        chain_spec: Arc<ChainSpec>,
    ) -> RpcResult<BundleStateWithReceipts> {
        let state_provider = self.provider().latest().to_rpc_result()?;

        let mut executor =
            EVMProcessor::new_with_db(chain_spec, StateProviderDatabase::new(&state_provider));

        let unsealed_block = block.clone().unseal();
        // Note: Setting total difficulty to U256::MAX makes this incompatible with pre merge POW
        // blocks
        // TODO: Check what exactly the "senders" argument is and if we can set it to None here
        executor
            .execute_and_verify_receipt(&unsealed_block, U256::MAX, None)
            .map_err(|e| internal_rpc_err(format!("Error executing transactions: {:}", e)))?;

        let state = executor.take_output_state();

        let state_root = state_provider
            .state_root(&state)
            .map_err(|e| internal_rpc_err(format!("Error computing state root: {e:?}")))?;
        if state_root != block.state_root {
            return Err(internal_rpc_err(format!(
                "State root mismatch. Expected: {}. Received: {}",
                state_root, block.state_root
            )));
        }

        Ok(state)
    }

    fn check_proposer_payment(
        &self,
        block: &SealedBlock,
        state: &BundleStateWithReceipts,
        expected_payment: &U256,
        fee_recipient: &Address,
    ) -> RpcResult<()> {
        if check_proposer_balance_change(state.state(), fee_recipient, expected_payment) {
            return Ok(());
        }

        check_proposer_payment_in_last_transaction(
            &block.body,
            state.receipts(),
            fee_recipient,
            expected_payment,
        )
    }
}

fn check_proposer_payment_in_last_transaction(
    transactions: &Vec<TransactionSigned>,
    receipts: &Receipts,
    fee_recipient: &Address,
    expected_payment: &U256,
) -> RpcResult<()> {
    if receipts.is_empty() || receipts[0].is_empty() {
        return Err(internal_rpc_err(
            "No receipts in block to verify proposer payment",
        ));
    }
    let receipts = &receipts[0];

    let num_transactions = transactions.len();
    if num_transactions == 0 {
        return Err(internal_rpc_err(
            "No transactions in block to verify proposer payment",
        ));
    }
    if num_transactions != receipts.len() {
        return Err(internal_rpc_err(format!(
            "Number of receipts ({}) does not match number of transactions ({})",
            receipts.len(),
            num_transactions
        )));
    }

    let proposer_payment_tx = transactions[num_transactions - 1].clone();
    if proposer_payment_tx.to() != Some(*fee_recipient) {
        return Err(internal_rpc_err(format!(
            "Proposer payment tx to address {:?} does not match fee recipient {}",
            proposer_payment_tx.to(),
            fee_recipient
        )));
    }

    if proposer_payment_tx.value() != *expected_payment {
        return Err(internal_rpc_err(format!(
            "Proposer payment tx value {} does not match expected payment {}",
            proposer_payment_tx.value(),
            expected_payment
        )));
    }

    let proposer_payment_receipt = receipts[num_transactions - 1]
        .clone()
        .ok_or_else(|| internal_rpc_err("Proposer payment receipt not found in block receipts"))?;
    if !proposer_payment_receipt.success {
        return Err(internal_rpc_err(format!(
            "Proposer payment tx failed: {:?}",
            proposer_payment_receipt
        )));
    }

    Ok(())
}

fn check_proposer_balance_change(
    output_state: &BundleState,
    fee_recipient: &Address,
    expected_payment: &U256,
) -> bool {
    let fee_receiver_account_state = match output_state.state.get(fee_recipient) {
        Some(account) => account,
        None => return false,
    };
    let fee_receiver_account_after = match fee_receiver_account_state.info.clone() {
        Some(account) => account,
        None => return false,
    };
    let fee_receiver_account_before = match fee_receiver_account_state.original_info.clone() {
        Some(account) => account,
        None => AccountInfo::default(), // TODO: In tests with the MockProvider this was None by default, check if this fallback is needed in production
    };

    fee_receiver_account_after.balance >= (fee_receiver_account_before.balance + expected_payment)
}

#[async_trait]
impl<Provider> ValidationApiServer for ValidationApi<Provider>
where
    Provider: BlockReaderIdExt
        + ChainSpecProvider
        + StateProviderFactory
        + HeaderProvider
        + AccountReader
        + WithdrawalsProvider
        + 'static,
{
    /// Validates a block submitted to the relay
    async fn validate_builder_submission_v2(
        &self,
        request_body: ValidationRequestBody,
    ) -> RpcResult<()> {
        let block = try_into_sealed_block(request_body.execution_payload.clone().into(), None)
            .to_rpc_result()?;
        // let chain_spec = self.provider().chain_spec();

        compare_values(
            "ParentHash",
            request_body.message.parent_hash,
            block.parent_hash,
        )?;
        compare_values("BlockHash", request_body.message.block_hash, block.hash())?;
        compare_values("GasLimit", request_body.message.gas_limit, block.gas_limit)?;
        compare_values("GasUsed", request_body.message.gas_used, block.gas_used)?;

        // full_validation(&block, self.provider(), &chain_spec).to_rpc_result()?;

        Ok(())

        // let state = self.execute_and_verify_block(&block, chain_spec.clone())?;

        // self.check_proposer_payment(
        //     &block,
        //     &state,
        //     &request_body.message.value,
        //     &request_body.execution_payload.fee_recipient,
        // )
    }
}

impl<Provider> std::fmt::Debug for ValidationApi<Provider> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidationApi").finish_non_exhaustive()
    }
}

impl<Provider> Clone for ValidationApi<Provider> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

pub struct ValidationApiInner<Provider> {
    /// The provider that can interact with the chain.
    provider: Provider,
}

fn compare_values<T: std::cmp::PartialEq + std::fmt::Display>(
    name: &str,
    expected: T,
    actual: T,
) -> RpcResult<()> {
    if expected != actual {
        Err(internal_rpc_err(format!(
            "incorrect {} {}, expected {}",
            name, actual, expected
        )))
    } else {
        Ok(())
    }
}
