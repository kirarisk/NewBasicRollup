use error::SolanaClientExtError;
use solana_client::{rpc_client, rpc_config::RpcSimulateTransactionConfig};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::{
    account::{feature_set::FeatureSet, fee::FeeStructure, AccountSharedData},
    message::Message,
    signers::Signers,
    transaction::{SanitizedTransaction as SolanaSanitizedTransaction, Transaction},
    transaction_context::TransactionContext,
};

use solana_bpf_loader_program::syscalls::create_program_runtime_environment_v1;
use solana_compute_budget::compute_budget::ComputeBudget;
use solana_svm::message_processor::process_message;
use {
    solana_bpf_loader_program::syscalls::create_program_runtime_environment_v1,
    solana_compute_budget::compute_budget_limits::ComputeBudgetLimits,
    solana_fee_structure::FeeDetails,
    solana_program_runtime::{
        execution_budget::SVMTransactionExecutionBudget,
        loaded_programs::{BlockRelation, ForkGraph, ProgramCacheEntry},
    },
    solana_sdk::{clock::Slot, transaction},
    solana_svm::{
        account_loader::CheckedTransactionDetails, transaction_processor::TransactionBatchProcessor,
    },
    solana_svm_callback::TransactionProcessingCallback,
    solana_svm_feature_set::SVMFeatureSet,
    solana_system_program::system_processor,
    std::sync::{Arc, RwLock},
};

use {
    solana_program_runtime::{
        invoke_context::{self, EnvironmentConfig, InvokeContext},
        loaded_programs::{sysvar_cache, ProgramCacheForTxBatch, ProgramRuntimeEnvironments},
    },
    solana_svm_transaction::svm_message::SVMMessage,
    solana_timings::{ExecuteDetailsTimings, ExecuteTimings},
};
mod error;

/// # RpcClientExt
///
/// `RpcClientExt` is an extension trait for the rust solana client.
/// This crate provides extensions for the Solana Rust client, focusing on compute unit estimation and optimization.
/// 
/// /// This function is also a mock. In the Agave validator, the bank pre-checks
/// transactions before providing them to the SVM API. We mock this step in
/// PayTube, since we don't need to perform such pre-checks.
pub(crate) fn get_transaction_check_results(
    len: usize,
) -> Vec<transaction::Result<CheckedTransactionDetails>> {
    let compute_budget_limit = ComputeBudgetLimits::default();
    vec![
        transaction::Result::Ok(CheckedTransactionDetails::new(
            None,
            Ok(compute_budget_limit.get_compute_budget_and_limits(
                compute_budget_limit.loaded_accounts_bytes,
                FeeDetails::default()
            )),
        ));
        len
    ]
}

pub struct RollUpChannel {
    /// I think you know why this is a bad idea...
    keys: Vec<Keypair>,
    rpc_client: RpcClient,
}

impl RollUpChannel {
    pub fn new(keys: Vec<Keypair>, rpc_client: RpcClient) -> Self {
        Self { keys, rpc_client }
    }

