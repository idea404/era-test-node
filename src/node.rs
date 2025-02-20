//! In-memory node, that supports forking other networks.
use crate::{
    bootloader_debug::{BootloaderDebug, BootloaderDebugTracer},
    console_log::ConsoleLogHandler,
    filters::{EthFilters, FilterType, LogFilter},
    fork::{ForkDetails, ForkSource, ForkStorage},
    formatter,
    system_contracts::{self, Options, SystemContracts},
    utils::{
        self, adjust_l1_gas_price_for_tx, bytecode_to_factory_dep, to_human_size, IntoBoxedFuture,
    },
};
use clap::Parser;
use colored::Colorize;
use core::fmt::Display;
use futures::FutureExt;
use itertools::Itertools;
use jsonrpc_core::BoxFuture;
use once_cell::sync::OnceCell;
use std::{
    cmp::{self},
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, RwLock},
};

use vm::{
    constants::{
        BLOCK_GAS_LIMIT, BLOCK_OVERHEAD_PUBDATA, ETH_CALL_GAS_LIMIT, MAX_PUBDATA_PER_BLOCK,
    },
    utils::{
        fee::derive_base_fee_and_gas_per_pubdata,
        l2_blocks::load_last_l2_block,
        overhead::{derive_overhead, OverheadCoeficients},
    },
    CallTracer, ExecutionResult, HistoryDisabled, L1BatchEnv, SystemEnv, TxExecutionMode, Vm,
    VmExecutionResultAndLogs, VmTracer,
};
use zksync_basic_types::{
    web3::{self, signing::keccak256},
    AccountTreeId, Address, Bytes, L1BatchNumber, H160, H256, U256, U64,
};
use zksync_contracts::BaseSystemContracts;
use zksync_core::api_server::web3::backend_jsonrpc::{
    error::into_jsrpc_error, namespaces::eth::EthNamespaceT,
};
use zksync_state::{ReadStorage, StoragePtr, StorageView, WriteStorage};
use zksync_types::{
    api::{Block, Log, TransactionReceipt, TransactionVariant},
    fee::Fee,
    get_code_key, get_nonce_key,
    l2::L2Tx,
    transaction_request::TransactionRequest,
    utils::{
        decompose_full_nonce, nonces_to_full_nonce, storage_key_for_eth_balance,
        storage_key_for_standard_token_balance,
    },
    PackedEthSignature, StorageKey, StorageLogQueryType, Transaction, ACCOUNT_CODE_STORAGE_ADDRESS,
    EIP_712_TX_TYPE, L2_ETH_TOKEN_ADDRESS, MAX_GAS_PER_PUBDATA_BYTE, MAX_L2_TX_GAS_LIMIT,
};
use zksync_utils::{
    bytecode::{compress_bytecode, hash_bytecode},
    h256_to_account_address, h256_to_u256, h256_to_u64, u256_to_h256,
};
use zksync_web3_decl::{
    error::Web3Error,
    types::{FeeHistory, Filter, FilterChanges},
};

/// Max possible size of an ABI encoded tx (in bytes).
pub const MAX_TX_SIZE: usize = 1_000_000;
/// Timestamp of the first block (if not running in fork mode).
pub const NON_FORK_FIRST_BLOCK_TIMESTAMP: u64 = 1_000;
/// Network ID we use for the test node.
pub const TEST_NODE_NETWORK_ID: u16 = 260;
/// L1 Gas Price.
pub const L1_GAS_PRICE: u64 = 50_000_000_000;
/// L2 Gas Price (0.25 gwei).
pub const L2_GAS_PRICE: u64 = 250_000_000;
/// L1 Gas Price Scale Factor for gas estimation.
pub const ESTIMATE_GAS_L1_GAS_PRICE_SCALE_FACTOR: f64 = 1.2;
/// The max possible number of gas that `eth_estimateGas` is allowed to overestimate.
pub const ESTIMATE_GAS_PUBLISH_BYTE_OVERHEAD: u32 = 100;
/// Acceptable gas overestimation limit.
pub const ESTIMATE_GAS_ACCEPTABLE_OVERESTIMATION: u32 = 1_000;
/// The factor by which to scale the gasLimit.
pub const ESTIMATE_GAS_SCALE_FACTOR: f32 = 1.3;

pub fn compute_hash(block_number: u32, tx_hash: H256) -> H256 {
    let digest = [&block_number.to_be_bytes()[..], tx_hash.as_bytes()].concat();
    H256(keccak256(&digest))
}

pub fn create_empty_block<TX>(block_number: u32, timestamp: u64, batch: u32) -> Block<TX> {
    let hash = compute_hash(block_number, H256::zero());
    Block {
        hash,
        number: U64::from(block_number),
        timestamp: U256::from(timestamp),
        l1_batch_number: Some(U64::from(batch)),
        transactions: vec![],
        gas_used: U256::from(0),
        gas_limit: U256::from(BLOCK_GAS_LIMIT),
        ..Default::default()
    }
}

/// Information about the executed transaction.
pub struct TxExecutionInfo {
    pub tx: L2Tx,
    // Batch number where transaction was executed.
    pub batch_number: u32,
    pub miniblock_number: u64,
    pub result: VmExecutionResultAndLogs,
}

#[derive(Debug, clap::Parser, Clone, clap::ValueEnum, PartialEq, Eq)]
pub enum ShowCalls {
    None,
    User,
    System,
    All,
}

impl FromStr for ShowCalls {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_ref() {
            "none" => Ok(ShowCalls::None),
            "user" => Ok(ShowCalls::User),
            "system" => Ok(ShowCalls::System),
            "all" => Ok(ShowCalls::All),
            _ => Err(format!(
                "Unknown ShowCalls value {} - expected one of none|user|system|all.",
                s
            )),
        }
    }
}

impl Display for ShowCalls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Parser, Clone, clap::ValueEnum, PartialEq, Eq)]
pub enum ShowStorageLogs {
    None,
    Read,
    Write,
    All,
}

impl FromStr for ShowStorageLogs {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_ref() {
            "none" => Ok(ShowStorageLogs::None),
            "read" => Ok(ShowStorageLogs::Read),
            "write" => Ok(ShowStorageLogs::Write),
            "all" => Ok(ShowStorageLogs::All),
            _ => Err(format!(
                "Unknown ShowStorageLogs value {} - expected one of none|read|write|all.",
                s
            )),
        }
    }
}

impl Display for ShowStorageLogs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Parser, Clone, clap::ValueEnum, PartialEq, Eq)]
pub enum ShowVMDetails {
    None,
    All,
}

impl FromStr for ShowVMDetails {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_ref() {
            "none" => Ok(ShowVMDetails::None),
            "all" => Ok(ShowVMDetails::All),
            _ => Err(format!(
                "Unknown ShowVMDetails value {} - expected one of none|all.",
                s
            )),
        }
    }
}

impl Display for ShowVMDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Parser, Clone, clap::ValueEnum, PartialEq, Eq)]
pub enum ShowGasDetails {
    None,
    All,
}

impl FromStr for ShowGasDetails {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_ref() {
            "none" => Ok(ShowGasDetails::None),
            "all" => Ok(ShowGasDetails::All),
            _ => Err(format!(
                "Unknown ShowGasDetails value {} - expected one of none|all.",
                s
            )),
        }
    }
}

impl Display for ShowGasDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{:?}", self)
    }
}

pub struct TransactionResult {
    info: TxExecutionInfo,
    receipt: TransactionReceipt,
}

/// Helper struct for InMemoryNode.
/// S - is the Source of the Fork.
pub struct InMemoryNodeInner<S> {
    /// Timestamp, batch number that will be used by the next block.
    pub current_timestamp: u64,
    /// Batch number that will be used by the next block.
    pub current_batch: u32,
    /// The latest miniblock number that was already generated.
    /// Next transaction will go to the block current_miniblock + 1
    /// (for now, this is a different behavior than the current_batch)
    pub current_miniblock: u64,
    pub l1_gas_price: u64,
    // Map from transaction to details about the exeuction
    pub tx_results: HashMap<H256, TransactionResult>,
    // Map from block hash to information about the block.
    pub blocks: HashMap<H256, Block<TransactionVariant>>,
    // Map from block number to a block hash.
    pub block_hashes: HashMap<u64, H256>,
    // Map from filter_id to the eth filter
    pub filters: EthFilters,
    // Underlying storage
    pub fork_storage: ForkStorage<S>,
    // Debug level information.
    pub show_calls: ShowCalls,
    // Displays storage logs.
    pub show_storage_logs: ShowStorageLogs,
    // Displays VM details.
    pub show_vm_details: ShowVMDetails,
    // Gas details information.
    pub show_gas_details: ShowGasDetails,
    // If true - will contact openchain to resolve the ABI to function names.
    pub resolve_hashes: bool,
    pub console_log_handler: ConsoleLogHandler,
    pub system_contracts: SystemContracts,
    pub impersonated_accounts: HashSet<Address>,
}

type L2TxResult = (
    HashMap<StorageKey, H256>,
    VmExecutionResultAndLogs,
    Block<TransactionVariant>,
    HashMap<U256, Vec<U256>>,
);

impl<S: std::fmt::Debug + ForkSource> InMemoryNodeInner<S> {
    pub fn create_l1_batch_env<ST: ReadStorage>(&self, storage: StoragePtr<ST>) -> L1BatchEnv {
        let last_l2_block = load_last_l2_block(storage);
        L1BatchEnv {
            // TODO: set the previous batch hash properly (take from fork, when forking, and from local storage, when this is not the first block).
            previous_batch_hash: None,
            number: L1BatchNumber::from(self.current_batch),
            timestamp: self.current_timestamp,
            l1_gas_price: self.l1_gas_price,
            fair_l2_gas_price: L2_GAS_PRICE,
            fee_account: H160::zero(),
            enforced_base_fee: None,
            first_l2_block: vm::L2BlockEnv {
                // the 'current_miniblock' contains the block that was already produced.
                // So the next one should be one higher.
                number: self.current_miniblock.saturating_add(1) as u32,
                timestamp: self.current_timestamp,
                prev_block_hash: last_l2_block.hash,
                // This is only used during zksyncEra block timestamp/number transition.
                // In case of starting a new network, it doesn't matter.
                // In theory , when forking mainnet, we should match this value
                // to the value that was set in the node at that time - but AFAIK
                // we don't have any API for this - so this might result in slightly
                // incorrect replays of transacions during the migration period, that
                // depend on block number or timestamp.
                max_virtual_blocks_to_create: 1,
            },
        }
    }

    pub fn create_system_env(
        &self,
        base_system_contracts: BaseSystemContracts,
        execution_mode: TxExecutionMode,
    ) -> SystemEnv {
        SystemEnv {
            zk_porter_available: false,
            // TODO: when forking, we could consider taking the protocol version id from the fork itself.
            version: zksync_types::ProtocolVersionId::latest(),
            base_system_smart_contracts: base_system_contracts,
            gas_limit: BLOCK_GAS_LIMIT,
            execution_mode,
            default_validation_computational_gas_limit: BLOCK_GAS_LIMIT,
            chain_id: self.fork_storage.chain_id,
        }
    }

    /// Estimates the gas required for a given call request.
    ///
    /// # Arguments
    ///
    /// * `req` - A `CallRequest` struct representing the call request to estimate gas for.
    ///
    /// # Returns
    ///
    /// A `Result` with a `Fee` representing the estimated gas related data.
    pub fn estimate_gas_impl(
        &self,
        req: zksync_types::transaction_request::CallRequest,
    ) -> jsonrpc_core::Result<Fee> {
        let mut l2_tx = match L2Tx::from_request(req.into(), MAX_TX_SIZE) {
            Ok(tx) => tx,
            Err(e) => {
                let error = Web3Error::SerializationError(e);
                return Err(into_jsrpc_error(error));
            }
        };

        let tx: Transaction = l2_tx.clone().into();
        let fair_l2_gas_price = L2_GAS_PRICE;

        // Calculate Adjusted L1 Price
        let l1_gas_price = {
            let current_l1_gas_price =
                ((self.l1_gas_price as f64) * ESTIMATE_GAS_L1_GAS_PRICE_SCALE_FACTOR) as u64;

            // In order for execution to pass smoothly, we need to ensure that block's required gasPerPubdata will be
            // <= to the one in the transaction itself.
            adjust_l1_gas_price_for_tx(
                current_l1_gas_price,
                L2_GAS_PRICE,
                tx.gas_per_pubdata_byte_limit(),
            )
        };

        let (base_fee, gas_per_pubdata_byte) =
            derive_base_fee_and_gas_per_pubdata(l1_gas_price, fair_l2_gas_price);

        // Properly format signature
        if l2_tx.common_data.signature.is_empty() {
            l2_tx.common_data.signature = vec![0u8; 65];
            l2_tx.common_data.signature[64] = 27;
        }

        l2_tx.common_data.fee.gas_per_pubdata_limit = MAX_GAS_PER_PUBDATA_BYTE.into();
        l2_tx.common_data.fee.max_fee_per_gas = base_fee.into();
        l2_tx.common_data.fee.max_priority_fee_per_gas = base_fee.into();

        let mut storage_view = StorageView::new(&self.fork_storage);

        // Calculate gas_for_bytecodes_pubdata
        let pubdata_for_factory_deps = l2_tx
            .execute
            .factory_deps
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|bytecode| {
                if storage_view.is_bytecode_known(&hash_bytecode(bytecode)) {
                    return 0;
                }

                let length = if let Ok(compressed) = compress_bytecode(bytecode) {
                    compressed.len()
                } else {
                    bytecode.len()
                };
                length as u32 + ESTIMATE_GAS_PUBLISH_BYTE_OVERHEAD
            })
            .sum::<u32>();

