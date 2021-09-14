// Copyright 2019. The Tari Project
//
// Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
// following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
// disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
// following disclaimer in the documentation and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
// products derived from this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
// INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
// WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
// USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
use super::LOG_TARGET;
use crate::{
    blocks::{Block, BlockHeader},
    chain_storage::{async_db::AsyncBlockchainDb, BlockchainBackend},
    consensus::ConsensusManager,
    crypto::tari_utilities::Hashable,
    transactions::{
        aggregated_body::AggregateBody,
        transaction::{KernelSum, TransactionError, TransactionInput, TransactionKernel, TransactionOutput},
        CryptoFactories,
    },
    validation::{
        block_validators::abort_on_drop::AbortOnDropJoinHandle,
        helpers,
        BlockSyncBodyValidation,
        ValidationError,
    },
};
use async_trait::async_trait;
use futures::{stream::FuturesUnordered, StreamExt};
use log::*;
use std::{
    cmp,
    sync::{Arc, Mutex},
};
use tari_common_types::types::{Commitment, HashOutput, PublicKey};
use tari_crypto::commitment::HomomorphicCommitmentFactory;
use tokio::task;

/// This validator checks whether a block satisfies consensus rules.
/// It implements two validators: one for the `BlockHeader` and one for `Block`. The `Block` validator ONLY validates
/// the block body using the header. It is assumed that the `BlockHeader` has already been validated.
#[derive(Clone)]
pub struct BlockValidator<B> {
    rules: ConsensusManager,
    factories: CryptoFactories,
    db: AsyncBlockchainDb<B>,
    concurrency: usize,
    bypass_range_proof_verification: bool,
}