    pub fn process_rollup_transfers(&self, transactions: &[Transaction])-> Vec<u64> {
        
        let sanitized = transactions.iter().map(SolanaSanitizedTransaction::from).collect();
        // PayTube default configs.
        //
        // These can be configurable for channel customization, including
        // imposing resource or feature restrictions, but more commonly they
        // would likely be hoisted from the cluster.
        //
        // For example purposes, they are provided as defaults here.
        let compute_budget = SVMTransactionExecutionBudget::default();
        let feature_set = SVMFeatureSet::all_enabled();
        let fee_structure = FeeStructure::default();
        let rent_collector = RentCollector::default();

        // PayTube loader/callback implementation.
        //
        // Required to provide the SVM API with a mechanism for loading
        // accounts.
        let account_loader = RollUpAccountLoader::new(&self.rpc_client);

        // Solana SVM transaction batch processor.
        //
        // Creates an instance of `TransactionBatchProcessor`, which can be
        // used by PayTube to process transactions using the SVM.
        //
        // This allows programs such as the System and Token programs to be
        // translated and executed within a provisioned virtual machine, as
        // well as offers many of the same functionality as the lower-level
        // Solana runtime.
        let fork_graph = Arc::new(RwLock::new(ForkRollUpGraph {}));
        let processor = create_transaction_batch_processor(
            &account_loader,
            &feature_set,
            &compute_budget,
            Arc::clone(&fork_graph),
        );

        // The PayTube transaction processing runtime environment.
        //
        // Again, these can be configurable or hoisted from the cluster.
        let processing_environment = TransactionProcessingEnvironment {
            blockhash: Hash::default(),
            blockhash_lamports_per_signature: fee_structure.lamports_per_signature,
            epoch_total_stake: 0,
            feature_set,
            rent_collector: None(),
        };

        // The PayTube transaction processing config for Solana SVM.
        //
        // Extended configurations for even more customization of the SVM API.
        let processing_config = TransactionProcessingConfig::default();

        // Step 1: Convert the batch of PayTube transactions into
        // SVM-compatible transactions for processing.
        //
        // In the future, the SVM API may allow for trait-based transactions.
        // In this case, `PayTubeTransaction` could simply implement the
        // interface, and avoid this conversion entirely.


        // Step 2: Process the SVM-compatible transactions with the SVM API.
        let results = processor.load_and_execute_sanitized_transactions(
            &account_loader,
            &sanitized,
            get_transaction_check_results(transactions.len()),
            &processing_environment,
            &processing_config,
        );

        let cu = results.processing_results.iter().map(|transaction_results|{
            let px = transaction_results.unwrap();
            px.executed_units();
        }).collect();
        cu

        // Step 3: Convert the SVM API processor results into a final ledger
        // using `PayTubeSettler`, and settle the resulting balance differences
        // to the Solana base chain.
        //
        // Here the settler is basically iterating over the transaction results
        // to track debits and credits, but only for those transactions which
        // were executed succesfully.
        //
        // The final ledger of debits and credits to each participant can then
        // be packaged into a minimal number of settlement transactions for
        // submission.

    }
}

pub(crate) struct ForkRollUpGraph {}

impl ForkGraph for ForkRollUpGraph {
    fn relationship(&self, _a: Slot, _b: Slot) -> BlockRelation {
        BlockRelation::Unknown
    }
}
pub struct RollUpAccountLoader<'a> {
    cache: RwLock<HashMap<Pubkey, AccountSharedData>>,
    rpc_client: &'a RpcClient,
}

impl<'a> RollUpAccountLoader<'a> {
    pub fn new(rpc_client: &'a RpcClient) -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            rpc_client,
        }
    }
}