        if pubdata_for_factory_deps > MAX_PUBDATA_PER_BLOCK {
            return Err(into_jsrpc_error(Web3Error::SubmitTransactionError(
                "exceeds limit for published pubdata".into(),
                Default::default(),
            )));
        }

        let gas_for_bytecodes_pubdata: u32 =
            pubdata_for_factory_deps * (gas_per_pubdata_byte as u32);

        let storage = storage_view.to_rc_ptr();

        let execution_mode = TxExecutionMode::EstimateFee;
        let mut batch_env = self.create_l1_batch_env(storage.clone());
        batch_env.l1_gas_price = l1_gas_price;
        let system_env = self.create_system_env(
            self.system_contracts.contracts_for_fee_estimate().clone(),
            execution_mode,
        );

        // We are using binary search to find the minimal values of gas_limit under which the transaction succeeds
        let mut lower_bound = 0;
        let mut upper_bound = MAX_L2_TX_GAS_LIMIT as u32;

        while lower_bound + ESTIMATE_GAS_ACCEPTABLE_OVERESTIMATION < upper_bound {
            let mid = (lower_bound + upper_bound) / 2;
            let try_gas_limit = gas_for_bytecodes_pubdata + mid;

            let estimate_gas_result = InMemoryNodeInner::estimate_gas_step(
                l2_tx.clone(),
                gas_per_pubdata_byte,
                try_gas_limit,
                l1_gas_price,
                batch_env.clone(),
                system_env.clone(),
                &self.fork_storage,
            );

            if estimate_gas_result.result.is_failed() {
                lower_bound = mid + 1;
            } else {
                upper_bound = mid;
            }
        }

        let tx_body_gas_limit = cmp::min(
            MAX_L2_TX_GAS_LIMIT as u32,
            (upper_bound as f32 * ESTIMATE_GAS_SCALE_FACTOR) as u32,
        );
        let suggested_gas_limit = tx_body_gas_limit + gas_for_bytecodes_pubdata;

        let estimate_gas_result = InMemoryNodeInner::estimate_gas_step(
            l2_tx.clone(),
            gas_per_pubdata_byte,
            suggested_gas_limit,
            l1_gas_price,
            batch_env.clone(),
            system_env.clone(),
            &self.fork_storage,
        );

        let coefficients = OverheadCoeficients::from_tx_type(EIP_712_TX_TYPE);
        let overhead: u32 = derive_overhead(
            suggested_gas_limit,
            gas_per_pubdata_byte as u32,
            tx.encoding_len(),
            coefficients,
        );

        match estimate_gas_result.result {
            ExecutionResult::Revert { output } => {
                log::info!("{}", format!("Unable to estimate gas for the request with our suggested gas limit of {}. The transaction is most likely unexecutable. Breakdown of estimation:", suggested_gas_limit + overhead).red());
                log::info!(
                    "{}",
                    format!(
                        "\tEstimated transaction body gas cost: {}",
                        tx_body_gas_limit
                    )
                    .red()
                );
                log::info!(
                    "{}",
                    format!("\tGas for pubdata: {}", gas_for_bytecodes_pubdata).red()
                );
                log::info!("{}", format!("\tOverhead: {}", overhead).red());
                let message = output.to_string();
                let pretty_message = format!(
                    "execution reverted{}{}",
                    if message.is_empty() { "" } else { ": " },
                    message
                );
                let data = output.encoded_data();
                log::info!("{}", pretty_message.on_red());
                Err(into_jsrpc_error(Web3Error::SubmitTransactionError(
                    pretty_message,
                    data,
                )))
            }
            ExecutionResult::Halt { reason } => {
                log::info!("{}", format!("Unable to estimate gas for the request with our suggested gas limit of {}. The transaction is most likely unexecutable. Breakdown of estimation:", suggested_gas_limit + overhead).red());
                log::info!(
                    "{}",
                    format!(
                        "\tEstimated transaction body gas cost: {}",
                        tx_body_gas_limit
                    )
                    .red()
                );
                log::info!(
                    "{}",
                    format!("\tGas for pubdata: {}", gas_for_bytecodes_pubdata).red()
                );
                log::info!("{}", format!("\tOverhead: {}", overhead).red());
                let message = reason.to_string();
                let pretty_message = format!(
                    "execution reverted{}{}",
                    if message.is_empty() { "" } else { ": " },
                    message
                );

                log::info!("{}", pretty_message.on_red());
                Err(into_jsrpc_error(Web3Error::SubmitTransactionError(
                    pretty_message,
                    vec![],
                )))
            }
            ExecutionResult::Success { .. } => {
                let full_gas_limit = match tx_body_gas_limit
                    .overflowing_add(gas_for_bytecodes_pubdata + overhead)
                {
                    (value, false) => value,
                    (_, true) => {
                        log::info!("{}", "Overflow when calculating gas estimation. We've exceeded the block gas limit by summing the following values:".red());
                        log::info!(
                            "{}",
                            format!(
                                "\tEstimated transaction body gas cost: {}",
                                tx_body_gas_limit
                            )
                            .red()
                        );
                        log::info!(
                            "{}",
                            format!("\tGas for pubdata: {}", gas_for_bytecodes_pubdata).red()
                        );
                        log::info!("{}", format!("\tOverhead: {}", overhead).red());
                        return Err(into_jsrpc_error(Web3Error::SubmitTransactionError(
                            "exceeds block gas limit".into(),
                            Default::default(),
                        )));
                    }
                };

                let fee = Fee {
                    max_fee_per_gas: base_fee.into(),
                    max_priority_fee_per_gas: 0u32.into(),
                    gas_limit: full_gas_limit.into(),
                    gas_per_pubdata_limit: gas_per_pubdata_byte.into(),
                };
                Ok(fee)
            }
        }
    }

    /// Runs fee estimation against a sandbox vm with the given gas_limit.
    #[allow(clippy::too_many_arguments)]
    fn estimate_gas_step(
        mut l2_tx: L2Tx,
        gas_per_pubdata_byte: u64,
        tx_gas_limit: u32,
        l1_gas_price: u64,
        mut batch_env: L1BatchEnv,
        system_env: SystemEnv,
        fork_storage: &ForkStorage<S>,
    ) -> VmExecutionResultAndLogs {
        let tx: Transaction = l2_tx.clone().into();
        let l1_gas_price =
            adjust_l1_gas_price_for_tx(l1_gas_price, L2_GAS_PRICE, tx.gas_per_pubdata_byte_limit());

        let coefficients = OverheadCoeficients::from_tx_type(EIP_712_TX_TYPE);
        // Set gas_limit for transaction
        let gas_limit_with_overhead = tx_gas_limit
            + derive_overhead(
                tx_gas_limit,
                gas_per_pubdata_byte as u32,
                tx.encoding_len(),
                coefficients,
            );
        l2_tx.common_data.fee.gas_limit = gas_limit_with_overhead.into();

        let storage = StorageView::new(fork_storage).to_rc_ptr();

        // The nonce needs to be updated
        let nonce = l2_tx.nonce();
        let nonce_key = get_nonce_key(&l2_tx.initiator_account());
        let full_nonce = storage.borrow_mut().read_value(&nonce_key);
        let (_, deployment_nonce) = decompose_full_nonce(h256_to_u256(full_nonce));
        let enforced_full_nonce = nonces_to_full_nonce(U256::from(nonce.0), deployment_nonce);
        storage
            .borrow_mut()
            .set_value(nonce_key, u256_to_h256(enforced_full_nonce));

        // We need to explicitly put enough balance into the account of the users
        let payer = l2_tx.payer();
        let balance_key = storage_key_for_eth_balance(&payer);
        let mut current_balance = h256_to_u256(storage.borrow_mut().read_value(&balance_key));
        let added_balance = l2_tx.common_data.fee.gas_limit * l2_tx.common_data.fee.max_fee_per_gas;
        current_balance += added_balance;
        storage
            .borrow_mut()
            .set_value(balance_key, u256_to_h256(current_balance));

        batch_env.l1_gas_price = l1_gas_price;

        let mut vm = Vm::new(batch_env, system_env, storage, HistoryDisabled);

        let tx: Transaction = l2_tx.into();
        vm.push_transaction(tx);

        vm.execute(vm::VmExecutionMode::OneTx)
    }

    /// Sets the `impersonated_account` field of the node.
    /// This field is used to override the `tx.initiator_account` field of the transaction in the `run_l2_tx` method.
    pub fn set_impersonated_account(&mut self, address: Address) -> bool {
        self.impersonated_accounts.insert(address)
    }

    /// Clears the `impersonated_account` field of the node.
    pub fn stop_impersonating_account(&mut self, address: Address) -> bool {
        self.impersonated_accounts.remove(&address)
    }
}

fn not_implemented<T: Send + 'static>(
    method_name: &str,
) -> jsonrpc_core::BoxFuture<Result<T, jsonrpc_core::Error>> {
    log::warn!("Method {} is not implemented", method_name);
    Err(jsonrpc_core::Error {
        data: None,
        code: jsonrpc_core::ErrorCode::MethodNotFound,
        message: format!("Method {} is not implemented", method_name),
    })
    .into_boxed_future()
}

/// In-memory node, that can be used for local & unit testing.
/// It also supports the option of forking testnet/mainnet.
/// All contents are removed when object is destroyed.
pub struct InMemoryNode<S> {
    inner: Arc<RwLock<InMemoryNodeInner<S>>>,
}

fn contract_address_from_tx_result(execution_result: &VmExecutionResultAndLogs) -> Option<H160> {
    for query in execution_result.logs.storage_logs.iter().rev() {
        if query.log_type == StorageLogQueryType::InitialWrite
            && query.log_query.address == ACCOUNT_CODE_STORAGE_ADDRESS
        {
            return Some(h256_to_account_address(&u256_to_h256(query.log_query.key)));
        }
    }
    None
}

impl<S: ForkSource + std::fmt::Debug> Default for InMemoryNode<S> {
    fn default() -> Self {
        InMemoryNode::new(
            None,
            crate::node::ShowCalls::None,
            ShowStorageLogs::None,
            ShowVMDetails::None,
            ShowGasDetails::None,
            false,
            &system_contracts::Options::BuiltIn,
        )
    }
}

impl<S: ForkSource + std::fmt::Debug> InMemoryNode<S> {
    pub fn new(
        fork: Option<ForkDetails<S>>,
        show_calls: ShowCalls,
        show_storage_logs: ShowStorageLogs,
        show_vm_details: ShowVMDetails,
        show_gas_details: ShowGasDetails,
        resolve_hashes: bool,
        system_contracts_options: &system_contracts::Options,
    ) -> Self {
        let inner = if let Some(f) = &fork {
            let mut block_hashes = HashMap::<u64, H256>::new();
            block_hashes.insert(f.l2_block.number.as_u64(), f.l2_block.hash);
            let mut blocks = HashMap::<H256, Block<TransactionVariant>>::new();
            blocks.insert(f.l2_block.hash, f.l2_block.clone());

            InMemoryNodeInner {
                current_timestamp: f.block_timestamp + 1,
                current_batch: f.l1_block.0 + 1,
                current_miniblock: f.l2_miniblock,
                l1_gas_price: f.l1_gas_price,
                tx_results: Default::default(),
                blocks,
                block_hashes,
                filters: Default::default(),
                fork_storage: ForkStorage::new(fork, system_contracts_options),
                show_calls,
                show_storage_logs,
                show_vm_details,
                show_gas_details,
                resolve_hashes,
                console_log_handler: ConsoleLogHandler::default(),
                system_contracts: SystemContracts::from_options(system_contracts_options),
                impersonated_accounts: Default::default(),
            }
        } else {
            let mut block_hashes = HashMap::<u64, H256>::new();
            block_hashes.insert(0, H256::zero());
            let mut blocks = HashMap::<H256, Block<TransactionVariant>>::new();
            blocks.insert(H256::zero(), create_empty_block(0, 0, 1));

            InMemoryNodeInner {
                current_timestamp: NON_FORK_FIRST_BLOCK_TIMESTAMP,
                current_batch: 1,
                current_miniblock: 0,
                l1_gas_price: L1_GAS_PRICE,
                tx_results: Default::default(),
                blocks,
                block_hashes,
                filters: Default::default(),
                fork_storage: ForkStorage::new(fork, system_contracts_options),
                show_calls,
                show_storage_logs,
                show_vm_details,
                show_gas_details,
                resolve_hashes,
                console_log_handler: ConsoleLogHandler::default(),
                system_contracts: SystemContracts::from_options(system_contracts_options),
                impersonated_accounts: Default::default(),
            }
        };

        InMemoryNode {
            inner: Arc::new(RwLock::new(inner)),
        }
    }

    pub fn get_inner(&self) -> Arc<RwLock<InMemoryNodeInner<S>>> {
        self.inner.clone()
    }

    /// Applies multiple transactions - but still one per L1 batch.
    pub fn apply_txs(&self, txs: Vec<L2Tx>) -> Result<(), String> {
        log::info!("Running {:?} transactions (one per batch)", txs.len());

        for tx in txs {
            self.run_l2_tx(tx, TxExecutionMode::VerifyExecute)?;
        }

        Ok(())
    }