impl<B: BlockchainBackend + 'static> BlockValidator<B> {
    pub fn new(
        db: AsyncBlockchainDb<B>,
        rules: ConsensusManager,
        factories: CryptoFactories,
        bypass_range_proof_verification: bool,
        concurrency: usize,
    ) -> Self {
        Self {
            rules,
            factories,
            db,
            concurrency,
            bypass_range_proof_verification,
        }
    }

    async fn check_mmr_roots(&self, block: Block) -> Result<Block, ValidationError> {
        let (block, mmr_roots) = self.db.calculate_mmr_roots(block).await?;
        helpers::check_mmr_roots(&block.header, &mmr_roots)?;
        Ok(block)
    }

    pub async fn validate_block_body(&self, block: Block) -> Result<Block, ValidationError> {
        let (valid_header, inputs, outputs, kernels) = block.dissolve();

        // Start all validation tasks concurrently
        let kernels_task = self.start_kernel_validation(&valid_header, kernels);
        let inputs_task =
            self.start_input_validation(&valid_header, outputs.iter().map(|o| o.hash()).collect(), inputs);

        // Output order cannot be checked concurrently so it is checked here first
        if !helpers::is_all_unique_and_sorted(&outputs) {
            return Err(ValidationError::UnsortedOrDuplicateOutput);
        }
        let outputs_task = self.start_output_validation(&valid_header, outputs);

        // Wait for them to complete
        let outputs_result = outputs_task.await??;
        let inputs_result = inputs_task.await??;
        let kernels_result = kernels_task.await??;

        // Perform final checks using validation outputs
        helpers::check_coinbase_reward(
            &self.factories.commitment,
            &self.rules,
            &valid_header,
            kernels_result.kernel_sum.fees,
            kernels_result.coinbase(),
            outputs_result.coinbase(),
        )?;

        helpers::check_script_offset(
            &valid_header,
            &outputs_result.aggregate_offset_pubkey,
            &inputs_result.aggregate_input_key,
        )?;

        helpers::check_kernel_sum(
            &self.factories.commitment,
            &kernels_result.kernel_sum,
            &outputs_result.commitment_sum,
            &inputs_result.commitment_sum,
        )?;

        let block = Block::new(
            valid_header,
            // UNCHECKED: the validator has checked all inputs/outputs are sorted and preserves order in it's output
            AggregateBody::new_sorted_unchecked(inputs_result.inputs, outputs_result.outputs, kernels_result.kernels),
        );

        Ok(block)
    }

    fn start_kernel_validation(
        &self,
        header: &BlockHeader,
        kernels: Vec<TransactionKernel>,
    ) -> AbortOnDropJoinHandle<Result<KernelValidationData, ValidationError>> {
        let height = header.height;
        let total_kernel_offset = header.total_kernel_offset.clone();
        let total_reward = self.rules.calculate_coinbase_and_fees(height, &kernels);
        let total_offset = self
            .factories
            .commitment
            .commit_value(&total_kernel_offset, total_reward.as_u64());

        task::spawn_blocking(move || {
            let mut kernel_sum = KernelSum {
                sum: total_offset,
                ..Default::default()
            };

            let mut coinbase_index = None;
            let mut max_kernel_timelock = 0;
            for (i, kernel) in kernels.iter().enumerate() {
                kernel.verify_signature()?;

                if kernel.is_coinbase() {
                    if coinbase_index.is_some() {
                        warn!(
                            target: LOG_TARGET,
                            "Block #{} failed to validate: more than one kernel coinbase", height
                        );
                        return Err(ValidationError::TransactionError(TransactionError::MoreThanOneCoinbase));
                    }
                    coinbase_index = Some(i);
                }

                max_kernel_timelock = cmp::max(max_kernel_timelock, kernel.lock_height);
                kernel_sum.fees += kernel.fee;
                kernel_sum.sum = &kernel_sum.sum + &kernel.excess;
            }

            if max_kernel_timelock > height {
                return Err(ValidationError::MaturityError);
            }

            if coinbase_index.is_none() {
                warn!(
                    target: LOG_TARGET,
                    "Block #{} failed to validate: no coinbase kernel", height
                );
                return Err(ValidationError::TransactionError(TransactionError::NoCoinbase));
            }

            let coinbase_index = coinbase_index.unwrap();

            Ok(KernelValidationData {
                kernels,
                kernel_sum,
                coinbase_index,
            })
        })
        .into()
    }

    fn start_input_validation(
        &self,
        header: &BlockHeader,
        output_hashes: Vec<HashOutput>,
        inputs: Vec<TransactionInput>,
    ) -> AbortOnDropJoinHandle<Result<InputValidationData, ValidationError>> {
        let block_height = header.height;
        let commitment_factory = self.factories.commitment.clone();
        let db = self.db.inner().clone();
        task::spawn_blocking(move || {
            let mut aggregate_input_key = PublicKey::default();
            let mut commitment_sum = Commitment::default();
            let mut not_found_inputs = Vec::new();
            let db = db.db_read_access()?;
            for (i, input) in inputs.iter().enumerate() {
                // Check for duplicates and/or incorrect sorting
                if i > 0 && input <= &inputs[i - 1] {
                    return Err(ValidationError::UnsortedOrDuplicateInput);
                }

                if !input.is_mature_at(block_height) {
                    warn!(
                        target: LOG_TARGET,
                        "Input found that has not yet matured to spending height: {}", block_height
                    );
                    return Err(TransactionError::InputMaturity.into());
                }

                match helpers::check_input_is_utxo(&*db, input) {
                    Err(ValidationError::UnknownInput) => {
                        // Check if the input spends from the current block
                        let output_hash = input.output_hash();
                        if output_hashes.iter().all(|hash| *hash != output_hash) {
                            warn!(
                                target: LOG_TARGET,
                                "Validation failed due to input: {} which does not exist yet", input
                            );
                            not_found_inputs.push(output_hash);
                        }
                    },
                    Err(err) => return Err(err),
                    _ => {},
                }

                // Once we've found unknown inputs, the aggregate data will be discarded and there is no reason to run
                // the tari script
                if not_found_inputs.is_empty() {
                    // lets count up the input script public keys
                    aggregate_input_key = aggregate_input_key + input.run_and_verify_script(&commitment_factory)?;
                    commitment_sum = &commitment_sum + &input.commitment;
                }
            }

            if !not_found_inputs.is_empty() {
                return Err(ValidationError::UnknownInputs(not_found_inputs));
            }

            Ok(InputValidationData {
                inputs,
                aggregate_input_key,
                commitment_sum,
            })
        })
        .into()
    }

    fn start_output_validation(
        &self,
        header: &BlockHeader,
        outputs: Vec<TransactionOutput>,
    ) -> AbortOnDropJoinHandle<Result<OutputValidationData, ValidationError>> {
        let height = header.height;
        let num_outputs = outputs.len();
        let concurrency = cmp::min(self.concurrency, num_outputs);
        let queue = Arc::new(Mutex::new(outputs.into_iter().enumerate().collect::<Vec<_>>()));
        let bypass_range_proof_verification = self.bypass_range_proof_verification;
        if bypass_range_proof_verification {
            warn!(target: LOG_TARGET, "Range proof verification will be bypassed!")
        }

        debug!(
            target: LOG_TARGET,
            "Using {} worker(s) to validate #{} ({} output(s))", concurrency, height, num_outputs
        );
        let mut output_tasks = (0..concurrency)
            .map(|_| {
                let queue = queue.clone();
                let range_proof_prover = self.factories.range_proof.clone();
                let db = self.db.inner().clone();
                task::spawn_blocking(move || {
                    let db = db.db_read_access()?;
                    let mut aggregate_sender_offset = PublicKey::default();
                    let mut commitment_sum = Commitment::default();
                    let mut outputs = Vec::new();
                    let mut coinbase_index = None;
                    while let Some((orig_idx, output)) = queue.lock().expect("lock poisoned").pop() {
                        if output.is_coinbase() {
                            if coinbase_index.is_some() {
                                warn!(
                                    target: LOG_TARGET,
                                    "Block #{} failed to validate: more than one coinbase output", height
                                );
                                return Err(ValidationError::TransactionError(TransactionError::MoreThanOneCoinbase));
                            }
                            coinbase_index = Some(orig_idx);
                        } else {
                            // Lets gather the output public keys and hashes.
                            // We should not count the coinbase tx here
                            aggregate_sender_offset = aggregate_sender_offset + &output.sender_offset_public_key;
                        }

                        output.verify_metadata_signature()?;
                        if !bypass_range_proof_verification {
                            output.verify_range_proof(&range_proof_prover)?;
                        }

                        helpers::check_not_duplicate_txo(&*db, &output)?;
                        commitment_sum = &commitment_sum + &output.commitment;
                        outputs.push((orig_idx, output));
                    }

                    Ok((outputs, aggregate_sender_offset, commitment_sum, coinbase_index))
                })
            })
            .collect::<FuturesUnordered<_>>();

        task::spawn(async move {
            let mut valid_outputs = Vec::with_capacity(num_outputs);
            let mut aggregate_offset_pubkey = PublicKey::default();
            let mut output_commitment_sum = Commitment::default();
            let mut coinbase_index = None;
            while let Some(output_validation_result) = output_tasks.next().await {
                let (outputs, agg_sender_offset, commitment_sum, cb_index) = output_validation_result??;
                aggregate_offset_pubkey = aggregate_offset_pubkey + agg_sender_offset;
                output_commitment_sum = &output_commitment_sum + &commitment_sum;
                if cb_index.is_some() {
                    if coinbase_index.is_some() {
                        return Err(ValidationError::TransactionError(TransactionError::MoreThanOneCoinbase));
                    }
                    coinbase_index = cb_index;
                }
                valid_outputs.extend(outputs);
            }

            if coinbase_index.is_none() {
                warn!(
                    target: LOG_TARGET,
                    "Block #{} failed to validate: no coinbase UTXO", height
                );
                return Err(ValidationError::TransactionError(TransactionError::NoCoinbase));
            }
            let coinbase_index = coinbase_index.unwrap();

            // Return result in original order
            valid_outputs.sort_by(|(a, _), (b, _)| a.cmp(b));
            let outputs = valid_outputs.into_iter().map(|(_, output)| output).collect();

            Ok(OutputValidationData {
                outputs,
                commitment_sum: output_commitment_sum,
                aggregate_offset_pubkey,
                coinbase_index,
            })
        })
        .into()
    }
}