impl InvokeContextCallback for RollUpAccountLoader<'_> {}
impl TransactionProcessingCallback for RollUpAccountLoader<'_> {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<AccountSharedData> {
        if let Some(account) = self.cache.read().unwrap().get(pubkey) {
            return Some(account.clone());
        }

        let account: AccountSharedData = self.rpc_client.get_account(pubkey).ok()?.into();
        self.cache.write().unwrap().insert(*pubkey, account.clone());

        Some(account)
    }

    fn account_matches_owners(&self, account: &Pubkey, owners: &[Pubkey]) -> Option<usize> {
        self.get_account_shared_data(account)
            .and_then(|account| owners.iter().position(|key| account.owner().eq(key)))
    }
}
/// This function encapsulates some initial setup required to tweak the
/// `TransactionBatchProcessor` for use within PayTube.
///
/// We're simply configuring the mocked fork graph on the SVM API's program
/// cache, then adding the System program to the processor's builtins.
pub(crate) fn create_transaction_batch_processor<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    feature_set: &SVMFeatureSet,
    compute_budget: &SVMTransactionExecutionBudget,
    fork_graph: Arc<RwLock<ForkRollUpGraph>>,
) -> TransactionBatchProcessor<ForkRollUpGraph> {
    // Create a new transaction batch processor.
    //
    // We're going to use slot 1 specifically because any programs we add will
    // be deployed in slot 0, and they are delayed visibility until the next
    // slot (1).
    // This includes programs owned by BPF Loader v2, which are automatically
    // marked as "depoyed" in slot 0.
    // See `solana_svm::program_loader::load_program_with_pubkey` for more
    // details.
    let processor = TransactionBatchProcessor::<ForkRollUpGraph>::new(
        /* slot */ 1,
        /* epoch */ 1,
        Arc::downgrade(&fork_graph),
        Some(Arc::new(
            create_program_runtime_environment_v1(feature_set, compute_budget, false, false)
                .unwrap(),
        )),
        None,
    );

    // Add the system program builtin.
    processor.add_builtin(
        callbacks,
        solana_system_program::id(),
        "system_program",
        ProgramCacheEntry::new_builtin(
            0,
            b"system_program".len(),
            system_processor::Entrypoint::vm,
        ),
    );

    // Add the BPF Loader v2 builtin, for the SPL Token program.
    processor.add_builtin(
        callbacks,
        solana_sdk::bpf_loader::id(),
        "solana_bpf_loader_program",
        ProgramCacheEntry::new_builtin(
            0,
            b"solana_bpf_loader_program".len(),
            solana_bpf_loader_program::Entrypoint::vm,
        ),
    );

    processor
}
pub trait RpcClientExt {
    fn estimate_compute_units_unsigned_tx<'a, I: Signers + ?Sized>(
        &self,
        unsigned_transaction: &Transaction,
        signers: &'a I,
    ) -> Result<u64, Box<dyn std::error::Error + 'static>>;

    fn estimate_compute_units_msg<'a, I: Signers + ?Sized>(
        &self,
        msg: &Message,
        signers: &'a I,
    ) -> Result<u64, Box<dyn std::error::Error + 'static>>;

    fn optimize_compute_units_unsigned_tx<'a, I: Signers + ?Sized>(
        &self,
        unsigned_transaction: &mut Transaction,
        signers: &'a I,
    ) -> Result<u32, Box<dyn std::error::Error + 'static>>;

    fn optimize_compute_units_msg<'a, I: Signers + ?Sized>(
        &self,
        message: &mut Message,
        signers: &'a I,
    ) -> Result<u32, Box<dyn std::error::Error + 'static>>;
}