    /// Adds a lot of tokens to a given account.
    pub fn set_rich_account(&self, address: H160) {
        let key = storage_key_for_eth_balance(&address);

        let mut inner = match self.inner.write() {
            Ok(guard) => guard,
            Err(e) => {
                log::info!("Failed to acquire write lock: {}", e);
                return;
            }
        };

        let keys = {
            let mut storage_view = StorageView::new(&inner.fork_storage);
            storage_view.set_value(key, u256_to_h256(U256::from(10u128.pow(30))));
            storage_view.modified_storage_keys().clone()
        };

        for (key, value) in keys.iter() {
            inner.fork_storage.set_value(*key, *value);
        }
    }

    /// Runs L2 'eth call' method - that doesn't commit to a block.
    fn run_l2_call(&self, l2_tx: L2Tx) -> Result<ExecutionResult, String> {
        let execution_mode = TxExecutionMode::EthCall;

        let inner = self
            .inner
            .write()
            .map_err(|e| format!("Failed to acquire write lock: {}", e))?;

        let storage = StorageView::new(&inner.fork_storage).to_rc_ptr();

        let bootloader_code = inner.system_contracts.contacts_for_l2_call();

        // init vm

        let batch_env = inner.create_l1_batch_env(storage.clone());
        let system_env = inner.create_system_env(bootloader_code.clone(), execution_mode);

        let mut vm = Vm::new(batch_env, system_env, storage, HistoryDisabled);

        let mut l2_tx = l2_tx.clone();
        // We must inject *some* signature (otherwise bootloader code fails to generate hash).
        if l2_tx.common_data.signature.is_empty() {
            l2_tx.common_data.signature = PackedEthSignature::default().serialize_packed().into();
        }

        let tx: Transaction = l2_tx.into();
        vm.push_transaction(tx);

        let call_tracer_result = Arc::new(OnceCell::default());

        let custom_tracers =
            vec![
                Box::new(CallTracer::new(call_tracer_result.clone(), HistoryDisabled))
                    as Box<dyn VmTracer<StorageView<&ForkStorage<S>>, HistoryDisabled>>,
            ];

        let tx_result = vm.inspect(custom_tracers, vm::VmExecutionMode::OneTx);

        let call_traces = Arc::try_unwrap(call_tracer_result)
            .unwrap()
            .take()
            .unwrap_or_default();

        match &tx_result.result {
            ExecutionResult::Success { output } => {
                log::info!("Call: {} {:?}", "SUCCESS".green(), output)
            }
            ExecutionResult::Revert { output } => {
                log::info!("Call: {}: {}", "FAILED".red(), output)
            }
            ExecutionResult::Halt { reason } => log::info!("Call: {} {}", "HALTED".red(), reason),
        };

        log::info!("=== Console Logs: ");
        for call in &call_traces {
            inner.console_log_handler.handle_call_recurive(call);
        }

        log::info!("=== Call traces:");
        for call in &call_traces {
            formatter::print_call(call, 0, &inner.show_calls, inner.resolve_hashes);
        }

        Ok(tx_result.result)
    }

    fn display_detailed_gas_info(
        &self,
        bootloader_debug_result: Option<&eyre::Result<BootloaderDebug, String>>,
        spent_on_pubdata: u32,
    ) -> eyre::Result<(), String> {
        if let Some(bootloader_result) = bootloader_debug_result {
            let debug = bootloader_result.clone()?;

            log::info!("┌─────────────────────────┐");
            log::info!("│       GAS DETAILS       │");
            log::info!("└─────────────────────────┘");

            // Total amount of gas (should match tx.gas_limit).
            let total_gas_limit = debug
                .total_gas_limit_from_user
                .saturating_sub(debug.reserved_gas);

            let intrinsic_gas = total_gas_limit - debug.gas_limit_after_intrinsic;
            let gas_for_validation = debug.gas_limit_after_intrinsic - debug.gas_after_validation;

            let gas_spent_on_compute =
                debug.gas_spent_on_execution - debug.gas_spent_on_bytecode_preparation;

            let gas_used = intrinsic_gas
                + gas_for_validation
                + debug.gas_spent_on_bytecode_preparation
                + gas_spent_on_compute;

            log::info!(
                "Gas - Limit: {} | Used: {} | Refunded: {}",
                to_human_size(total_gas_limit),
                to_human_size(gas_used),
                to_human_size(debug.refund_by_operator)
            );

            if debug.total_gas_limit_from_user != total_gas_limit {
                log::info!(
                    "{}",
                    format!(
                "  WARNING: user actually provided more gas {}, but system had a lower max limit.",
                to_human_size(debug.total_gas_limit_from_user)
            )
                    .yellow()
                );
            }
            if debug.refund_computed != debug.refund_by_operator {
                log::info!(
                    "{}",
                    format!(
                        "  WARNING: Refund by VM: {}, but operator refunded more: {}",
                        to_human_size(debug.refund_computed),
                        to_human_size(debug.refund_by_operator)
                    )
                    .yellow()
                );
            }

            if debug.refund_computed + gas_used != total_gas_limit {
                log::info!(
                    "{}",
                    format!(
                        "  WARNING: Gas totals don't match. {} != {} , delta: {}",
                        to_human_size(debug.refund_computed + gas_used),
                        to_human_size(total_gas_limit),
                        to_human_size(total_gas_limit.abs_diff(debug.refund_computed + gas_used))
                    )
                    .yellow()
                );
            }

            let bytes_published = spent_on_pubdata / debug.gas_per_pubdata.as_u32();

            log::info!(
                "During execution published {} bytes to L1, @{} each - in total {} gas",
                to_human_size(bytes_published.into()),
                to_human_size(debug.gas_per_pubdata),
                to_human_size(spent_on_pubdata.into())
            );

            log::info!("Out of {} gas used, we spent:", to_human_size(gas_used));
            log::info!(
                "  {:>15} gas ({:>2}%) for transaction setup",
                to_human_size(intrinsic_gas),
                to_human_size(intrinsic_gas * 100 / gas_used)
            );
            log::info!(
                "  {:>15} gas ({:>2}%) for bytecode preparation (decompression etc)",
                to_human_size(debug.gas_spent_on_bytecode_preparation),
                to_human_size(debug.gas_spent_on_bytecode_preparation * 100 / gas_used)
            );
            log::info!(
                "  {:>15} gas ({:>2}%) for account validation",
                to_human_size(gas_for_validation),
                to_human_size(gas_for_validation * 100 / gas_used)
            );
            log::info!(
                "  {:>15} gas ({:>2}%) for computations (opcodes)",
                to_human_size(gas_spent_on_compute),
                to_human_size(gas_spent_on_compute * 100 / gas_used)
            );

            log::info!("");
            log::info!("");
            log::info!(
                "{}",
                "=== Transaction setup cost breakdown ===".to_owned().bold(),
            );

            log::info!("Total cost: {}", to_human_size(intrinsic_gas).bold());
            log::info!(
                "  {:>15} gas ({:>2}%) fixed cost",
                to_human_size(debug.intrinsic_overhead),
                to_human_size(debug.intrinsic_overhead * 100 / intrinsic_gas)
            );
            log::info!(
                "  {:>15} gas ({:>2}%) operator cost",
                to_human_size(debug.operator_overhead),
                to_human_size(debug.operator_overhead * 100 / intrinsic_gas)
            );

            log::info!("");
            log::info!(
                "  FYI: operator could have charged up to: {}, so you got {}% discount",
                to_human_size(debug.required_overhead),
                to_human_size(
                    (debug.required_overhead - debug.operator_overhead) * 100
                        / debug.required_overhead
                )
            );

            let publish_block_l1_bytes = BLOCK_OVERHEAD_PUBDATA;
            log::info!(
            "Publishing full block costs the operator up to: {}, where {} is due to {} bytes published to L1",
            to_human_size(debug.total_overhead_for_block),
            to_human_size(debug.gas_per_pubdata * publish_block_l1_bytes),
            to_human_size(publish_block_l1_bytes.into())
        );
            log::info!("Your transaction has contributed to filling up the block in the following way (we take the max contribution as the cost):");
            log::info!(
                "  Circuits overhead:{:>15} ({}% of the full block: {})",
                to_human_size(debug.overhead_for_circuits),
                to_human_size(debug.overhead_for_circuits * 100 / debug.total_overhead_for_block),
                to_human_size(debug.total_overhead_for_block)
            );
            log::info!(
                "  Length overhead:  {:>15}",
                to_human_size(debug.overhead_for_length)
            );
            log::info!(
                "  Slot overhead:    {:>15}",
                to_human_size(debug.overhead_for_slot)
            );
            Ok(())
        } else {
            Err("Booloader tracer didn't finish.".to_owned())
        }
    }

    /// Executes the given L2 transaction and returns all the VM logs.
    pub fn run_l2_tx_inner(
        &self,
        l2_tx: L2Tx,
        execution_mode: TxExecutionMode,
    ) -> Result<L2TxResult, String> {
        let inner = self
            .inner
            .write()
            .map_err(|e| format!("Failed to acquire write lock: {}", e))?;

        let storage = StorageView::new(&inner.fork_storage).to_rc_ptr();

        let batch_env = inner.create_l1_batch_env(storage.clone());

        // if we are impersonating an account, we need to use non-verifying system contracts
        let nonverifying_contracts;
        let bootloader_code = {
            if inner
                .impersonated_accounts
                .contains(&l2_tx.common_data.initiator_address)
            {
                tracing::info!(
                    "🕵️ Executing tx from impersonated account {:?}",
                    l2_tx.common_data.initiator_address
                );
                nonverifying_contracts =
                    SystemContracts::from_options(&Options::BuiltInWithoutSecurity);
                nonverifying_contracts.contracts(execution_mode)
            } else {
                inner.system_contracts.contracts(execution_mode)
            }
        };
        let system_env = inner.create_system_env(bootloader_code.clone(), execution_mode);

        let mut vm = Vm::new(
            batch_env.clone(),
            system_env,
            storage.clone(),
            HistoryDisabled,
        );

        let tx: Transaction = l2_tx.clone().into();

        vm.push_transaction(tx.clone());

        let call_tracer_result = Arc::new(OnceCell::default());
        let bootloader_debug_result = Arc::new(OnceCell::default());

        let custom_tracers = vec![
            Box::new(CallTracer::new(call_tracer_result.clone(), HistoryDisabled))
                as Box<dyn VmTracer<StorageView<&ForkStorage<S>>, HistoryDisabled>>,
            Box::new(BootloaderDebugTracer {
                result: bootloader_debug_result.clone(),
            }) as Box<dyn VmTracer<StorageView<&ForkStorage<S>>, HistoryDisabled>>,
        ];

        let tx_result = vm.inspect(custom_tracers, vm::VmExecutionMode::OneTx);

        let call_traces = Arc::try_unwrap(call_tracer_result)
            .unwrap()
            .take()
            .unwrap_or_default();

        let spent_on_pubdata =
            tx_result.statistics.gas_used - tx_result.statistics.computational_gas_used;

        log::info!("┌─────────────────────────┐");
        log::info!("│   TRANSACTION SUMMARY   │");
        log::info!("└─────────────────────────┘");

        match &tx_result.result {
            ExecutionResult::Success { .. } => log::info!("Transaction: {}", "SUCCESS".green()),
            ExecutionResult::Revert { .. } => log::info!("Transaction: {}", "FAILED".red()),
            ExecutionResult::Halt { .. } => log::info!("Transaction: {}", "HALTED".red()),
        }

        log::info!("Initiator: {:?}", tx.initiator_account());
        log::info!("Payer: {:?}", tx.payer());
        log::info!(
            "Gas - Limit: {} | Used: {} | Refunded: {}",
            to_human_size(tx.gas_limit()),
            to_human_size(tx.gas_limit() - tx_result.refunds.gas_refunded),
            to_human_size(tx_result.refunds.gas_refunded.into())
        );

        match inner.show_gas_details {
            ShowGasDetails::None => log::info!(
                "Use --show-gas-details flag or call config_setShowGasDetails to display more info"
            ),
            ShowGasDetails::All => {
                if self
                    .display_detailed_gas_info(bootloader_debug_result.get(), spent_on_pubdata)
                    .is_err()
                {
                    log::info!(
                        "{}",
                        "!!! FAILED TO GET DETAILED GAS INFO !!!".to_owned().red()
                    );
                }
            }
        }

        if inner.show_storage_logs != ShowStorageLogs::None {
            log::info!("");
            log::info!("┌──────────────────┐");
            log::info!("│   STORAGE LOGS   │");
            log::info!("└──────────────────┘");
        }

        for log_query in &tx_result.logs.storage_logs {
            match inner.show_storage_logs {
                ShowStorageLogs::Write => {
                    if matches!(
                        log_query.log_type,
                        StorageLogQueryType::RepeatedWrite | StorageLogQueryType::InitialWrite
                    ) {
                        formatter::print_logs(log_query);
                    }
                }
                ShowStorageLogs::Read => {
                    if log_query.log_type == StorageLogQueryType::Read {
                        formatter::print_logs(log_query);
                    }
                }
                ShowStorageLogs::All => {
                    formatter::print_logs(log_query);
                }
                _ => {}
            }
        }

        if inner.show_vm_details != ShowVMDetails::None {
            formatter::print_vm_details(&tx_result);
        }

        log::info!("");
        log::info!("==== Console logs: ");
        for call in &call_traces {
            inner.console_log_handler.handle_call_recurive(call);
        }
        log::info!("");
        log::info!(
            "==== {} Use --show-calls flag or call config_setShowCalls to display more info.",
            format!("{:?} call traces. ", call_traces.len()).bold()
        );

        if inner.show_calls != ShowCalls::None {
            for call in &call_traces {
                formatter::print_call(call, 0, &inner.show_calls, inner.resolve_hashes);
            }
        }
        log::info!("");
        log::info!(
            "==== {}",
            format!("{} events", tx_result.logs.events.len()).bold()
        );
        for event in &tx_result.logs.events {
            formatter::print_event(event, inner.resolve_hashes);
        }

        // The computed block hash here will be different than that in production.
        let hash = compute_hash(batch_env.number.0, l2_tx.hash());
        let block = Block {
            hash,
            number: U64::from(inner.current_miniblock.saturating_add(1)),
            timestamp: U256::from(batch_env.timestamp),
            l1_batch_number: Some(U64::from(batch_env.number.0)),
            transactions: vec![TransactionVariant::Full(
                zksync_types::api::Transaction::from(l2_tx),
            )],
            gas_used: U256::from(tx_result.statistics.gas_used),
            gas_limit: U256::from(BLOCK_GAS_LIMIT),
            ..Default::default()
        };

        log::info!("");
        log::info!("");

        let bytecodes = vm
            .get_last_tx_compressed_bytecodes()
            .iter()
            .map(|b| bytecode_to_factory_dep(b.original.clone()))
            .collect();

        vm.execute(vm::VmExecutionMode::Bootloader);

        let modified_keys = storage.borrow().modified_storage_keys().clone();
        Ok((modified_keys, tx_result, block, bytecodes))
    }