#[async_trait]
impl<B: BlockchainBackend + 'static> BlockSyncBodyValidation for BlockValidator<B> {
    /// The following consensus checks are done:
    /// 1. Does the block satisfy the stateless checks?
    /// 1. Are the block header MMR roots valid?
    async fn validate_body(&self, block: Block) -> Result<Block, ValidationError> {
        let block_id = format!("block #{}", block.header.height);
        debug!(
            target: LOG_TARGET,
            "Validating {} ({})",
            block_id,
            block.body.to_counts_string()
        );

        let constants = self.rules.consensus_constants(block.header.height);
        helpers::check_block_weight(&block, &constants)?;
        trace!(target: LOG_TARGET, "SV - Block weight is ok for {} ", &block_id);

        let block = self.validate_block_body(block).await?;

        trace!(target: LOG_TARGET, "SV - accounting balance correct for {}", &block_id);
        debug!(target: LOG_TARGET, "{} has PASSED VALIDATION check.", &block_id);

        let block = self.check_mmr_roots(block).await?;
        trace!(
            target: LOG_TARGET,
            "Block validation: MMR roots are valid for {}",
            block_id
        );

        debug!(target: LOG_TARGET, "Block validation: Block is VALID for {}.", block_id,);
        Ok(block)
    }
}

struct KernelValidationData {
    pub kernels: Vec<TransactionKernel>,
    pub kernel_sum: KernelSum,
    pub coinbase_index: usize,
}

impl KernelValidationData {
    pub fn coinbase(&self) -> &TransactionKernel {
        &self.kernels[self.coinbase_index]
    }
}

struct OutputValidationData {
    pub outputs: Vec<TransactionOutput>,
    pub commitment_sum: Commitment,
    pub aggregate_offset_pubkey: PublicKey,
    pub coinbase_index: usize,
}

impl OutputValidationData {
    pub fn coinbase(&self) -> &TransactionOutput {
        &self.outputs[self.coinbase_index]
    }
}

struct InputValidationData {
    pub inputs: Vec<TransactionInput>,
    pub aggregate_input_key: PublicKey,
    pub commitment_sum: Commitment,
}