impl RpcClientExt for solana_client::rpc_client::RpcClient {
    fn estimate_compute_units_unsigned_tx<'a, I: Signers + ?Sized>(
        &self,
        transaction: &Transaction,
        _signers: &'a I,
    ) -> Result<u64, Box<dyn std::error::Error + 'static>> {
        // GET SVM MESSAGE



        for key in accounts {
            let data: AccountSharedData = self.get_account(&key).unwrap().into();
            accounts_data.push(data);
        }
        let accounts = transaction.message.account_keys;
        let rollup_c = RollUpChannel::new(accounts, self);
        let used_cu = rollup_c.process_rollup_transfers(&[transaction]);


        Ok(used_cu)
    }

    fn estimate_compute_units_msg<'a, I: Signers + ?Sized>(
        &self,
        message: &Message,
        signers: &'a I,
    ) -> Result<u64, Box<dyn std::error::Error + 'static>> {
        let config = RpcSimulateTransactionConfig {
            sig_verify: true,
            ..RpcSimulateTransactionConfig::default()
        };
        let mut tx = Transaction::new_unsigned(message.clone());
        tx.sign(signers, self.get_latest_blockhash()?);
        let result = self.simulate_transaction_with_config(&tx, config)?;

        let consumed_cu = result.value.units_consumed.ok_or(Box::new(
            SolanaClientExtError::ComputeUnitsError(
                "Missing Compute Units from transaction simulation.".into(),
            ),
        ))?;

        if consumed_cu == 0 {
            return Err(Box::new(SolanaClientExtError::RpcError(
                "Transaction simulation failed.".into(),
            )));
        }

        Ok(consumed_cu)
    }

    fn optimize_compute_units_unsigned_tx<'a, I: Signers + ?Sized>(
        &self,
        transaction: &mut Transaction,
        signers: &'a I,
    ) -> Result<u32, Box<dyn std::error::Error + 'static>> {
        let optimal_cu =
            u32::try_from(self.estimate_compute_units_unsigned_tx(transaction, signers)?)?;
        let optimize_ix = ComputeBudgetInstruction::set_compute_unit_limit(
            optimal_cu.saturating_add(optimal_cu.saturating_div(100) * 20),
        );
        transaction
            .message
            .account_keys
            .push(solana_sdk::compute_budget::id());
        let compiled_ix = transaction.message.compile_instruction(&optimize_ix);

        transaction.message.instructions.insert(0, compiled_ix);

        Ok(optimal_cu)
    }

    /// Simulates the transaction to get compute units used for the transaction
    /// and adds an instruction to the message to request
    /// only the required compute units from the ComputeBudget program
    /// to complete the transaction with this Message.
    ///
    /// ```
    /// use solana_client::rpc_client::RpcClient;
    /// use solana_client_ext::RpcClientExt;
    /// use solana_sdk::{
    ///     message::Message, signature::read_keypair_file, signer::Signer, system_instruction,
    ///     transaction::Transaction,
    /// };
    /// fn main() {
    ///     let rpc_client = RpcClient::new("https://api.devnet.solana.com");
    ///     let keypair = read_keypair_file("~/.config/solana/id.json").unwrap();
    ///     let keypair2 = read_keypair_file("~/.config/solana/_id.json").unwrap();
    ///     let created_ix = system_instruction::transfer(&keypair.pubkey(), &keypair2.pubkey(), 10000);
    ///     let mut msg = Message::new(&[created_ix], Some(&keypair.pubkey()));
    ///
    ///     let optimized_cu = rpc_client
    ///         .optimize_compute_units_msg(&mut msg, &[&keypair])
    ///         .unwrap();
    ///     println!("optimized cu {}", optimized_cu);
    ///
    ///     let tx = Transaction::new(&[keypair], msg, rpc_client.get_latest_blockhash().unwrap());
    ///     let result = rpc_client
    ///         .send_and_confirm_transaction_with_spinner(&tx)
    ///         .unwrap();
    ///
    ///     println!(
    ///         "sig https://explorer.solana.com/tx/{}?cluster=devnet",
    ///         result
    ///     );
    /// }
    ///
    ///
    /// ```
    fn optimize_compute_units_msg<'a, I: Signers + ?Sized>(
        &self,
        message: &mut Message,
        signers: &'a I,
    ) -> Result<u32, Box<dyn std::error::Error + 'static>> {
        let optimal_cu = u32::try_from(self.estimate_compute_units_msg(message, signers)?)?;
        let optimize_ix = ComputeBudgetInstruction::set_compute_unit_limit(
            optimal_cu.saturating_add(150 /*optimal_cu.saturating_div(100)*100*/),
        );
        message.account_keys.push(solana_sdk::compute_budget::id());
        let compiled_ix = message.compile_instruction(&optimize_ix);
        message.instructions.insert(0, compiled_ix);

        Ok(optimal_cu)
    }
}

#[cfg(test)]
mod tests {
    use solana_sdk::{pubkey::Pubkey, signature::Keypair, signer::Signer, system_instruction};

    use super::*;

    #[test]
    fn cu() {
        let rpc_client = solana_client::rpc_client::RpcClient::new("https://api.devnet.solana.com");
        let new_keypair = Keypair::new();
        rpc_client
            .request_airdrop(&new_keypair.pubkey(), 50000)
            .unwrap();
        let transfer_ix =
            system_instruction::transfer(&new_keypair.pubkey(), &Pubkey::new_unique(), 10000);
        let mut msg = Message::new(&[transfer_ix], Some(&new_keypair.pubkey()));
        let blockhash = rpc_client.get_latest_blockhash().unwrap();
        let mut tx = Transaction::new(&[&new_keypair], msg, blockhash);
        let _optimized_cu = rpc_client
            .optimize_compute_units_unsigned_tx(&mut tx, &[&new_keypair])
            .unwrap();

        println!("{_optimized_cu}");
        let tx = Transaction::new(&[&new_keypair], msg, blockhash);
        let result = rpc_client
            .send_and_confirm_transaction_with_spinner(&tx)
            .unwrap();
        println!(
            "sig https://explorer.solana.com/tx/{}?cluster=devnet",
            result
        );
        println!("{:?}", tx);
    }
}