    /// Runs L2 transaction and commits it to a new block.
    fn run_l2_tx(&self, l2_tx: L2Tx, execution_mode: TxExecutionMode) -> Result<(), String> {
        let tx_hash = l2_tx.hash();
        log::info!("");
        log::info!("Executing {}", format!("{:?}", tx_hash).bold());

        {
            let mut inner = self
                .inner
                .write()
                .map_err(|e| format!("Failed to acquire write lock: {}", e))?;
            inner.filters.notify_new_pending_transaction(tx_hash);
        }

        let (keys, result, block, bytecodes) =
            self.run_l2_tx_inner(l2_tx.clone(), execution_mode)?;

        if let ExecutionResult::Halt { reason } = result.result {
            // Halt means that something went really bad with the transaction execution (in most cases invalid signature,
            // but it could also be bootloader panic etc).
            // In such case, we should not persist the VM data, and we should pretend that transaction never existed.
            return Err(format!("Transaction HALT: {}", reason));
        }
        // Write all the mutated keys (storage slots).
        let mut inner = self
            .inner
            .write()
            .map_err(|e| format!("Failed to acquire write lock: {}", e))?;
        for (key, value) in keys.iter() {
            inner.fork_storage.set_value(*key, *value);
        }

        // Write all the factory deps.
        for (hash, code) in bytecodes.iter() {
            inner.fork_storage.store_factory_dep(
                u256_to_h256(*hash),
                code.iter()
                    .flat_map(|entry| {
                        let mut bytes = vec![0u8; 32];
                        entry.to_big_endian(&mut bytes);
                        bytes.to_vec()
                    })
                    .collect(),
            )
        }

        let current_miniblock = inner.current_miniblock.saturating_add(1);

        for (log_idx, event) in result.logs.events.iter().enumerate() {
            inner.filters.notify_new_log(
                &Log {
                    address: event.address,
                    topics: event.indexed_topics.clone(),
                    data: Bytes(event.value.clone()),
                    block_hash: Some(block.hash),
                    block_number: Some(block.number),
                    l1_batch_number: block.l1_batch_number,
                    transaction_hash: Some(tx_hash),
                    transaction_index: Some(U64::zero()),
                    log_index: Some(U256::from(log_idx)),
                    transaction_log_index: Some(U256::from(log_idx)),
                    log_type: None,
                    removed: None,
                },
                U64::from(current_miniblock),
            );
        }
        let tx_receipt = TransactionReceipt {
            transaction_hash: tx_hash,
            transaction_index: U64::from(0),
            block_hash: Some(block.hash),
            block_number: Some(block.number),
            l1_batch_tx_index: None,
            l1_batch_number: block.l1_batch_number,
            from: l2_tx.initiator_account(),
            to: Some(l2_tx.recipient_account()),
            cumulative_gas_used: Default::default(),
            gas_used: Some(l2_tx.common_data.fee.gas_limit - result.refunds.gas_refunded),
            contract_address: contract_address_from_tx_result(&result),
            logs: result
                .logs
                .events
                .iter()
                .enumerate()
                .map(|(log_idx, log)| Log {
                    address: log.address,
                    topics: log.indexed_topics.clone(),
                    data: Bytes(log.value.clone()),
                    block_hash: Some(block.hash),
                    block_number: Some(block.number),
                    l1_batch_number: block.l1_batch_number,
                    transaction_hash: Some(tx_hash),
                    transaction_index: Some(U64::zero()),
                    log_index: Some(U256::from(log_idx)),
                    transaction_log_index: Some(U256::from(log_idx)),
                    log_type: None,
                    removed: None,
                })
                .collect(),
            l2_to_l1_logs: vec![],
            status: Some(if result.result.is_failed() {
                U64::from(0)
            } else {
                U64::from(1)
            }),
            effective_gas_price: Some(L2_GAS_PRICE.into()),
            ..Default::default()
        };
        inner.tx_results.insert(
            tx_hash,
            TransactionResult {
                info: TxExecutionInfo {
                    tx: l2_tx,
                    batch_number: block.l1_batch_number.unwrap_or_default().as_u32(),
                    miniblock_number: current_miniblock,
                    result,
                },
                receipt: tx_receipt,
            },
        );
        let block_hash = block.hash;
        inner.block_hashes.insert(current_miniblock, block.hash);
        inner.blocks.insert(block.hash, block);
        inner.filters.notify_new_block(block_hash);

        // With the introduction of 'l2 blocks' (and virtual blocks),
        // we are adding one l2 block at the end of each batch (to handle things like remaining events etc).

        let empty_block_at_end_of_batch = create_empty_block(
            (current_miniblock + 1) as u32,
            inner.current_timestamp + 1,
            inner.current_batch,
        );
        let empty_block_hash = empty_block_at_end_of_batch.hash;
        inner
            .block_hashes
            .insert(current_miniblock + 1, empty_block_hash);
        inner.blocks.insert(
            empty_block_at_end_of_batch.hash,
            empty_block_at_end_of_batch,
        );
        inner.filters.notify_new_block(empty_block_hash);

        {
            // That's why here, we increase the batch by 1, but miniblock (and timestamp) by 2.
            // You can look at insert_fictive_l2_block function in VM to see how this fake block is inserted.

            inner.current_batch += 1;
            inner.current_miniblock += 2;
            inner.current_timestamp += 2;
        }

        Ok(())
    }
}

