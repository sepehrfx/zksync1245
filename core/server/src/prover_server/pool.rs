// Built-in
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};
use std::{thread, time};
// External
use crate::franklin_crypto::bellman::pairing::ff::PrimeField;
use log::info;
// Workspace deps
use circuit::witness::change_pubkey_offchain::{
    apply_change_pubkey_offchain_tx, calculate_change_pubkey_offchain_from_witness,
};
use circuit::witness::close_account::apply_close_account_tx;
use circuit::witness::close_account::calculate_close_account_operations_from_witness;
use circuit::witness::deposit::apply_deposit_tx;
use circuit::witness::deposit::calculate_deposit_operations_from_witness;
use circuit::witness::full_exit::{
    apply_full_exit_tx, calculate_full_exit_operations_from_witness,
};
use circuit::witness::transfer::apply_transfer_tx;
use circuit::witness::transfer::calculate_transfer_operations_from_witness;
use circuit::witness::transfer_to_new::apply_transfer_to_new_tx;
use circuit::witness::transfer_to_new::calculate_transfer_to_new_operations_from_witness;
use circuit::witness::utils::prepare_sig_data;
use circuit::witness::utils::WitnessBuilder;
use circuit::witness::withdraw::apply_withdraw_tx;
use circuit::witness::withdraw::calculate_withdraw_operations_from_witness;
use models::circuit::account::CircuitAccount;
use models::circuit::CircuitAccountTree;
use models::node::{BlockNumber, Fr, FranklinOp};
use models::Operation;
use plasma::state::CollectedFee;
use prover::prover_data::ProverData;

struct BlockSizedOperationsQueue {
    operations: VecDeque<Operation>,
    last_loaded_block: BlockNumber,
    block_size: usize,
}

impl BlockSizedOperationsQueue {
    fn new(block_size: usize) -> Self {
        Self {
            operations: VecDeque::new(),
            last_loaded_block: 0,
            block_size,
        }
    }

    fn take_next_commits_if_needed(
        &mut self,
        conn_pool: &storage::ConnectionPool,
        limit: i64,
    ) -> Result<(), String> {
        if self.operations.len() < limit as usize {
            let storage = conn_pool.access_storage().expect("failed to connect to db");
            let ops = storage
                .load_unverified_commits_after_block(self.block_size, self.last_loaded_block, limit)
                .map_err(|e| format!("failed to read commit operations: {}", e))?;

            self.operations.extend(ops);

            if let Some(op) = self.operations.back() {
                self.last_loaded_block = op.block.block_number;
            }

            trace!(
                "Operations size {}: {:?}",
                self.block_size,
                self.operations
                    .iter()
                    .map(|op| op.block.block_number)
                    .collect::<Vec<_>>()
            );
        }

        Ok(())
    }

    fn prepare_next_if_any(
        &mut self,
        conn_pool: &storage::ConnectionPool,
    ) -> Result<Option<(BlockNumber, ProverData)>, String> {
        match self.operations.pop_front() {
            Some(op) => {
                let storage = conn_pool.access_storage().expect("failed to connect to db");
                let pd = build_prover_data(&storage, &op)?;
                Ok(Some((op.block.block_number, pd)))
            }
            None => Ok(None),
        }
    }
}

pub struct ProversDataPool {
    limit: i64,
    op_queues: HashMap<usize, BlockSizedOperationsQueue>,
    prepared: HashMap<BlockNumber, ProverData>,
}

impl ProversDataPool {
    pub fn new(limit: i64) -> Self {
        let mut res = Self {
            limit,
            op_queues: HashMap::new(),
            prepared: HashMap::new(),
        };

        for block_size in models::params::block_chunk_sizes() {
            res.op_queues
                .insert(*block_size, BlockSizedOperationsQueue::new(*block_size));
        }

        res
    }

    pub fn get(&self, block: BlockNumber) -> Option<&ProverData> {
        self.prepared.get(&block)
    }

    pub fn clean_up(&mut self, block: BlockNumber) {
        self.prepared.remove(&block);
    }

    fn take_next_commits_if_needed(
        &mut self,
        conn_pool: &storage::ConnectionPool,
    ) -> Result<(), String> {
        for (_, queue) in self.op_queues.iter_mut() {
            queue.take_next_commits_if_needed(conn_pool, self.limit)?;
        }

        Ok(())
    }