impl<S: Send + Sync + 'static + ForkSource + std::fmt::Debug> EthNamespaceT for InMemoryNode<S> {
    /// Returns the chain ID of the node.
    fn chain_id(&self) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<zksync_basic_types::U64>> {
        match self.inner.read() {
            Ok(inner) => Ok(U64::from(inner.fork_storage.chain_id.0 as u64)).into_boxed_future(),
            Err(_) => Err(into_jsrpc_error(Web3Error::InternalError)).into_boxed_future(),
        }
    }

    /// Calls the specified function on the L2 contract with the given arguments.
    ///
    /// # Arguments
    ///
    /// * `req` - The call request containing the function name and arguments.
    /// * `_block` - The block ID variant (unused).
    ///
    /// # Returns
    ///
    /// A boxed future containing the result of the function call.
    fn call(
        &self,
        req: zksync_types::transaction_request::CallRequest,
        _block: Option<zksync_types::api::BlockIdVariant>,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<zksync_basic_types::Bytes>> {
        match L2Tx::from_request(req.into(), MAX_TX_SIZE) {
            Ok(mut tx) => {
                tx.common_data.fee.gas_limit = ETH_CALL_GAS_LIMIT.into();
                let result = self.run_l2_call(tx);

                match result {
                    Ok(execution_result) => match execution_result {
                        ExecutionResult::Success { output } => {
                            Ok(output.into()).into_boxed_future()
                        }
                        ExecutionResult::Revert { output } => {
                            let message = output.to_user_friendly_string();
                            let pretty_message = format!(
                                "execution reverted{}{}",
                                if message.is_empty() { "" } else { ": " },
                                message
                            );

                            log::info!("{}", pretty_message.on_red());
                            Err(into_jsrpc_error(Web3Error::SubmitTransactionError(
                                pretty_message,
                                output.encoded_data(),
                            )))
                            .into_boxed_future()
                        }
                        ExecutionResult::Halt { reason } => {
                            let message = reason.to_string();
                            let pretty_message = format!(
                                "execution halted {}{}",
                                if message.is_empty() { "" } else { ": " },
                                message
                            );

                            log::info!("{}", pretty_message.on_red());
                            Err(into_jsrpc_error(Web3Error::SubmitTransactionError(
                                pretty_message,
                                vec![],
                            )))
                            .into_boxed_future()
                        }
                    },
                    Err(e) => {
                        let error = Web3Error::InvalidTransactionData(
                            zksync_types::ethabi::Error::InvalidName(e),
                        );
                        Err(into_jsrpc_error(error)).into_boxed_future()
                    }
                }
            }
            Err(e) => {
                let error = Web3Error::SerializationError(e);
                Err(into_jsrpc_error(error)).into_boxed_future()
            }
        }
    }

    /// Returns the balance of the specified address.
    ///
    /// # Arguments
    ///
    /// * `address` - The address to get the balance of.
    /// * `_block` - The block ID variant (optional).
    ///
    /// # Returns
    ///
    /// A `BoxFuture` that resolves to a `Result` containing the balance of the specified address as a `U256` or a `jsonrpc_core::Error` if an error occurred.
    fn get_balance(
        &self,
        address: zksync_basic_types::Address,
        _block: Option<zksync_types::api::BlockIdVariant>,
    ) -> BoxFuture<Result<U256, jsonrpc_core::Error>> {
        let inner = Arc::clone(&self.inner);

        Box::pin(async move {
            let balance_key = storage_key_for_standard_token_balance(
                AccountTreeId::new(L2_ETH_TOKEN_ADDRESS),
                &address,
            );

            match inner.write() {
                Ok(mut inner_guard) => {
                    let balance = inner_guard.fork_storage.read_value(&balance_key);
                    Ok(h256_to_u256(balance))
                }
                Err(_) => {
                    let web3_error = Web3Error::InternalError;
                    Err(into_jsrpc_error(web3_error))
                }
            }
        })
    }

    /// Returns a block by its number.
    ///
    /// # Arguments
    ///
    /// * `block_number` - A `BlockNumber` enum variant representing the block number to retrieve.
    /// * `full_transactions` - A boolean value indicating whether to retrieve full transactions or not.
    ///
    /// # Returns
    ///
    /// A `BoxFuture` containing a `jsonrpc_core::Result` that resolves to an `Option` of `zksync_types::api::Block<zksync_types::api::TransactionVariant>`.
    fn get_block_by_number(
        &self,
        block_number: zksync_types::api::BlockNumber,
        full_transactions: bool,
    ) -> BoxFuture<
        jsonrpc_core::Result<
            Option<zksync_types::api::Block<zksync_types::api::TransactionVariant>>,
        >,
    > {
        let inner = Arc::clone(&self.inner);

        Box::pin(async move {
            let maybe_block = {
                let reader = match inner.read() {
                    Ok(r) => r,
                    Err(_) => return Err(into_jsrpc_error(Web3Error::InternalError)),
                };
                let number =
                    utils::to_real_block_number(block_number, U64::from(reader.current_miniblock))
                        .as_u64();

                reader
                    .block_hashes
                    .get(&number)
                    .and_then(|hash| reader.blocks.get(hash))
                    .cloned()
                    .or_else(|| {
                        reader
                            .fork_storage
                            .inner
                            .read()
                            .expect("failed reading fork storage")
                            .fork
                            .as_ref()
                            .and_then(|fork| {
                                fork.fork_source
                                    .get_block_by_number(block_number, true)
                                    .ok()
                                    .flatten()
                            })
                    })
            };

            match maybe_block {
                Some(mut block) => {
                    let block_hash = block.hash;
                    block.transactions = block
                        .transactions
                        .into_iter()
                        .map(|transaction| match &transaction {
                            TransactionVariant::Full(inner) => {
                                if full_transactions {
                                    transaction
                                } else {
                                    TransactionVariant::Hash(inner.hash)
                                }
                            }
                            TransactionVariant::Hash(_) => {
                                if full_transactions {
                                    panic!(
                                        "unexpected non full transaction for block {}",
                                        block_hash
                                    )
                                } else {
                                    transaction
                                }
                            }
                        })
                        .collect();

                    Ok(Some(block))
                }
                None => Err(into_jsrpc_error(Web3Error::NoBlock)),
            }
        })
    }

    /// Returns the code stored at the specified address.
    ///
    /// # Arguments
    ///
    /// * `address` - The address to retrieve the code from.
    /// * `_block` - An optional block ID variant.
    ///
    /// # Returns
    ///
    /// A `BoxFuture` containing the result of the operation, which is a `jsonrpc_core::Result` containing
    /// the code as a `zksync_basic_types::Bytes` object.
    fn get_code(
        &self,
        address: zksync_basic_types::Address,
        _block: Option<zksync_types::api::BlockIdVariant>,
    ) -> BoxFuture<jsonrpc_core::Result<zksync_basic_types::Bytes>> {
        let inner = Arc::clone(&self.inner);

        Box::pin(async move {
            let code_key = get_code_key(&address);

            match inner.write() {
                Ok(mut guard) => {
                    let code_hash = guard.fork_storage.read_value(&code_key);

                    let code = guard
                        .fork_storage
                        .load_factory_dep_internal(code_hash)
                        .unwrap_or_default();

                    Ok(Bytes::from(code))
                }
                Err(_) => Err(into_jsrpc_error(Web3Error::InternalError)),
            }
        })
    }

    /// Returns the transaction count for a given address.
    ///
    /// # Arguments
    ///
    /// * `address` - The address to get the transaction count for.
    /// * `_block` - Optional block ID variant.
    ///
    /// # Returns
    ///
    /// Returns a `BoxFuture` containing the transaction count as a `U256` wrapped in a `jsonrpc_core::Result`.
    fn get_transaction_count(
        &self,
        address: zksync_basic_types::Address,
        _block: Option<zksync_types::api::BlockIdVariant>,
    ) -> BoxFuture<jsonrpc_core::Result<U256>> {
        let inner = Arc::clone(&self.inner);

        Box::pin(async move {
            let nonce_key = get_nonce_key(&address);

            match inner.write() {
                Ok(mut guard) => {
                    let result = guard.fork_storage.read_value(&nonce_key);
                    Ok(h256_to_u64(result).into())
                }
                Err(_) => Err(into_jsrpc_error(Web3Error::InternalError)),
            }
        })
    }

    /// Retrieves the transaction receipt for a given transaction hash.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the transaction to retrieve the receipt for.
    ///
    /// # Returns
    ///
    /// A `BoxFuture` that resolves to an `Option` of a `TransactionReceipt` or an error.
    fn get_transaction_receipt(
        &self,
        hash: zksync_basic_types::H256,
    ) -> BoxFuture<jsonrpc_core::Result<Option<zksync_types::api::TransactionReceipt>>> {
        let inner = Arc::clone(&self.inner);

        Box::pin(async move {
            let reader = match inner.read() {
                Ok(r) => r,
                Err(_) => return Err(into_jsrpc_error(Web3Error::InternalError)),
            };

            let receipt = reader
                .tx_results
                .get(&hash)
                .map(|info| info.receipt.clone());
            Ok(receipt)
        })
    }

    /// Sends a raw transaction to the L2 network.
    ///
    /// # Arguments
    ///
    /// * `tx_bytes` - The transaction bytes to send.
    ///
    /// # Returns
    ///
    /// A future that resolves to the hash of the transaction if successful, or an error if the transaction is invalid or execution fails.
    fn send_raw_transaction(
        &self,
        tx_bytes: zksync_basic_types::Bytes,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<zksync_basic_types::H256>> {
        let chain_id = match self.inner.read() {
            Ok(reader) => reader.fork_storage.chain_id,
            Err(_) => {
                return futures::future::err(into_jsrpc_error(Web3Error::InternalError)).boxed()
            }
        };

        let (tx_req, hash) = match TransactionRequest::from_bytes(&tx_bytes.0, chain_id.0) {
            Ok(result) => result,
            Err(e) => {
                return futures::future::err(into_jsrpc_error(Web3Error::SerializationError(e)))
                    .boxed()
            }
        };

        let mut l2_tx: L2Tx = match L2Tx::from_request(tx_req, MAX_TX_SIZE) {
            Ok(tx) => tx,
            Err(e) => {
                return futures::future::err(into_jsrpc_error(Web3Error::SerializationError(e)))
                    .boxed()
            }
        };

        l2_tx.set_input(tx_bytes.0, hash);
        if hash != l2_tx.hash() {
            return futures::future::err(into_jsrpc_error(Web3Error::InvalidTransactionData(
                zksync_types::ethabi::Error::InvalidData,
            )))
            .boxed();
        };

        match self.run_l2_tx(l2_tx.clone(), TxExecutionMode::VerifyExecute) {
            Ok(_) => Ok(hash).into_boxed_future(),
            Err(e) => {
                let error_message = format!("Execution error: {}", e);
                futures::future::err(into_jsrpc_error(Web3Error::SubmitTransactionError(
                    error_message,
                    l2_tx.hash().as_bytes().to_vec(),
                )))
                .boxed()
            }
        }
    }

    /// Returns a block by its hash. Currently, only hashes for blocks in memory are supported.
    ///
    /// # Arguments
    ///
    /// * `hash` - A `H256` type representing the hash of the block to retrieve.
    /// * `full_transactions` - A boolean value indicating whether to retrieve full transactions or not.
    ///
    /// # Returns
    ///
    /// A `BoxFuture` containing a `jsonrpc_core::Result` that resolves to an `Option` of `zksync_types::api::Block<zksync_types::api::TransactionVariant>`.
    fn get_block_by_hash(
        &self,
        hash: zksync_basic_types::H256,
        full_transactions: bool,
    ) -> jsonrpc_core::BoxFuture<
        jsonrpc_core::Result<
            Option<zksync_types::api::Block<zksync_types::api::TransactionVariant>>,
        >,
    > {
        let inner = Arc::clone(&self.inner);

        Box::pin(async move {
            let maybe_block = {
                let reader = inner
                    .read()
                    .map_err(|_| into_jsrpc_error(Web3Error::InternalError))?;

                // try retrieving block from memory, and if unavailable subsequently from the fork
                reader.blocks.get(&hash).cloned().or_else(|| {
                    reader
                        .fork_storage
                        .inner
                        .read()
                        .expect("failed reading fork storage")
                        .fork
                        .as_ref()
                        .and_then(|fork| {
                            fork.fork_source
                                .get_block_by_hash(hash, true)
                                .ok()
                                .flatten()
                        })
                })
            };

            match maybe_block {
                Some(mut block) => {
                    let block_hash = block.hash;
                    block.transactions = block
                        .transactions
                        .into_iter()
                        .map(|transaction| match &transaction {
                            TransactionVariant::Full(inner) => {
                                if full_transactions {
                                    transaction
                                } else {
                                    TransactionVariant::Hash(inner.hash)
                                }
                            }
                            TransactionVariant::Hash(_) => {
                                if full_transactions {
                                    panic!(
                                        "unexpected non full transaction for block {}",
                                        block_hash
                                    )
                                } else {
                                    transaction
                                }
                            }
                        })
                        .collect();

                    Ok(Some(block))
                }
                None => Err(into_jsrpc_error(Web3Error::NoBlock)),
            }
        })
    }

    /// Returns a future that resolves to an optional transaction with the given hash.
    ///
    /// # Arguments
    ///
    /// * `hash` - A 32-byte hash of the transaction.
    ///
    /// # Returns
    ///
    /// A `jsonrpc_core::BoxFuture` that resolves to a `jsonrpc_core::Result` containing an optional `zksync_types::api::Transaction`.
    fn get_transaction_by_hash(
        &self,
        hash: zksync_basic_types::H256,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<Option<zksync_types::api::Transaction>>> {
        let inner = Arc::clone(&self.inner);

        Box::pin(async move {
            let reader = inner
                .read()
                .map_err(|_| into_jsrpc_error(Web3Error::InternalError))?;
            let tx_result = reader.tx_results.get(&hash);

            Ok(tx_result.and_then(|TransactionResult { info, .. }| {
                let input_data = info.tx.common_data.input.clone().or(None)?;

                let chain_id = info.tx.common_data.extract_chain_id().or(None)?;

                Some(zksync_types::api::Transaction {
                    hash,
                    nonce: U256::from(info.tx.common_data.nonce.0),
                    block_hash: Some(hash),
                    block_number: Some(U64::from(info.miniblock_number)),
                    transaction_index: Some(U64::from(1)),
                    from: Some(info.tx.initiator_account()),
                    to: Some(info.tx.recipient_account()),
                    value: info.tx.execute.value,
                    gas_price: Default::default(),
                    gas: Default::default(),
                    input: input_data.data.into(),
                    v: Some(chain_id.into()),
                    r: Some(U256::zero()),
                    s: Some(U256::zero()),
                    raw: None,
                    transaction_type: {
                        let tx_type = match info.tx.common_data.transaction_type {
                            zksync_types::l2::TransactionType::LegacyTransaction => 0,
                            zksync_types::l2::TransactionType::EIP2930Transaction => 1,
                            zksync_types::l2::TransactionType::EIP1559Transaction => 2,
                            zksync_types::l2::TransactionType::EIP712Transaction => 113,
                            zksync_types::l2::TransactionType::PriorityOpTransaction => 255,
                            zksync_types::l2::TransactionType::ProtocolUpgradeTransaction => 254,
                        };
                        Some(tx_type.into())
                    },
                    access_list: None,
                    max_fee_per_gas: Some(info.tx.common_data.fee.max_fee_per_gas),
                    max_priority_fee_per_gas: Some(
                        info.tx.common_data.fee.max_priority_fee_per_gas,
                    ),
                    chain_id: chain_id.into(),
                    l1_batch_number: Some(U64::from(info.batch_number as u64)),
                    l1_batch_tx_index: None,
                })
            }))
        })
    }

    /// Returns the current block number as a `U64` wrapped in a `BoxFuture`.
    fn get_block_number(
        &self,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<zksync_basic_types::U64>> {
        let inner = Arc::clone(&self.inner);

        Box::pin(async move {
            let reader = inner
                .read()
                .map_err(|_| into_jsrpc_error(Web3Error::InternalError))?;
            Ok(U64::from(reader.current_miniblock))
        })
    }

    /// Estimates the gas required for a given call request.
    ///
    /// # Arguments
    ///
    /// * `req` - A `CallRequest` struct representing the call request to estimate gas for.
    /// * `_block` - An optional `BlockNumber` struct representing the block number to estimate gas for.
    ///
    /// # Returns
    ///
    /// A `BoxFuture` containing a `Result` with a `U256` representing the estimated gas required.
    fn estimate_gas(
        &self,
        req: zksync_types::transaction_request::CallRequest,
        _block: Option<zksync_types::api::BlockNumber>,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<U256>> {
        let inner = Arc::clone(&self.inner);
        let reader = match inner.read() {
            Ok(r) => r,
            Err(_) => {
                return futures::future::err(into_jsrpc_error(Web3Error::InternalError)).boxed()
            }
        };

        let result: jsonrpc_core::Result<Fee> = reader.estimate_gas_impl(req);
        match result {
            Ok(fee) => Ok(fee.gas_limit).into_boxed_future(),
            Err(err) => return futures::future::err(err).boxed(),
        }
    }

    /// Returns the current gas price in U256 format.
    fn gas_price(&self) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<U256>> {
        let fair_l2_gas_price: u64 = L2_GAS_PRICE;
        Ok(U256::from(fair_l2_gas_price)).into_boxed_future()
    }

    /// Creates a filter object, based on filter options, to notify when the state changes (logs).
    /// To check if the state has changed, call `eth_getFilterChanges`.
    ///
    /// # Arguments
    ///
    /// * `filter`: The filter options -
    ///     fromBlock: - Integer block number, or the string "latest", "earliest" or "pending".
    ///     toBlock: - Integer block number, or the string "latest", "earliest" or "pending".
    ///     address: - Contract address or a list of addresses from which the logs should originate.
    ///     topics: - [H256] topics. Topics are order-dependent. Each topic can also be an array with "or" options.
    ///
    /// If the from `fromBlock` or `toBlock` option are equal to "latest" the filter continually appends logs for newly mined blocks.
    /// Topics are order-dependent. A transaction with a log with topics [A, B] will be matched by the following topic filters:
    ///     * [] "anything"
    ///     * [A] "A in first position (and anything after)"
    ///     * [null, B] "anything in first position AND B in second position (and anything after)"
    ///     * [A, B] "A in first position AND B in second position (and anything after)"
    ///     * [[A, B], [A, B]] "(A OR B) in first position AND (A OR B) in second position (and anything after)"
    ///
    /// # Returns
    ///
    /// A `BoxFuture` containing a `jsonrpc_core::Result` that resolves to an `U256` filter id.
    fn new_filter(&self, filter: Filter) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<U256>> {
        let inner = Arc::clone(&self.inner);
        let mut writer = match inner.write() {
            Ok(r) => r,
            Err(_) => {
                return futures::future::err(into_jsrpc_error(Web3Error::InternalError)).boxed()
            }
        };

        let from_block = filter
            .from_block
            .unwrap_or(zksync_types::api::BlockNumber::Latest);
        let to_block = filter
            .to_block
            .unwrap_or(zksync_types::api::BlockNumber::Latest);
        let addresses = filter.address.unwrap_or_default().0;
        let mut topics: [Option<HashSet<H256>>; 4] = Default::default();

        if let Some(filter_topics) = filter.topics {
            filter_topics
                .into_iter()
                .take(4)
                .enumerate()
                .for_each(|(i, maybe_topic_set)| {
                    if let Some(topic_set) = maybe_topic_set {
                        topics[i] = Some(topic_set.0.into_iter().collect());
                    }
                })
        }

        writer
            .filters
            .add_log_filter(from_block, to_block, addresses, topics)
            .map_err(|_| into_jsrpc_error(Web3Error::InternalError))
            .into_boxed_future()
    }

    /// Creates a filter in the node, to notify when a new block arrives.
    /// To check if the state has changed, call `eth_getFilterChanges`.
    ///
    /// # Returns
    ///
    /// A `BoxFuture` containing a `jsonrpc_core::Result` that resolves to an `U256` filter id.
    fn new_block_filter(&self) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<U256>> {
        let inner = Arc::clone(&self.inner);
        let mut writer = match inner.write() {
            Ok(r) => r,
            Err(_) => {
                return futures::future::err(into_jsrpc_error(Web3Error::InternalError)).boxed()
            }
        };

        writer
            .filters
            .add_block_filter()
            .map_err(|_| into_jsrpc_error(Web3Error::InternalError))
            .into_boxed_future()
    }

    /// Creates a filter in the node, to notify when new pending transactions arrive.
    /// To check if the state has changed, call `eth_getFilterChanges`.
    ///
    /// # Returns
    ///
    /// A `BoxFuture` containing a `jsonrpc_core::Result` that resolves to an `U256` filter id.
    fn new_pending_transaction_filter(
        &self,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<U256>> {
        let inner = Arc::clone(&self.inner);
        let mut writer = match inner.write() {
            Ok(r) => r,
            Err(_) => {
                return futures::future::err(into_jsrpc_error(Web3Error::InternalError)).boxed()
            }
        };

        writer
            .filters
            .add_pending_transaction_filter()
            .map_err(|_| into_jsrpc_error(Web3Error::InternalError))
            .into_boxed_future()
    }

    /// Uninstalls a filter with given id. Should always be called when watch is no longer needed.
    ///
    /// # Arguments
    ///
    /// * `id`: The filter id
    ///
    /// # Returns
    ///
    /// A `BoxFuture` containing a `jsonrpc_core::Result` that resolves to an `U256` filter id.
    fn uninstall_filter(&self, id: U256) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<bool>> {
        let inner = Arc::clone(&self.inner);
        let mut writer = match inner.write() {
            Ok(r) => r,
            Err(_) => {
                return futures::future::err(into_jsrpc_error(Web3Error::InternalError)).boxed()
            }
        };

        let result = writer.filters.remove_filter(id);
        Ok(result).into_boxed_future()
    }

    /// Returns an array of all logs matching a given filter.
    ///
    /// # Arguments
    ///
    /// * `filter`: The filter options -
    ///     fromBlock   - Integer block number, or the string "latest", "earliest" or "pending".
    ///     toBlock     - Integer block number, or the string "latest", "earliest" or "pending".
    ///     address     - Contract address or a list of addresses from which the logs should originate.
    ///     topics      - [H256] topics. Topics are order-dependent. Each topic can also be an array with "or" options.
    ///                   See `new_filter` documention for how to specify topics.
    ///
    /// # Returns
    ///
    /// A `BoxFuture` containing a `jsonrpc_core::Result` that resolves to an array of logs.
    fn get_logs(
        &self,
        filter: Filter,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<Vec<zksync_types::api::Log>>> {
        let reader = match self.inner.read() {
            Ok(r) => r,
            Err(_) => {
                return futures::future::err(into_jsrpc_error(Web3Error::InternalError)).boxed()
            }
        };
        let from_block = filter
            .from_block
            .unwrap_or(zksync_types::api::BlockNumber::Earliest);
        let to_block = filter
            .to_block
            .unwrap_or(zksync_types::api::BlockNumber::Latest);
        let addresses = filter.address.unwrap_or_default().0;
        let mut topics: [Option<HashSet<H256>>; 4] = Default::default();

        if let Some(filter_topics) = filter.topics {
            filter_topics
                .into_iter()
                .take(4)
                .enumerate()
                .for_each(|(i, maybe_topic_set)| {
                    if let Some(topic_set) = maybe_topic_set {
                        topics[i] = Some(topic_set.0.into_iter().collect());
                    }
                })
        }

        let log_filter = LogFilter::new(from_block, to_block, addresses, topics);

        let latest_block_number = U64::from(reader.current_miniblock);
        let logs = reader
            .tx_results
            .values()
            .flat_map(|tx_result| {
                tx_result
                    .receipt
                    .logs
                    .iter()
                    .filter(|log| log_filter.matches(log, latest_block_number))
                    .cloned()
            })
            .collect_vec();

        Ok(logs).into_boxed_future()
    }

    /// Returns an array of all logs matching filter with given id.
    ///
    /// # Arguments
    ///
    /// * `id`: The filter id
    ///
    /// # Returns
    ///
    /// A `BoxFuture` containing a `jsonrpc_core::Result` that resolves to an array of logs.
    fn get_filter_logs(
        &self,
        id: U256,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<FilterChanges>> {
        let reader = match self.inner.read() {
            Ok(r) => r,
            Err(_) => {
                return futures::future::err(into_jsrpc_error(Web3Error::InternalError)).boxed()
            }
        };

        let latest_block_number = U64::from(reader.current_miniblock);
        let logs = match reader.filters.get_filter(id) {
            Some(FilterType::Log(f)) => reader
                .tx_results
                .values()
                .flat_map(|tx_result| {
                    tx_result
                        .receipt
                        .logs
                        .iter()
                        .filter(|log| f.matches(log, latest_block_number))
                        .cloned()
                })
                .collect_vec(),
            _ => return futures::future::err(into_jsrpc_error(Web3Error::InternalError)).boxed(),
        };

        Ok(FilterChanges::Logs(logs)).into_boxed_future()
    }

    /// Polling method for a filter, which returns an array of logs, block hashes, or transaction hashes,
    /// depending on the filter type, which occurred since last poll.
    ///
    /// # Arguments
    ///
    /// * `id`: The filter id
    ///
    /// # Returns
    ///
    /// A `BoxFuture` containing a `jsonrpc_core::Result` that resolves to an array of logs, block hashes, or transaction hashes,
    /// depending on the filter type, which occurred since last poll.
    /// * Filters created with `eth_newFilter` return [Log] objects.
    /// * Filters created with `eth_newBlockFilter` return block hashes.
    /// * Filters created with `eth_newPendingTransactionFilter` return transaction hashes.
    fn get_filter_changes(
        &self,
        id: U256,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<FilterChanges>> {
        let inner = Arc::clone(&self.inner);
        let mut writer = match inner.write() {
            Ok(r) => r,
            Err(_) => {
                return futures::future::err(into_jsrpc_error(Web3Error::InternalError)).boxed()
            }
        };

        writer
            .filters
            .get_new_changes(id)
            .map_err(|_| into_jsrpc_error(Web3Error::InternalError))
            .into_boxed_future()
    }

    fn get_block_transaction_count_by_number(
        &self,
        block_number: zksync_types::api::BlockNumber,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<Option<U256>>> {
        let inner = Arc::clone(&self.inner);

        Box::pin(async move {
            let maybe_result = {
                let reader = match inner.read() {
                    Ok(r) => r,
                    Err(_) => return Err(into_jsrpc_error(Web3Error::InternalError)),
                };
                let number =
                    utils::to_real_block_number(block_number, U64::from(reader.current_miniblock))
                        .as_u64();

                reader
                    .block_hashes
                    .get(&number)
                    .and_then(|hash| reader.blocks.get(hash))
                    .map(|block| U256::from(block.transactions.len()))
                    .or_else(|| {
                        reader
                            .fork_storage
                            .inner
                            .read()
                            .expect("failed reading fork storage")
                            .fork
                            .as_ref()
                            .and_then(|fork| {
                                fork.fork_source
                                    .get_block_transaction_count_by_number(block_number)
                                    .ok()
                                    .flatten()
                            })
                    })
            };

            match maybe_result {
                Some(value) => Ok(Some(value)),
                None => Err(into_jsrpc_error(Web3Error::NoBlock)),
            }
        })
    }

    fn get_block_transaction_count_by_hash(
        &self,
        block_hash: zksync_basic_types::H256,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<Option<U256>>> {
        let inner = Arc::clone(&self.inner);

        Box::pin(async move {
            let reader = inner
                .read()
                .map_err(|_| into_jsrpc_error(Web3Error::InternalError))?;

            // try retrieving block from memory, and if unavailable subsequently from the fork
            let maybe_result = reader
                .blocks
                .get(&block_hash)
                .map(|block| U256::from(block.transactions.len()))
                .or_else(|| {
                    reader
                        .fork_storage
                        .inner
                        .read()
                        .expect("failed reading fork storage")
                        .fork
                        .as_ref()
                        .and_then(|fork| {
                            fork.fork_source
                                .get_block_transaction_count_by_hash(block_hash)
                                .ok()
                                .flatten()
                        })
                });

            match maybe_result {
                Some(value) => Ok(Some(value)),
                None => Err(into_jsrpc_error(Web3Error::NoBlock)),
            }
        })
    }

    fn get_storage(
        &self,
        _address: zksync_basic_types::Address,
        _idx: U256,
        _block: Option<zksync_types::api::BlockIdVariant>,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<zksync_basic_types::H256>> {
        not_implemented("get_storage")
    }

    fn get_transaction_by_block_hash_and_index(
        &self,
        _block_hash: zksync_basic_types::H256,
        _index: zksync_basic_types::web3::types::Index,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<Option<zksync_types::api::Transaction>>> {
        not_implemented("get_transaction_by_block_hash_and_index")
    }

    fn get_transaction_by_block_number_and_index(
        &self,
        _block_number: zksync_types::api::BlockNumber,
        _index: zksync_basic_types::web3::types::Index,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<Option<zksync_types::api::Transaction>>> {
        not_implemented("get_transaction_by_block_number_and_index")
    }

    fn protocol_version(&self) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<String>> {
        not_implemented("protocol_version")
    }

    fn syncing(
        &self,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<zksync_basic_types::web3::types::SyncState>>
    {
        Ok(zksync_basic_types::web3::types::SyncState::NotSyncing).into_boxed_future()
    }

    fn accounts(
        &self,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<Vec<zksync_basic_types::Address>>> {
        not_implemented("accounts")
    }

    fn coinbase(
        &self,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<zksync_basic_types::Address>> {
        not_implemented("coinbase")
    }

    fn compilers(&self) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<Vec<String>>> {
        not_implemented("compilers")
    }

    fn hashrate(&self) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<U256>> {
        not_implemented("hashrate")
    }

    fn get_uncle_count_by_block_hash(
        &self,
        _hash: zksync_basic_types::H256,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<Option<U256>>> {
        not_implemented("get_uncle_count_by_block_hash")
    }

    fn get_uncle_count_by_block_number(
        &self,
        _number: zksync_types::api::BlockNumber,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<Option<U256>>> {
        not_implemented("get_uncle_count_by_block_number")
    }

    fn mining(&self) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<bool>> {
        not_implemented("mining")
    }

    /// Returns the fee history for a given range of blocks.
    ///
    /// Note: This implementation is limited to using the hard-coded value
    /// of L2_GAS_PRICE as the history gas price
    ///
    /// # Arguments
    ///
    /// * `block_count` - The number of blocks in the requested range. Between 1 and 1024 blocks can be requested in a single query. It will return less than the requested range if not all blocks are available.
    /// * `newest_block` - The highest number block of the requested range. As this implementation is using hard-coded values, this argument is ignored.
    /// * `reward_percentiles` - A list of percentile values with a monotonic increase in value.
    ///
    /// # Returns
    ///
    /// A `BoxFuture` containing a `Result` with a `FeeHistory` representing the fee history of the specified range of blocks.
    fn fee_history(
        &self,
        block_count: U64,
        _newest_block: zksync_types::api::BlockNumber,
        reward_percentiles: Vec<f32>,
    ) -> jsonrpc_core::BoxFuture<jsonrpc_core::Result<FeeHistory>> {
        let inner = Arc::clone(&self.inner);

        Box::pin(async move {
            let reader = inner
                .read()
                .map_err(|_| into_jsrpc_error(Web3Error::InternalError))?;

            let block_count = block_count
                .as_u64()
                .min(1024)
                // Can't be more than the total number of blocks
                .clamp(1, reader.current_miniblock + 1);

            let mut base_fee_per_gas = vec![U256::from(L2_GAS_PRICE); block_count as usize];

            let oldest_block = reader.current_miniblock + 1 - base_fee_per_gas.len() as u64;
            // We do not store gas used ratio for blocks, returns array of zeroes as a placeholder.
            let gas_used_ratio = vec![0.0; base_fee_per_gas.len()];
            // Effective priority gas price is currently 0.
            let reward = Some(vec![
                vec![U256::zero(); reward_percentiles.len()];
                base_fee_per_gas.len()
            ]);

            // `base_fee_per_gas` for next miniblock cannot be calculated, appending last fee as a placeholder.
            base_fee_per_gas.push(*base_fee_per_gas.last().unwrap());

            Ok(FeeHistory {
                oldest_block: web3::types::BlockNumber::Number(oldest_block.into()),
                base_fee_per_gas,
                gas_used_ratio,
                reward,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        cache::CacheConfig,
        http_fork_source::HttpForkSource,
        node::InMemoryNode,
        testing::{self, ForkBlockConfig, LogBuilder, MockServer},
    };
    use zksync_types::api::BlockNumber;
    use zksync_web3_decl::types::{SyncState, ValueOrArray};

    use super::*;

    #[tokio::test]
    async fn test_eth_syncing() {
        let node = InMemoryNode::<HttpForkSource>::default();
        let syncing = node.syncing().await.expect("failed syncing");
        assert!(matches!(syncing, SyncState::NotSyncing));
    }

    #[tokio::test]
    async fn test_get_fee_history_with_1_block() {
        let node = InMemoryNode::<HttpForkSource>::default();

        let fee_history = node
            .fee_history(U64::from(1), BlockNumber::Latest, vec![25.0, 50.0, 75.0])
            .await
            .expect("fee_history failed");

        assert_eq!(
            fee_history.oldest_block,
            web3::types::BlockNumber::Number(U64::from(0))
        );
        assert_eq!(
            fee_history.base_fee_per_gas,
            vec![U256::from(L2_GAS_PRICE); 2]
        );
        assert_eq!(fee_history.gas_used_ratio, vec![0.0]);
        assert_eq!(fee_history.reward, Some(vec![vec![U256::from(0); 3]]));
    }

    #[tokio::test]
    async fn test_get_fee_history_with_no_reward_percentiles() {
        let node = InMemoryNode::<HttpForkSource>::default();

        let fee_history = node
            .fee_history(U64::from(1), BlockNumber::Latest, vec![])
            .await
            .expect("fee_history failed");

        assert_eq!(
            fee_history.oldest_block,
            web3::types::BlockNumber::Number(U64::from(0))
        );
        assert_eq!(
            fee_history.base_fee_per_gas,
            vec![U256::from(L2_GAS_PRICE); 2]
        );
        assert_eq!(fee_history.gas_used_ratio, vec![0.0]);
        assert_eq!(fee_history.reward, Some(vec![vec![]]));
    }

    #[tokio::test]
    async fn test_get_fee_history_with_multiple_blocks() {
        // Arrange
        let node = InMemoryNode::<HttpForkSource>::default();
        testing::apply_tx(&node, H256::repeat_byte(0x01));

        // Act
        let latest_block = node
            .get_block_number()
            .await
            .expect("Block number fetch failed");
        let fee_history = node
            .fee_history(U64::from(2), BlockNumber::Latest, vec![25.0, 50.0, 75.0])
            .await
            .expect("fee_history failed");

        // Assert
        // We should receive 2 fees: from block 1 and 2.
        assert_eq!(latest_block, U64::from(2));
        assert_eq!(
            fee_history.oldest_block,
            web3::types::BlockNumber::Number(U64::from(1))
        );
        assert_eq!(
            fee_history.base_fee_per_gas,
            vec![U256::from(L2_GAS_PRICE); 3]
        );
        assert_eq!(fee_history.gas_used_ratio, vec![0.0, 0.0]);
        assert_eq!(fee_history.reward, Some(vec![vec![U256::from(0); 3]; 2]));
    }

    #[tokio::test]
    async fn test_get_block_by_hash_produces_no_block_error_for_non_existing_block() {
        let node = InMemoryNode::<HttpForkSource>::default();

        let expected_err = into_jsrpc_error(Web3Error::NoBlock);
        let result = node.get_block_by_hash(H256::repeat_byte(0x01), false).await;

        assert_eq!(expected_err, result.unwrap_err());
    }

    #[tokio::test]
    async fn test_node_run_has_genesis_block() {
        let node = InMemoryNode::<HttpForkSource>::default();

        let block = node
            .get_block_by_number(BlockNumber::Latest, false)
            .await
            .expect("failed fetching block by hash")
            .expect("no block");

        assert_eq!(0, block.number.as_u64());
        assert_eq!(compute_hash(0, H256::zero()), block.hash);
    }

    #[tokio::test]
    async fn test_get_block_by_hash_for_produced_block() {
        let node = InMemoryNode::<HttpForkSource>::default();
        let expected_block_hash = testing::apply_tx(&node, H256::repeat_byte(0x01));

        let actual_block = node
            .get_block_by_hash(expected_block_hash, false)
            .await
            .expect("failed fetching block by hash")
            .expect("no block");

        assert_eq!(expected_block_hash, actual_block.hash);
        assert_eq!(U64::from(1), actual_block.number);
        assert_eq!(Some(U64::from(1)), actual_block.l1_batch_number);
    }

    #[tokio::test]
    async fn test_node_block_mapping_is_correctly_populated_when_using_fork_source() {
        let input_block_number = 8;
        let input_block_hash = H256::repeat_byte(0x01);
        let mock_server = MockServer::run_with_config(ForkBlockConfig {
            number: input_block_number,
            hash: input_block_hash,
            transaction_count: 0,
        });

        let node = InMemoryNode::<HttpForkSource>::new(
            Some(ForkDetails::from_network(&mock_server.url(), None, CacheConfig::None).await),
            crate::node::ShowCalls::None,
            ShowStorageLogs::None,
            ShowVMDetails::None,
            ShowGasDetails::None,
            false,
            &system_contracts::Options::BuiltIn,
        );

        let inner = node.inner.read().unwrap();
        assert!(
            inner.blocks.contains_key(&input_block_hash),
            "block wasn't cached"
        );
        assert!(
            inner.block_hashes.contains_key(&input_block_number),
            "block number wasn't cached"
        );
    }

    #[tokio::test]
    async fn test_get_block_by_hash_uses_fork_source() {
        let input_block_hash = H256::repeat_byte(0x01);

        let mock_server = MockServer::run_with_config(ForkBlockConfig {
            number: 10,
            hash: H256::repeat_byte(0xab),
            transaction_count: 0,
        });
        let mock_block_number = 8;
        let block_response = testing::BlockResponseBuilder::new()
            .set_hash(input_block_hash)
            .set_number(mock_block_number)
            .build();
        mock_server.expect(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "eth_getBlockByHash",
                "params": [
                    format!("{input_block_hash:#x}"),
                    true
                ],
            }),
            block_response,
        );
        let node = InMemoryNode::<HttpForkSource>::new(
            Some(ForkDetails::from_network(&mock_server.url(), None, CacheConfig::None).await),
            crate::node::ShowCalls::None,
            ShowStorageLogs::None,
            ShowVMDetails::None,
            ShowGasDetails::None,
            false,
            &system_contracts::Options::BuiltIn,
        );

        let actual_block = node
            .get_block_by_hash(input_block_hash, false)
            .await
            .expect("failed fetching block by hash")
            .expect("no block");

        assert_eq!(input_block_hash, actual_block.hash);
        assert_eq!(U64::from(mock_block_number), actual_block.number);
        assert_eq!(Some(U64::from(6)), actual_block.l1_batch_number);
    }

    #[tokio::test]
    async fn test_get_block_by_number_produces_no_block_error_for_non_existing_block() {
        let node = InMemoryNode::<HttpForkSource>::default();

        let expected_err = into_jsrpc_error(Web3Error::NoBlock);
        let result = node
            .get_block_by_number(BlockNumber::Number(U64::from(42)), false)
            .await;

        assert_eq!(expected_err, result.unwrap_err());
    }

    #[tokio::test]
    async fn test_get_block_by_number_for_produced_block() {
        let node = InMemoryNode::<HttpForkSource>::default();
        testing::apply_tx(&node, H256::repeat_byte(0x01));
        let expected_block_number = 1;

        let actual_block = node
            .get_block_by_number(BlockNumber::Number(U64::from(expected_block_number)), false)
            .await
            .expect("failed fetching block by hash")
            .expect("no block");

        assert_eq!(U64::from(expected_block_number), actual_block.number);
        assert_eq!(1, actual_block.transactions.len());
    }

    #[tokio::test]
    async fn test_get_block_by_number_uses_fork_source_if_missing_number() {
        let mock_server = MockServer::run_with_config(ForkBlockConfig {
            number: 10,
            hash: H256::repeat_byte(0xab),
            transaction_count: 0,
        });
        let mock_block_number = 8;
        let block_response = testing::BlockResponseBuilder::new()
            .set_number(mock_block_number)
            .build();
        mock_server.expect(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "eth_getBlockByNumber",
                "params": [
                    "0x8",
                    true
                ],
            }),
            block_response,
        );
        let node = InMemoryNode::<HttpForkSource>::new(
            Some(ForkDetails::from_network(&mock_server.url(), None, CacheConfig::None).await),
            crate::node::ShowCalls::None,
            ShowStorageLogs::None,
            ShowVMDetails::None,
            ShowGasDetails::None,
            false,
            &system_contracts::Options::BuiltIn,
        );

        let actual_block = node
            .get_block_by_number(BlockNumber::Number(U64::from(8)), false)
            .await
            .expect("failed fetching block by hash")
            .expect("no block");
        assert_eq!(U64::from(mock_block_number), actual_block.number);
    }

    #[tokio::test]
    async fn test_get_block_by_number_for_latest_block_produced_locally() {
        let node = InMemoryNode::<HttpForkSource>::default();
        testing::apply_tx(&node, H256::repeat_byte(0x01));

        // The latest block, will be the 'virtual' one with 0 transactions (block 2).
        let virtual_block = node
            .get_block_by_number(BlockNumber::Latest, true)
            .await
            .expect("failed fetching block by hash")
            .expect("no block");

        assert_eq!(U64::from(2), virtual_block.number);
        assert_eq!(0, virtual_block.transactions.len());

        let actual_block = node
            .get_block_by_number(BlockNumber::Number(U64::from(1)), true)
            .await
            .expect("failed fetching block by hash")
            .expect("no block");

        assert_eq!(U64::from(1), actual_block.number);
        assert_eq!(1, actual_block.transactions.len());
    }

    #[tokio::test]
    async fn test_get_block_by_number_uses_locally_available_block_for_latest_block() {
        let input_block_number = 10;
        let mock_server = MockServer::run_with_config(ForkBlockConfig {
            number: input_block_number,
            hash: H256::repeat_byte(0x01),
            transaction_count: 0,
        });

        let node = InMemoryNode::<HttpForkSource>::new(
            Some(ForkDetails::from_network(&mock_server.url(), None, CacheConfig::None).await),
            crate::node::ShowCalls::None,
            ShowStorageLogs::None,
            ShowVMDetails::None,
            ShowGasDetails::None,
            false,
            &system_contracts::Options::BuiltIn,
        );

        let actual_block = node
            .get_block_by_number(BlockNumber::Latest, false)
            .await
            .expect("failed fetching block by hash")
            .expect("no block");
        assert_eq!(U64::from(input_block_number), actual_block.number);
    }

    #[tokio::test]
    async fn test_get_block_by_number_uses_fork_source_for_earliest_block() {
        let mock_server = MockServer::run_with_config(ForkBlockConfig {
            number: 10,
            hash: H256::repeat_byte(0xab),
            transaction_count: 0,
        });
        let input_block_number = 1;
        mock_server.expect(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "eth_getBlockByNumber",
                "params": [
                    "earliest",
                    true
                ],
            }),
            testing::BlockResponseBuilder::new()
                .set_number(input_block_number)
                .build(),
        );
        let node = InMemoryNode::<HttpForkSource>::new(
            Some(ForkDetails::from_network(&mock_server.url(), None, CacheConfig::None).await),
            crate::node::ShowCalls::None,
            ShowStorageLogs::None,
            ShowVMDetails::None,
            ShowGasDetails::None,
            false,
            &system_contracts::Options::BuiltIn,
        );

        let actual_block = node
            .get_block_by_number(BlockNumber::Earliest, false)
            .await
            .expect("failed fetching block by hash")
            .expect("no block");
        assert_eq!(U64::from(input_block_number), actual_block.number);
    }

    #[tokio::test]
    async fn test_get_block_by_number_uses_locally_available_for_latest_alike_blocks() {
        for block_number in [
            BlockNumber::Pending,
            BlockNumber::Committed,
            BlockNumber::Finalized,
        ] {
            let input_block_number = 10;
            let mock_server = MockServer::run_with_config(ForkBlockConfig {
                number: input_block_number,
                hash: H256::repeat_byte(0xab),
                transaction_count: 0,
            });
            let node = InMemoryNode::<HttpForkSource>::new(
                Some(ForkDetails::from_network(&mock_server.url(), None, CacheConfig::None).await),
                crate::node::ShowCalls::None,
                ShowStorageLogs::None,
                ShowVMDetails::None,
                ShowGasDetails::None,
                false,
                &system_contracts::Options::BuiltIn,
            );

            let actual_block = node
                .get_block_by_number(block_number, false)
                .await
                .expect("failed fetching block by hash")
                .expect("no block");
            assert_eq!(
                U64::from(input_block_number),
                actual_block.number,
                "case {}",
                block_number,
            );
        }
    }

    #[tokio::test]
    async fn test_get_block_transaction_count_by_hash_for_produced_block() {
        let node = InMemoryNode::<HttpForkSource>::default();

        let expected_block_hash = testing::apply_tx(&node, H256::repeat_byte(0x01));
        let actual_transaction_count = node
            .get_block_transaction_count_by_hash(expected_block_hash)
            .await
            .expect("failed fetching block by hash")
            .expect("no result");

        assert_eq!(U256::from(1), actual_transaction_count);
    }

    #[tokio::test]
    async fn test_get_block_transaction_count_by_hash_uses_fork_source() {
        let mock_server = MockServer::run_with_config(ForkBlockConfig {
            number: 10,
            hash: H256::repeat_byte(0xab),
            transaction_count: 0,
        });
        let input_block_hash = H256::repeat_byte(0x01);
        let input_transaction_count = 1;
        mock_server.expect(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "eth_getBlockTransactionCountByHash",
                "params": [
                    format!("{:#x}", input_block_hash),
                ],
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "result": format!("{:#x}", input_transaction_count),
            }),
        );
        let node = InMemoryNode::<HttpForkSource>::new(
            Some(ForkDetails::from_network(&mock_server.url(), None, CacheConfig::None).await),
            crate::node::ShowCalls::None,
            ShowStorageLogs::None,
            ShowVMDetails::None,
            ShowGasDetails::None,
            false,
            &system_contracts::Options::BuiltIn,
        );

        let actual_transaction_count = node
            .get_block_transaction_count_by_hash(input_block_hash)
            .await
            .expect("failed fetching block by hash")
            .expect("no result");

        assert_eq!(
            U256::from(input_transaction_count),
            actual_transaction_count
        );
    }

    #[tokio::test]
    async fn test_get_block_transaction_count_by_number_for_produced_block() {
        let node = InMemoryNode::<HttpForkSource>::default();

        testing::apply_tx(&node, H256::repeat_byte(0x01));
        let actual_transaction_count = node
            .get_block_transaction_count_by_number(BlockNumber::Number(U64::from(1)))
            .await
            .expect("failed fetching block by hash")
            .expect("no result");

        assert_eq!(U256::from(1), actual_transaction_count);
    }

    #[tokio::test]
    async fn test_get_block_transaction_count_by_number_uses_fork_source() {
        let mock_server = MockServer::run_with_config(ForkBlockConfig {
            number: 10,
            hash: H256::repeat_byte(0xab),
            transaction_count: 0,
        });
        let input_block_number = 1;
        let input_transaction_count = 1;
        mock_server.expect(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "eth_getBlockTransactionCountByNumber",
                "params": [
                    format!("{:#x}", input_block_number),
                ],
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "result": format!("{:#x}", input_transaction_count),
            }),
        );

        let node = InMemoryNode::<HttpForkSource>::new(
            Some(ForkDetails::from_network(&mock_server.url(), None, CacheConfig::None).await),
            crate::node::ShowCalls::None,
            ShowStorageLogs::None,
            ShowVMDetails::None,
            ShowGasDetails::None,
            false,
            &system_contracts::Options::BuiltIn,
        );

        let actual_transaction_count = node
            .get_block_transaction_count_by_number(BlockNumber::Number(U64::from(1)))
            .await
            .expect("failed fetching block by hash")
            .expect("no result");

        assert_eq!(
            U256::from(input_transaction_count),
            actual_transaction_count
        );
    }

    #[tokio::test]
    async fn test_get_block_transaction_count_by_number_earliest_uses_fork_source() {
        let mock_server = MockServer::run_with_config(ForkBlockConfig {
            number: 10,
            hash: H256::repeat_byte(0xab),
            transaction_count: 0,
        });
        let input_transaction_count = 1;
        mock_server.expect(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "eth_getBlockTransactionCountByNumber",
                "params": [
                    "earliest",
                ],
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "result": format!("{:#x}", input_transaction_count),
            }),
        );

        let node = InMemoryNode::<HttpForkSource>::new(
            Some(ForkDetails::from_network(&mock_server.url(), None, CacheConfig::None).await),
            crate::node::ShowCalls::None,
            ShowStorageLogs::None,
            ShowVMDetails::None,
            ShowGasDetails::None,
            false,
            &system_contracts::Options::BuiltIn,
        );

        let actual_transaction_count = node
            .get_block_transaction_count_by_number(BlockNumber::Earliest)
            .await
            .expect("failed fetching block by hash")
            .expect("no result");

        assert_eq!(
            U256::from(input_transaction_count),
            actual_transaction_count
        );
    }

    #[tokio::test]
    async fn test_get_block_transaction_count_by_number_latest_alike_uses_fork_source() {
        for block_number in [
            BlockNumber::Latest,
            BlockNumber::Pending,
            BlockNumber::Committed,
            BlockNumber::Finalized,
        ] {
            let input_transaction_count = 1;
            let mock_server = MockServer::run_with_config(ForkBlockConfig {
                number: 10,
                transaction_count: input_transaction_count,
                hash: H256::repeat_byte(0xab),
            });

            let node = InMemoryNode::<HttpForkSource>::new(
                Some(ForkDetails::from_network(&mock_server.url(), None, CacheConfig::None).await),
                crate::node::ShowCalls::None,
                ShowStorageLogs::None,
                ShowVMDetails::None,
                ShowGasDetails::None,
                false,
                &system_contracts::Options::BuiltIn,
            );

            let actual_transaction_count = node
                .get_block_transaction_count_by_number(block_number)
                .await
                .expect("failed fetching block by hash")
                .expect("no result");

            assert_eq!(
                U256::from(input_transaction_count),
                actual_transaction_count,
                "case {}",
                block_number,
            );
        }
    }

    #[tokio::test]
    async fn test_get_transaction_receipt_uses_produced_block_hash() {
        let node = InMemoryNode::<HttpForkSource>::default();
        let tx_hash = H256::repeat_byte(0x01);
        let expected_block_hash = testing::apply_tx(&node, tx_hash);

        let actual_tx_receipt = node
            .get_transaction_receipt(tx_hash)
            .await
            .expect("failed fetching transaction receipt by hash")
            .expect("no transaction receipt");

        assert_eq!(Some(expected_block_hash), actual_tx_receipt.block_hash);
    }

    #[tokio::test]
    async fn test_new_block_filter_returns_filter_id() {
        let node = InMemoryNode::<HttpForkSource>::default();

        let actual_filter_id = node
            .new_block_filter()
            .await
            .expect("failed creating filter");

        assert_eq!(U256::from(1), actual_filter_id);
    }

    #[tokio::test]
    async fn test_new_filter_returns_filter_id() {
        let node = InMemoryNode::<HttpForkSource>::default();

        let actual_filter_id = node
            .new_filter(Filter::default())
            .await
            .expect("failed creating filter");

        assert_eq!(U256::from(1), actual_filter_id);
    }

    #[tokio::test]
    async fn test_new_pending_transaction_filter_returns_filter_id() {
        let node = InMemoryNode::<HttpForkSource>::default();

        let actual_filter_id = node
            .new_pending_transaction_filter()
            .await
            .expect("failed creating filter");

        assert_eq!(U256::from(1), actual_filter_id);
    }

    #[tokio::test]
    async fn test_uninstall_filter_returns_true_if_filter_exists() {
        let node = InMemoryNode::<HttpForkSource>::default();
        let filter_id = node
            .new_block_filter()
            .await
            .expect("failed creating filter");

        let actual_result = node
            .uninstall_filter(filter_id)
            .await
            .expect("failed creating filter");

        assert!(actual_result);
    }

    #[tokio::test]
    async fn test_uninstall_filter_returns_false_if_filter_does_not_exist() {
        let node = InMemoryNode::<HttpForkSource>::default();

        let actual_result = node
            .uninstall_filter(U256::from(100))
            .await
            .expect("failed creating filter");

        assert!(!actual_result);
    }

    #[tokio::test]
    async fn test_get_filter_changes_returns_block_hash_updates_only_once() {
        let node = InMemoryNode::<HttpForkSource>::default();
        let filter_id = node
            .new_block_filter()
            .await
            .expect("failed creating filter");
        let block_hash = testing::apply_tx(&node, H256::repeat_byte(0x1));

        match node
            .get_filter_changes(filter_id)
            .await
            .expect("failed getting filter changes")
        {
            FilterChanges::Hashes(result) => {
                // Get the block hash and the virtual block hash.
                assert_eq!(2, result.len());
                assert_eq!(block_hash, result[0]);
            }
            changes => panic!("unexpected filter changes: {:?}", changes),
        }

        match node
            .get_filter_changes(filter_id)
            .await
            .expect("failed getting filter changes")
        {
            FilterChanges::Empty(_) => (),
            changes => panic!("expected no changes in the second call, got {:?}", changes),
        }
    }

    #[tokio::test]
    async fn test_get_filter_changes_returns_log_updates_only_once() {
        let node = InMemoryNode::<HttpForkSource>::default();
        let filter_id = node
            .new_filter(Filter {
                from_block: None,
                to_block: None,
                address: None,
                topics: None,
                block_hash: None,
            })
            .await
            .expect("failed creating filter");
        testing::apply_tx(&node, H256::repeat_byte(0x1));

        match node
            .get_filter_changes(filter_id)
            .await
            .expect("failed getting filter changes")
        {
            FilterChanges::Logs(result) => assert_eq!(3, result.len()),
            changes => panic!("unexpected filter changes: {:?}", changes),
        }

        match node
            .get_filter_changes(filter_id)
            .await
            .expect("failed getting filter changes")
        {
            FilterChanges::Empty(_) => (),
            changes => panic!("expected no changes in the second call, got {:?}", changes),
        }
    }

    #[tokio::test]
    async fn test_get_filter_changes_returns_pending_transaction_updates_only_once() {
        let node = InMemoryNode::<HttpForkSource>::default();
        let filter_id = node
            .new_pending_transaction_filter()
            .await
            .expect("failed creating filter");
        testing::apply_tx(&node, H256::repeat_byte(0x1));

        match node
            .get_filter_changes(filter_id)
            .await
            .expect("failed getting filter changes")
        {
            FilterChanges::Hashes(result) => assert_eq!(vec![H256::repeat_byte(0x1)], result),
            changes => panic!("unexpected filter changes: {:?}", changes),
        }

        match node
            .get_filter_changes(filter_id)
            .await
            .expect("failed getting filter changes")
        {
            FilterChanges::Empty(_) => (),
            changes => panic!("expected no changes in the second call, got {:?}", changes),
        }
    }

    #[tokio::test]
    async fn test_get_filter_logs_returns_matching_logs_for_valid_id() {
        let node = InMemoryNode::<HttpForkSource>::default();

        // populate tx receipts with 2 tx each having logs
        {
            let mut writer = node.inner.write().unwrap();
            writer.tx_results.insert(
                H256::repeat_byte(0x1),
                TransactionResult {
                    info: testing::default_tx_execution_info(),
                    receipt: TransactionReceipt {
                        logs: vec![LogBuilder::new()
                            .set_address(H160::repeat_byte(0xa1))
                            .build()],
                        ..Default::default()
                    },
                },
            );
            writer.tx_results.insert(
                H256::repeat_byte(0x2),
                TransactionResult {
                    info: testing::default_tx_execution_info(),
                    receipt: TransactionReceipt {
                        logs: vec![
                            LogBuilder::new()
                                .set_address(H160::repeat_byte(0xa1))
                                .build(),
                            LogBuilder::new()
                                .set_address(H160::repeat_byte(0xa2))
                                .build(),
                        ],
                        ..Default::default()
                    },
                },
            );
        }

        let filter_id = node
            .new_filter(Filter {
                address: Some(ValueOrArray(vec![H160::repeat_byte(0xa1)])),
                ..Default::default()
            })
            .await
            .expect("failed creating filter");

        match node
            .get_filter_logs(filter_id)
            .await
            .expect("failed getting filter changes")
        {
            FilterChanges::Logs(result) => assert_eq!(2, result.len()),
            changes => panic!("unexpected filter changes: {:?}", changes),
        }
    }

    #[tokio::test]
    async fn test_get_filter_logs_returns_error_for_invalid_id() {
        let node = InMemoryNode::<HttpForkSource>::default();

        // populate tx receipts with 2 tx each having logs
        {
            let mut writer = node.inner.write().unwrap();
            writer.tx_results.insert(
                H256::repeat_byte(0x1),
                TransactionResult {
                    info: testing::default_tx_execution_info(),
                    receipt: TransactionReceipt {
                        logs: vec![LogBuilder::new()
                            .set_address(H160::repeat_byte(0xa1))
                            .build()],
                        ..Default::default()
                    },
                },
            );
        }

        let invalid_filter_id = U256::from(100);
        let result = node.get_filter_logs(invalid_filter_id).await;

        assert!(result.is_err(), "expected an error for invalid filter id");
    }

    #[tokio::test]
    async fn test_get_logs_returns_matching_logs() {
        let node = InMemoryNode::<HttpForkSource>::default();

        // populate tx receipts with 2 tx each having logs
        {
            let mut writer = node.inner.write().unwrap();
            writer.tx_results.insert(
                H256::repeat_byte(0x1),
                TransactionResult {
                    info: testing::default_tx_execution_info(),
                    receipt: TransactionReceipt {
                        logs: vec![LogBuilder::new()
                            .set_address(H160::repeat_byte(0xa1))
                            .build()],
                        ..Default::default()
                    },
                },
            );
            writer.tx_results.insert(
                H256::repeat_byte(0x2),
                TransactionResult {
                    info: testing::default_tx_execution_info(),
                    receipt: TransactionReceipt {
                        logs: vec![
                            LogBuilder::new()
                                .set_address(H160::repeat_byte(0xa1))
                                .build(),
                            LogBuilder::new()
                                .set_address(H160::repeat_byte(0xa2))
                                .build(),
                        ],
                        ..Default::default()
                    },
                },
            );
        }

        let result = node
            .get_logs(Filter {
                address: Some(ValueOrArray(vec![H160::repeat_byte(0xa2)])),
                ..Default::default()
            })
            .await
            .expect("failed getting filter changes");
        assert_eq!(1, result.len());

        let result = node
            .get_logs(Filter {
                address: Some(ValueOrArray(vec![H160::repeat_byte(0xa1)])),
                ..Default::default()
            })
            .await
            .expect("failed getting filter changes");
        assert_eq!(2, result.len());

        let result = node
            .get_logs(Filter {
                address: Some(ValueOrArray(vec![H160::repeat_byte(0x11)])),
                ..Default::default()
            })
            .await
            .expect("failed getting filter changes");
        assert_eq!(0, result.len());
    }
}