    fn prepare_next(&mut self, conn_pool: &storage::ConnectionPool) -> Result<(), String> {
        for (_, queue) in self.op_queues.iter_mut() {
            if let Some((block_number, pd)) = queue.prepare_next_if_any(conn_pool)? {
                self.prepared.insert(block_number, pd);
            }
        }

        Ok(())
    }
}

pub fn maintain(
    conn_pool: storage::ConnectionPool,
    data: Arc<RwLock<ProversDataPool>>,
    rounds_interval: time::Duration,
) {
    info!("preparing prover data routine started");
    loop {
        let mut pool = data.write().expect("failed to get write lock on data");
        pool.take_next_commits_if_needed(&conn_pool)
            .expect("couldn't get next commits");
        pool.prepare_next(&conn_pool)
            .expect("couldn't prepare next commits");
        thread::sleep(rounds_interval);
    }
}

fn build_prover_data(
    storage: &storage::StorageProcessor,
    commit_operation: &models::Operation,
) -> Result<ProverData, String> {
    let block_number = commit_operation.block.block_number;
    let block_size = commit_operation.block.smallest_block_size();

    info!("building prover data for block {}", &block_number);

    let account_tree = {
        let (_, accounts) = storage
            .load_committed_state(Some(block_number - 1))
            .map_err(|e| format!("failed to load commited state: {}", e))?;
        let mut account_tree = CircuitAccountTree::new(models::params::account_tree_depth() as u32);
        for (account_id, account) in accounts {
            let circuit_account = CircuitAccount::from(account.clone());
            account_tree.insert(account_id, circuit_account);
        }
        account_tree
    };

    let mut witness_accum = WitnessBuilder::new(
        account_tree,
        commit_operation.block.fee_account,
        block_number,
    );

    let initial_root = witness_accum.account_tree.root_hash();
    let ops = storage
        .get_block_operations(block_number)
        .map_err(|e| format!("failed to get block operations {}", e))?;

    let mut operations = vec![];
    let mut pub_data = vec![];
    let mut fees = vec![];
    for op in ops {
        match op {
            FranklinOp::Deposit(deposit) => {
                let deposit_witness = apply_deposit_tx(&mut witness_accum.account_tree, &deposit);

                let deposit_operations =
                    calculate_deposit_operations_from_witness(&deposit_witness);
                operations.extend(deposit_operations);
                pub_data.extend(deposit_witness.get_pubdata());
            }
            FranklinOp::Transfer(transfer) => {
                let transfer_witness =
                    apply_transfer_tx(&mut witness_accum.account_tree, &transfer);

                let sig_packed = transfer
                    .tx
                    .signature
                    .signature
                    .serialize_packed()
                    .map_err(|e| format!("failed to pack transaction signature {}", e))?;

                let (
                    first_sig_msg,
                    second_sig_msg,
                    third_sig_msg,
                    signature_data,
                    signer_packed_key_bits,
                ) = prepare_sig_data(
                    &sig_packed,
                    &transfer.tx.get_bytes(),
                    &transfer.tx.signature.pub_key,
                )?;

                let transfer_operations = calculate_transfer_operations_from_witness(
                    &transfer_witness,
                    &first_sig_msg,
                    &second_sig_msg,
                    &third_sig_msg,
                    &signature_data,
                    &signer_packed_key_bits,
                );

                operations.extend(transfer_operations);
                fees.push(CollectedFee {
                    token: transfer.tx.token,
                    amount: transfer.tx.fee,
                });
                pub_data.extend(transfer_witness.get_pubdata());
            }
            FranklinOp::TransferToNew(transfer_to_new) => {
                let transfer_to_new_witness =
                    apply_transfer_to_new_tx(&mut witness_accum.account_tree, &transfer_to_new);

                let sig_packed = transfer_to_new
                    .tx
                    .signature
                    .signature
                    .serialize_packed()
                    .map_err(|e| format!("failed to pack transaction signature {}", e))?;

                let (
                    first_sig_msg,
                    second_sig_msg,
                    third_sig_msg,
                    signature_data,
                    signer_packed_key_bits,
                ) = prepare_sig_data(
                    &sig_packed,
                    &transfer_to_new.tx.get_bytes(),
                    &transfer_to_new.tx.signature.pub_key,
                )?;

                let transfer_to_new_operations = calculate_transfer_to_new_operations_from_witness(
                    &transfer_to_new_witness,
                    &first_sig_msg,
                    &second_sig_msg,
                    &third_sig_msg,
                    &signature_data,
                    &signer_packed_key_bits,
                );

                operations.extend(transfer_to_new_operations);
                fees.push(CollectedFee {
                    token: transfer_to_new.tx.token,
                    amount: transfer_to_new.tx.fee,
                });
                pub_data.extend(transfer_to_new_witness.get_pubdata());
            }
            FranklinOp::Withdraw(withdraw) => {
                let withdraw_witness =
                    apply_withdraw_tx(&mut witness_accum.account_tree, &withdraw);

                let sig_packed = withdraw
                    .tx
                    .signature
                    .signature
                    .serialize_packed()
                    .map_err(|e| format!("failed to pack transaction signature {}", e))?;

                let (
                    first_sig_msg,
                    second_sig_msg,
                    third_sig_msg,
                    signature_data,
                    signer_packed_key_bits,
                ) = prepare_sig_data(
                    &sig_packed,
                    &withdraw.tx.get_bytes(),
                    &withdraw.tx.signature.pub_key,
                )?;

                let withdraw_operations = calculate_withdraw_operations_from_witness(
                    &withdraw_witness,
                    &first_sig_msg,
                    &second_sig_msg,
                    &third_sig_msg,
                    &signature_data,
                    &signer_packed_key_bits,
                );

                operations.extend(withdraw_operations);
                fees.push(CollectedFee {
                    token: withdraw.tx.token,
                    amount: withdraw.tx.fee,
                });
                pub_data.extend(withdraw_witness.get_pubdata());
            }
            FranklinOp::Close(close) => {
                let close_account_witness =
                    apply_close_account_tx(&mut witness_accum.account_tree, &close);

                let sig_packed = close
                    .tx
                    .signature
                    .signature
                    .serialize_packed()
                    .map_err(|e| format!("failed to pack signature: {}", e))?;

                let (
                    first_sig_msg,
                    second_sig_msg,
                    third_sig_msg,
                    signature_data,
                    signer_packed_key_bits,
                ) = prepare_sig_data(
                    &sig_packed,
                    &close.tx.get_bytes(),
                    &close.tx.signature.pub_key,
                )?;

                let close_account_operations = calculate_close_account_operations_from_witness(
                    &close_account_witness,
                    &first_sig_msg,
                    &second_sig_msg,
                    &third_sig_msg,
                    &signature_data,
                    &signer_packed_key_bits,
                );

                operations.extend(close_account_operations);
                pub_data.extend(close_account_witness.get_pubdata());
            }
            FranklinOp::FullExit(full_exit_op) => {
                let success = full_exit_op.withdraw_amount.is_some();

                let full_exit_witness =
                    apply_full_exit_tx(&mut witness_accum.account_tree, &full_exit_op, success);

                let full_exit_operations =
                    calculate_full_exit_operations_from_witness(&full_exit_witness);

                operations.extend(full_exit_operations);
                pub_data.extend(full_exit_witness.get_pubdata());
            }
            FranklinOp::ChangePubKeyOffchain(change_pkhash_op) => {
                let change_pkhash_witness = apply_change_pubkey_offchain_tx(
                    &mut witness_accum.account_tree,
                    &change_pkhash_op,
                );

                let change_pkhash_operations =
                    calculate_change_pubkey_offchain_from_witness(&change_pkhash_witness);

                operations.extend(change_pkhash_operations);
                pub_data.extend(change_pkhash_witness.get_pubdata());
            }
            FranklinOp::Noop(_) => {} // Noops are handled below
        }
    }

    witness_accum.add_operation_with_pubdata(operations, pub_data);
    witness_accum.extend_pubdata_with_noops();
    assert_eq!(witness_accum.pubdata.len(), 64 * block_size);
    assert_eq!(witness_accum.operations.len(), block_size);

    witness_accum.collect_fees(&fees);
    assert_eq!(
        witness_accum
            .root_after_fees
            .expect("root_after_fees not present"),
        commit_operation.block.new_root_hash
    );
    witness_accum.calculate_pubdata_commitment();

    Ok(ProverData {
        public_data_commitment: witness_accum.pubdata_commitment.unwrap(),
        old_root: initial_root,
        new_root: commit_operation.block.new_root_hash,
        validator_address: Fr::from_str(&commit_operation.block.fee_account.to_string())
            .expect("failed to parse"),
        operations: witness_accum.operations,
        validator_balances: witness_accum.fee_account_balances.unwrap(),
        validator_audit_path: witness_accum.fee_account_audit_path.unwrap(),
        validator_account: witness_accum.fee_account_witness.unwrap(),
    })
}
