//! The `bank` module tracks client accounts and the progress of on-chain
//! programs.
//!
//! A single bank relates to a block produced by a single leader and each bank
//! except for the genesis bank points back to a parent bank.
//!
//! The bank is the main entrypoint for processing verified transactions with the function
//! `Bank::process_transactions`
//!
//! It does this by loading the accounts using the reference it holds on the account store,
//! and then passing those to an InvokeContext which handles loading the programs specified
//! by the Transaction and executing it.
//!
//! The bank then stores the results to the accounts store.
//!
//! It then has APIs for retrieving if a transaction has been processed and it's status.
//! See `get_signature_status` et al.
//!
//! Bank lifecycle:
//!
//! A bank is newly created and open to transactions. Transactions are applied
//! until either the bank reached the tick count when the node is the leader for that slot, or the
//! node has applied all transactions present in all `Entry`s in the slot.
//!
//! Once it is complete, the bank can then be frozen. After frozen, no more transactions can
//! be applied or state changes made. At the frozen step, rent will be applied and various
//! sysvar special accounts update to the new state of the system.
//!
//! After frozen, and the bank has had the appropriate number of votes on it, then it can become
//! rooted. At this point, it will not be able to be removed from the chain and the
//! state is finalized.
//!
//! It offers a high-level API that signs transactions
//! on behalf of the caller, and a low-level API for when they have
//! already been signed and verified.
use {
    crate::{
        account_saver::collect_accounts_to_store,
        bank::{
            metrics::*,
            partitioned_epoch_rewards::{EpochRewardStatus, StakeRewards, VoteRewardsAccounts},
        },
        bank_forks::BankForks,
        epoch_stakes::{NodeVoteAccounts, VersionedEpochStakes},
        inflation_rewards::points::InflationPointCalculationEvent,
        installed_scheduler_pool::{BankWithScheduler, InstalledSchedulerRwLock},
        rent_collector::RentCollectorWithMetrics,
        runtime_config::RuntimeConfig,
        snapshot_hash::SnapshotHash,
        stake_account::StakeAccount,
        stake_weighted_timestamp::{
            calculate_stake_weighted_timestamp, MaxAllowableDrift,
            MAX_ALLOWABLE_DRIFT_PERCENTAGE_FAST, MAX_ALLOWABLE_DRIFT_PERCENTAGE_SLOW_V2,
        },
        stakes::{SerdeStakesToStakeFormat, Stakes, StakesCache},
        status_cache::{SlotDelta, StatusCache},
        transaction_batch::{OwnedOrBorrowed, TransactionBatch},
    },
    accounts_lt_hash::{CacheValue as AccountsLtHashCacheValue, Stats as AccountsLtHashStats},
    agave_feature_set::{self as feature_set, FeatureSet},
    agave_precompiles::{get_precompile, get_precompiles, is_precompile},
    agave_reserved_account_keys::ReservedAccountKeys,
    agave_syscalls::{
        create_program_runtime_environment_v1, create_program_runtime_environment_v2,
    },
    ahash::{AHashSet, RandomState},
    dashmap::DashMap,
    log::*,
    partitioned_epoch_rewards::PartitionedRewardsCalculation,
    rayon::ThreadPoolBuilder,
    serde::Serialize,
    solana_account::{
        create_account_shared_data_with_fields as create_account, from_account, Account,
        AccountSharedData, InheritableAccountFields, ReadableAccount, WritableAccount,
    },
    solana_accounts_db::{
        account_locks::validate_account_locks,
        accounts::{AccountAddressFilter, Accounts, PubkeyAccountSlot},
        accounts_db::{AccountStorageEntry, AccountsDb, AccountsDbConfig, DuplicatesLtHash},
        accounts_hash::AccountsLtHash,
        accounts_index::{IndexKey, ScanConfig, ScanResult},
        accounts_update_notifier_interface::AccountsUpdateNotifier,
        ancestors::{Ancestors, AncestorsForSerialization},
        blockhash_queue::BlockhashQueue,
        storable_accounts::StorableAccounts,
    },
    solana_builtins::{prototype::BuiltinPrototype, BUILTINS, STATELESS_BUILTINS},
    solana_clock::{
        BankId, Epoch, Slot, SlotIndex, UnixTimestamp, INITIAL_RENT_EPOCH, MAX_PROCESSING_AGE,
        MAX_TRANSACTION_FORWARDING_DELAY,
    },
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_compute_budget_instruction::instructions_processor::process_compute_budget_instructions,
    solana_cost_model::{block_cost_limits::simd_0286_block_limits, cost_tracker::CostTracker},
    solana_epoch_info::EpochInfo,
    solana_epoch_schedule::EpochSchedule,
    solana_feature_gate_interface as feature,
    solana_fee::FeeFeatures,
    solana_fee_calculator::FeeRateGovernor,
    solana_fee_structure::{FeeBudgetLimits, FeeDetails, FeeStructure},
    solana_genesis_config::{ClusterType, GenesisConfig},
    solana_hard_forks::HardForks,
    solana_hash::Hash,
    solana_inflation::Inflation,
    solana_keypair::Keypair,
    solana_lattice_hash::lt_hash::LtHash,
    solana_measure::{meas_dur, measure::Measure, measure_time, measure_us},
    solana_message::{inner_instruction::InnerInstructions, AccountKeys, SanitizedMessage},
    solana_native_token::LAMPORTS_PER_SOL,
    solana_packet::PACKET_DATA_SIZE,
    solana_precompile_error::PrecompileError,
    solana_program_runtime::{
        invoke_context::BuiltinFunctionWithContext, loaded_programs::ProgramCacheEntry,
    },
    solana_pubkey::Pubkey,
    solana_rent_collector::RentCollector,
    solana_reward_info::RewardInfo,
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_with_meta::TransactionWithMeta,
    },
    solana_sdk_ids::{bpf_loader_upgradeable, incinerator, native_loader},
    solana_sha256_hasher::hashv,
    solana_signature::Signature,
    solana_slot_hashes::SlotHashes,
    solana_slot_history::{Check, SlotHistory},
    solana_stake_interface::state::Delegation,
    solana_svm::{
        account_loader::LoadedTransaction,
        account_overrides::AccountOverrides,
        program_loader::load_program_with_pubkey,
        transaction_balances::BalanceCollector,
        transaction_commit_result::{CommittedTransaction, TransactionCommitResult},
        transaction_error_metrics::TransactionErrorMetrics,
        transaction_execution_result::{
            TransactionExecutionDetails, TransactionLoadedAccountsStats,
        },
        transaction_processing_result::{
            ProcessedTransaction, TransactionProcessingResult,
            TransactionProcessingResultExtensions,
        },
        transaction_processor::{
            ExecutionRecordingConfig, TransactionBatchProcessor, TransactionLogMessages,
            TransactionProcessingConfig, TransactionProcessingEnvironment,
        },
    },
    solana_svm_callback::{AccountState, InvokeContextCallback, TransactionProcessingCallback},
    solana_svm_transaction::svm_message::SVMMessage,
    solana_system_transaction as system_transaction,
    solana_sysvar::{self as sysvar, last_restart_slot::LastRestartSlot, Sysvar},
    solana_sysvar_id::SysvarId,
    solana_time_utils::years_as_slots,
    solana_timings::{ExecuteTimingType, ExecuteTimings},
    solana_transaction::{
        sanitized::{MessageHash, SanitizedTransaction, MAX_TX_ACCOUNT_LOCKS},
        versioned::VersionedTransaction,
        Transaction, TransactionVerificationMode,
    },
    solana_transaction_context::{TransactionAccount, TransactionReturnData},
    solana_transaction_error::{TransactionError, TransactionResult as Result},
    solana_vote::vote_account::{VoteAccount, VoteAccountsHashMap},
    std::{
        collections::{HashMap, HashSet},
        fmt,
        ops::{AddAssign, RangeFull},
        path::PathBuf,
        slice,
        sync::{
            atomic::{
                AtomicBool, AtomicI64, AtomicU64,
                Ordering::{self, AcqRel, Acquire, Relaxed},
            },
            Arc, LockResult, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak,
        },
        thread::Builder,
        time::{Duration, Instant},
    },
};
#[cfg(feature = "dev-context-only-utils")]
use {
    dashmap::DashSet,
    rayon::iter::{IntoParallelRefIterator, ParallelIterator},
    solana_accounts_db::accounts_db::{
        ACCOUNTS_DB_CONFIG_FOR_BENCHMARKS, ACCOUNTS_DB_CONFIG_FOR_TESTING,
    },
    solana_nonce as nonce,
    solana_nonce_account::{get_system_account_kind, SystemAccountKind},
    solana_program_runtime::{loaded_programs::ProgramCacheForTxBatch, sysvar_cache::SysvarCache},
};
pub use {partitioned_epoch_rewards::KeyedRewardsAndNumPartitions, solana_reward_info::RewardType};

/// params to `verify_accounts_hash`
struct VerifyAccountsHashConfig {
    require_rooted_bank: bool,
    run_in_background: bool,
}

mod accounts_lt_hash;
mod address_lookup_table;
pub mod bank_hash_details;
mod builtin_programs;
pub mod builtins;
mod check_transactions;
mod fee_distribution;
mod metrics;
pub(crate) mod partitioned_epoch_rewards;
mod recent_blockhashes_account;
mod serde_snapshot;
mod sysvar_cache;
pub(crate) mod tests;

pub const SECONDS_PER_YEAR: f64 = 365.25 * 24.0 * 60.0 * 60.0;

pub const MAX_LEADER_SCHEDULE_STAKES: Epoch = 5;

pub type BankStatusCache = StatusCache<Result<()>>;
#[cfg_attr(
    feature = "frozen-abi",
    frozen_abi(digest = "5dfDCRGWPV7thfoZtLpTJAV8cC93vQUXgTm6BnrfeUsN")
)]
pub type BankSlotDelta = SlotDelta<Result<()>>;

#[derive(Default, Copy, Clone, Debug, PartialEq, Eq)]
pub struct SquashTiming {
    pub squash_accounts_ms: u64,
    pub squash_accounts_cache_ms: u64,
    pub squash_accounts_index_ms: u64,
    pub squash_accounts_store_ms: u64,

    pub squash_cache_ms: u64,
}

impl AddAssign for SquashTiming {
    fn add_assign(&mut self, rhs: Self) {
        self.squash_accounts_ms += rhs.squash_accounts_ms;
        self.squash_accounts_cache_ms += rhs.squash_accounts_cache_ms;
        self.squash_accounts_index_ms += rhs.squash_accounts_index_ms;
        self.squash_accounts_store_ms += rhs.squash_accounts_store_ms;
        self.squash_cache_ms += rhs.squash_cache_ms;
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CollectorFeeDetails {
    transaction_fee: u64,
    priority_fee: u64,
}

impl CollectorFeeDetails {
    pub(crate) fn accumulate(&mut self, fee_details: &FeeDetails) {
        self.transaction_fee = self
            .transaction_fee
            .saturating_add(fee_details.transaction_fee());
        self.priority_fee = self
            .priority_fee
            .saturating_add(fee_details.prioritization_fee());
    }

    pub fn total_transaction_fee(&self) -> u64 {
        self.transaction_fee.saturating_add(self.priority_fee)
    }

    pub fn total_priority_fee(&self) -> u64 {
        self.priority_fee
    }
}

impl From<FeeDetails> for CollectorFeeDetails {
    fn from(fee_details: FeeDetails) -> Self {
        CollectorFeeDetails {
            transaction_fee: fee_details.transaction_fee(),
            priority_fee: fee_details.prioritization_fee(),
        }
    }
}

#[derive(Debug)]
pub struct BankRc {
    /// where all the Accounts are stored
    pub accounts: Arc<Accounts>,

    /// Previous checkpoint of this bank
    pub(crate) parent: RwLock<Option<Arc<Bank>>>,

    pub(crate) bank_id_generator: Arc<AtomicU64>,
}

impl BankRc {
    pub(crate) fn new(accounts: Accounts) -> Self {
        Self {
            accounts: Arc::new(accounts),
            parent: RwLock::new(None),
            bank_id_generator: Arc::new(AtomicU64::new(0)),
        }
    }
}

pub struct LoadAndExecuteTransactionsOutput {
    // Vector of results indicating whether a transaction was processed or could not
    // be processed. Note processed transactions can still have failed!
    pub processing_results: Vec<TransactionProcessingResult>,
    // Processed transaction counts used to update bank transaction counts and
    // for metrics reporting.
    pub processed_counts: ProcessedTransactionCounts,
    // Balances accumulated for TransactionStatusSender when transaction
    // balance recording is enabled.
    pub balance_collector: Option<BalanceCollector>,
}

#[derive(Debug, PartialEq)]
pub struct TransactionSimulationResult {
    pub result: Result<()>,
    pub logs: TransactionLogMessages,
    pub post_simulation_accounts: Vec<TransactionAccount>,
    pub units_consumed: u64,
    pub loaded_accounts_data_size: u32,
    pub return_data: Option<TransactionReturnData>,
    pub inner_instructions: Option<Vec<InnerInstructions>>,
}

#[derive(Clone, Debug)]
pub struct TransactionBalancesSet {
    pub pre_balances: TransactionBalances,
    pub post_balances: TransactionBalances,
}

impl TransactionBalancesSet {
    pub fn new(pre_balances: TransactionBalances, post_balances: TransactionBalances) -> Self {
        assert_eq!(pre_balances.len(), post_balances.len());
        Self {
            pre_balances,
            post_balances,
        }
    }
}
pub type TransactionBalances = Vec<Vec<u64>>;

pub type PreCommitResult<'a> = Result<Option<RwLockReadGuard<'a, Hash>>>;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum TransactionLogCollectorFilter {
    All,
    AllWithVotes,
    None,
    OnlyMentionedAddresses,
}

impl Default for TransactionLogCollectorFilter {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Default)]
pub struct TransactionLogCollectorConfig {
    pub mentioned_addresses: HashSet<Pubkey>,
    pub filter: TransactionLogCollectorFilter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionLogInfo {
    pub signature: Signature,
    pub result: Result<()>,
    pub is_vote: bool,
    pub log_messages: TransactionLogMessages,
}

#[derive(Default, Debug)]
pub struct TransactionLogCollector {
    // All the logs collected for from this Bank.  Exact contents depend on the
    // active `TransactionLogCollectorFilter`
    pub logs: Vec<TransactionLogInfo>,

    // For each `mentioned_addresses`, maintain a list of indices into `logs` to easily
    // locate the logs from transactions that included the mentioned addresses.
    pub mentioned_address_map: HashMap<Pubkey, Vec<usize>>,
}

impl TransactionLogCollector {
    pub fn get_logs_for_address(
        &self,
        address: Option<&Pubkey>,
    ) -> Option<Vec<TransactionLogInfo>> {
        match address {
            None => Some(self.logs.clone()),
            Some(address) => self.mentioned_address_map.get(address).map(|log_indices| {
                log_indices
                    .iter()
                    .filter_map(|i| self.logs.get(*i).cloned())
                    .collect()
            }),
        }
    }
}

/// Bank's common fields shared by all supported snapshot versions for deserialization.
/// Sync fields with BankFieldsToSerialize! This is paired with it.
/// All members are made public to remain Bank's members private and to make versioned deserializer workable on this
/// Note that some fields are missing from the serializer struct. This is because of fields added later.
/// Since it is difficult to insert fields to serialize/deserialize against existing code already deployed,
/// new fields can be optionally serialized and optionally deserialized. At some point, the serialization and
/// deserialization will use a new mechanism or otherwise be in sync more clearly.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "dev-context-only-utils", derive(PartialEq))]
pub struct BankFieldsToDeserialize {
    pub(crate) blockhash_queue: BlockhashQueue,
    pub(crate) ancestors: AncestorsForSerialization,
    pub(crate) hash: Hash,
    pub(crate) parent_hash: Hash,
    pub(crate) parent_slot: Slot,
    pub(crate) hard_forks: HardForks,
    pub(crate) transaction_count: u64,
    pub(crate) tick_height: u64,
    pub(crate) signature_count: u64,
    pub(crate) capitalization: u64,
    pub(crate) max_tick_height: u64,
    pub(crate) hashes_per_tick: Option<u64>,
    pub(crate) ticks_per_slot: u64,
    pub(crate) ns_per_slot: u128,
    pub(crate) genesis_creation_time: UnixTimestamp,
    pub(crate) slots_per_year: f64,
    pub(crate) slot: Slot,
    pub(crate) epoch: Epoch,
    pub(crate) block_height: u64,
    pub(crate) collector_id: Pubkey,
    pub(crate) collector_fees: u64,
    pub(crate) fee_rate_governor: FeeRateGovernor,
    pub(crate) rent_collector: RentCollector,
    pub(crate) epoch_schedule: EpochSchedule,
    pub(crate) inflation: Inflation,
    pub(crate) stakes: Stakes<Delegation>,
    pub(crate) versioned_epoch_stakes: HashMap<Epoch, VersionedEpochStakes>,
    pub(crate) is_delta: bool,
    pub(crate) accounts_data_len: u64,
    pub(crate) accounts_lt_hash: AccountsLtHash,
    pub(crate) bank_hash_stats: BankHashStats,
}

/// Bank's common fields shared by all supported snapshot versions for serialization.
/// This was separated from BankFieldsToDeserialize to avoid cloning by using refs.
/// So, sync fields with BankFieldsToDeserialize!
/// all members are made public to keep Bank private and to make versioned serializer workable on this.
/// Note that some fields are missing from the serializer struct. This is because of fields added later.
/// Since it is difficult to insert fields to serialize/deserialize against existing code already deployed,
/// new fields can be optionally serialized and optionally deserialized. At some point, the serialization and
/// deserialization will use a new mechanism or otherwise be in sync more clearly.
#[derive(Debug)]
pub struct BankFieldsToSerialize {
    pub blockhash_queue: BlockhashQueue,
    pub ancestors: AncestorsForSerialization,
    pub hash: Hash,
    pub parent_hash: Hash,
    pub parent_slot: Slot,
    pub hard_forks: HardForks,
    pub transaction_count: u64,
    pub tick_height: u64,
    pub signature_count: u64,
    pub capitalization: u64,
    pub max_tick_height: u64,
    pub hashes_per_tick: Option<u64>,
    pub ticks_per_slot: u64,
    pub ns_per_slot: u128,
    pub genesis_creation_time: UnixTimestamp,
    pub slots_per_year: f64,
    pub slot: Slot,
    pub epoch: Epoch,
    pub block_height: u64,
    pub collector_id: Pubkey,
    pub collector_fees: u64,
    pub fee_rate_governor: FeeRateGovernor,
    pub rent_collector: RentCollector,
    pub epoch_schedule: EpochSchedule,
    pub inflation: Inflation,
    pub stakes: Stakes<StakeAccount<Delegation>>,
    pub is_delta: bool,
    pub accounts_data_len: u64,
    pub versioned_epoch_stakes: HashMap<u64, VersionedEpochStakes>,
    pub accounts_lt_hash: AccountsLtHash,
}

// Can't derive PartialEq because RwLock doesn't implement PartialEq
#[cfg(feature = "dev-context-only-utils")]
impl PartialEq for Bank {
    fn eq(&self, other: &Self) -> bool {
        if std::ptr::eq(self, other) {
            return true;
        }
        // Suppress rustfmt until https://github.com/rust-lang/rustfmt/issues/5920 is fixed ...
        #[rustfmt::skip]
        let Self {
            rc: _,
            status_cache: _,
            blockhash_queue,
            ancestors,
            hash,
            parent_hash,
            parent_slot,
            hard_forks,
            transaction_count,
            non_vote_transaction_count_since_restart: _,
            transaction_error_count: _,
            transaction_entries_count: _,
            transactions_per_entry_max: _,
            tick_height,
            signature_count,
            capitalization,
            max_tick_height,
            hashes_per_tick,
            ticks_per_slot,
            ns_per_slot,
            genesis_creation_time,
            slots_per_year,
            slot,
            bank_id: _,
            epoch,
            block_height,
            collector_id,
            collector_fees,
            fee_rate_governor,
            rent_collector,
            epoch_schedule,
            inflation,
            stakes_cache,
            epoch_stakes,
            is_delta,
            #[cfg(feature = "dev-context-only-utils")]
            hash_overrides,
            accounts_lt_hash,
            // TODO: Confirm if all these fields are intentionally ignored!
            rewards: _,
            cluster_type: _,
            rewards_pool_pubkeys: _,
            transaction_debug_keys: _,
            transaction_log_collector_config: _,
            transaction_log_collector: _,
            feature_set: _,
            reserved_account_keys: _,
            drop_callback: _,
            freeze_started: _,
            vote_only_bank: _,
            cost_tracker: _,
            accounts_data_size_initial: _,
            accounts_data_size_delta_on_chain: _,
            accounts_data_size_delta_off_chain: _,
            epoch_reward_status: _,
            transaction_processor: _,
            check_program_modification_slot: _,
            collector_fee_details: _,
            compute_budget: _,
            transaction_account_lock_limit: _,
            fee_structure: _,
            cache_for_accounts_lt_hash: _,
            stats_for_accounts_lt_hash: _,
            block_id,
            bank_hash_stats: _,
            epoch_rewards_calculation_cache: _,
            // Ignore new fields explicitly if they do not impact PartialEq.
            // Adding ".." will remove compile-time checks that if a new field
            // is added to the struct, this PartialEq is accordingly updated.
        } = self;
        *blockhash_queue.read().unwrap() == *other.blockhash_queue.read().unwrap()
            && ancestors == &other.ancestors
            && *hash.read().unwrap() == *other.hash.read().unwrap()
            && parent_hash == &other.parent_hash
            && parent_slot == &other.parent_slot
            && *hard_forks.read().unwrap() == *other.hard_forks.read().unwrap()
            && transaction_count.load(Relaxed) == other.transaction_count.load(Relaxed)
            && tick_height.load(Relaxed) == other.tick_height.load(Relaxed)
            && signature_count.load(Relaxed) == other.signature_count.load(Relaxed)
            && capitalization.load(Relaxed) == other.capitalization.load(Relaxed)
            && max_tick_height == &other.max_tick_height
            && hashes_per_tick == &other.hashes_per_tick
            && ticks_per_slot == &other.ticks_per_slot
            && ns_per_slot == &other.ns_per_slot
            && genesis_creation_time == &other.genesis_creation_time
            && slots_per_year == &other.slots_per_year
            && slot == &other.slot
            && epoch == &other.epoch
            && block_height == &other.block_height
            && collector_id == &other.collector_id
            && collector_fees.load(Relaxed) == other.collector_fees.load(Relaxed)
            && fee_rate_governor == &other.fee_rate_governor
            && rent_collector == &other.rent_collector
            && epoch_schedule == &other.epoch_schedule
            && *inflation.read().unwrap() == *other.inflation.read().unwrap()
            && *stakes_cache.stakes() == *other.stakes_cache.stakes()
            && epoch_stakes == &other.epoch_stakes
            && is_delta.load(Relaxed) == other.is_delta.load(Relaxed)
            // No deadlock is possbile, when Arc::ptr_eq() returns false, because of being
            // different Mutexes.
            && (Arc::ptr_eq(hash_overrides, &other.hash_overrides) ||
                *hash_overrides.lock().unwrap() == *other.hash_overrides.lock().unwrap())
            && *accounts_lt_hash.lock().unwrap() == *other.accounts_lt_hash.lock().unwrap()
            && *block_id.read().unwrap() == *other.block_id.read().unwrap()
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl BankFieldsToSerialize {
    /// Create a new BankFieldsToSerialize where basically every field is defaulted.
    /// Only use for tests; many of the fields are invalid!
    pub fn default_for_tests() -> Self {
        Self {
            blockhash_queue: BlockhashQueue::default(),
            ancestors: AncestorsForSerialization::default(),
            hash: Hash::default(),
            parent_hash: Hash::default(),
            parent_slot: Slot::default(),
            hard_forks: HardForks::default(),
            transaction_count: u64::default(),
            tick_height: u64::default(),
            signature_count: u64::default(),
            capitalization: u64::default(),
            max_tick_height: u64::default(),
            hashes_per_tick: Option::default(),
            ticks_per_slot: u64::default(),
            ns_per_slot: u128::default(),
            genesis_creation_time: UnixTimestamp::default(),
            slots_per_year: f64::default(),
            slot: Slot::default(),
            epoch: Epoch::default(),
            block_height: u64::default(),
            collector_id: Pubkey::default(),
            collector_fees: u64::default(),
            fee_rate_governor: FeeRateGovernor::default(),
            rent_collector: RentCollector::default(),
            epoch_schedule: EpochSchedule::default(),
            inflation: Inflation::default(),
            stakes: Stakes::<StakeAccount<Delegation>>::default(),
            is_delta: bool::default(),
            accounts_data_len: u64::default(),
            versioned_epoch_stakes: HashMap::default(),
            accounts_lt_hash: AccountsLtHash(LtHash([0x7E57; LtHash::NUM_ELEMENTS])),
        }
    }
}

#[derive(Debug)]
pub enum RewardCalculationEvent<'a, 'b> {
    Staking(&'a Pubkey, &'b InflationPointCalculationEvent),
}

/// type alias is not supported for trait in rust yet. As a workaround, we define the
/// `RewardCalcTracer` trait explicitly and implement it on any type that implement
/// `Fn(&RewardCalculationEvent) + Send + Sync`.
pub trait RewardCalcTracer: Fn(&RewardCalculationEvent) + Send + Sync {}

impl<T: Fn(&RewardCalculationEvent) + Send + Sync> RewardCalcTracer for T {}

fn null_tracer() -> Option<impl RewardCalcTracer> {
    None::<fn(&RewardCalculationEvent)>
}

pub trait DropCallback: fmt::Debug {
    fn callback(&self, b: &Bank);
    fn clone_box(&self) -> Box<dyn DropCallback + Send + Sync>;
}

#[derive(Debug, Default)]
pub struct OptionalDropCallback(Option<Box<dyn DropCallback + Send + Sync>>);

#[derive(Default, Debug, Clone, PartialEq)]
#[cfg(feature = "dev-context-only-utils")]
pub struct HashOverrides {
    hashes: HashMap<Slot, HashOverride>,
}

#[cfg(feature = "dev-context-only-utils")]
impl HashOverrides {
    fn get_hash_override(&self, slot: Slot) -> Option<&HashOverride> {
        self.hashes.get(&slot)
    }

    fn get_blockhash_override(&self, slot: Slot) -> Option<&Hash> {
        self.get_hash_override(slot)
            .map(|hash_override| &hash_override.blockhash)
    }

    fn get_bank_hash_override(&self, slot: Slot) -> Option<&Hash> {
        self.get_hash_override(slot)
            .map(|hash_override| &hash_override.bank_hash)
    }

    pub fn add_override(&mut self, slot: Slot, blockhash: Hash, bank_hash: Hash) {
        let is_new = self
            .hashes
            .insert(
                slot,
                HashOverride {
                    blockhash,
                    bank_hash,
                },
            )
            .is_none();
        assert!(is_new);
    }
}

#[derive(Debug, Clone, PartialEq)]
#[cfg(feature = "dev-context-only-utils")]
struct HashOverride {
    blockhash: Hash,
    bank_hash: Hash,
}

/// Manager for the state of all accounts and programs after processing its entries.
pub struct Bank {
    /// References to accounts, parent and signature status
    pub rc: BankRc,

    /// A cache of signature statuses
    pub status_cache: Arc<RwLock<BankStatusCache>>,

    /// FIFO queue of `recent_blockhash` items
    blockhash_queue: RwLock<BlockhashQueue>,

    /// The set of parents including this bank
    pub ancestors: Ancestors,

    /// Hash of this Bank's state. Only meaningful after freezing.
    hash: RwLock<Hash>,

    /// Hash of this Bank's parent's state
    parent_hash: Hash,

    /// parent's slot
    parent_slot: Slot,

    /// slots to hard fork at
    hard_forks: Arc<RwLock<HardForks>>,

    /// The number of committed transactions since genesis.
    transaction_count: AtomicU64,

    /// The number of non-vote transactions committed since the most
    /// recent boot from snapshot or genesis. This value is only stored in
    /// blockstore for the RPC method "getPerformanceSamples". It is not
    /// retained within snapshots, but is preserved in `Bank::new_from_parent`.
    non_vote_transaction_count_since_restart: AtomicU64,

    /// The number of transaction errors in this slot
    transaction_error_count: AtomicU64,

    /// The number of transaction entries in this slot
    transaction_entries_count: AtomicU64,

    /// The max number of transaction in an entry in this slot
    transactions_per_entry_max: AtomicU64,

    /// Bank tick height
    tick_height: AtomicU64,

    /// The number of signatures from valid transactions in this slot
    signature_count: AtomicU64,

    /// Total capitalization, used to calculate inflation
    capitalization: AtomicU64,

    // Bank max_tick_height
    max_tick_height: u64,

    /// The number of hashes in each tick. None value means hashing is disabled.
    hashes_per_tick: Option<u64>,

    /// The number of ticks in each slot.
    ticks_per_slot: u64,

    /// length of a slot in ns
    pub ns_per_slot: u128,

    /// genesis time, used for computed clock
    genesis_creation_time: UnixTimestamp,

    /// The number of slots per year, used for inflation
    slots_per_year: f64,

    /// Bank slot (i.e. block)
    slot: Slot,

    bank_id: BankId,

    /// Bank epoch
    epoch: Epoch,

    /// Bank block_height
    block_height: u64,

    /// The pubkey to send transactions fees to.
    collector_id: Pubkey,

    /// Fees that have been collected
    collector_fees: AtomicU64,

    /// Track cluster signature throughput and adjust fee rate
    pub(crate) fee_rate_governor: FeeRateGovernor,

    /// latest rent collector, knows the epoch
    rent_collector: RentCollector,

    /// initialized from genesis
    pub(crate) epoch_schedule: EpochSchedule,

    /// inflation specs
    inflation: Arc<RwLock<Inflation>>,

    /// cache of vote_account and stake_account state for this fork
    stakes_cache: StakesCache,

    /// staked nodes on epoch boundaries, saved off when a bank.slot() is at
    ///   a leader schedule calculation boundary
    epoch_stakes: HashMap<Epoch, VersionedEpochStakes>,

    /// A boolean reflecting whether any entries were recorded into the PoH
    /// stream for the slot == self.slot
    is_delta: AtomicBool,

    /// Protocol-level rewards that were distributed by this bank
    pub rewards: RwLock<Vec<(Pubkey, RewardInfo)>>,

    pub cluster_type: Option<ClusterType>,

    // this is temporary field only to remove rewards_pool entirely
    pub rewards_pool_pubkeys: Arc<HashSet<Pubkey>>,

    transaction_debug_keys: Option<Arc<HashSet<Pubkey>>>,

    // Global configuration for how transaction logs should be collected across all banks
    pub transaction_log_collector_config: Arc<RwLock<TransactionLogCollectorConfig>>,

    // Logs from transactions that this Bank executed collected according to the criteria in
    // `transaction_log_collector_config`
    pub transaction_log_collector: Arc<RwLock<TransactionLogCollector>>,

    pub feature_set: Arc<FeatureSet>,

    /// Set of reserved account keys that cannot be write locked
    reserved_account_keys: Arc<ReservedAccountKeys>,

    /// callback function only to be called when dropping and should only be called once
    pub drop_callback: RwLock<OptionalDropCallback>,

    pub freeze_started: AtomicBool,

    vote_only_bank: bool,

    cost_tracker: RwLock<CostTracker>,

    /// The initial accounts data size at the start of this Bank, before processing any transactions/etc
    accounts_data_size_initial: u64,
    /// The change to accounts data size in this Bank, due on-chain events (i.e. transactions)
    accounts_data_size_delta_on_chain: AtomicI64,
    /// The change to accounts data size in this Bank, due to off-chain events (i.e. rent collection)
    accounts_data_size_delta_off_chain: AtomicI64,

    epoch_reward_status: EpochRewardStatus,

    transaction_processor: TransactionBatchProcessor<BankForks>,

    check_program_modification_slot: bool,

    /// Collected fee details
    collector_fee_details: RwLock<CollectorFeeDetails>,

    /// The compute budget to use for transaction execution.
    compute_budget: Option<ComputeBudget>,

    /// The max number of accounts that a transaction may lock.
    transaction_account_lock_limit: Option<usize>,

    /// Fee structure to use for assessing transaction fees.
    fee_structure: FeeStructure,

    /// blockhash and bank_hash overrides keyed by slot for simulated block production.
    /// This _field_ was needed to be DCOU-ed to avoid 2 locks per bank freezing...
    #[cfg(feature = "dev-context-only-utils")]
    hash_overrides: Arc<Mutex<HashOverrides>>,

    /// The lattice hash of all accounts
    ///
    /// The value is only meaningful after freezing.
    accounts_lt_hash: Mutex<AccountsLtHash>,

    /// A cache of *the initial state* of accounts modified in this slot
    ///
    /// The accounts lt hash needs both the initial and final state of each
    /// account that was modified in this slot.  Cache the initial state here.
    ///
    /// Note: The initial state must be strictly from an ancestor,
    /// and not an intermediate state within this slot.
    cache_for_accounts_lt_hash: DashMap<Pubkey, AccountsLtHashCacheValue, ahash::RandomState>,

    /// Stats related to the accounts lt hash
    stats_for_accounts_lt_hash: AccountsLtHashStats,

    /// The unique identifier for the corresponding block for this bank.
    /// None for banks that have not yet completed replay or for leader banks as we cannot populate block_id
    /// until bankless leader. Can be computed directly from shreds without needing to execute transactions.
    block_id: RwLock<Option<Hash>>,

    /// Accounts stats for computing the bank hash
    bank_hash_stats: AtomicBankHashStats,

    /// The cache of epoch rewards calculation results
    /// This is used to avoid recalculating the same epoch rewards at epoch boundary.
    /// The hashmap is keyed by parent_hash.
    epoch_rewards_calculation_cache: Arc<Mutex<HashMap<Hash, Arc<PartitionedRewardsCalculation>>>>,
}

#[derive(Debug)]
struct VoteReward {
    vote_account: AccountSharedData,
    commission: u8,
    vote_rewards: u64,
}

type VoteRewards = DashMap<Pubkey, VoteReward, RandomState>;

#[derive(Debug, Default)]
pub struct NewBankOptions {
    pub vote_only_bank: bool,
}

#[cfg(feature = "dev-context-only-utils")]
#[derive(Debug)]
pub struct BankTestConfig {
    pub accounts_db_config: AccountsDbConfig,
}

#[cfg(feature = "dev-context-only-utils")]
impl Default for BankTestConfig {
    fn default() -> Self {
        Self {
            accounts_db_config: ACCOUNTS_DB_CONFIG_FOR_TESTING,
        }
    }
}

#[derive(Debug)]
struct PrevEpochInflationRewards {
    validator_rewards: u64,
    prev_epoch_duration_in_years: f64,
    validator_rate: f64,
    foundation_rate: f64,
}

#[derive(Debug, Default, PartialEq)]
pub struct ProcessedTransactionCounts {
    pub processed_transactions_count: u64,
    pub processed_non_vote_transactions_count: u64,
    pub processed_with_successful_result_count: u64,
    pub signature_count: u64,
}

/// Account stats for computing the bank hash
/// This struct is serialized and stored in the snapshot.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BankHashStats {
    pub num_updated_accounts: u64,
    pub num_removed_accounts: u64,
    pub num_lamports_stored: u64,
    pub total_data_len: u64,
    pub num_executable_accounts: u64,
}

impl BankHashStats {
    pub fn update<T: ReadableAccount>(&mut self, account: &T) {
        if account.lamports() == 0 {
            self.num_removed_accounts += 1;
        } else {
            self.num_updated_accounts += 1;
        }
        self.total_data_len = self
            .total_data_len
            .wrapping_add(account.data().len() as u64);
        if account.executable() {
            self.num_executable_accounts += 1;
        }
        self.num_lamports_stored = self.num_lamports_stored.wrapping_add(account.lamports());
    }
    pub fn accumulate(&mut self, other: &BankHashStats) {
        self.num_updated_accounts += other.num_updated_accounts;
        self.num_removed_accounts += other.num_removed_accounts;
        self.total_data_len = self.total_data_len.wrapping_add(other.total_data_len);
        self.num_lamports_stored = self
            .num_lamports_stored
            .wrapping_add(other.num_lamports_stored);
        self.num_executable_accounts += other.num_executable_accounts;
    }
}

#[derive(Debug, Default)]
pub struct AtomicBankHashStats {
    pub num_updated_accounts: AtomicU64,
    pub num_removed_accounts: AtomicU64,
    pub num_lamports_stored: AtomicU64,
    pub total_data_len: AtomicU64,
    pub num_executable_accounts: AtomicU64,
}

impl AtomicBankHashStats {
    pub fn new(stat: &BankHashStats) -> Self {
        AtomicBankHashStats {
            num_updated_accounts: AtomicU64::new(stat.num_updated_accounts),
            num_removed_accounts: AtomicU64::new(stat.num_removed_accounts),
            num_lamports_stored: AtomicU64::new(stat.num_lamports_stored),
            total_data_len: AtomicU64::new(stat.total_data_len),
            num_executable_accounts: AtomicU64::new(stat.num_executable_accounts),
        }
    }

    pub fn accumulate(&self, other: &BankHashStats) {
        self.num_updated_accounts
            .fetch_add(other.num_updated_accounts, Relaxed);
        self.num_removed_accounts
            .fetch_add(other.num_removed_accounts, Relaxed);
        self.total_data_len.fetch_add(other.total_data_len, Relaxed);
        self.num_lamports_stored
            .fetch_add(other.num_lamports_stored, Relaxed);
        self.num_executable_accounts
            .fetch_add(other.num_executable_accounts, Relaxed);
    }

    pub fn load(&self) -> BankHashStats {
        BankHashStats {
            num_updated_accounts: self.num_updated_accounts.load(Relaxed),
            num_removed_accounts: self.num_removed_accounts.load(Relaxed),
            num_lamports_stored: self.num_lamports_stored.load(Relaxed),
            total_data_len: self.total_data_len.load(Relaxed),
            num_executable_accounts: self.num_executable_accounts.load(Relaxed),
        }
    }
}

impl Bank {
    fn default_with_accounts(accounts: Accounts) -> Self {
        let mut bank = Self {
            rc: BankRc::new(accounts),
            status_cache: Arc::<RwLock<BankStatusCache>>::default(),
            blockhash_queue: RwLock::<BlockhashQueue>::default(),
            ancestors: Ancestors::default(),
            hash: RwLock::<Hash>::default(),
            parent_hash: Hash::default(),
            parent_slot: Slot::default(),
            hard_forks: Arc::<RwLock<HardForks>>::default(),
            transaction_count: AtomicU64::default(),
            non_vote_transaction_count_since_restart: AtomicU64::default(),
            transaction_error_count: AtomicU64::default(),
            transaction_entries_count: AtomicU64::default(),
            transactions_per_entry_max: AtomicU64::default(),
            tick_height: AtomicU64::default(),
            signature_count: AtomicU64::default(),
            capitalization: AtomicU64::default(),
            max_tick_height: u64::default(),
            hashes_per_tick: Option::<u64>::default(),
            ticks_per_slot: u64::default(),
            ns_per_slot: u128::default(),
            genesis_creation_time: UnixTimestamp::default(),
            slots_per_year: f64::default(),
            slot: Slot::default(),
            bank_id: BankId::default(),
            epoch: Epoch::default(),
            block_height: u64::default(),
            collector_id: Pubkey::default(),
            collector_fees: AtomicU64::default(),
            fee_rate_governor: FeeRateGovernor::default(),
            rent_collector: RentCollector::default(),
            epoch_schedule: EpochSchedule::default(),
            inflation: Arc::<RwLock<Inflation>>::default(),
            stakes_cache: StakesCache::default(),
            epoch_stakes: HashMap::<Epoch, VersionedEpochStakes>::default(),
            is_delta: AtomicBool::default(),
            rewards: RwLock::<Vec<(Pubkey, RewardInfo)>>::default(),
            cluster_type: Option::<ClusterType>::default(),
            rewards_pool_pubkeys: Arc::<HashSet<Pubkey>>::default(),
            transaction_debug_keys: Option::<Arc<HashSet<Pubkey>>>::default(),
            transaction_log_collector_config: Arc::<RwLock<TransactionLogCollectorConfig>>::default(
            ),
            transaction_log_collector: Arc::<RwLock<TransactionLogCollector>>::default(),
            feature_set: Arc::<FeatureSet>::default(),
            reserved_account_keys: Arc::<ReservedAccountKeys>::default(),
            drop_callback: RwLock::new(OptionalDropCallback(None)),
            freeze_started: AtomicBool::default(),
            vote_only_bank: false,
            cost_tracker: RwLock::<CostTracker>::default(),
            accounts_data_size_initial: 0,
            accounts_data_size_delta_on_chain: AtomicI64::new(0),
            accounts_data_size_delta_off_chain: AtomicI64::new(0),
            epoch_reward_status: EpochRewardStatus::default(),
            transaction_processor: TransactionBatchProcessor::default(),
            check_program_modification_slot: false,
            collector_fee_details: RwLock::new(CollectorFeeDetails::default()),
            compute_budget: None,
            transaction_account_lock_limit: None,
            fee_structure: FeeStructure::default(),
            #[cfg(feature = "dev-context-only-utils")]
            hash_overrides: Arc::new(Mutex::new(HashOverrides::default())),
            accounts_lt_hash: Mutex::new(AccountsLtHash(LtHash::identity())),
            cache_for_accounts_lt_hash: DashMap::default(),
            stats_for_accounts_lt_hash: AccountsLtHashStats::default(),
            block_id: RwLock::new(None),
            bank_hash_stats: AtomicBankHashStats::default(),
            epoch_rewards_calculation_cache: Arc::new(Mutex::new(HashMap::default())),
        };

        bank.transaction_processor =
            TransactionBatchProcessor::new_uninitialized(bank.slot, bank.epoch);

        bank.accounts_data_size_initial = bank.calculate_accounts_data_size().unwrap();

        bank
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_paths(
        genesis_config: &GenesisConfig,
        runtime_config: Arc<RuntimeConfig>,
        paths: Vec<PathBuf>,
        debug_keys: Option<Arc<HashSet<Pubkey>>>,
        additional_builtins: Option<&[BuiltinPrototype]>,
        debug_do_not_add_builtins: bool,
        accounts_db_config: Option<AccountsDbConfig>,
        accounts_update_notifier: Option<AccountsUpdateNotifier>,
        #[allow(unused)] collector_id_for_tests: Option<Pubkey>,
        exit: Arc<AtomicBool>,
        #[allow(unused)] genesis_hash: Option<Hash>,
        #[allow(unused)] feature_set: Option<FeatureSet>,
    ) -> Self {
        let accounts_db =
            AccountsDb::new_with_config(paths, accounts_db_config, accounts_update_notifier, exit);
        let accounts = Accounts::new(Arc::new(accounts_db));
        let mut bank = Self::default_with_accounts(accounts);
        bank.ancestors = Ancestors::from(vec![bank.slot()]);
        bank.compute_budget = runtime_config.compute_budget;
        bank.transaction_account_lock_limit = runtime_config.transaction_account_lock_limit;
        bank.transaction_debug_keys = debug_keys;
        bank.cluster_type = Some(genesis_config.cluster_type);

        #[cfg(feature = "dev-context-only-utils")]
        {
            bank.feature_set = Arc::new(feature_set.unwrap_or_default());
        }

        #[cfg(not(feature = "dev-context-only-utils"))]
        bank.process_genesis_config(genesis_config);
        #[cfg(feature = "dev-context-only-utils")]
        bank.process_genesis_config(genesis_config, collector_id_for_tests, genesis_hash);

        bank.finish_init(
            genesis_config,
            additional_builtins,
            debug_do_not_add_builtins,
        );

        // genesis needs stakes for all epochs up to the epoch implied by
        //  slot = 0 and genesis configuration
        {
            let stakes = bank.stakes_cache.stakes().clone();
            let stakes = SerdeStakesToStakeFormat::from(stakes);
            for epoch in 0..=bank.get_leader_schedule_epoch(bank.slot) {
                bank.epoch_stakes
                    .insert(epoch, VersionedEpochStakes::new(stakes.clone(), epoch));
            }
            bank.update_stake_history(None);
        }
        bank.update_clock(None);
        bank.update_rent();
        bank.update_epoch_schedule();
        bank.update_recent_blockhashes();
        bank.update_last_restart_slot();
        bank.transaction_processor
            .fill_missing_sysvar_cache_entries(&bank);
        bank
    }

    /// Create a new bank that points to an immutable checkpoint of another bank.
    pub fn new_from_parent(parent: Arc<Bank>, collector_id: &Pubkey, slot: Slot) -> Self {
        Self::_new_from_parent(
            parent,
            collector_id,
            slot,
            null_tracer(),
            NewBankOptions::default(),
        )
    }

    pub fn new_from_parent_with_options(
        parent: Arc<Bank>,
        collector_id: &Pubkey,
        slot: Slot,
        new_bank_options: NewBankOptions,
    ) -> Self {
        Self::_new_from_parent(parent, collector_id, slot, null_tracer(), new_bank_options)
    }

    pub fn new_from_parent_with_tracer(
        parent: Arc<Bank>,
        collector_id: &Pubkey,
        slot: Slot,
        reward_calc_tracer: impl RewardCalcTracer,
    ) -> Self {
        Self::_new_from_parent(
            parent,
            collector_id,
            slot,
            Some(reward_calc_tracer),
            NewBankOptions::default(),
        )
    }

    fn get_rent_collector_from(rent_collector: &RentCollector, epoch: Epoch) -> RentCollector {
        rent_collector.clone_with_epoch(epoch)
    }

    fn _new_from_parent(
        parent: Arc<Bank>,
        collector_id: &Pubkey,
        slot: Slot,
        reward_calc_tracer: Option<impl RewardCalcTracer>,
        new_bank_options: NewBankOptions,
    ) -> Self {
        let mut time = Measure::start("bank::new_from_parent");
        let NewBankOptions { vote_only_bank } = new_bank_options;

        parent.freeze();
        assert_ne!(slot, parent.slot());

        let epoch_schedule = parent.epoch_schedule().clone();
        let epoch = epoch_schedule.get_epoch(slot);

        let (rc, bank_rc_creation_time_us) = measure_us!({
            let accounts_db = Arc::clone(&parent.rc.accounts.accounts_db);
            BankRc {
                accounts: Arc::new(Accounts::new(accounts_db)),
                parent: RwLock::new(Some(Arc::clone(&parent))),
                bank_id_generator: Arc::clone(&parent.rc.bank_id_generator),
            }
        });

        let (status_cache, status_cache_time_us) = measure_us!(Arc::clone(&parent.status_cache));

        let (fee_rate_governor, fee_components_time_us) = measure_us!(
            FeeRateGovernor::new_derived(&parent.fee_rate_governor, parent.signature_count())
        );

        let bank_id = rc.bank_id_generator.fetch_add(1, Relaxed) + 1;
        let (blockhash_queue, blockhash_queue_time_us) =
            measure_us!(RwLock::new(parent.blockhash_queue.read().unwrap().clone()));

        let (stakes_cache, stakes_cache_time_us) =
            measure_us!(StakesCache::new(parent.stakes_cache.stakes().clone()));

        let (epoch_stakes, epoch_stakes_time_us) = measure_us!(parent.epoch_stakes.clone());

        let (transaction_processor, builtin_program_ids_time_us) = measure_us!(
            TransactionBatchProcessor::new_from(&parent.transaction_processor, slot, epoch)
        );

        let (rewards_pool_pubkeys, rewards_pool_pubkeys_time_us) =
            measure_us!(parent.rewards_pool_pubkeys.clone());

        let (transaction_debug_keys, transaction_debug_keys_time_us) =
            measure_us!(parent.transaction_debug_keys.clone());

        let (transaction_log_collector_config, transaction_log_collector_config_time_us) =
            measure_us!(parent.transaction_log_collector_config.clone());

        let (feature_set, feature_set_time_us) = measure_us!(parent.feature_set.clone());

        let accounts_data_size_initial = parent.load_accounts_data_size();
        let mut new = Self {
            rc,
            status_cache,
            slot,
            bank_id,
            epoch,
            blockhash_queue,

            // TODO: clean this up, so much special-case copying...
            hashes_per_tick: parent.hashes_per_tick,
            ticks_per_slot: parent.ticks_per_slot,
            ns_per_slot: parent.ns_per_slot,
            genesis_creation_time: parent.genesis_creation_time,
            slots_per_year: parent.slots_per_year,
            epoch_schedule,
            rent_collector: Self::get_rent_collector_from(&parent.rent_collector, epoch),
            max_tick_height: slot
                .checked_add(1)
                .expect("max tick height addition overflowed")
                .checked_mul(parent.ticks_per_slot)
                .expect("max tick height multiplication overflowed"),
            block_height: parent
                .block_height
                .checked_add(1)
                .expect("block height addition overflowed"),
            fee_rate_governor,
            capitalization: AtomicU64::new(parent.capitalization()),
            vote_only_bank,
            inflation: parent.inflation.clone(),
            transaction_count: AtomicU64::new(parent.transaction_count()),
            non_vote_transaction_count_since_restart: AtomicU64::new(
                parent.non_vote_transaction_count_since_restart(),
            ),
            transaction_error_count: AtomicU64::new(0),
            transaction_entries_count: AtomicU64::new(0),
            transactions_per_entry_max: AtomicU64::new(0),
            // we will .clone_with_epoch() this soon after stake data update; so just .clone() for now
            stakes_cache,
            epoch_stakes,
            parent_hash: parent.hash(),
            parent_slot: parent.slot(),
            collector_id: *collector_id,
            collector_fees: AtomicU64::new(0),
            ancestors: Ancestors::default(),
            hash: RwLock::new(Hash::default()),
            is_delta: AtomicBool::new(false),
            tick_height: AtomicU64::new(parent.tick_height.load(Relaxed)),
            signature_count: AtomicU64::new(0),
            hard_forks: parent.hard_forks.clone(),
            rewards: RwLock::new(vec![]),
            cluster_type: parent.cluster_type,
            rewards_pool_pubkeys,
            transaction_debug_keys,
            transaction_log_collector_config,
            transaction_log_collector: Arc::new(RwLock::new(TransactionLogCollector::default())),
            feature_set: Arc::clone(&feature_set),
            reserved_account_keys: parent.reserved_account_keys.clone(),
            drop_callback: RwLock::new(OptionalDropCallback(
                parent
                    .drop_callback
                    .read()
                    .unwrap()
                    .0
                    .as_ref()
                    .map(|drop_callback| drop_callback.clone_box()),
            )),
            freeze_started: AtomicBool::new(false),
            cost_tracker: RwLock::new(parent.read_cost_tracker().unwrap().new_from_parent_limits()),
            accounts_data_size_initial,
            accounts_data_size_delta_on_chain: AtomicI64::new(0),
            accounts_data_size_delta_off_chain: AtomicI64::new(0),
            epoch_reward_status: parent.epoch_reward_status.clone(),
            transaction_processor,
            check_program_modification_slot: false,
            collector_fee_details: RwLock::new(CollectorFeeDetails::default()),
            compute_budget: parent.compute_budget,
            transaction_account_lock_limit: parent.transaction_account_lock_limit,
            fee_structure: parent.fee_structure.clone(),
            #[cfg(feature = "dev-context-only-utils")]
            hash_overrides: parent.hash_overrides.clone(),
            accounts_lt_hash: Mutex::new(parent.accounts_lt_hash.lock().unwrap().clone()),
            cache_for_accounts_lt_hash: DashMap::default(),
            stats_for_accounts_lt_hash: AccountsLtHashStats::default(),
            block_id: RwLock::new(None),
            bank_hash_stats: AtomicBankHashStats::default(),
            epoch_rewards_calculation_cache: parent.epoch_rewards_calculation_cache.clone(),
        };

        let (_, ancestors_time_us) = measure_us!({
            let mut ancestors = Vec::with_capacity(1 + new.parents().len());
            ancestors.push(new.slot());
            new.parents().iter().for_each(|p| {
                ancestors.push(p.slot());
            });
            new.ancestors = Ancestors::from(ancestors);
        });

        // Following code may touch AccountsDb, requiring proper ancestors
        let (_, update_epoch_time_us) = measure_us!({
            if parent.epoch() < new.epoch() {
                new.process_new_epoch(
                    parent.epoch(),
                    parent.slot(),
                    parent.block_height(),
                    reward_calc_tracer,
                );
            } else {
                // Save a snapshot of stakes for use in consensus and stake weighted networking
                let leader_schedule_epoch = new.epoch_schedule().get_leader_schedule_epoch(slot);
                new.update_epoch_stakes(leader_schedule_epoch);
            }
            new.distribute_partitioned_epoch_rewards();
        });

        let (_, cache_preparation_time_us) =
            measure_us!(new.prepare_program_cache_for_upcoming_feature_set());

        // Update sysvars before processing transactions
        let (_, update_sysvars_time_us) = measure_us!({
            new.update_slot_hashes();
            new.update_stake_history(Some(parent.epoch()));
            new.update_clock(Some(parent.epoch()));
            new.update_last_restart_slot()
        });

        let (_, fill_sysvar_cache_time_us) = measure_us!(new
            .transaction_processor
            .fill_missing_sysvar_cache_entries(&new));

        let (num_accounts_modified_this_slot, populate_cache_for_accounts_lt_hash_us) =
            measure_us!({
                // The cache for accounts lt hash needs to be made aware of accounts modified
                // before transaction processing begins.  Otherwise we may calculate the wrong
                // accounts lt hash due to having the wrong initial state of the account.  The
                // lt hash cache's initial state must always be from an ancestor, and cannot be
                // an intermediate state within this Bank's slot.  If the lt hash cache has the
                // wrong initial account state, we'll mix out the wrong lt hash value, and thus
                // have the wrong overall accounts lt hash, and diverge.
                let accounts_modified_this_slot =
                    new.rc.accounts.accounts_db.get_pubkeys_for_slot(slot);
                let num_accounts_modified_this_slot = accounts_modified_this_slot.len();
                for pubkey in accounts_modified_this_slot {
                    new.cache_for_accounts_lt_hash
                        .entry(pubkey)
                        .or_insert(AccountsLtHashCacheValue::BankNew);
                }
                num_accounts_modified_this_slot
            });

        time.stop();
        report_new_bank_metrics(
            slot,
            parent.slot(),
            new.block_height,
            num_accounts_modified_this_slot,
            NewBankTimings {
                bank_rc_creation_time_us,
                total_elapsed_time_us: time.as_us(),
                status_cache_time_us,
                fee_components_time_us,
                blockhash_queue_time_us,
                stakes_cache_time_us,
                epoch_stakes_time_us,
                builtin_program_ids_time_us,
                rewards_pool_pubkeys_time_us,
                executor_cache_time_us: 0,
                transaction_debug_keys_time_us,
                transaction_log_collector_config_time_us,
                feature_set_time_us,
                ancestors_time_us,
                update_epoch_time_us,
                cache_preparation_time_us,
                update_sysvars_time_us,
                fill_sysvar_cache_time_us,
                populate_cache_for_accounts_lt_hash_us,
            },
        );

        report_loaded_programs_stats(
            &parent
                .transaction_processor
                .program_cache
                .read()
                .unwrap()
                .stats,
            parent.slot(),
        );

        new.transaction_processor
            .program_cache
            .write()
            .unwrap()
            .stats
            .reset();

        new
    }

    pub fn set_fork_graph_in_program_cache(&self, fork_graph: Weak<RwLock<BankForks>>) {
        self.transaction_processor
            .program_cache
            .write()
            .unwrap()
            .set_fork_graph(fork_graph);
    }

    fn prepare_program_cache_for_upcoming_feature_set(&self) {
        let (_epoch, slot_index) = self.epoch_schedule.get_epoch_and_slot_index(self.slot);
        let slots_in_epoch = self.epoch_schedule.get_slots_in_epoch(self.epoch);
        let compute_budget = self.compute_budget.unwrap_or_default().to_budget();
        let (upcoming_feature_set, _newly_activated) = self.compute_active_feature_set(true);

        // Recompile loaded programs one at a time before the next epoch hits
        let slots_in_recompilation_phase =
            (solana_program_runtime::loaded_programs::MAX_LOADED_ENTRY_COUNT as u64)
                .min(slots_in_epoch)
                .checked_div(2)
                .unwrap();

        let mut program_cache = self.transaction_processor.program_cache.write().unwrap();

        if program_cache.upcoming_environments.is_some() {
            if let Some((key, program_to_recompile)) = program_cache.programs_to_recompile.pop() {
                let effective_epoch = program_cache.latest_root_epoch.saturating_add(1);
                drop(program_cache);
                let environments_for_epoch = self
                    .transaction_processor
                    .program_cache
                    .read()
                    .unwrap()
                    .get_environments_for_epoch(effective_epoch);
                if let Some(recompiled) = load_program_with_pubkey(
                    self,
                    &environments_for_epoch,
                    &key,
                    self.slot,
                    &mut ExecuteTimings::default(),
                    false,
                ) {
                    recompiled.tx_usage_counter.fetch_add(
                        program_to_recompile
                            .tx_usage_counter
                            .load(Ordering::Relaxed),
                        Ordering::Relaxed,
                    );
                    recompiled.ix_usage_counter.fetch_add(
                        program_to_recompile
                            .ix_usage_counter
                            .load(Ordering::Relaxed),
                        Ordering::Relaxed,
                    );
                    let mut program_cache =
                        self.transaction_processor.program_cache.write().unwrap();
                    program_cache.assign_program(key, recompiled);
                }
            }
        } else if self.epoch != program_cache.latest_root_epoch
            || slot_index.saturating_add(slots_in_recompilation_phase) >= slots_in_epoch
        {
            // Anticipate the upcoming program runtime environment for the next epoch,
            // so we can try to recompile loaded programs before the feature transition hits.
            drop(program_cache);
            let mut program_cache = self.transaction_processor.program_cache.write().unwrap();
            let program_runtime_environment_v1 = create_program_runtime_environment_v1(
                &upcoming_feature_set.runtime_features(),
                &compute_budget,
                false, /* deployment */
                false, /* debugging_features */
            )
            .unwrap();
            let program_runtime_environment_v2 = create_program_runtime_environment_v2(
                &compute_budget,
                false, /* debugging_features */
            );
            let mut upcoming_environments = program_cache.environments.clone();
            let changed_program_runtime_v1 =
                *upcoming_environments.program_runtime_v1 != program_runtime_environment_v1;
            let changed_program_runtime_v2 =
                *upcoming_environments.program_runtime_v2 != program_runtime_environment_v2;
            if changed_program_runtime_v1 {
                upcoming_environments.program_runtime_v1 = Arc::new(program_runtime_environment_v1);
            }
            if changed_program_runtime_v2 {
                upcoming_environments.program_runtime_v2 = Arc::new(program_runtime_environment_v2);
            }
            program_cache.upcoming_environments = Some(upcoming_environments);
            program_cache.programs_to_recompile = program_cache
                .get_flattened_entries(changed_program_runtime_v1, changed_program_runtime_v2);
            program_cache
                .programs_to_recompile
                .sort_by_cached_key(|(_id, program)| program.decayed_usage_counter(self.slot));
        }
    }

    pub fn prune_program_cache(&self, new_root_slot: Slot, new_root_epoch: Epoch) {
        self.transaction_processor
            .program_cache
            .write()
            .unwrap()
            .prune(new_root_slot, new_root_epoch);
    }

    pub fn prune_program_cache_by_deployment_slot(&self, deployment_slot: Slot) {
        self.transaction_processor
            .program_cache
            .write()
            .unwrap()
            .prune_by_deployment_slot(deployment_slot);
    }

    /// Epoch in which the new cooldown warmup rate for stake was activated
    pub fn new_warmup_cooldown_rate_epoch(&self) -> Option<Epoch> {
        self.feature_set
            .new_warmup_cooldown_rate_epoch(&self.epoch_schedule)
    }

    /// process for the start of a new epoch
    fn process_new_epoch(
        &mut self,
        parent_epoch: Epoch,
        parent_slot: Slot,
        parent_height: u64,
        reward_calc_tracer: Option<impl RewardCalcTracer>,
    ) {
        let epoch = self.epoch();
        let slot = self.slot();
        let (thread_pool, thread_pool_time_us) = measure_us!(ThreadPoolBuilder::new()
            .thread_name(|i| format!("solBnkNewEpch{i:02}"))
            .build()
            .expect("new rayon threadpool"));

        let (_, apply_feature_activations_time_us) = measure_us!(thread_pool.install(|| {
            self.apply_feature_activations(ApplyFeatureActivationsCaller::NewFromParent, false)
        }));

        // Add new entry to stakes.stake_history, set appropriate epoch and
        // update vote accounts with warmed up stakes before saving a
        // snapshot of stakes in epoch stakes
        let (_, activate_epoch_time_us) = measure_us!(self.stakes_cache.activate_epoch(
            epoch,
            &thread_pool,
            self.new_warmup_cooldown_rate_epoch()
        ));

        // Save a snapshot of stakes for use in consensus and stake weighted networking
        let leader_schedule_epoch = self.epoch_schedule.get_leader_schedule_epoch(slot);
        let (_, update_epoch_stakes_time_us) =
            measure_us!(self.update_epoch_stakes(leader_schedule_epoch));

        let mut rewards_metrics = RewardsMetrics::default();
        // After saving a snapshot of stakes, apply stake rewards and commission
        let (_, update_rewards_with_thread_pool_time_us) = measure_us!(self
            .begin_partitioned_rewards(
                reward_calc_tracer,
                &thread_pool,
                parent_epoch,
                parent_slot,
                parent_height,
                &mut rewards_metrics,
            ));

        report_new_epoch_metrics(
            epoch,
            slot,
            parent_slot,
            NewEpochTimings {
                thread_pool_time_us,
                apply_feature_activations_time_us,
                activate_epoch_time_us,
                update_epoch_stakes_time_us,
                update_rewards_with_thread_pool_time_us,
            },
            rewards_metrics,
        );
    }

    pub fn byte_limit_for_scans(&self) -> Option<usize> {
        self.rc
            .accounts
            .accounts_db
            .accounts_index
            .scan_results_limit_bytes
    }

    pub fn proper_ancestors_set(&self) -> HashSet<Slot> {
        HashSet::from_iter(self.proper_ancestors())
    }

    /// Returns all ancestors excluding self.slot.
    pub(crate) fn proper_ancestors(&self) -> impl Iterator<Item = Slot> + '_ {
        self.ancestors
            .keys()
            .into_iter()
            .filter(move |slot| *slot != self.slot)
    }

    pub fn set_callback(&self, callback: Option<Box<dyn DropCallback + Send + Sync>>) {
        *self.drop_callback.write().unwrap() = OptionalDropCallback(callback);
    }

    pub fn vote_only_bank(&self) -> bool {
        self.vote_only_bank
    }

    /// Like `new_from_parent` but additionally:
    /// * Doesn't assume that the parent is anywhere near `slot`, parent could be millions of slots
    ///   in the past
    /// * Adjusts the new bank's tick height to avoid having to run PoH for millions of slots
    /// * Freezes the new bank, assuming that the user will `Bank::new_from_parent` from this bank
    pub fn warp_from_parent(parent: Arc<Bank>, collector_id: &Pubkey, slot: Slot) -> Self {
        parent.freeze();
        let parent_timestamp = parent.clock().unix_timestamp;
        let mut new = Bank::new_from_parent(parent, collector_id, slot);
        new.apply_feature_activations(ApplyFeatureActivationsCaller::WarpFromParent, false);
        new.update_epoch_stakes(new.epoch_schedule().get_epoch(slot));
        new.tick_height.store(new.max_tick_height(), Relaxed);

        let mut clock = new.clock();
        clock.epoch_start_timestamp = parent_timestamp;
        clock.unix_timestamp = parent_timestamp;
        new.update_sysvar_account(&sysvar::clock::id(), |account| {
            create_account(
                &clock,
                new.inherit_specially_retained_account_fields(account),
            )
        });
        new.transaction_processor
            .fill_missing_sysvar_cache_entries(&new);
        new.freeze();
        new
    }

    /// Create a bank from explicit arguments and deserialized fields from snapshot
    pub(crate) fn new_from_fields(
        bank_rc: BankRc,
        genesis_config: &GenesisConfig,
        runtime_config: Arc<RuntimeConfig>,
        fields: BankFieldsToDeserialize,
        debug_keys: Option<Arc<HashSet<Pubkey>>>,
        additional_builtins: Option<&[BuiltinPrototype]>,
        debug_do_not_add_builtins: bool,
        accounts_data_size_initial: u64,
    ) -> Self {
        let now = Instant::now();
        let ancestors = Ancestors::from(&fields.ancestors);
        // For backward compatibility, we can only serialize and deserialize
        // Stakes<Delegation> in BankFieldsTo{Serialize,Deserialize}. But Bank
        // caches Stakes<StakeAccount>. Below Stakes<StakeAccount> is obtained
        // from Stakes<Delegation> by reading the full account state from
        // accounts-db. Note that it is crucial that these accounts are loaded
        // at the right slot and match precisely with serialized Delegations.
        //
        // Note that we are disabling the read cache while we populate the stakes cache.
        // The stakes accounts will not be expected to be loaded again.
        // If we populate the read cache with these loads, then we'll just soon have to evict these.
        let (stakes, stakes_time) = measure_time!(Stakes::new(&fields.stakes, |pubkey| {
            let (account, _slot) = bank_rc
                .accounts
                .load_with_fixed_root_do_not_populate_read_cache(&ancestors, pubkey)?;
            Some(account)
        })
        .expect(
            "Stakes cache is inconsistent with accounts-db. This can indicate \
            a corrupted snapshot or bugs in cached accounts or accounts-db.",
        ));
        info!("Loading Stakes took: {stakes_time}");
        let stakes_accounts_load_duration = now.elapsed();
        let mut bank = Self {
            rc: bank_rc,
            status_cache: Arc::<RwLock<BankStatusCache>>::default(),
            blockhash_queue: RwLock::new(fields.blockhash_queue),
            ancestors,
            hash: RwLock::new(fields.hash),
            parent_hash: fields.parent_hash,
            parent_slot: fields.parent_slot,
            hard_forks: Arc::new(RwLock::new(fields.hard_forks)),
            transaction_count: AtomicU64::new(fields.transaction_count),
            non_vote_transaction_count_since_restart: AtomicU64::default(),
            transaction_error_count: AtomicU64::default(),
            transaction_entries_count: AtomicU64::default(),
            transactions_per_entry_max: AtomicU64::default(),
            tick_height: AtomicU64::new(fields.tick_height),
            signature_count: AtomicU64::new(fields.signature_count),
            capitalization: AtomicU64::new(fields.capitalization),
            max_tick_height: fields.max_tick_height,
            hashes_per_tick: fields.hashes_per_tick,
            ticks_per_slot: fields.ticks_per_slot,
            ns_per_slot: fields.ns_per_slot,
            genesis_creation_time: fields.genesis_creation_time,
            slots_per_year: fields.slots_per_year,
            slot: fields.slot,
            bank_id: 0,
            epoch: fields.epoch,
            block_height: fields.block_height,
            collector_id: fields.collector_id,
            collector_fees: AtomicU64::new(fields.collector_fees),
            fee_rate_governor: fields.fee_rate_governor,
            // clone()-ing is needed to consider a gated behavior in rent_collector
            rent_collector: Self::get_rent_collector_from(&fields.rent_collector, fields.epoch),
            epoch_schedule: fields.epoch_schedule,
            inflation: Arc::new(RwLock::new(fields.inflation)),
            stakes_cache: StakesCache::new(stakes),
            epoch_stakes: fields.versioned_epoch_stakes,
            is_delta: AtomicBool::new(fields.is_delta),
            rewards: RwLock::new(vec![]),
            cluster_type: Some(genesis_config.cluster_type),
            rewards_pool_pubkeys: Arc::<HashSet<Pubkey>>::default(),
            transaction_debug_keys: debug_keys,
            transaction_log_collector_config: Arc::<RwLock<TransactionLogCollectorConfig>>::default(
            ),
            transaction_log_collector: Arc::<RwLock<TransactionLogCollector>>::default(),
            feature_set: Arc::<FeatureSet>::default(),
            reserved_account_keys: Arc::<ReservedAccountKeys>::default(),
            drop_callback: RwLock::new(OptionalDropCallback(None)),
            freeze_started: AtomicBool::new(fields.hash != Hash::default()),
            vote_only_bank: false,
            cost_tracker: RwLock::new(CostTracker::default()),
            accounts_data_size_initial,
            accounts_data_size_delta_on_chain: AtomicI64::new(0),
            accounts_data_size_delta_off_chain: AtomicI64::new(0),
            epoch_reward_status: EpochRewardStatus::default(),
            transaction_processor: TransactionBatchProcessor::default(),
            check_program_modification_slot: false,
            // collector_fee_details is not serialized to snapshot
            collector_fee_details: RwLock::new(CollectorFeeDetails::default()),
            compute_budget: runtime_config.compute_budget,
            transaction_account_lock_limit: runtime_config.transaction_account_lock_limit,
            fee_structure: FeeStructure::default(),
            #[cfg(feature = "dev-context-only-utils")]
            hash_overrides: Arc::new(Mutex::new(HashOverrides::default())),
            accounts_lt_hash: Mutex::new(fields.accounts_lt_hash),
            cache_for_accounts_lt_hash: DashMap::default(),
            stats_for_accounts_lt_hash: AccountsLtHashStats::default(),
            block_id: RwLock::new(None),
            bank_hash_stats: AtomicBankHashStats::new(&fields.bank_hash_stats),
            epoch_rewards_calculation_cache: Arc::new(Mutex::new(HashMap::default())),
        };

        bank.transaction_processor =
            TransactionBatchProcessor::new_uninitialized(bank.slot, bank.epoch);

        // TODO: Only create the thread pool if we need to recalculate rewards,
        // i.e. epoch_reward_status is active. Currently, this thread pool is
        // always created and used for recalculate_partitioned_rewards and
        // lt_hash calculation. Once lt_hash feature is active, lt_hash won't
        // need the thread pool. Thereby, after lt_hash feature activation, we
        // can change to create the thread pool only when we need to recalculate
        // rewards.
        let thread_pool = ThreadPoolBuilder::new()
            .thread_name(|i| format!("solBnkNewFlds{i:02}"))
            .build()
            .expect("new rayon threadpool");
        bank.recalculate_partitioned_rewards(null_tracer(), &thread_pool);

        bank.finish_init(
            genesis_config,
            additional_builtins,
            debug_do_not_add_builtins,
        );
        bank.transaction_processor
            .fill_missing_sysvar_cache_entries(&bank);

        // Sanity assertions between bank snapshot and genesis config
        // Consider removing from serializable bank state
        // (BankFieldsToSerialize/BankFieldsToDeserialize) and initializing
        // from the passed in genesis_config instead (as new()/new_with_paths() already do)
        assert_eq!(
            bank.genesis_creation_time, genesis_config.creation_time,
            "Bank snapshot genesis creation time does not match genesis.bin creation time. \
             The snapshot and genesis.bin might pertain to different clusters"
        );
        assert_eq!(bank.ticks_per_slot, genesis_config.ticks_per_slot);
        assert_eq!(
            bank.ns_per_slot,
            genesis_config.poh_config.target_tick_duration.as_nanos()
                * genesis_config.ticks_per_slot as u128
        );
        assert_eq!(bank.max_tick_height, (bank.slot + 1) * bank.ticks_per_slot);
        assert_eq!(
            bank.slots_per_year,
            years_as_slots(
                1.0,
                &genesis_config.poh_config.target_tick_duration,
                bank.ticks_per_slot,
            )
        );
        assert_eq!(bank.epoch_schedule, genesis_config.epoch_schedule);
        assert_eq!(bank.epoch, bank.epoch_schedule.get_epoch(bank.slot));

        datapoint_info!(
            "bank-new-from-fields",
            (
                "accounts_data_len-from-snapshot",
                fields.accounts_data_len as i64,
                i64
            ),
            (
                "accounts_data_len-from-generate_index",
                accounts_data_size_initial as i64,
                i64
            ),
            (
                "stakes_accounts_load_duration_us",
                stakes_accounts_load_duration.as_micros(),
                i64
            ),
        );
        bank
    }

    /// Return subset of bank fields representing serializable state
    pub(crate) fn get_fields_to_serialize(&self) -> BankFieldsToSerialize {
        BankFieldsToSerialize {
            blockhash_queue: self.blockhash_queue.read().unwrap().clone(),
            ancestors: AncestorsForSerialization::from(&self.ancestors),
            hash: *self.hash.read().unwrap(),
            parent_hash: self.parent_hash,
            parent_slot: self.parent_slot,
            hard_forks: self.hard_forks.read().unwrap().clone(),
            transaction_count: self.transaction_count.load(Relaxed),
            tick_height: self.tick_height.load(Relaxed),
            signature_count: self.signature_count.load(Relaxed),
            capitalization: self.capitalization.load(Relaxed),
            max_tick_height: self.max_tick_height,
            hashes_per_tick: self.hashes_per_tick,
            ticks_per_slot: self.ticks_per_slot,
            ns_per_slot: self.ns_per_slot,
            genesis_creation_time: self.genesis_creation_time,
            slots_per_year: self.slots_per_year,
            slot: self.slot,
            epoch: self.epoch,
            block_height: self.block_height,
            collector_id: self.collector_id,
            collector_fees: self.collector_fees.load(Relaxed),
            fee_rate_governor: self.fee_rate_governor.clone(),
            rent_collector: self.rent_collector.clone(),
            epoch_schedule: self.epoch_schedule.clone(),
            inflation: *self.inflation.read().unwrap(),
            stakes: self.stakes_cache.stakes().clone(),
            is_delta: self.is_delta.load(Relaxed),
            accounts_data_len: self.load_accounts_data_size(),
            versioned_epoch_stakes: self.epoch_stakes.clone(),
            accounts_lt_hash: self.accounts_lt_hash.lock().unwrap().clone(),
        }
    }

    pub fn collector_id(&self) -> &Pubkey {
        &self.collector_id
    }

    pub fn genesis_creation_time(&self) -> UnixTimestamp {
        self.genesis_creation_time
    }

    pub fn slot(&self) -> Slot {
        self.slot
    }

    pub fn bank_id(&self) -> BankId {
        self.bank_id
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    pub fn first_normal_epoch(&self) -> Epoch {
        self.epoch_schedule().first_normal_epoch
    }

    pub fn freeze_lock(&self) -> RwLockReadGuard<Hash> {
        self.hash.read().unwrap()
    }

    pub fn hash(&self) -> Hash {
        *self.hash.read().unwrap()
    }

    pub fn is_frozen(&self) -> bool {
        *self.hash.read().unwrap() != Hash::default()
    }

    pub fn freeze_started(&self) -> bool {
        self.freeze_started.load(Relaxed)
    }

    pub fn status_cache_ancestors(&self) -> Vec<u64> {
        let mut roots = self.status_cache.read().unwrap().roots().clone();
        let min = roots.iter().min().cloned().unwrap_or(0);
        for ancestor in self.ancestors.keys() {
            if ancestor >= min {
                roots.insert(ancestor);
            }
        }

        let mut ancestors: Vec<_> = roots.into_iter().collect();
        #[allow(clippy::stable_sort_primitive)]
        ancestors.sort();
        ancestors
    }

    /// computed unix_timestamp at this slot height
    pub fn unix_timestamp_from_genesis(&self) -> i64 {
        self.genesis_creation_time.saturating_add(
            (self.slot as u128)
                .saturating_mul(self.ns_per_slot)
                .saturating_div(1_000_000_000) as i64,
        )
    }

    fn update_sysvar_account<F>(&self, pubkey: &Pubkey, updater: F)
    where
        F: Fn(&Option<AccountSharedData>) -> AccountSharedData,
    {
        let old_account = self.get_account_with_fixed_root(pubkey);
        let mut new_account = updater(&old_account);

        // When new sysvar comes into existence (with RENT_UNADJUSTED_INITIAL_BALANCE lamports),
        // this code ensures that the sysvar's balance is adjusted to be rent-exempt.
        //
        // More generally, this code always re-calculates for possible sysvar data size change,
        // although there is no such sysvars currently.
        self.adjust_sysvar_balance_for_rent(&mut new_account);
        self.store_account_and_update_capitalization(pubkey, &new_account);
    }

    fn inherit_specially_retained_account_fields(
        &self,
        old_account: &Option<AccountSharedData>,
    ) -> InheritableAccountFields {
        const RENT_UNADJUSTED_INITIAL_BALANCE: u64 = 1;

        (
            old_account
                .as_ref()
                .map(|a| a.lamports())
                .unwrap_or(RENT_UNADJUSTED_INITIAL_BALANCE),
            old_account
                .as_ref()
                .map(|a| a.rent_epoch())
                .unwrap_or(INITIAL_RENT_EPOCH),
        )
    }

    pub fn clock(&self) -> sysvar::clock::Clock {
        from_account(&self.get_account(&sysvar::clock::id()).unwrap_or_default())
            .unwrap_or_default()
    }

    fn update_clock(&self, parent_epoch: Option<Epoch>) {
        let mut unix_timestamp = self.clock().unix_timestamp;
        // set epoch_start_timestamp to None to warp timestamp
        let epoch_start_timestamp = {
            let epoch = if let Some(epoch) = parent_epoch {
                epoch
            } else {
                self.epoch()
            };
            let first_slot_in_epoch = self.epoch_schedule().get_first_slot_in_epoch(epoch);
            Some((first_slot_in_epoch, self.clock().epoch_start_timestamp))
        };
        let max_allowable_drift = MaxAllowableDrift {
            fast: MAX_ALLOWABLE_DRIFT_PERCENTAGE_FAST,
            slow: MAX_ALLOWABLE_DRIFT_PERCENTAGE_SLOW_V2,
        };

        let ancestor_timestamp = self.clock().unix_timestamp;
        if let Some(timestamp_estimate) =
            self.get_timestamp_estimate(max_allowable_drift, epoch_start_timestamp)
        {
            unix_timestamp = timestamp_estimate;
            if timestamp_estimate < ancestor_timestamp {
                unix_timestamp = ancestor_timestamp;
            }
        }
        datapoint_info!(
            "bank-timestamp-correction",
            ("slot", self.slot(), i64),
            ("from_genesis", self.unix_timestamp_from_genesis(), i64),
            ("corrected", unix_timestamp, i64),
            ("ancestor_timestamp", ancestor_timestamp, i64),
        );
        let mut epoch_start_timestamp =
            // On epoch boundaries, update epoch_start_timestamp
            if parent_epoch.is_some() && parent_epoch.unwrap() != self.epoch() {
                unix_timestamp
            } else {
                self.clock().epoch_start_timestamp
            };
        if self.slot == 0 {
            unix_timestamp = self.unix_timestamp_from_genesis();
            epoch_start_timestamp = self.unix_timestamp_from_genesis();
        }
        let clock = sysvar::clock::Clock {
            slot: self.slot,
            epoch_start_timestamp,
            epoch: self.epoch_schedule().get_epoch(self.slot),
            leader_schedule_epoch: self.epoch_schedule().get_leader_schedule_epoch(self.slot),
            unix_timestamp,
        };
        self.update_sysvar_account(&sysvar::clock::id(), |account| {
            create_account(
                &clock,
                self.inherit_specially_retained_account_fields(account),
            )
        });
    }

    pub fn update_last_restart_slot(&self) {
        let feature_flag = self
            .feature_set
            .is_active(&feature_set::last_restart_slot_sysvar::id());

        if feature_flag {
            // First, see what the currently stored last restart slot is. This
            // account may not exist yet if the feature was just activated.
            let current_last_restart_slot = self
                .get_account(&sysvar::last_restart_slot::id())
                .and_then(|account| {
                    let lrs: Option<LastRestartSlot> = from_account(&account);
                    lrs
                })
                .map(|account| account.last_restart_slot);

            let last_restart_slot = {
                let slot = self.slot;
                let hard_forks_r = self.hard_forks.read().unwrap();

                // Only consider hard forks <= this bank's slot to avoid prematurely applying
                // a hard fork that is set to occur in the future.
                hard_forks_r
                    .iter()
                    .rev()
                    .find(|(hard_fork, _)| *hard_fork <= slot)
                    .map(|(slot, _)| *slot)
                    .unwrap_or(0)
            };

            // Only need to write if the last restart has changed
            if current_last_restart_slot != Some(last_restart_slot) {
                self.update_sysvar_account(&sysvar::last_restart_slot::id(), |account| {
                    create_account(
                        &LastRestartSlot { last_restart_slot },
                        self.inherit_specially_retained_account_fields(account),
                    )
                });
            }
        }
    }

    pub fn set_sysvar_for_tests<T>(&self, sysvar: &T)
    where
        T: Sysvar + SysvarId,
    {
        self.update_sysvar_account(&T::id(), |account| {
            create_account(
                sysvar,
                self.inherit_specially_retained_account_fields(account),
            )
        });
        // Simply force fill sysvar cache rather than checking which sysvar was
        // actually updated since tests don't need to be optimized for performance.
        self.transaction_processor.reset_sysvar_cache();
        self.transaction_processor
            .fill_missing_sysvar_cache_entries(self);
    }

    fn update_slot_history(&self) {
        self.update_sysvar_account(&sysvar::slot_history::id(), |account| {
            let mut slot_history = account
                .as_ref()
                .map(|account| from_account::<SlotHistory, _>(account).unwrap())
                .unwrap_or_default();
            slot_history.add(self.slot());
            create_account(
                &slot_history,
                self.inherit_specially_retained_account_fields(account),
            )
        });
    }

    fn update_slot_hashes(&self) {
        self.update_sysvar_account(&sysvar::slot_hashes::id(), |account| {
            let mut slot_hashes = account
                .as_ref()
                .map(|account| from_account::<SlotHashes, _>(account).unwrap())
                .unwrap_or_default();
            slot_hashes.add(self.parent_slot, self.parent_hash);
            create_account(
                &slot_hashes,
                self.inherit_specially_retained_account_fields(account),
            )
        });
    }

    pub fn get_slot_history(&self) -> SlotHistory {
        from_account(&self.get_account(&sysvar::slot_history::id()).unwrap()).unwrap()
    }

    fn update_epoch_stakes(&mut self, leader_schedule_epoch: Epoch) {
        // update epoch_stakes cache
        //  if my parent didn't populate for this staker's epoch, we've
        //  crossed a boundary
        if !self.epoch_stakes.contains_key(&leader_schedule_epoch) {
            self.epoch_stakes.retain(|&epoch, _| {
                epoch >= leader_schedule_epoch.saturating_sub(MAX_LEADER_SCHEDULE_STAKES)
            });
            let stakes = self.stakes_cache.stakes().clone();
            let stakes = SerdeStakesToStakeFormat::from(stakes);
            let new_epoch_stakes = VersionedEpochStakes::new(stakes, leader_schedule_epoch);
            info!(
                "new epoch stakes, epoch: {}, total_stake: {}",
                leader_schedule_epoch,
                new_epoch_stakes.total_stake(),
            );

            // It is expensive to log the details of epoch stakes. Only log them at "trace"
            // level for debugging purpose.
            if log::log_enabled!(log::Level::Trace) {
                let vote_stakes: HashMap<_, _> = self
                    .stakes_cache
                    .stakes()
                    .vote_accounts()
                    .delegated_stakes()
                    .map(|(pubkey, stake)| (*pubkey, stake))
                    .collect();
                trace!("new epoch stakes, stakes: {vote_stakes:#?}");
            }
            self.epoch_stakes
                .insert(leader_schedule_epoch, new_epoch_stakes);
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn set_epoch_stakes_for_test(&mut self, epoch: Epoch, stakes: VersionedEpochStakes) {
        self.epoch_stakes.insert(epoch, stakes);
    }

    fn update_rent(&self) {
        self.update_sysvar_account(&sysvar::rent::id(), |account| {
            create_account(
                &self.rent_collector.rent,
                self.inherit_specially_retained_account_fields(account),
            )
        });
    }

    fn update_epoch_schedule(&self) {
        self.update_sysvar_account(&sysvar::epoch_schedule::id(), |account| {
            create_account(
                self.epoch_schedule(),
                self.inherit_specially_retained_account_fields(account),
            )
        });
    }

    fn update_stake_history(&self, epoch: Option<Epoch>) {
        if epoch == Some(self.epoch()) {
            return;
        }
        // if I'm the first Bank in an epoch, ensure stake_history is updated
        self.update_sysvar_account(&sysvar::stake_history::id(), |account| {
            create_account::<sysvar::stake_history::StakeHistory>(
                self.stakes_cache.stakes().history(),
                self.inherit_specially_retained_account_fields(account),
            )
        });
    }

    pub fn epoch_duration_in_years(&self, prev_epoch: Epoch) -> f64 {
        // period: time that has passed as a fraction of a year, basically the length of
        //  an epoch as a fraction of a year
        //  calculated as: slots_elapsed / (slots / year)
        self.epoch_schedule().get_slots_in_epoch(prev_epoch) as f64 / self.slots_per_year
    }

    // Calculates the starting-slot for inflation from the activation slot.
    // This method assumes that `pico_inflation` will be enabled before `full_inflation`, giving
    // precedence to the latter. However, since `pico_inflation` is fixed-rate Inflation, should
    // `pico_inflation` be enabled 2nd, the incorrect start slot provided here should have no
    // effect on the inflation calculation.
    fn get_inflation_start_slot(&self) -> Slot {
        let mut slots = self
            .feature_set
            .full_inflation_features_enabled()
            .iter()
            .filter_map(|id| self.feature_set.activated_slot(id))
            .collect::<Vec<_>>();
        slots.sort_unstable();
        slots.first().cloned().unwrap_or_else(|| {
            self.feature_set
                .activated_slot(&feature_set::pico_inflation::id())
                .unwrap_or(0)
        })
    }

    fn get_inflation_num_slots(&self) -> u64 {
        let inflation_activation_slot = self.get_inflation_start_slot();
        // Normalize inflation_start to align with the start of rewards accrual.
        let inflation_start_slot = self.epoch_schedule().get_first_slot_in_epoch(
            self.epoch_schedule()
                .get_epoch(inflation_activation_slot)
                .saturating_sub(1),
        );
        self.epoch_schedule().get_first_slot_in_epoch(self.epoch()) - inflation_start_slot
    }

    pub fn slot_in_year_for_inflation(&self) -> f64 {
        let num_slots = self.get_inflation_num_slots();

        // calculated as: num_slots / (slots / year)
        num_slots as f64 / self.slots_per_year
    }

    fn calculate_previous_epoch_inflation_rewards(
        &self,
        prev_epoch_capitalization: u64,
        prev_epoch: Epoch,
    ) -> PrevEpochInflationRewards {
        let slot_in_year = self.slot_in_year_for_inflation();
        let (validator_rate, foundation_rate) = {
            let inflation = self.inflation.read().unwrap();
            (
                (*inflation).validator(slot_in_year),
                (*inflation).foundation(slot_in_year),
            )
        };

        let prev_epoch_duration_in_years = self.epoch_duration_in_years(prev_epoch);
        let validator_rewards = (validator_rate
            * prev_epoch_capitalization as f64
            * prev_epoch_duration_in_years) as u64;

        PrevEpochInflationRewards {
            validator_rewards,
            prev_epoch_duration_in_years,
            validator_rate,
            foundation_rate,
        }
    }

    fn filter_stake_delegations<'a>(
        &self,
        stakes: &'a Stakes<StakeAccount<Delegation>>,
    ) -> Vec<(&'a Pubkey, &'a StakeAccount<Delegation>)> {
        if self
            .feature_set
            .is_active(&feature_set::stake_minimum_delegation_for_rewards::id())
        {
            let num_stake_delegations = stakes.stake_delegations().len();
            let min_stake_delegation = solana_stake_program::get_minimum_delegation(
                self.feature_set
                    .is_active(&agave_feature_set::stake_raise_minimum_delegation_to_1_sol::id()),
            )
            .max(LAMPORTS_PER_SOL);

            let (stake_delegations, filter_time_us) = measure_us!(stakes
                .stake_delegations()
                .iter()
                .filter(|(_stake_pubkey, cached_stake_account)| {
                    cached_stake_account.delegation().stake >= min_stake_delegation
                })
                .collect::<Vec<_>>());

            datapoint_info!(
                "stake_account_filter_time",
                ("filter_time_us", filter_time_us, i64),
                ("num_stake_delegations_before", num_stake_delegations, i64),
                ("num_stake_delegations_after", stake_delegations.len(), i64)
            );
            stake_delegations
        } else {
            stakes.stake_delegations().iter().collect()
        }
    }

    /// return reward info for each vote account
    /// return account data for each vote account that needs to be stored
    /// This return value is a little awkward at the moment so that downstream existing code in the non-partitioned rewards code path can be re-used without duplication or modification.
    /// This function is copied from the existing code path's `store_vote_accounts`.
    /// The primary differences:
    /// - we want this fn to have no side effects (such as actually storing vote accounts) so that we
    ///   can compare the expected results with the current code path
    /// - we want to be able to batch store the vote accounts later for improved performance/cache updating
    fn calc_vote_accounts_to_store(vote_account_rewards: VoteRewards) -> VoteRewardsAccounts {
        let len = vote_account_rewards.len();
        let mut result = VoteRewardsAccounts {
            rewards: Vec::with_capacity(len),
            accounts_to_store: Vec::with_capacity(len),
            total_vote_rewards_lamports: 0,
        };
        vote_account_rewards.into_iter().for_each(
            |(
                vote_pubkey,
                VoteReward {
                    mut vote_account,
                    commission,
                    vote_rewards,
                },
            )| {
                if let Err(err) = vote_account.checked_add_lamports(vote_rewards) {
                    debug!("reward redemption failed for {}: {:?}", vote_pubkey, err);
                    return;
                }

                result.rewards.push((
                    vote_pubkey,
                    RewardInfo {
                        reward_type: RewardType::Voting,
                        lamports: vote_rewards as i64,
                        post_balance: vote_account.lamports(),
                        commission: Some(commission),
                    },
                ));
                result.accounts_to_store.push((vote_pubkey, vote_account));
                result.total_vote_rewards_lamports += vote_rewards;
            },
        );
        result
    }

    fn update_reward_history(
        &self,
        stake_rewards: StakeRewards,
        vote_rewards: &[(Pubkey, RewardInfo)],
    ) {
        let additional_reserve = stake_rewards.len() + vote_rewards.len();
        let mut rewards = self.rewards.write().unwrap();
        rewards.reserve(additional_reserve);
        vote_rewards.iter().for_each(|(vote_pubkey, vote_reward)| {
            rewards.push((*vote_pubkey, *vote_reward));
        });
        stake_rewards
            .into_iter()
            .filter(|x| x.get_stake_reward() > 0)
            .for_each(|x| rewards.push((x.stake_pubkey, x.stake_reward_info)));
    }

    fn update_recent_blockhashes_locked(&self, locked_blockhash_queue: &BlockhashQueue) {
        #[allow(deprecated)]
        self.update_sysvar_account(&sysvar::recent_blockhashes::id(), |account| {
            let recent_blockhash_iter = locked_blockhash_queue.get_recent_blockhashes();
            recent_blockhashes_account::create_account_with_data_and_fields(
                recent_blockhash_iter,
                self.inherit_specially_retained_account_fields(account),
            )
        });
    }

    pub fn update_recent_blockhashes(&self) {
        let blockhash_queue = self.blockhash_queue.read().unwrap();
        self.update_recent_blockhashes_locked(&blockhash_queue);
    }

    fn get_timestamp_estimate(
        &self,
        max_allowable_drift: MaxAllowableDrift,
        epoch_start_timestamp: Option<(Slot, UnixTimestamp)>,
    ) -> Option<UnixTimestamp> {
        let mut get_timestamp_estimate_time = Measure::start("get_timestamp_estimate");
        let slots_per_epoch = self.epoch_schedule().slots_per_epoch;
        let vote_accounts = self.vote_accounts();
        let recent_timestamps = vote_accounts.iter().filter_map(|(pubkey, (_, account))| {
            let vote_state = account.vote_state_view();
            let last_timestamp = vote_state.last_timestamp();
            let slot_delta = self.slot().checked_sub(last_timestamp.slot)?;
            (slot_delta <= slots_per_epoch)
                .then_some((*pubkey, (last_timestamp.slot, last_timestamp.timestamp)))
        });
        let slot_duration = Duration::from_nanos(self.ns_per_slot as u64);
        let epoch = self.epoch_schedule().get_epoch(self.slot());
        let stakes = self.epoch_vote_accounts(epoch)?;
        let stake_weighted_timestamp = calculate_stake_weighted_timestamp(
            recent_timestamps,
            stakes,
            self.slot(),
            slot_duration,
            epoch_start_timestamp,
            max_allowable_drift,
            self.feature_set
                .is_active(&feature_set::warp_timestamp_again::id()),
        );
        get_timestamp_estimate_time.stop();
        datapoint_info!(
            "bank-timestamp",
            (
                "get_timestamp_estimate_us",
                get_timestamp_estimate_time.as_us(),
                i64
            ),
        );
        stake_weighted_timestamp
    }

    /// Recalculates the bank hash
    ///
    /// This is used by ledger-tool when creating a snapshot, which
    /// recalcuates the bank hash.
    ///
    /// Note that the account state is *not* allowed to change by rehashing.
    /// If modifying accounts in ledger-tool is needed, create a new bank.
    pub fn rehash(&self) {
        let mut hash = self.hash.write().unwrap();
        let new = self.hash_internal_state();
        if new != *hash {
            warn!("Updating bank hash to {new}");
            *hash = new;
        }
    }

    pub fn freeze(&self) {
        // This lock prevents any new commits from BankingStage
        // `Consumer::execute_and_commit_transactions_locked()` from
        // coming in after the last tick is observed. This is because in
        // BankingStage, any transaction successfully recorded in
        // `record_transactions()` is recorded after this `hash` lock
        // is grabbed. At the time of the successful record,
        // this means the PoH has not yet reached the last tick,
        // so this means freeze() hasn't been called yet. And because
        // BankingStage doesn't release this hash lock until both
        // record and commit are finished, those transactions will be
        // committed before this write lock can be obtained here.
        let mut hash = self.hash.write().unwrap();
        if *hash == Hash::default() {
            // finish up any deferred changes to account state
            self.distribute_transaction_fee_details();
            self.update_slot_history();
            self.run_incinerator();

            // freeze is a one-way trip, idempotent
            self.freeze_started.store(true, Relaxed);
            // updating the accounts lt hash must happen *outside* of hash_internal_state() so
            // that rehash() can be called and *not* modify self.accounts_lt_hash.
            self.update_accounts_lt_hash();
            *hash = self.hash_internal_state();
            self.rc.accounts.accounts_db.mark_slot_frozen(self.slot());
        }
    }

    // dangerous; don't use this; this is only needed for ledger-tool's special command
    #[cfg(feature = "dev-context-only-utils")]
    pub fn unfreeze_for_ledger_tool(&self) {
        self.freeze_started.store(false, Relaxed);
    }

    pub fn epoch_schedule(&self) -> &EpochSchedule {
        &self.epoch_schedule
    }

    /// squash the parent's state up into this Bank,
    ///   this Bank becomes a root
    /// Note that this function is not thread-safe. If it is called concurrently on the same bank
    /// by multiple threads, the end result could be inconsistent.
    /// Calling code does not currently call this concurrently.
    pub fn squash(&self) -> SquashTiming {
        self.freeze();

        //this bank and all its parents are now on the rooted path
        let mut roots = vec![self.slot()];
        roots.append(&mut self.parents().iter().map(|p| p.slot()).collect());

        let mut total_index_us = 0;
        let mut total_cache_us = 0;
        let mut total_store_us = 0;

        let mut squash_accounts_time = Measure::start("squash_accounts_time");
        for slot in roots.iter().rev() {
            // root forks cannot be purged
            let add_root_timing = self.rc.accounts.add_root(*slot);
            total_index_us += add_root_timing.index_us;
            total_cache_us += add_root_timing.cache_us;
            total_store_us += add_root_timing.store_us;
        }
        squash_accounts_time.stop();

        *self.rc.parent.write().unwrap() = None;

        let mut squash_cache_time = Measure::start("squash_cache_time");
        roots
            .iter()
            .for_each(|slot| self.status_cache.write().unwrap().add_root(*slot));
        squash_cache_time.stop();

        SquashTiming {
            squash_accounts_ms: squash_accounts_time.as_ms(),
            squash_accounts_index_ms: total_index_us / 1000,
            squash_accounts_cache_ms: total_cache_us / 1000,
            squash_accounts_store_ms: total_store_us / 1000,

            squash_cache_ms: squash_cache_time.as_ms(),
        }
    }

    /// Return the more recent checkpoint of this bank instance.
    pub fn parent(&self) -> Option<Arc<Bank>> {
        self.rc.parent.read().unwrap().clone()
    }

    pub fn parent_slot(&self) -> Slot {
        self.parent_slot
    }

    pub fn parent_hash(&self) -> Hash {
        self.parent_hash
    }

    fn process_genesis_config(
        &mut self,
        genesis_config: &GenesisConfig,
        #[cfg(feature = "dev-context-only-utils")] collector_id_for_tests: Option<Pubkey>,
        #[cfg(feature = "dev-context-only-utils")] genesis_hash: Option<Hash>,
    ) {
        // Bootstrap validator collects fees until `new_from_parent` is called.
        self.fee_rate_governor = genesis_config.fee_rate_governor.clone();

        for (pubkey, account) in genesis_config.accounts.iter() {
            assert!(
                self.get_account(pubkey).is_none(),
                "{pubkey} repeated in genesis config"
            );
            self.store_account(pubkey, &account.to_account_shared_data());
            self.capitalization.fetch_add(account.lamports(), Relaxed);
            self.accounts_data_size_initial += account.data().len() as u64;
        }

        for (pubkey, account) in genesis_config.rewards_pools.iter() {
            assert!(
                self.get_account(pubkey).is_none(),
                "{pubkey} repeated in genesis config"
            );
            self.store_account(pubkey, &account.to_account_shared_data());
            self.accounts_data_size_initial += account.data().len() as u64;
        }

        // After storing genesis accounts, the bank stakes cache will be warmed
        // up and can be used to set the collector id to the highest staked
        // node. If no staked nodes exist, allow fallback to an unstaked test
        // collector id during tests.
        let collector_id = self.stakes_cache.stakes().highest_staked_node().copied();
        #[cfg(feature = "dev-context-only-utils")]
        let collector_id = collector_id.or(collector_id_for_tests);
        self.collector_id =
            collector_id.expect("genesis processing failed because no staked nodes exist");

        #[cfg(not(feature = "dev-context-only-utils"))]
        let genesis_hash = genesis_config.hash();
        #[cfg(feature = "dev-context-only-utils")]
        let genesis_hash = genesis_hash.unwrap_or(genesis_config.hash());

        self.blockhash_queue.write().unwrap().genesis_hash(
            &genesis_hash,
            genesis_config.fee_rate_governor.lamports_per_signature,
        );

        self.hashes_per_tick = genesis_config.hashes_per_tick();
        self.ticks_per_slot = genesis_config.ticks_per_slot();
        self.ns_per_slot = genesis_config.ns_per_slot();
        self.genesis_creation_time = genesis_config.creation_time;
        self.max_tick_height = (self.slot + 1) * self.ticks_per_slot;
        self.slots_per_year = genesis_config.slots_per_year();

        self.epoch_schedule = genesis_config.epoch_schedule.clone();

        self.inflation = Arc::new(RwLock::new(genesis_config.inflation));

        self.rent_collector = RentCollector::new(
            self.epoch,
            self.epoch_schedule().clone(),
            self.slots_per_year,
            genesis_config.rent.clone(),
        );

        // Add additional builtin programs specified in the genesis config
        for (name, program_id) in &genesis_config.native_instruction_processors {
            self.add_builtin_account(name, program_id);
        }
    }

    fn burn_and_purge_account(&self, program_id: &Pubkey, mut account: AccountSharedData) {
        let old_data_size = account.data().len();
        self.capitalization.fetch_sub(account.lamports(), Relaxed);
        // Both resetting account balance to 0 and zeroing the account data
        // is needed to really purge from AccountsDb and flush the Stakes cache
        account.set_lamports(0);
        account.data_as_mut_slice().fill(0);
        self.store_account(program_id, &account);
        self.calculate_and_update_accounts_data_size_delta_off_chain(old_data_size, 0);
    }

    /// Add a precompiled program account
    pub fn add_precompiled_account(&self, program_id: &Pubkey) {
        self.add_precompiled_account_with_owner(program_id, native_loader::id())
    }

    // Used by tests to simulate clusters with precompiles that aren't owned by the native loader
    fn add_precompiled_account_with_owner(&self, program_id: &Pubkey, owner: Pubkey) {
        if let Some(account) = self.get_account_with_fixed_root(program_id) {
            if account.executable() {
                return;
            } else {
                // malicious account is pre-occupying at program_id
                self.burn_and_purge_account(program_id, account);
            }
        };

        assert!(
            !self.freeze_started(),
            "Can't change frozen bank by adding not-existing new precompiled program ({program_id}). \
                Maybe, inconsistent program activation is detected on snapshot restore?"
        );

        // Add a bogus executable account, which will be loaded and ignored.
        let (lamports, rent_epoch) = self.inherit_specially_retained_account_fields(&None);

        let account = AccountSharedData::from(Account {
            lamports,
            owner,
            data: vec![],
            executable: true,
            rent_epoch,
        });
        self.store_account_and_update_capitalization(program_id, &account);
    }

    pub fn set_rent_burn_percentage(&mut self, burn_percent: u8) {
        self.rent_collector.rent.burn_percent = burn_percent;
    }

    pub fn set_hashes_per_tick(&mut self, hashes_per_tick: Option<u64>) {
        self.hashes_per_tick = hashes_per_tick;
    }

    /// Return the last block hash registered.
    pub fn last_blockhash(&self) -> Hash {
        self.blockhash_queue.read().unwrap().last_hash()
    }

    pub fn last_blockhash_and_lamports_per_signature(&self) -> (Hash, u64) {
        let blockhash_queue = self.blockhash_queue.read().unwrap();
        let last_hash = blockhash_queue.last_hash();
        let last_lamports_per_signature = blockhash_queue
            .get_lamports_per_signature(&last_hash)
            .unwrap(); // safe so long as the BlockhashQueue is consistent
        (last_hash, last_lamports_per_signature)
    }

    pub fn is_blockhash_valid(&self, hash: &Hash) -> bool {
        let blockhash_queue = self.blockhash_queue.read().unwrap();
        blockhash_queue.is_hash_valid_for_age(hash, MAX_PROCESSING_AGE)
    }

    pub fn get_minimum_balance_for_rent_exemption(&self, data_len: usize) -> u64 {
        self.rent_collector.rent.minimum_balance(data_len).max(1)
    }

    pub fn get_lamports_per_signature(&self) -> u64 {
        self.fee_rate_governor.lamports_per_signature
    }

    pub fn get_lamports_per_signature_for_blockhash(&self, hash: &Hash) -> Option<u64> {
        let blockhash_queue = self.blockhash_queue.read().unwrap();
        blockhash_queue.get_lamports_per_signature(hash)
    }

    pub fn get_fee_for_message(&self, message: &SanitizedMessage) -> Option<u64> {
        let lamports_per_signature = {
            let blockhash_queue = self.blockhash_queue.read().unwrap();
            blockhash_queue.get_lamports_per_signature(message.recent_blockhash())
        }
        .or_else(|| {
            self.load_message_nonce_account(message).map(
                |(_nonce_address, _nonce_account, nonce_data)| {
                    nonce_data.get_lamports_per_signature()
                },
            )
        })?;
        Some(self.get_fee_for_message_with_lamports_per_signature(message, lamports_per_signature))
    }

    /// Returns true when startup accounts hash verification has completed or never had to run in background.
    pub fn get_startup_verification_complete(&self) -> &Arc<AtomicBool> {
        &self
            .rc
            .accounts
            .accounts_db
            .verify_accounts_hash_in_bg
            .verified
    }

    pub fn get_fee_for_message_with_lamports_per_signature(
        &self,
        message: &impl SVMMessage,
        lamports_per_signature: u64,
    ) -> u64 {
        let fee_budget_limits = FeeBudgetLimits::from(
            process_compute_budget_instructions(
                message.program_instructions_iter(),
                &self.feature_set,
            )
            .unwrap_or_default(),
        );
        solana_fee::calculate_fee(
            message,
            lamports_per_signature == 0,
            self.fee_structure().lamports_per_signature,
            fee_budget_limits.prioritization_fee,
            FeeFeatures::from(self.feature_set.as_ref()),
        )
    }

    pub fn get_blockhash_last_valid_block_height(&self, blockhash: &Hash) -> Option<Slot> {
        let blockhash_queue = self.blockhash_queue.read().unwrap();
        // This calculation will need to be updated to consider epoch boundaries if BlockhashQueue
        // length is made variable by epoch
        blockhash_queue
            .get_hash_age(blockhash)
            .map(|age| self.block_height + MAX_PROCESSING_AGE as u64 - age)
    }

    pub fn confirmed_last_blockhash(&self) -> Hash {
        const NUM_BLOCKHASH_CONFIRMATIONS: usize = 3;

        let parents = self.parents();
        if parents.is_empty() {
            self.last_blockhash()
        } else {
            let index = NUM_BLOCKHASH_CONFIRMATIONS.min(parents.len() - 1);
            parents[index].last_blockhash()
        }
    }

    /// Forget all signatures. Useful for benchmarking.
    pub fn clear_signatures(&self) {
        self.status_cache.write().unwrap().clear();
    }

    pub fn clear_slot_signatures(&self, slot: Slot) {
        self.status_cache.write().unwrap().clear_slot_entries(slot);
    }

    fn update_transaction_statuses(
        &self,
        sanitized_txs: &[impl TransactionWithMeta],
        processing_results: &[TransactionProcessingResult],
    ) {
        let mut status_cache = self.status_cache.write().unwrap();
        assert_eq!(sanitized_txs.len(), processing_results.len());
        for (tx, processing_result) in sanitized_txs.iter().zip(processing_results) {
            if let Ok(processed_tx) = &processing_result {
                // Add the message hash to the status cache to ensure that this message
                // won't be processed again with a different signature.
                status_cache.insert(
                    tx.recent_blockhash(),
                    tx.message_hash(),
                    self.slot(),
                    processed_tx.status(),
                );
                // Add the transaction signature to the status cache so that transaction status
                // can be queried by transaction signature over RPC. In the future, this should
                // only be added for API nodes because voting validators don't need to do this.
                status_cache.insert(
                    tx.recent_blockhash(),
                    tx.signature(),
                    self.slot(),
                    processed_tx.status(),
                );
            }
        }
    }

    /// Register a new recent blockhash in the bank's recent blockhash queue. Called when a bank
    /// reaches its max tick height. Can be called by tests to get new blockhashes for transaction
    /// processing without advancing to a new bank slot.
    fn register_recent_blockhash(&self, blockhash: &Hash, scheduler: &InstalledSchedulerRwLock) {
        // This is needed because recent_blockhash updates necessitate synchronizations for
        // consistent tx check_age handling.
        BankWithScheduler::wait_for_paused_scheduler(self, scheduler);

        // Only acquire the write lock for the blockhash queue on block boundaries because
        // readers can starve this write lock acquisition and ticks would be slowed down too
        // much if the write lock is acquired for each tick.
        let mut w_blockhash_queue = self.blockhash_queue.write().unwrap();

        #[cfg(feature = "dev-context-only-utils")]
        let blockhash_override = self
            .hash_overrides
            .lock()
            .unwrap()
            .get_blockhash_override(self.slot())
            .copied()
            .inspect(|blockhash_override| {
                if blockhash_override != blockhash {
                    info!(
                        "bank: slot: {}: overrode blockhash: {} with {}",
                        self.slot(),
                        blockhash,
                        blockhash_override
                    );
                }
            });
        #[cfg(feature = "dev-context-only-utils")]
        let blockhash = blockhash_override.as_ref().unwrap_or(blockhash);

        w_blockhash_queue.register_hash(blockhash, self.fee_rate_governor.lamports_per_signature);
        self.update_recent_blockhashes_locked(&w_blockhash_queue);
    }

    // gating this under #[cfg(feature = "dev-context-only-utils")] isn't easy due to
    // solana-program-test's usage...
    pub fn register_unique_recent_blockhash_for_test(&self) {
        self.register_recent_blockhash(
            &Hash::new_unique(),
            &BankWithScheduler::no_scheduler_available(),
        )
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn register_recent_blockhash_for_test(
        &self,
        blockhash: &Hash,
        lamports_per_signature: Option<u64>,
    ) {
        // Only acquire the write lock for the blockhash queue on block boundaries because
        // readers can starve this write lock acquisition and ticks would be slowed down too
        // much if the write lock is acquired for each tick.
        let mut w_blockhash_queue = self.blockhash_queue.write().unwrap();
        if let Some(lamports_per_signature) = lamports_per_signature {
            w_blockhash_queue.register_hash(blockhash, lamports_per_signature);
        } else {
            w_blockhash_queue
                .register_hash(blockhash, self.fee_rate_governor.lamports_per_signature);
        }
    }

    /// Tell the bank which Entry IDs exist on the ledger. This function assumes subsequent calls
    /// correspond to later entries, and will boot the oldest ones once its internal cache is full.
    /// Once boot, the bank will reject transactions using that `hash`.
    ///
    /// This is NOT thread safe because if tick height is updated by two different threads, the
    /// block boundary condition could be missed.
    pub fn register_tick(&self, hash: &Hash, scheduler: &InstalledSchedulerRwLock) {
        assert!(
            !self.freeze_started(),
            "register_tick() working on a bank that is already frozen or is undergoing freezing!"
        );

        if self.is_block_boundary(self.tick_height.load(Relaxed) + 1) {
            self.register_recent_blockhash(hash, scheduler);
        }

        // ReplayStage will start computing the accounts delta hash when it
        // detects the tick height has reached the boundary, so the system
        // needs to guarantee all account updates for the slot have been
        // committed before this tick height is incremented (like the blockhash
        // sysvar above)
        self.tick_height.fetch_add(1, Relaxed);
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn register_tick_for_test(&self, hash: &Hash) {
        self.register_tick(hash, &BankWithScheduler::no_scheduler_available())
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn register_default_tick_for_test(&self) {
        self.register_tick_for_test(&Hash::default())
    }

    pub fn is_complete(&self) -> bool {
        self.tick_height() == self.max_tick_height()
    }

    pub fn is_block_boundary(&self, tick_height: u64) -> bool {
        tick_height == self.max_tick_height
    }

    /// Get the max number of accounts that a transaction may lock in this block
    pub fn get_transaction_account_lock_limit(&self) -> usize {
        if let Some(transaction_account_lock_limit) = self.transaction_account_lock_limit {
            transaction_account_lock_limit
        } else if self
            .feature_set
            .is_active(&feature_set::increase_tx_account_lock_limit::id())
        {
            MAX_TX_ACCOUNT_LOCKS
        } else {
            64
        }
    }

    /// Prepare a transaction batch from a list of versioned transactions from
    /// an entry. Used for tests only.
    pub fn prepare_entry_batch(
        &self,
        txs: Vec<VersionedTransaction>,
    ) -> Result<TransactionBatch<RuntimeTransaction<SanitizedTransaction>>> {
        let sanitized_txs = txs
            .into_iter()
            .map(|tx| {
                RuntimeTransaction::try_create(
                    tx,
                    MessageHash::Compute,
                    None,
                    self,
                    self.get_reserved_account_keys(),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(TransactionBatch::new(
            self.try_lock_accounts(&sanitized_txs),
            self,
            OwnedOrBorrowed::Owned(sanitized_txs),
        ))
    }

    /// Attempt to take locks on the accounts in a transaction batch
    pub fn try_lock_accounts(&self, txs: &[impl TransactionWithMeta]) -> Vec<Result<()>> {
        self.try_lock_accounts_with_results(txs, txs.iter().map(|_| Ok(())))
    }

    /// Attempt to take locks on the accounts in a transaction batch, and their cost
    /// limited packing status and duplicate transaction conflict status
    pub fn try_lock_accounts_with_results(
        &self,
        txs: &[impl TransactionWithMeta],
        tx_results: impl Iterator<Item = Result<()>>,
    ) -> Vec<Result<()>> {
        let tx_account_lock_limit = self.get_transaction_account_lock_limit();
        let relax_intrabatch_account_locks = self
            .feature_set
            .is_active(&feature_set::relax_intrabatch_account_locks::id());

        // with simd83 enabled, we must fail transactions that duplicate a prior message hash
        // previously, conflicting account locks would fail such transactions as a side effect
        let mut batch_message_hashes = AHashSet::with_capacity(txs.len());
        let tx_results = tx_results
            .enumerate()
            .map(|(i, tx_result)| match tx_result {
                Ok(()) if relax_intrabatch_account_locks => {
                    // `HashSet::insert()` returns `true` when the value does *not* already exist
                    if batch_message_hashes.insert(txs[i].message_hash()) {
                        Ok(())
                    } else {
                        Err(TransactionError::AlreadyProcessed)
                    }
                }
                Ok(()) => Ok(()),
                Err(e) => Err(e),
            });

        self.rc.accounts.lock_accounts(
            txs.iter(),
            tx_results,
            tx_account_lock_limit,
            relax_intrabatch_account_locks,
        )
    }

    /// Prepare a locked transaction batch from a list of sanitized transactions.
    pub fn prepare_sanitized_batch<'a, 'b, Tx: TransactionWithMeta>(
        &'a self,
        txs: &'b [Tx],
    ) -> TransactionBatch<'a, 'b, Tx> {
        self.prepare_sanitized_batch_with_results(txs, txs.iter().map(|_| Ok(())))
    }

    /// Prepare a locked transaction batch from a list of sanitized transactions, and their cost
    /// limited packing status
    pub fn prepare_sanitized_batch_with_results<'a, 'b, Tx: TransactionWithMeta>(
        &'a self,
        transactions: &'b [Tx],
        transaction_results: impl Iterator<Item = Result<()>>,
    ) -> TransactionBatch<'a, 'b, Tx> {
        // this lock_results could be: Ok, AccountInUse, WouldExceedBlockMaxLimit or WouldExceedAccountMaxLimit
        TransactionBatch::new(
            self.try_lock_accounts_with_results(transactions, transaction_results),
            self,
            OwnedOrBorrowed::Borrowed(transactions),
        )
    }

    /// Prepare a transaction batch from a single transaction without locking accounts
    pub fn prepare_unlocked_batch_from_single_tx<'a, Tx: SVMMessage>(
        &'a self,
        transaction: &'a Tx,
    ) -> TransactionBatch<'a, 'a, Tx> {
        let tx_account_lock_limit = self.get_transaction_account_lock_limit();
        let lock_result = validate_account_locks(transaction.account_keys(), tx_account_lock_limit);
        let mut batch = TransactionBatch::new(
            vec![lock_result],
            self,
            OwnedOrBorrowed::Borrowed(slice::from_ref(transaction)),
        );
        batch.set_needs_unlock(false);
        batch
    }

    /// Prepare a transaction batch from a single transaction after locking accounts
    pub fn prepare_locked_batch_from_single_tx<'a, Tx: TransactionWithMeta>(
        &'a self,
        transaction: &'a Tx,
    ) -> TransactionBatch<'a, 'a, Tx> {
        self.prepare_sanitized_batch(slice::from_ref(transaction))
    }

    /// Run transactions against a frozen bank without committing the results
    pub fn simulate_transaction(
        &self,
        transaction: &impl TransactionWithMeta,
        enable_cpi_recording: bool,
    ) -> TransactionSimulationResult {
        assert!(self.is_frozen(), "simulation bank must be frozen");

        self.simulate_transaction_unchecked(transaction, enable_cpi_recording)
    }

    /// Run transactions against a bank without committing the results; does not check if the bank
    /// is frozen, enabling use in single-Bank test frameworks
    pub fn simulate_transaction_unchecked(
        &self,
        transaction: &impl TransactionWithMeta,
        enable_cpi_recording: bool,
    ) -> TransactionSimulationResult {
        let account_keys = transaction.account_keys();
        let number_of_accounts = account_keys.len();
        let account_overrides = self.get_account_overrides_for_simulation(&account_keys);
        let batch = self.prepare_unlocked_batch_from_single_tx(transaction);
        let mut timings = ExecuteTimings::default();

        let LoadAndExecuteTransactionsOutput {
            mut processing_results,
            ..
        } = self.load_and_execute_transactions(
            &batch,
            // After simulation, transactions will need to be forwarded to the leader
            // for processing. During forwarding, the transaction could expire if the
            // delay is not accounted for.
            MAX_PROCESSING_AGE - MAX_TRANSACTION_FORWARDING_DELAY,
            &mut timings,
            &mut TransactionErrorMetrics::default(),
            TransactionProcessingConfig {
                account_overrides: Some(&account_overrides),
                check_program_modification_slot: self.check_program_modification_slot,
                log_messages_bytes_limit: None,
                limit_to_load_programs: true,
                recording_config: ExecutionRecordingConfig {
                    enable_cpi_recording,
                    enable_log_recording: true,
                    enable_return_data_recording: true,
                    enable_transaction_balance_recording: false,
                },
            },
        );

        debug!("simulate_transaction: {:?}", timings);

        let processing_result = processing_results
            .pop()
            .unwrap_or(Err(TransactionError::InvalidProgramForExecution));
        let (
            post_simulation_accounts,
            result,
            logs,
            return_data,
            inner_instructions,
            units_consumed,
            loaded_accounts_data_size,
        ) = match processing_result {
            Ok(processed_tx) => match processed_tx {
                ProcessedTransaction::Executed(executed_tx) => {
                    let details = executed_tx.execution_details;
                    let post_simulation_accounts = executed_tx
                        .loaded_transaction
                        .accounts
                        .into_iter()
                        .take(number_of_accounts)
                        .collect::<Vec<_>>();
                    (
                        post_simulation_accounts,
                        details.status,
                        details.log_messages,
                        details.return_data,
                        details.inner_instructions,
                        details.executed_units,
                        executed_tx.loaded_transaction.loaded_accounts_data_size,
                    )
                }
                ProcessedTransaction::FeesOnly(fees_only_tx) => (
                    vec![],
                    Err(fees_only_tx.load_error),
                    None,
                    None,
                    None,
                    0,
                    fees_only_tx.rollback_accounts.data_size() as u32,
                ),
            },
            Err(error) => (vec![], Err(error), None, None, None, 0, 0),
        };
        let logs = logs.unwrap_or_default();

        TransactionSimulationResult {
            result,
            logs,
            post_simulation_accounts,
            units_consumed,
            loaded_accounts_data_size,
            return_data,
            inner_instructions,
        }
    }

    fn get_account_overrides_for_simulation(&self, account_keys: &AccountKeys) -> AccountOverrides {
        let mut account_overrides = AccountOverrides::default();
        let slot_history_id = sysvar::slot_history::id();
        if account_keys.iter().any(|pubkey| *pubkey == slot_history_id) {
            let current_account = self.get_account_with_fixed_root(&slot_history_id);
            let slot_history = current_account
                .as_ref()
                .map(|account| from_account::<SlotHistory, _>(account).unwrap())
                .unwrap_or_default();
            if slot_history.check(self.slot()) == Check::Found {
                let ancestors = Ancestors::from(self.proper_ancestors().collect::<Vec<_>>());
                if let Some((account, _)) =
                    self.load_slow_with_fixed_root(&ancestors, &slot_history_id)
                {
                    account_overrides.set_slot_history(Some(account));
                }
            }
        }
        account_overrides
    }

    pub fn unlock_accounts<'a, Tx: SVMMessage + 'a>(
        &self,
        txs_and_results: impl Iterator<Item = (&'a Tx, &'a Result<()>)> + Clone,
    ) {
        self.rc.accounts.unlock_accounts(txs_and_results)
    }

    pub fn remove_unrooted_slots(&self, slots: &[(Slot, BankId)]) {
        self.rc.accounts.accounts_db.remove_unrooted_slots(slots)
    }

    pub fn get_hash_age(&self, hash: &Hash) -> Option<u64> {
        self.blockhash_queue.read().unwrap().get_hash_age(hash)
    }

    pub fn is_hash_valid_for_age(&self, hash: &Hash, max_age: usize) -> bool {
        self.blockhash_queue
            .read()
            .unwrap()
            .is_hash_valid_for_age(hash, max_age)
    }

    pub fn collect_balances(
        &self,
        batch: &TransactionBatch<impl SVMMessage>,
    ) -> TransactionBalances {
        let mut balances: TransactionBalances = vec![];
        for transaction in batch.sanitized_transactions() {
            let mut transaction_balances: Vec<u64> = vec![];
            for account_key in transaction.account_keys().iter() {
                transaction_balances.push(self.get_balance(account_key));
            }
            balances.push(transaction_balances);
        }
        balances
    }

    pub fn load_and_execute_transactions(
        &self,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        max_age: usize,
        timings: &mut ExecuteTimings,
        error_counters: &mut TransactionErrorMetrics,
        processing_config: TransactionProcessingConfig,
    ) -> LoadAndExecuteTransactionsOutput {
        let sanitized_txs = batch.sanitized_transactions();

        let (check_results, check_us) = measure_us!(self.check_transactions(
            sanitized_txs,
            batch.lock_results(),
            max_age,
            error_counters,
        ));
        timings.saturating_add_in_place(ExecuteTimingType::CheckUs, check_us);

        let (blockhash, blockhash_lamports_per_signature) =
            self.last_blockhash_and_lamports_per_signature();
        let rent_collector_with_metrics =
            RentCollectorWithMetrics::new(self.rent_collector.clone());
        let processing_environment = TransactionProcessingEnvironment {
            blockhash,
            blockhash_lamports_per_signature,
            epoch_total_stake: self.get_current_epoch_total_stake(),
            feature_set: self.feature_set.runtime_features(),
            rent_collector: Some(&rent_collector_with_metrics),
        };

        let sanitized_output = self
            .transaction_processor
            .load_and_execute_sanitized_transactions(
                self,
                sanitized_txs,
                check_results,
                &processing_environment,
                &processing_config,
            );

        // Accumulate the errors returned by the batch processor.
        error_counters.accumulate(&sanitized_output.error_metrics);

        // Accumulate the transaction batch execution timings.
        timings.accumulate(&sanitized_output.execute_timings);

        let ((), collect_logs_us) =
            measure_us!(self.collect_logs(sanitized_txs, &sanitized_output.processing_results));
        timings.saturating_add_in_place(ExecuteTimingType::CollectLogsUs, collect_logs_us);

        let mut processed_counts = ProcessedTransactionCounts::default();
        let err_count = &mut error_counters.total;

        for (processing_result, tx) in sanitized_output
            .processing_results
            .iter()
            .zip(sanitized_txs)
        {
            if let Some(debug_keys) = &self.transaction_debug_keys {
                for key in tx.account_keys().iter() {
                    if debug_keys.contains(key) {
                        let result = processing_result.flattened_result();
                        info!("slot: {} result: {:?} tx: {:?}", self.slot, result, tx);
                        break;
                    }
                }
            }

            if processing_result.was_processed() {
                // Signature count must be accumulated only if the transaction
                // is processed, otherwise a mismatched count between banking
                // and replay could occur
                processed_counts.signature_count +=
                    tx.signature_details().num_transaction_signatures();
                processed_counts.processed_transactions_count += 1;

                if !tx.is_simple_vote_transaction() {
                    processed_counts.processed_non_vote_transactions_count += 1;
                }
            }

            match processing_result.flattened_result() {
                Ok(()) => {
                    processed_counts.processed_with_successful_result_count += 1;
                }
                Err(err) => {
                    if err_count.0 == 0 {
                        debug!("tx error: {:?} {:?}", err, tx);
                    }
                    *err_count += 1;
                }
            }
        }

        LoadAndExecuteTransactionsOutput {
            processing_results: sanitized_output.processing_results,
            processed_counts,
            balance_collector: sanitized_output.balance_collector,
        }
    }

    fn collect_logs(
        &self,
        transactions: &[impl TransactionWithMeta],
        processing_results: &[TransactionProcessingResult],
    ) {
        let transaction_log_collector_config =
            self.transaction_log_collector_config.read().unwrap();
        if transaction_log_collector_config.filter == TransactionLogCollectorFilter::None {
            return;
        }

        let collected_logs: Vec<_> = processing_results
            .iter()
            .zip(transactions)
            .filter_map(|(processing_result, transaction)| {
                // Skip log collection for unprocessed transactions
                let processed_tx = processing_result.processed_transaction()?;
                // Skip log collection for unexecuted transactions
                let execution_details = processed_tx.execution_details()?;
                Self::collect_transaction_logs(
                    &transaction_log_collector_config,
                    transaction,
                    execution_details,
                )
            })
            .collect();

        if !collected_logs.is_empty() {
            let mut transaction_log_collector = self.transaction_log_collector.write().unwrap();
            for (log, filtered_mentioned_addresses) in collected_logs {
                let transaction_log_index = transaction_log_collector.logs.len();
                transaction_log_collector.logs.push(log);
                for key in filtered_mentioned_addresses.into_iter() {
                    transaction_log_collector
                        .mentioned_address_map
                        .entry(key)
                        .or_default()
                        .push(transaction_log_index);
                }
            }
        }
    }

    fn collect_transaction_logs(
        transaction_log_collector_config: &TransactionLogCollectorConfig,
        transaction: &impl TransactionWithMeta,
        execution_details: &TransactionExecutionDetails,
    ) -> Option<(TransactionLogInfo, Vec<Pubkey>)> {
        // Skip log collection if no log messages were recorded
        let log_messages = execution_details.log_messages.as_ref()?;

        let mut filtered_mentioned_addresses = Vec::new();
        if !transaction_log_collector_config
            .mentioned_addresses
            .is_empty()
        {
            for key in transaction.account_keys().iter() {
                if transaction_log_collector_config
                    .mentioned_addresses
                    .contains(key)
                {
                    filtered_mentioned_addresses.push(*key);
                }
            }
        }

        let is_vote = transaction.is_simple_vote_transaction();
        let store = match transaction_log_collector_config.filter {
            TransactionLogCollectorFilter::All => {
                !is_vote || !filtered_mentioned_addresses.is_empty()
            }
            TransactionLogCollectorFilter::AllWithVotes => true,
            TransactionLogCollectorFilter::None => false,
            TransactionLogCollectorFilter::OnlyMentionedAddresses => {
                !filtered_mentioned_addresses.is_empty()
            }
        };

        if store {
            Some((
                TransactionLogInfo {
                    signature: *transaction.signature(),
                    result: execution_details.status.clone(),
                    is_vote,
                    log_messages: log_messages.clone(),
                },
                filtered_mentioned_addresses,
            ))
        } else {
            None
        }
    }

    /// Load the accounts data size, in bytes
    pub fn load_accounts_data_size(&self) -> u64 {
        self.accounts_data_size_initial
            .saturating_add_signed(self.load_accounts_data_size_delta())
    }

    /// Load the change in accounts data size in this Bank, in bytes
    pub fn load_accounts_data_size_delta(&self) -> i64 {
        let delta_on_chain = self.load_accounts_data_size_delta_on_chain();
        let delta_off_chain = self.load_accounts_data_size_delta_off_chain();
        delta_on_chain.saturating_add(delta_off_chain)
    }

    /// Load the change in accounts data size in this Bank, in bytes, from on-chain events
    /// i.e. transactions
    pub fn load_accounts_data_size_delta_on_chain(&self) -> i64 {
        self.accounts_data_size_delta_on_chain.load(Acquire)
    }

    /// Load the change in accounts data size in this Bank, in bytes, from off-chain events
    /// i.e. rent collection
    pub fn load_accounts_data_size_delta_off_chain(&self) -> i64 {
        self.accounts_data_size_delta_off_chain.load(Acquire)
    }

    /// Update the accounts data size delta from on-chain events by adding `amount`.
    /// The arithmetic saturates.
    fn update_accounts_data_size_delta_on_chain(&self, amount: i64) {
        if amount == 0 {
            return;
        }

        self.accounts_data_size_delta_on_chain
            .fetch_update(AcqRel, Acquire, |accounts_data_size_delta_on_chain| {
                Some(accounts_data_size_delta_on_chain.saturating_add(amount))
            })
            // SAFETY: unwrap() is safe since our update fn always returns `Some`
            .unwrap();
    }

    /// Update the accounts data size delta from off-chain events by adding `amount`.
    /// The arithmetic saturates.
    fn update_accounts_data_size_delta_off_chain(&self, amount: i64) {
        if amount == 0 {
            return;
        }

        self.accounts_data_size_delta_off_chain
            .fetch_update(AcqRel, Acquire, |accounts_data_size_delta_off_chain| {
                Some(accounts_data_size_delta_off_chain.saturating_add(amount))
            })
            // SAFETY: unwrap() is safe since our update fn always returns `Some`
            .unwrap();
    }

    /// Calculate the data size delta and update the off-chain accounts data size delta
    fn calculate_and_update_accounts_data_size_delta_off_chain(
        &self,
        old_data_size: usize,
        new_data_size: usize,
    ) {
        let data_size_delta = calculate_data_size_delta(old_data_size, new_data_size);
        self.update_accounts_data_size_delta_off_chain(data_size_delta);
    }

    fn filter_program_errors_and_collect_fee_details(
        &self,
        processing_results: &[TransactionProcessingResult],
    ) {
        let mut accumulated_fee_details = FeeDetails::default();

        processing_results.iter().for_each(|processing_result| {
            if let Ok(processed_tx) = processing_result {
                accumulated_fee_details.accumulate(&processed_tx.fee_details());
            }
        });

        self.collector_fee_details
            .write()
            .unwrap()
            .accumulate(&accumulated_fee_details);
    }

    fn update_bank_hash_stats<'a>(&self, accounts: &impl StorableAccounts<'a>) {
        let mut stats = BankHashStats::default();
        (0..accounts.len()).for_each(|i| {
            accounts.account(i, |account| {
                stats.update(&account);
            })
        });
        self.bank_hash_stats.accumulate(&stats);
    }

    pub fn commit_transactions(
        &self,
        sanitized_txs: &[impl TransactionWithMeta],
        processing_results: Vec<TransactionProcessingResult>,
        processed_counts: &ProcessedTransactionCounts,
        timings: &mut ExecuteTimings,
    ) -> Vec<TransactionCommitResult> {
        assert!(
            !self.freeze_started(),
            "commit_transactions() working on a bank that is already frozen or is undergoing freezing!"
        );

        let ProcessedTransactionCounts {
            processed_transactions_count,
            processed_non_vote_transactions_count,
            processed_with_successful_result_count,
            signature_count,
        } = *processed_counts;

        self.increment_transaction_count(processed_transactions_count);
        self.increment_non_vote_transaction_count_since_restart(
            processed_non_vote_transactions_count,
        );
        self.increment_signature_count(signature_count);

        let processed_with_failure_result_count =
            processed_transactions_count.saturating_sub(processed_with_successful_result_count);
        self.transaction_error_count
            .fetch_add(processed_with_failure_result_count, Relaxed);

        if processed_transactions_count > 0 {
            self.is_delta.store(true, Relaxed);
            self.transaction_entries_count.fetch_add(1, Relaxed);
            self.transactions_per_entry_max
                .fetch_max(processed_transactions_count, Relaxed);
        }

        let ((), store_accounts_us) = measure_us!({
            // If geyser is present, we must collect `SanitizedTransaction`
            // references in order to comply with that interface - until it
            // is changed.
            let maybe_transaction_refs = self
                .accounts()
                .accounts_db
                .has_accounts_update_notifier()
                .then(|| {
                    sanitized_txs
                        .iter()
                        .map(|tx| tx.as_sanitized_transaction())
                        .collect::<Vec<_>>()
                });

            let (accounts_to_store, transactions) = collect_accounts_to_store(
                sanitized_txs,
                &maybe_transaction_refs,
                &processing_results,
            );

            let to_store = (self.slot(), accounts_to_store.as_slice());
            self.update_bank_hash_stats(&to_store);
            self.rc
                .accounts
                .store_cached(to_store, transactions.as_deref());
        });

        // Cached vote and stake accounts are synchronized with accounts-db
        // after each transaction.
        let ((), update_stakes_cache_us) =
            measure_us!(self.update_stakes_cache(sanitized_txs, &processing_results));

        let ((), update_executors_us) = measure_us!({
            let mut cache = None;
            for processing_result in &processing_results {
                if let Some(ProcessedTransaction::Executed(executed_tx)) =
                    processing_result.processed_transaction()
                {
                    let programs_modified_by_tx = &executed_tx.programs_modified_by_tx;
                    if executed_tx.was_successful() && !programs_modified_by_tx.is_empty() {
                        cache
                            .get_or_insert_with(|| {
                                self.transaction_processor.program_cache.write().unwrap()
                            })
                            .merge(programs_modified_by_tx);
                    }
                }
            }
        });

        let accounts_data_len_delta = processing_results
            .iter()
            .filter_map(|processing_result| processing_result.processed_transaction())
            .filter_map(|processed_tx| processed_tx.execution_details())
            .filter_map(|details| {
                details
                    .status
                    .is_ok()
                    .then_some(details.accounts_data_len_delta)
            })
            .sum();
        self.update_accounts_data_size_delta_on_chain(accounts_data_len_delta);

        let ((), update_transaction_statuses_us) =
            measure_us!(self.update_transaction_statuses(sanitized_txs, &processing_results));

        self.filter_program_errors_and_collect_fee_details(&processing_results);

        timings.saturating_add_in_place(ExecuteTimingType::StoreUs, store_accounts_us);
        timings.saturating_add_in_place(
            ExecuteTimingType::UpdateStakesCacheUs,
            update_stakes_cache_us,
        );
        timings.saturating_add_in_place(ExecuteTimingType::UpdateExecutorsUs, update_executors_us);
        timings.saturating_add_in_place(
            ExecuteTimingType::UpdateTransactionStatuses,
            update_transaction_statuses_us,
        );

        Self::create_commit_results(processing_results)
    }

    fn create_commit_results(
        processing_results: Vec<TransactionProcessingResult>,
    ) -> Vec<TransactionCommitResult> {
        processing_results
            .into_iter()
            .map(|processing_result| {
                let processing_result = processing_result?;
                let executed_units = processing_result.executed_units();
                let loaded_accounts_data_size = processing_result.loaded_accounts_data_size();

                match processing_result {
                    ProcessedTransaction::Executed(executed_tx) => {
                        let execution_details = executed_tx.execution_details;
                        let LoadedTransaction {
                            accounts: loaded_accounts,
                            fee_details,
                            ..
                        } = executed_tx.loaded_transaction;

                        Ok(CommittedTransaction {
                            status: execution_details.status,
                            log_messages: execution_details.log_messages,
                            inner_instructions: execution_details.inner_instructions,
                            return_data: execution_details.return_data,
                            executed_units,
                            fee_details,
                            loaded_account_stats: TransactionLoadedAccountsStats {
                                loaded_accounts_count: loaded_accounts.len(),
                                loaded_accounts_data_size,
                            },
                        })
                    }
                    ProcessedTransaction::FeesOnly(fees_only_tx) => Ok(CommittedTransaction {
                        status: Err(fees_only_tx.load_error),
                        log_messages: None,
                        inner_instructions: None,
                        return_data: None,
                        executed_units,
                        fee_details: fees_only_tx.fee_details,
                        loaded_account_stats: TransactionLoadedAccountsStats {
                            loaded_accounts_count: fees_only_tx.rollback_accounts.count(),
                            loaded_accounts_data_size,
                        },
                    }),
                }
            })
            .collect()
    }

    fn run_incinerator(&self) {
        if let Some((account, _)) =
            self.get_account_modified_since_parent_with_fixed_root(&incinerator::id())
        {
            self.capitalization.fetch_sub(account.lamports(), Relaxed);
            self.store_account(&incinerator::id(), &AccountSharedData::default());
        }
    }

    /// Returns the accounts, sorted by pubkey, that were part of accounts lt hash calculation
    /// This is used when writing a bank hash details file.
    pub(crate) fn get_accounts_for_bank_hash_details(&self) -> Vec<(Pubkey, AccountSharedData)> {
        let mut accounts = self
            .rc
            .accounts
            .accounts_db
            .get_pubkey_account_for_slot(self.slot());
        // Sort the accounts by pubkey to make diff deterministic.
        accounts.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        accounts
    }

    pub fn cluster_type(&self) -> ClusterType {
        // unwrap is safe; self.cluster_type is ensured to be Some() always...
        // we only using Option here for ABI compatibility...
        self.cluster_type.unwrap()
    }

    /// Process a batch of transactions.
    #[must_use]
    pub fn load_execute_and_commit_transactions(
        &self,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        max_age: usize,
        recording_config: ExecutionRecordingConfig,
        timings: &mut ExecuteTimings,
        log_messages_bytes_limit: Option<usize>,
    ) -> (Vec<TransactionCommitResult>, Option<BalanceCollector>) {
        self.do_load_execute_and_commit_transactions_with_pre_commit_callback(
            batch,
            max_age,
            recording_config,
            timings,
            log_messages_bytes_limit,
            None::<fn(&mut _, &_) -> _>,
        )
        .unwrap()
    }

    pub fn load_execute_and_commit_transactions_with_pre_commit_callback<'a>(
        &'a self,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        max_age: usize,
        recording_config: ExecutionRecordingConfig,
        timings: &mut ExecuteTimings,
        log_messages_bytes_limit: Option<usize>,
        pre_commit_callback: impl FnOnce(
            &mut ExecuteTimings,
            &[TransactionProcessingResult],
        ) -> PreCommitResult<'a>,
    ) -> Result<(Vec<TransactionCommitResult>, Option<BalanceCollector>)> {
        self.do_load_execute_and_commit_transactions_with_pre_commit_callback(
            batch,
            max_age,
            recording_config,
            timings,
            log_messages_bytes_limit,
            Some(pre_commit_callback),
        )
    }

    fn do_load_execute_and_commit_transactions_with_pre_commit_callback<'a>(
        &'a self,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        max_age: usize,
        recording_config: ExecutionRecordingConfig,
        timings: &mut ExecuteTimings,
        log_messages_bytes_limit: Option<usize>,
        pre_commit_callback: Option<
            impl FnOnce(&mut ExecuteTimings, &[TransactionProcessingResult]) -> PreCommitResult<'a>,
        >,
    ) -> Result<(Vec<TransactionCommitResult>, Option<BalanceCollector>)> {
        let LoadAndExecuteTransactionsOutput {
            processing_results,
            processed_counts,
            balance_collector,
        } = self.load_and_execute_transactions(
            batch,
            max_age,
            timings,
            &mut TransactionErrorMetrics::default(),
            TransactionProcessingConfig {
                account_overrides: None,
                check_program_modification_slot: self.check_program_modification_slot,
                log_messages_bytes_limit,
                limit_to_load_programs: false,
                recording_config,
            },
        );

        // pre_commit_callback could initiate an atomic operation (i.e. poh recording with block
        // producing unified scheduler). in that case, it returns Some(freeze_lock), which should
        // unlocked only after calling commit_transactions() immediately after calling the
        // callback.
        let freeze_lock = if let Some(pre_commit_callback) = pre_commit_callback {
            pre_commit_callback(timings, &processing_results)?
        } else {
            None
        };
        let commit_results = self.commit_transactions(
            batch.sanitized_transactions(),
            processing_results,
            &processed_counts,
            timings,
        );
        drop(freeze_lock);
        Ok((commit_results, balance_collector))
    }

    /// Process a Transaction. This is used for unit tests and simply calls the vector
    /// Bank::process_transactions method.
    pub fn process_transaction(&self, tx: &Transaction) -> Result<()> {
        self.try_process_transactions(std::iter::once(tx))?[0].clone()
    }

    /// Process a Transaction and store metadata. This is used for tests and the banks services. It
    /// replicates the vector Bank::process_transaction method with metadata recording enabled.
    pub fn process_transaction_with_metadata(
        &self,
        tx: impl Into<VersionedTransaction>,
    ) -> Result<CommittedTransaction> {
        let txs = vec![tx.into()];
        let batch = self.prepare_entry_batch(txs)?;

        let (mut commit_results, ..) = self.load_execute_and_commit_transactions(
            &batch,
            MAX_PROCESSING_AGE,
            ExecutionRecordingConfig {
                enable_cpi_recording: false,
                enable_log_recording: true,
                enable_return_data_recording: true,
                enable_transaction_balance_recording: false,
            },
            &mut ExecuteTimings::default(),
            Some(1000 * 1000),
        );

        commit_results.remove(0)
    }

    /// Process multiple transaction in a single batch. This is used for benches and unit tests.
    /// Short circuits if any of the transactions do not pass sanitization checks.
    pub fn try_process_transactions<'a>(
        &self,
        txs: impl Iterator<Item = &'a Transaction>,
    ) -> Result<Vec<Result<()>>> {
        let txs = txs
            .map(|tx| VersionedTransaction::from(tx.clone()))
            .collect();
        self.try_process_entry_transactions(txs)
    }

    /// Process multiple transaction in a single batch. This is used for benches and unit tests.
    /// Short circuits if any of the transactions do not pass sanitization checks.
    pub fn try_process_entry_transactions(
        &self,
        txs: Vec<VersionedTransaction>,
    ) -> Result<Vec<Result<()>>> {
        let batch = self.prepare_entry_batch(txs)?;
        Ok(self.process_transaction_batch(&batch))
    }

    #[must_use]
    fn process_transaction_batch(
        &self,
        batch: &TransactionBatch<impl TransactionWithMeta>,
    ) -> Vec<Result<()>> {
        self.load_execute_and_commit_transactions(
            batch,
            MAX_PROCESSING_AGE,
            ExecutionRecordingConfig::new_single_setting(false),
            &mut ExecuteTimings::default(),
            None,
        )
        .0
        .into_iter()
        .map(|commit_result| commit_result.and_then(|committed_tx| committed_tx.status))
        .collect()
    }

    /// Create, sign, and process a Transaction from `keypair` to `to` of
    /// `n` lamports where `blockhash` is the last Entry ID observed by the client.
    pub fn transfer(&self, n: u64, keypair: &Keypair, to: &Pubkey) -> Result<Signature> {
        let blockhash = self.last_blockhash();
        let tx = system_transaction::transfer(keypair, to, n, blockhash);
        let signature = tx.signatures[0];
        self.process_transaction(&tx).map(|_| signature)
    }

    pub fn read_balance(account: &AccountSharedData) -> u64 {
        account.lamports()
    }
    /// Each program would need to be able to introspect its own state
    /// this is hard-coded to the Budget language
    pub fn get_balance(&self, pubkey: &Pubkey) -> u64 {
        self.get_account(pubkey)
            .map(|x| Self::read_balance(&x))
            .unwrap_or(0)
    }

    /// Compute all the parents of the bank in order
    pub fn parents(&self) -> Vec<Arc<Bank>> {
        let mut parents = vec![];
        let mut bank = self.parent();
        while let Some(parent) = bank {
            parents.push(parent.clone());
            bank = parent.parent();
        }
        parents
    }

    /// Compute all the parents of the bank including this bank itself
    pub fn parents_inclusive(self: Arc<Self>) -> Vec<Arc<Bank>> {
        let mut parents = self.parents();
        parents.insert(0, self);
        parents
    }

    /// fn store the single `account` with `pubkey`.
    /// Uses `store_accounts`, which works on a vector of accounts.
    pub fn store_account(&self, pubkey: &Pubkey, account: &AccountSharedData) {
        self.store_accounts((self.slot(), &[(pubkey, account)][..]))
    }

    pub fn store_accounts<'a>(&self, accounts: impl StorableAccounts<'a>) {
        assert!(!self.freeze_started());
        let mut m = Measure::start("stakes_cache.check_and_store");
        let new_warmup_cooldown_rate_epoch = self.new_warmup_cooldown_rate_epoch();

        (0..accounts.len()).for_each(|i| {
            accounts.account(i, |account| {
                self.stakes_cache.check_and_store(
                    account.pubkey(),
                    &account,
                    new_warmup_cooldown_rate_epoch,
                )
            })
        });
        self.update_bank_hash_stats(&accounts);
        self.rc.accounts.store_accounts_cached(accounts);
        m.stop();
        self.rc
            .accounts
            .accounts_db
            .stats
            .stakes_cache_check_and_store_us
            .fetch_add(m.as_us(), Relaxed);
    }

    pub fn force_flush_accounts_cache(&self) {
        self.rc
            .accounts
            .accounts_db
            .flush_accounts_cache(true, Some(self.slot()))
    }

    pub fn flush_accounts_cache_if_needed(&self) {
        self.rc
            .accounts
            .accounts_db
            .flush_accounts_cache(false, Some(self.slot()))
    }

    /// Technically this issues (or even burns!) new lamports,
    /// so be extra careful for its usage
    fn store_account_and_update_capitalization(
        &self,
        pubkey: &Pubkey,
        new_account: &AccountSharedData,
    ) {
        let old_account_data_size =
            if let Some(old_account) = self.get_account_with_fixed_root_no_cache(pubkey) {
                match new_account.lamports().cmp(&old_account.lamports()) {
                    std::cmp::Ordering::Greater => {
                        let increased = new_account.lamports() - old_account.lamports();
                        trace!(
                            "store_account_and_update_capitalization: increased: {} {}",
                            pubkey,
                            increased
                        );
                        self.capitalization.fetch_add(increased, Relaxed);
                    }
                    std::cmp::Ordering::Less => {
                        let decreased = old_account.lamports() - new_account.lamports();
                        trace!(
                            "store_account_and_update_capitalization: decreased: {} {}",
                            pubkey,
                            decreased
                        );
                        self.capitalization.fetch_sub(decreased, Relaxed);
                    }
                    std::cmp::Ordering::Equal => {}
                }
                old_account.data().len()
            } else {
                trace!(
                    "store_account_and_update_capitalization: created: {} {}",
                    pubkey,
                    new_account.lamports()
                );
                self.capitalization
                    .fetch_add(new_account.lamports(), Relaxed);
                0
            };

        self.store_account(pubkey, new_account);
        self.calculate_and_update_accounts_data_size_delta_off_chain(
            old_account_data_size,
            new_account.data().len(),
        );
    }

    pub fn accounts(&self) -> Arc<Accounts> {
        self.rc.accounts.clone()
    }

    fn apply_simd_0306_cost_tracker_changes(&mut self) {
        let mut cost_tracker = self.write_cost_tracker().unwrap();
        let block_cost_limit = cost_tracker.get_block_limit();
        let vote_cost_limit = cost_tracker.get_vote_limit();
        // SIMD-0306 makes account cost limit 40% of the block cost limit.
        let account_cost_limit = block_cost_limit.saturating_mul(40).saturating_div(100);
        cost_tracker.set_limits(account_cost_limit, block_cost_limit, vote_cost_limit);
    }

    fn finish_init(
        &mut self,
        genesis_config: &GenesisConfig,
        additional_builtins: Option<&[BuiltinPrototype]>,
        debug_do_not_add_builtins: bool,
    ) {
        if let Some(compute_budget) = self.compute_budget {
            self.transaction_processor
                .set_execution_cost(compute_budget.to_cost());
        }

        self.rewards_pool_pubkeys =
            Arc::new(genesis_config.rewards_pools.keys().cloned().collect());

        self.apply_feature_activations(
            ApplyFeatureActivationsCaller::FinishInit,
            debug_do_not_add_builtins,
        );

        // Cost-Tracker is not serialized in snapshot or any configs.
        // We must apply previously activated features related to limits here
        // so that the initial bank state is consistent with the feature set.
        // Cost-tracker limits are propagated through children banks.
        if self
            .feature_set
            .is_active(&feature_set::raise_block_limits_to_100m::id())
        {
            let block_cost_limit = simd_0286_block_limits();
            let mut cost_tracker = self.write_cost_tracker().unwrap();
            let account_cost_limit = cost_tracker.get_account_limit();
            let vote_cost_limit = cost_tracker.get_vote_limit();
            cost_tracker.set_limits(account_cost_limit, block_cost_limit, vote_cost_limit);
        }

        if self
            .feature_set
            .is_active(&feature_set::raise_account_cu_limit::id())
        {
            self.apply_simd_0306_cost_tracker_changes();
        }

        if !debug_do_not_add_builtins {
            for builtin in BUILTINS
                .iter()
                .chain(additional_builtins.unwrap_or(&[]).iter())
            {
                // The builtin should be added if it has no enable feature ID
                // and it has not been migrated to Core BPF.
                //
                // If a program was previously migrated to Core BPF, accountsDB
                // from snapshot should contain the BPF program accounts.
                let builtin_is_bpf = |program_id: &Pubkey| {
                    self.get_account(program_id)
                        .map(|a| a.owner() == &bpf_loader_upgradeable::id())
                        .unwrap_or(false)
                };
                if builtin.enable_feature_id.is_none() && !builtin_is_bpf(&builtin.program_id) {
                    self.transaction_processor.add_builtin(
                        self,
                        builtin.program_id,
                        builtin.name,
                        ProgramCacheEntry::new_builtin(0, builtin.name.len(), builtin.entrypoint),
                    );
                }
            }
            for precompile in get_precompiles() {
                if precompile.feature.is_none() {
                    self.add_precompile(&precompile.program_id);
                }
            }
        }

        self.transaction_processor
            .configure_program_runtime_environments(
                Some(Arc::new(
                    create_program_runtime_environment_v1(
                        &self.feature_set.runtime_features(),
                        &self.compute_budget().unwrap_or_default().to_budget(),
                        false, /* deployment */
                        false, /* debugging_features */
                    )
                    .unwrap(),
                )),
                Some(Arc::new(create_program_runtime_environment_v2(
                    &self.compute_budget().unwrap_or_default().to_budget(),
                    false, /* debugging_features */
                ))),
            );
    }

    pub fn set_inflation(&self, inflation: Inflation) {
        *self.inflation.write().unwrap() = inflation;
    }

    /// Get a snapshot of the current set of hard forks
    pub fn hard_forks(&self) -> HardForks {
        self.hard_forks.read().unwrap().clone()
    }

    pub fn register_hard_fork(&self, new_hard_fork_slot: Slot) {
        let bank_slot = self.slot();

        let lock = self.freeze_lock();
        let bank_frozen = *lock != Hash::default();
        if new_hard_fork_slot < bank_slot {
            warn!(
                "Hard fork at slot {new_hard_fork_slot} ignored, the hard fork is older \
                than the bank at slot {bank_slot} that attempted to register it."
            );
        } else if (new_hard_fork_slot == bank_slot) && bank_frozen {
            warn!(
                "Hard fork at slot {new_hard_fork_slot} ignored, the hard fork is the same \
                slot as the bank at slot {bank_slot} that attempted to register it, but that \
                bank is already frozen."
            );
        } else {
            self.hard_forks
                .write()
                .unwrap()
                .register(new_hard_fork_slot);
        }
    }

    pub fn get_account_with_fixed_root_no_cache(
        &self,
        pubkey: &Pubkey,
    ) -> Option<AccountSharedData> {
        self.load_account_with(pubkey, |_| false)
            .map(|(acc, _slot)| acc)
    }

    fn load_account_with(
        &self,
        pubkey: &Pubkey,
        callback: impl for<'local> Fn(&'local AccountSharedData) -> bool,
    ) -> Option<(AccountSharedData, Slot)> {
        self.rc
            .accounts
            .accounts_db
            .load_account_with(&self.ancestors, pubkey, callback)
    }

    // Hi! leaky abstraction here....
    // try to use get_account_with_fixed_root() if it's called ONLY from on-chain runtime account
    // processing. That alternative fn provides more safety.
    pub fn get_account(&self, pubkey: &Pubkey) -> Option<AccountSharedData> {
        self.get_account_modified_slot(pubkey)
            .map(|(acc, _slot)| acc)
    }

    // Hi! leaky abstraction here....
    // use this over get_account() if it's called ONLY from on-chain runtime account
    // processing (i.e. from in-band replay/banking stage; that ensures root is *fixed* while
    // running).
    // pro: safer assertion can be enabled inside AccountsDb
    // con: panics!() if called from off-chain processing
    pub fn get_account_with_fixed_root(&self, pubkey: &Pubkey) -> Option<AccountSharedData> {
        self.get_account_modified_slot_with_fixed_root(pubkey)
            .map(|(acc, _slot)| acc)
    }

    // See note above get_account_with_fixed_root() about when to prefer this function
    pub fn get_account_modified_slot_with_fixed_root(
        &self,
        pubkey: &Pubkey,
    ) -> Option<(AccountSharedData, Slot)> {
        self.load_slow_with_fixed_root(&self.ancestors, pubkey)
    }

    pub fn get_account_modified_slot(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        self.load_slow(&self.ancestors, pubkey)
    }

    fn load_slow(
        &self,
        ancestors: &Ancestors,
        pubkey: &Pubkey,
    ) -> Option<(AccountSharedData, Slot)> {
        // get_account (= primary this fn caller) may be called from on-chain Bank code even if we
        // try hard to use get_account_with_fixed_root for that purpose...
        // so pass safer LoadHint:Unspecified here as a fallback
        self.rc.accounts.load_without_fixed_root(ancestors, pubkey)
    }

    fn load_slow_with_fixed_root(
        &self,
        ancestors: &Ancestors,
        pubkey: &Pubkey,
    ) -> Option<(AccountSharedData, Slot)> {
        self.rc.accounts.load_with_fixed_root(ancestors, pubkey)
    }

    pub fn get_program_accounts(
        &self,
        program_id: &Pubkey,
        config: &ScanConfig,
    ) -> ScanResult<Vec<TransactionAccount>> {
        self.rc
            .accounts
            .load_by_program(&self.ancestors, self.bank_id, program_id, config)
    }

    pub fn get_filtered_program_accounts<F: Fn(&AccountSharedData) -> bool>(
        &self,
        program_id: &Pubkey,
        filter: F,
        config: &ScanConfig,
    ) -> ScanResult<Vec<TransactionAccount>> {
        self.rc.accounts.load_by_program_with_filter(
            &self.ancestors,
            self.bank_id,
            program_id,
            filter,
            config,
        )
    }

    pub fn get_filtered_indexed_accounts<F: Fn(&AccountSharedData) -> bool>(
        &self,
        index_key: &IndexKey,
        filter: F,
        config: &ScanConfig,
        byte_limit_for_scan: Option<usize>,
    ) -> ScanResult<Vec<TransactionAccount>> {
        self.rc.accounts.load_by_index_key_with_filter(
            &self.ancestors,
            self.bank_id,
            index_key,
            filter,
            config,
            byte_limit_for_scan,
        )
    }

    pub fn account_indexes_include_key(&self, key: &Pubkey) -> bool {
        self.rc.accounts.account_indexes_include_key(key)
    }

    /// Returns all the accounts this bank can load
    pub fn get_all_accounts(&self, sort_results: bool) -> ScanResult<Vec<PubkeyAccountSlot>> {
        self.rc
            .accounts
            .load_all(&self.ancestors, self.bank_id, sort_results)
    }

    // Scans all the accounts this bank can load, applying `scan_func`
    pub fn scan_all_accounts<F>(&self, scan_func: F, sort_results: bool) -> ScanResult<()>
    where
        F: FnMut(Option<(&Pubkey, AccountSharedData, Slot)>),
    {
        self.rc
            .accounts
            .scan_all(&self.ancestors, self.bank_id, scan_func, sort_results)
    }

    pub fn get_program_accounts_modified_since_parent(
        &self,
        program_id: &Pubkey,
    ) -> Vec<TransactionAccount> {
        self.rc
            .accounts
            .load_by_program_slot(self.slot(), Some(program_id))
    }

    pub fn get_transaction_logs(
        &self,
        address: Option<&Pubkey>,
    ) -> Option<Vec<TransactionLogInfo>> {
        self.transaction_log_collector
            .read()
            .unwrap()
            .get_logs_for_address(address)
    }

    /// Returns all the accounts stored in this slot
    pub fn get_all_accounts_modified_since_parent(&self) -> Vec<TransactionAccount> {
        self.rc.accounts.load_by_program_slot(self.slot(), None)
    }

    // if you want get_account_modified_since_parent without fixed_root, please define so...
    fn get_account_modified_since_parent_with_fixed_root(
        &self,
        pubkey: &Pubkey,
    ) -> Option<(AccountSharedData, Slot)> {
        let just_self: Ancestors = Ancestors::from(vec![self.slot()]);
        if let Some((account, slot)) = self.load_slow_with_fixed_root(&just_self, pubkey) {
            if slot == self.slot() {
                return Some((account, slot));
            }
        }
        None
    }

    pub fn get_largest_accounts(
        &self,
        num: usize,
        filter_by_address: &HashSet<Pubkey>,
        filter: AccountAddressFilter,
        sort_results: bool,
    ) -> ScanResult<Vec<(Pubkey, u64)>> {
        self.rc.accounts.load_largest_accounts(
            &self.ancestors,
            self.bank_id,
            num,
            filter_by_address,
            filter,
            sort_results,
        )
    }

    /// Return the accumulated executed transaction count
    pub fn transaction_count(&self) -> u64 {
        self.transaction_count.load(Relaxed)
    }

    /// Returns the number of non-vote transactions processed without error
    /// since the most recent boot from snapshot or genesis.
    /// This value is not shared though the network, nor retained
    /// within snapshots, but is preserved in `Bank::new_from_parent`.
    pub fn non_vote_transaction_count_since_restart(&self) -> u64 {
        self.non_vote_transaction_count_since_restart.load(Relaxed)
    }

    /// Return the transaction count executed only in this bank
    pub fn executed_transaction_count(&self) -> u64 {
        self.transaction_count()
            .saturating_sub(self.parent().map_or(0, |parent| parent.transaction_count()))
    }

    pub fn transaction_error_count(&self) -> u64 {
        self.transaction_error_count.load(Relaxed)
    }

    pub fn transaction_entries_count(&self) -> u64 {
        self.transaction_entries_count.load(Relaxed)
    }

    pub fn transactions_per_entry_max(&self) -> u64 {
        self.transactions_per_entry_max.load(Relaxed)
    }

    fn increment_transaction_count(&self, tx_count: u64) {
        self.transaction_count.fetch_add(tx_count, Relaxed);
    }

    fn increment_non_vote_transaction_count_since_restart(&self, tx_count: u64) {
        self.non_vote_transaction_count_since_restart
            .fetch_add(tx_count, Relaxed);
    }

    pub fn signature_count(&self) -> u64 {
        self.signature_count.load(Relaxed)
    }

    fn increment_signature_count(&self, signature_count: u64) {
        self.signature_count.fetch_add(signature_count, Relaxed);
    }

    pub fn get_signature_status_processed_since_parent(
        &self,
        signature: &Signature,
    ) -> Option<Result<()>> {
        if let Some((slot, status)) = self.get_signature_status_slot(signature) {
            if slot <= self.slot() {
                return Some(status);
            }
        }
        None
    }

    pub fn get_signature_status_with_blockhash(
        &self,
        signature: &Signature,
        blockhash: &Hash,
    ) -> Option<Result<()>> {
        let rcache = self.status_cache.read().unwrap();
        rcache
            .get_status(signature, blockhash, &self.ancestors)
            .map(|v| v.1)
    }

    pub fn get_committed_transaction_status_and_slot(
        &self,
        message_hash: &Hash,
        transaction_blockhash: &Hash,
    ) -> Option<(Slot, bool)> {
        let rcache = self.status_cache.read().unwrap();
        rcache
            .get_status(message_hash, transaction_blockhash, &self.ancestors)
            .map(|(slot, status)| (slot, status.is_ok()))
    }

    pub fn get_signature_status_slot(&self, signature: &Signature) -> Option<(Slot, Result<()>)> {
        let rcache = self.status_cache.read().unwrap();
        rcache.get_status_any_blockhash(signature, &self.ancestors)
    }

    pub fn get_signature_status(&self, signature: &Signature) -> Option<Result<()>> {
        self.get_signature_status_slot(signature).map(|v| v.1)
    }

    pub fn has_signature(&self, signature: &Signature) -> bool {
        self.get_signature_status_slot(signature).is_some()
    }

    /// Hash the `accounts` HashMap. This represents a validator's interpretation
    ///  of the delta of the ledger since the last vote and up to now
    fn hash_internal_state(&self) -> Hash {
        let measure_total = Measure::start("");
        let slot = self.slot();

        let mut hash = hashv(&[
            self.parent_hash.as_ref(),
            &self.signature_count().to_le_bytes(),
            self.last_blockhash().as_ref(),
        ]);

        let accounts_lt_hash_checksum = {
            let accounts_lt_hash = &*self.accounts_lt_hash.lock().unwrap();
            let lt_hash_bytes = bytemuck::must_cast_slice(&accounts_lt_hash.0 .0);
            hash = hashv(&[hash.as_ref(), lt_hash_bytes]);
            accounts_lt_hash.0.checksum()
        };

        let buf = self
            .hard_forks
            .read()
            .unwrap()
            .get_hash_data(slot, self.parent_slot());
        if let Some(buf) = buf {
            let hard_forked_hash = hashv(&[hash.as_ref(), &buf]);
            warn!("hard fork at slot {slot} by hashing {buf:?}: {hash} => {hard_forked_hash}");
            hash = hard_forked_hash;
        }

        #[cfg(feature = "dev-context-only-utils")]
        let hash_override = self
            .hash_overrides
            .lock()
            .unwrap()
            .get_bank_hash_override(slot)
            .copied()
            .inspect(|&hash_override| {
                if hash_override != hash {
                    info!(
                        "bank: slot: {}: overrode bank hash: {} with {}",
                        self.slot(),
                        hash,
                        hash_override
                    );
                }
            });
        // Avoid to optimize out `hash` along with the whole computation by super smart rustc.
        // hash_override is used by ledger-tool's simulate-block-production, which prefers
        // the actual bank freezing processing for accurate simulation.
        #[cfg(feature = "dev-context-only-utils")]
        let hash = hash_override.unwrap_or(std::hint::black_box(hash));

        let bank_hash_stats = self.bank_hash_stats.load();

        let total_us = measure_total.end_as_us();

        datapoint_info!(
            "bank-hash_internal_state",
            ("slot", slot, i64),
            ("total_us", total_us, i64),
        );
        info!(
            "bank frozen: {slot} hash: {hash} signature_count: {} last_blockhash: {} \
             capitalization: {}, accounts_lt_hash checksum: {accounts_lt_hash_checksum}, \
             stats: {bank_hash_stats:?}",
            self.signature_count(),
            self.last_blockhash(),
            self.capitalization(),
        );
        hash
    }

    pub fn collector_fees(&self) -> u64 {
        self.collector_fees.load(Relaxed)
    }

    /// Used by ledger tool to run a final hash calculation once all ledger replay has completed.
    /// This should not be called by validator code.
    pub fn run_final_hash_calc(&self) {
        self.force_flush_accounts_cache();
        // note that this slot may not be a root
        _ = self.verify_accounts_hash(
            VerifyAccountsHashConfig {
                require_rooted_bank: false,
                run_in_background: false,
            },
            None,
        );
    }

    /// Recalculate the accounts hash from the account stores. Used to verify a snapshot.
    /// return true if all is good
    /// Only called from startup or test code.
    #[must_use]
    fn verify_accounts_hash(
        &self,
        mut config: VerifyAccountsHashConfig,
        duplicates_lt_hash: Option<Box<DuplicatesLtHash>>,
    ) -> bool {
        let accounts_db = &self.rc.accounts.accounts_db;
        // Wait until initial hash calc is complete before starting a new hash calc.
        // This should only occur when we halt at a slot in ledger-tool.
        accounts_db
            .verify_accounts_hash_in_bg
            .join_background_thread();

        let slot = self.slot();

        if duplicates_lt_hash.is_none() {
            // Calculating the accounts lt hash from storages *requires* a duplicates_lt_hash.
            // If it is None here, then we must use the index instead, which also means we
            // cannot run in the background.
            config.run_in_background = false;
        }

        if config.require_rooted_bank && !accounts_db.accounts_index.is_alive_root(slot) {
            if let Some(parent) = self.parent() {
                info!(
                    "slot {slot} is not a root, so verify accounts hash on parent bank at slot {}",
                    parent.slot(),
                );
                // The duplicates_lt_hash is only valid for the current slot, so we must fall
                // back to verifying the accounts lt hash with the index (which also means we
                // cannot run in the background).
                config.run_in_background = false;
                return parent.verify_accounts_hash(config, None);
            } else {
                // this will result in mismatch errors
                // accounts hash calc doesn't include unrooted slots
                panic!("cannot verify accounts hash because slot {slot} is not a root");
            }
        }

        fn check_lt_hash(
            expected_accounts_lt_hash: &AccountsLtHash,
            calculated_accounts_lt_hash: &AccountsLtHash,
        ) -> bool {
            let is_ok = calculated_accounts_lt_hash == expected_accounts_lt_hash;
            if !is_ok {
                let expected = expected_accounts_lt_hash.0.checksum();
                let calculated = calculated_accounts_lt_hash.0.checksum();
                error!(
                    "Verifying accounts failed: accounts lattice hashes do not match, \
                     expected: {expected}, calculated: {calculated}",
                );
            }
            is_ok
        }

        // The snapshot storages must be captured *before* starting the background verification.
        // Otherwise, it is possible that a delayed call to `get_snapshot_storages()` will *not*
        // get the correct storages required to calculate and verify the accounts hashes.
        let snapshot_storages = accounts_db.get_storages(RangeFull);
        let expected_accounts_lt_hash = self.accounts_lt_hash.lock().unwrap().clone();
        if config.run_in_background {
            let accounts_db_ = Arc::clone(accounts_db);
            accounts_db.verify_accounts_hash_in_bg.start(|| {
                Builder::new()
                    .name("solBgHashVerify".into())
                    .spawn(move || {
                        info!("Initial background accounts hash verification has started");
                        let start = Instant::now();
                        let (calculated_accounts_lt_hash, lattice_verify_time) =
                            meas_dur!(accounts_db_.thread_pool_hash.install(|| {
                                accounts_db_.calculate_accounts_lt_hash_at_startup_from_storages(
                                    snapshot_storages.0.as_slice(),
                                    &duplicates_lt_hash.unwrap(),
                                )
                            }));
                        let is_ok =
                            check_lt_hash(&expected_accounts_lt_hash, &calculated_accounts_lt_hash);
                        accounts_db_
                            .verify_accounts_hash_in_bg
                            .background_finished();
                        let total_time = start.elapsed();
                        datapoint_info!(
                            "startup_verify_accounts",
                            ("total_us", total_time.as_micros(), i64),
                            (
                                "verify_accounts_lt_hash_us",
                                lattice_verify_time.as_micros(),
                                i64
                            ),
                        );
                        info!("Initial background accounts hash verification has stopped in {total_time:?}");
                        is_ok
                    })
                    .unwrap()
            });
            true // initial result is true. We haven't failed yet. If verification fails, we'll panic from bg thread.
        } else {
            let calculated_accounts_lt_hash = if let Some(duplicates_lt_hash) = duplicates_lt_hash {
                accounts_db.calculate_accounts_lt_hash_at_startup_from_storages(
                    snapshot_storages.0.as_slice(),
                    &duplicates_lt_hash,
                )
            } else {
                accounts_db.calculate_accounts_lt_hash_at_startup_from_index(&self.ancestors, slot)
            };
            let is_ok = check_lt_hash(&expected_accounts_lt_hash, &calculated_accounts_lt_hash);
            self.set_initial_accounts_hash_verification_completed();
            is_ok
        }
    }

    /// Specify that initial verification has completed.
    /// Called internally when verification runs in the foreground thread.
    /// Also has to be called by some tests which don't do verification on startup.
    pub fn set_initial_accounts_hash_verification_completed(&self) {
        self.rc
            .accounts
            .accounts_db
            .verify_accounts_hash_in_bg
            .verification_complete();
    }

    /// return true if bg hash verification is complete
    /// return false if bg hash verification has not completed yet
    /// if hash verification failed, a panic will occur
    pub fn has_initial_accounts_hash_verification_completed(&self) -> bool {
        self.rc
            .accounts
            .accounts_db
            .verify_accounts_hash_in_bg
            .check_complete()
    }

    /// Get this bank's storages to use for snapshots.
    ///
    /// If a base slot is provided, return only the storages that are *higher* than this slot.
    pub fn get_snapshot_storages(&self, base_slot: Option<Slot>) -> Vec<Arc<AccountStorageEntry>> {
        // if a base slot is provided, request storages starting at the slot *after*
        let start_slot = base_slot.map_or(0, |slot| slot.saturating_add(1));
        // we want to *include* the storage at our slot
        let requested_slots = start_slot..=self.slot();

        self.rc.accounts.accounts_db.get_storages(requested_slots).0
    }

    #[must_use]
    fn verify_hash(&self) -> bool {
        assert!(self.is_frozen());
        let calculated_hash = self.hash_internal_state();
        let expected_hash = self.hash();

        if calculated_hash == expected_hash {
            true
        } else {
            warn!(
                "verify failed: slot: {}, {} (calculated) != {} (expected)",
                self.slot(),
                calculated_hash,
                expected_hash
            );
            false
        }
    }

    pub fn verify_transaction(
        &self,
        tx: VersionedTransaction,
        verification_mode: TransactionVerificationMode,
    ) -> Result<RuntimeTransaction<SanitizedTransaction>> {
        let sanitized_tx = {
            let size =
                bincode::serialized_size(&tx).map_err(|_| TransactionError::SanitizeFailure)?;
            if size > PACKET_DATA_SIZE as u64 {
                return Err(TransactionError::SanitizeFailure);
            }
            let message_hash = if verification_mode == TransactionVerificationMode::FullVerification
            {
                tx.verify_and_hash_message()?
            } else {
                tx.message.hash()
            };

            RuntimeTransaction::try_create(
                tx,
                MessageHash::Precomputed(message_hash),
                None,
                self,
                self.get_reserved_account_keys(),
            )
        }?;

        Ok(sanitized_tx)
    }

    pub fn fully_verify_transaction(
        &self,
        tx: VersionedTransaction,
    ) -> Result<RuntimeTransaction<SanitizedTransaction>> {
        self.verify_transaction(tx, TransactionVerificationMode::FullVerification)
    }

    /// Checks if the transaction violates the bank's reserved keys.
    /// This needs to be checked upon epoch boundary crosses because the
    /// reserved key set may have changed since the initial sanitization.
    pub fn check_reserved_keys(&self, tx: &impl SVMMessage) -> Result<()> {
        // Check keys against the reserved set - these failures simply require us
        // to re-sanitize the transaction. We do not need to drop the transaction.
        let reserved_keys = self.get_reserved_account_keys();
        for (index, key) in tx.account_keys().iter().enumerate() {
            if tx.is_writable(index) && reserved_keys.contains(key) {
                return Err(TransactionError::ResanitizationNeeded);
            }
        }

        Ok(())
    }

    /// Calculates and returns the capitalization.
    ///
    /// Panics if capitalization overflows a u64.
    ///
    /// Note, this is *very* expensive!  It walks the whole accounts index,
    /// account-by-account, summing each account's balance.
    ///
    /// Only intended to be called at startup by ledger-tool or tests.
    /// (cannot be made DCOU due to solana-program-test)
    pub fn calculate_capitalization_for_tests(&self) -> u64 {
        self.rc
            .accounts
            .accounts_db
            .verify_accounts_hash_in_bg
            .join_background_thread();
        self.rc
            .accounts
            .accounts_db
            .calculate_capitalization_at_startup_from_index(&self.ancestors, self.slot())
    }

    /// Sets the capitalization.
    ///
    /// Only intended to be called by ledger-tool or tests.
    /// (cannot be made DCOU due to solana-program-test)
    pub fn set_capitalization_for_tests(&self, capitalization: u64) {
        self.capitalization.store(capitalization, Relaxed);
    }

    /// Returns the `SnapshotHash` for this bank's slot
    ///
    /// This fn is used at startup to verify the bank was rebuilt correctly.
    pub fn get_snapshot_hash(&self) -> SnapshotHash {
        SnapshotHash::new(self.accounts_lt_hash.lock().unwrap().0.checksum())
    }

    pub fn load_account_into_read_cache(&self, key: &Pubkey) {
        self.rc
            .accounts
            .accounts_db
            .load_account_into_read_cache(&self.ancestors, key);
    }

    /// A snapshot bank should be purged of 0 lamport accounts which are not part of the hash
    /// calculation and could shield other real accounts.
    pub fn verify_snapshot_bank(
        &self,
        skip_shrink: bool,
        force_clean: bool,
        latest_full_snapshot_slot: Slot,
        duplicates_lt_hash: Option<Box<DuplicatesLtHash>>,
    ) -> bool {
        // If we verify the accounts using the lattice-based hash *and* with storages (as opposed
        // to the index), then we rely on the DuplicatesLtHash as given by generate_index().  Since
        // the duplicates are based on a specific set of storages, we must use the exact same
        // storages to do the lattice-based accounts verification.  This means we must wait to
        // clean/shrink until *after* we've gotten Arcs to the storages (this prevents their
        // untimely removal).  Simply, we call `verify_accounts_hash()` before we call `clean` or
        // `shrink`.
        let (verified_accounts, verify_accounts_time_us) = measure_us!({
            let should_verify_accounts = !self.rc.accounts.accounts_db.skip_initial_hash_calc;
            if should_verify_accounts {
                info!("Verifying accounts...");
                let verified = self.verify_accounts_hash(
                    VerifyAccountsHashConfig {
                        require_rooted_bank: false,
                        run_in_background: true,
                    },
                    duplicates_lt_hash,
                );
                info!("Verifying accounts... In background.");
                verified
            } else {
                info!("Verifying accounts... Skipped.");
                self.set_initial_accounts_hash_verification_completed();
                true
            }
        });

        let (_, clean_time_us) = measure_us!({
            let should_clean = force_clean || (!skip_shrink && self.slot() > 0);
            if should_clean {
                info!("Cleaning...");
                // We cannot clean past the latest full snapshot's slot because we are about to
                // perform an accounts hash calculation *up to that slot*.  If we cleaned *past*
                // that slot, then accounts could be removed from older storages, which would
                // change the accounts hash.
                self.rc.accounts.accounts_db.clean_accounts(
                    Some(latest_full_snapshot_slot),
                    true,
                    self.epoch_schedule(),
                );
                info!("Cleaning... Done.");
            } else {
                info!("Cleaning... Skipped.");
            }
        });

        let (_, shrink_time_us) = measure_us!({
            let should_shrink = !skip_shrink && self.slot() > 0;
            if should_shrink {
                info!("Shrinking...");
                self.rc.accounts.accounts_db.shrink_all_slots(
                    true,
                    self.epoch_schedule(),
                    // we cannot allow the snapshot slot to be shrunk
                    Some(self.slot()),
                );
                info!("Shrinking... Done.");
            } else {
                info!("Shrinking... Skipped.");
            }
        });

        info!("Verifying bank...");
        let (verified_bank, verify_bank_time_us) = measure_us!(self.verify_hash());
        info!("Verifying bank... Done.");

        datapoint_info!(
            "verify_snapshot_bank",
            ("clean_us", clean_time_us, i64),
            ("shrink_us", shrink_time_us, i64),
            ("verify_accounts_us", verify_accounts_time_us, i64),
            ("verify_bank_us", verify_bank_time_us, i64),
        );

        verified_accounts && verified_bank
    }

    /// Return the number of hashes per tick
    pub fn hashes_per_tick(&self) -> &Option<u64> {
        &self.hashes_per_tick
    }

    /// Return the number of ticks per slot
    pub fn ticks_per_slot(&self) -> u64 {
        self.ticks_per_slot
    }

    /// Return the number of slots per year
    pub fn slots_per_year(&self) -> f64 {
        self.slots_per_year
    }

    /// Return the number of ticks since genesis.
    pub fn tick_height(&self) -> u64 {
        self.tick_height.load(Relaxed)
    }

    /// Return the inflation parameters of the Bank
    pub fn inflation(&self) -> Inflation {
        *self.inflation.read().unwrap()
    }

    /// Return the rent collector for this Bank
    pub fn rent_collector(&self) -> &RentCollector {
        &self.rent_collector
    }

    /// Return the total capitalization of the Bank
    pub fn capitalization(&self) -> u64 {
        self.capitalization.load(Relaxed)
    }

    /// Return this bank's max_tick_height
    pub fn max_tick_height(&self) -> u64 {
        self.max_tick_height
    }

    /// Return the block_height of this bank
    pub fn block_height(&self) -> u64 {
        self.block_height
    }

    /// Return the number of slots per epoch for the given epoch
    pub fn get_slots_in_epoch(&self, epoch: Epoch) -> u64 {
        self.epoch_schedule().get_slots_in_epoch(epoch)
    }

    /// returns the epoch for which this bank's leader_schedule_slot_offset and slot would
    ///  need to cache leader_schedule
    pub fn get_leader_schedule_epoch(&self, slot: Slot) -> Epoch {
        self.epoch_schedule().get_leader_schedule_epoch(slot)
    }

    /// Returns whether the specified epoch should use the new vote account
    /// keyed leader schedule
    pub fn should_use_vote_keyed_leader_schedule(&self, epoch: Epoch) -> Option<bool> {
        let effective_epoch = self
            .feature_set
            .activated_slot(&agave_feature_set::enable_vote_address_leader_schedule::id())
            .map(|activation_slot| {
                // If the feature was activated at genesis, then the new leader
                // schedule should be effective immediately in the first epoch
                if activation_slot == 0 {
                    return 0;
                }

                // Calculate the epoch that the feature became activated in
                let activation_epoch = self.epoch_schedule.get_epoch(activation_slot);

                // The effective epoch is the epoch immediately after the
                // activation epoch
                activation_epoch.wrapping_add(1)
            });

        // Starting from the effective epoch, always use the new leader schedule
        if let Some(effective_epoch) = effective_epoch {
            return Some(epoch >= effective_epoch);
        }

        // Calculate the max epoch we can cache a leader schedule for
        let max_cached_leader_schedule = self.get_leader_schedule_epoch(self.slot());
        if epoch <= max_cached_leader_schedule {
            // The feature cannot be effective by the specified epoch
            Some(false)
        } else {
            // Cannot determine if an epoch should use the new leader schedule if the
            // the epoch is too far in the future because we won't know if the feature
            // will have been activated by then or not.
            None
        }
    }

    /// a bank-level cache of vote accounts and stake delegation info
    fn update_stakes_cache(
        &self,
        txs: &[impl SVMMessage],
        processing_results: &[TransactionProcessingResult],
    ) {
        debug_assert_eq!(txs.len(), processing_results.len());
        let new_warmup_cooldown_rate_epoch = self.new_warmup_cooldown_rate_epoch();
        txs.iter()
            .zip(processing_results)
            .filter_map(|(tx, processing_result)| {
                processing_result
                    .processed_transaction()
                    .map(|processed_tx| (tx, processed_tx))
            })
            .filter_map(|(tx, processed_tx)| {
                processed_tx
                    .executed_transaction()
                    .map(|executed_tx| (tx, executed_tx))
            })
            .filter(|(_, executed_tx)| executed_tx.was_successful())
            .flat_map(|(tx, executed_tx)| {
                let num_account_keys = tx.account_keys().len();
                let loaded_tx = &executed_tx.loaded_transaction;
                loaded_tx.accounts.iter().take(num_account_keys)
            })
            .for_each(|(pubkey, account)| {
                // note that this could get timed to: self.rc.accounts.accounts_db.stats.stakes_cache_check_and_store_us,
                //  but this code path is captured separately in ExecuteTimingType::UpdateStakesCacheUs
                self.stakes_cache
                    .check_and_store(pubkey, account, new_warmup_cooldown_rate_epoch);
            });
    }

    /// current vote accounts for this bank along with the stake
    ///   attributed to each account
    pub fn vote_accounts(&self) -> Arc<VoteAccountsHashMap> {
        let stakes = self.stakes_cache.stakes();
        Arc::from(stakes.vote_accounts())
    }

    /// Vote account for the given vote account pubkey.
    pub fn get_vote_account(&self, vote_account: &Pubkey) -> Option<VoteAccount> {
        let stakes = self.stakes_cache.stakes();
        let vote_account = stakes.vote_accounts().get(vote_account)?;
        Some(vote_account.clone())
    }

    /// Get the EpochStakes for the current Bank::epoch
    pub fn current_epoch_stakes(&self) -> &VersionedEpochStakes {
        // The stakes for a given epoch (E) in self.epoch_stakes are keyed by leader schedule epoch
        // (E + 1) so the stakes for the current epoch are stored at self.epoch_stakes[E + 1]
        self.epoch_stakes
            .get(&self.epoch.saturating_add(1))
            .expect("Current epoch stakes must exist")
    }

    /// Get the EpochStakes for a given epoch
    pub fn epoch_stakes(&self, epoch: Epoch) -> Option<&VersionedEpochStakes> {
        self.epoch_stakes.get(&epoch)
    }

    pub fn epoch_stakes_map(&self) -> &HashMap<Epoch, VersionedEpochStakes> {
        &self.epoch_stakes
    }

    /// Get the staked nodes map for the current Bank::epoch
    pub fn current_epoch_staked_nodes(&self) -> Arc<HashMap<Pubkey, u64>> {
        self.current_epoch_stakes().stakes().staked_nodes()
    }

    pub fn epoch_staked_nodes(&self, epoch: Epoch) -> Option<Arc<HashMap<Pubkey, u64>>> {
        Some(self.epoch_stakes.get(&epoch)?.stakes().staked_nodes())
    }

    /// Get the total epoch stake for the given epoch.
    pub fn epoch_total_stake(&self, epoch: Epoch) -> Option<u64> {
        self.epoch_stakes
            .get(&epoch)
            .map(|epoch_stakes| epoch_stakes.total_stake())
    }

    /// Get the total epoch stake for the current Bank::epoch
    pub fn get_current_epoch_total_stake(&self) -> u64 {
        self.current_epoch_stakes().total_stake()
    }

    /// vote accounts for the specific epoch along with the stake
    ///   attributed to each account
    pub fn epoch_vote_accounts(&self, epoch: Epoch) -> Option<&VoteAccountsHashMap> {
        let epoch_stakes = self.epoch_stakes.get(&epoch)?.stakes();
        Some(epoch_stakes.vote_accounts().as_ref())
    }

    /// Get the vote accounts along with the stake attributed to each account
    /// for the current Bank::epoch
    pub fn get_current_epoch_vote_accounts(&self) -> &VoteAccountsHashMap {
        self.current_epoch_stakes()
            .stakes()
            .vote_accounts()
            .as_ref()
    }

    /// Get the fixed authorized voter for the given vote account for the
    /// current epoch
    pub fn epoch_authorized_voter(&self, vote_account: &Pubkey) -> Option<&Pubkey> {
        self.epoch_stakes
            .get(&self.epoch)
            .expect("Epoch stakes for bank's own epoch must exist")
            .epoch_authorized_voters()
            .get(vote_account)
    }

    /// Get the fixed set of vote accounts for the given node id for the
    /// current epoch
    pub fn epoch_vote_accounts_for_node_id(&self, node_id: &Pubkey) -> Option<&NodeVoteAccounts> {
        self.epoch_stakes
            .get(&self.epoch)
            .expect("Epoch stakes for bank's own epoch must exist")
            .node_id_to_vote_accounts()
            .get(node_id)
    }

    /// Get the total stake belonging to vote accounts associated with the given node id for the
    /// given epoch.
    pub fn epoch_node_id_to_stake(&self, epoch: Epoch, node_id: &Pubkey) -> Option<u64> {
        self.epoch_stakes(epoch)
            .and_then(|epoch_stakes| epoch_stakes.node_id_to_stake(node_id))
    }

    /// Get the fixed total stake of all vote accounts for current epoch
    pub fn total_epoch_stake(&self) -> u64 {
        self.epoch_stakes
            .get(&self.epoch)
            .expect("Epoch stakes for bank's own epoch must exist")
            .total_stake()
    }

    /// Get the fixed stake of the given vote account for the current epoch
    pub fn epoch_vote_account_stake(&self, vote_account: &Pubkey) -> u64 {
        *self
            .epoch_vote_accounts(self.epoch())
            .expect("Bank epoch vote accounts must contain entry for the bank's own epoch")
            .get(vote_account)
            .map(|(stake, _)| stake)
            .unwrap_or(&0)
    }

    /// given a slot, return the epoch and offset into the epoch this slot falls
    /// e.g. with a fixed number for slots_per_epoch, the calculation is simply:
    ///
    ///  ( slot/slots_per_epoch, slot % slots_per_epoch )
    ///
    pub fn get_epoch_and_slot_index(&self, slot: Slot) -> (Epoch, SlotIndex) {
        self.epoch_schedule().get_epoch_and_slot_index(slot)
    }

    pub fn get_epoch_info(&self) -> EpochInfo {
        let absolute_slot = self.slot();
        let block_height = self.block_height();
        let (epoch, slot_index) = self.get_epoch_and_slot_index(absolute_slot);
        let slots_in_epoch = self.get_slots_in_epoch(epoch);
        let transaction_count = Some(self.transaction_count());
        EpochInfo {
            epoch,
            slot_index,
            slots_in_epoch,
            absolute_slot,
            block_height,
            transaction_count,
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.is_delta.load(Relaxed)
    }

    pub fn add_mockup_builtin(
        &mut self,
        program_id: Pubkey,
        builtin_function: BuiltinFunctionWithContext,
    ) {
        self.transaction_processor.add_builtin(
            self,
            program_id,
            "mockup",
            ProgramCacheEntry::new_builtin(self.slot, 0, builtin_function),
        );
    }

    pub fn add_precompile(&mut self, program_id: &Pubkey) {
        debug!("Adding precompiled program {}", program_id);
        self.add_precompiled_account(program_id);
        debug!("Added precompiled program {:?}", program_id);
    }

    // Call AccountsDb::clean_accounts()
    //
    // This fn is meant to be called by the snapshot handler in Accounts Background Service.  If
    // calling from elsewhere, ensure the same invariants hold/expectations are met.
    pub(crate) fn clean_accounts(&self) {
        // Don't clean the slot we're snapshotting because it may have zero-lamport
        // accounts that were included in the bank delta hash when the bank was frozen,
        // and if we clean them here, any newly created snapshot's hash for this bank
        // may not match the frozen hash.
        //
        // So when we're snapshotting, the highest slot to clean is lowered by one.
        let highest_slot_to_clean = self.slot().saturating_sub(1);

        self.rc.accounts.accounts_db.clean_accounts(
            Some(highest_slot_to_clean),
            false,
            self.epoch_schedule(),
        );
    }

    pub fn print_accounts_stats(&self) {
        self.rc.accounts.accounts_db.print_accounts_stats("");
    }

    pub fn shrink_candidate_slots(&self) -> usize {
        self.rc
            .accounts
            .accounts_db
            .shrink_candidate_slots(self.epoch_schedule())
    }

    pub(crate) fn shrink_ancient_slots(&self) {
        self.rc
            .accounts
            .accounts_db
            .shrink_ancient_slots(self.epoch_schedule())
    }

    pub fn read_cost_tracker(&self) -> LockResult<RwLockReadGuard<CostTracker>> {
        self.cost_tracker.read()
    }

    pub fn write_cost_tracker(&self) -> LockResult<RwLockWriteGuard<CostTracker>> {
        self.cost_tracker.write()
    }

    // Check if the wallclock time from bank creation to now has exceeded the allotted
    // time for transaction processing
    pub fn should_bank_still_be_processing_txs(
        bank_creation_time: &Instant,
        max_tx_ingestion_nanos: u128,
    ) -> bool {
        // Do this check outside of the PoH lock, hence not a method on PohRecorder
        bank_creation_time.elapsed().as_nanos() <= max_tx_ingestion_nanos
    }

    pub fn deactivate_feature(&mut self, id: &Pubkey) {
        let mut feature_set = Arc::make_mut(&mut self.feature_set).clone();
        feature_set.active_mut().remove(id);
        feature_set.inactive_mut().insert(*id);
        self.feature_set = Arc::new(feature_set);
    }

    pub fn activate_feature(&mut self, id: &Pubkey) {
        let mut feature_set = Arc::make_mut(&mut self.feature_set).clone();
        feature_set.inactive_mut().remove(id);
        feature_set.active_mut().insert(*id, 0);
        self.feature_set = Arc::new(feature_set);
    }

    pub fn fill_bank_with_ticks_for_tests(&self) {
        self.do_fill_bank_with_ticks_for_tests(&BankWithScheduler::no_scheduler_available())
    }

    pub(crate) fn do_fill_bank_with_ticks_for_tests(&self, scheduler: &InstalledSchedulerRwLock) {
        if self.tick_height.load(Relaxed) < self.max_tick_height {
            let last_blockhash = self.last_blockhash();
            while self.last_blockhash() == last_blockhash {
                self.register_tick(&Hash::new_unique(), scheduler)
            }
        } else {
            warn!("Bank already reached max tick height, cannot fill it with more ticks");
        }
    }

    /// Get a set of all actively reserved account keys that are not allowed to
    /// be write-locked during transaction processing.
    pub fn get_reserved_account_keys(&self) -> &HashSet<Pubkey> {
        &self.reserved_account_keys.active
    }

    // This is called from snapshot restore AND for each epoch boundary
    // The entire code path herein must be idempotent
    fn apply_feature_activations(
        &mut self,
        caller: ApplyFeatureActivationsCaller,
        debug_do_not_add_builtins: bool,
    ) {
        use ApplyFeatureActivationsCaller as Caller;
        let allow_new_activations = match caller {
            Caller::FinishInit => false,
            Caller::NewFromParent => true,
            Caller::WarpFromParent => false,
        };
        let (feature_set, new_feature_activations) =
            self.compute_active_feature_set(allow_new_activations);
        self.feature_set = Arc::new(feature_set);

        // Update activation slot of features in `new_feature_activations`
        for feature_id in new_feature_activations.iter() {
            if let Some(mut account) = self.get_account_with_fixed_root(feature_id) {
                if let Some(mut feature) = feature::from_account(&account) {
                    feature.activated_at = Some(self.slot());
                    if feature::to_account(&feature, &mut account).is_some() {
                        self.store_account(feature_id, &account);
                    }
                    info!("Feature {} activated at slot {}", feature_id, self.slot());
                }
            }
        }

        // Update active set of reserved account keys which are not allowed to be write locked
        self.reserved_account_keys = {
            let mut reserved_keys = ReservedAccountKeys::clone(&self.reserved_account_keys);
            reserved_keys.update_active_set(&self.feature_set);
            Arc::new(reserved_keys)
        };

        if new_feature_activations.contains(&feature_set::pico_inflation::id()) {
            *self.inflation.write().unwrap() = Inflation::pico();
            self.fee_rate_governor.burn_percent = solana_fee_calculator::DEFAULT_BURN_PERCENT; // 50% fee burn
            self.rent_collector.rent.burn_percent = 50; // 50% rent burn
        }

        if !new_feature_activations.is_disjoint(&self.feature_set.full_inflation_features_enabled())
        {
            *self.inflation.write().unwrap() = Inflation::full();
            self.fee_rate_governor.burn_percent = solana_fee_calculator::DEFAULT_BURN_PERCENT; // 50% fee burn
            self.rent_collector.rent.burn_percent = 50; // 50% rent burn
        }

        if !debug_do_not_add_builtins {
            self.apply_builtin_program_feature_transitions(
                allow_new_activations,
                &new_feature_activations,
            );
        }

        if new_feature_activations.contains(&feature_set::raise_block_limits_to_100m::id()) {
            let block_cost_limit = simd_0286_block_limits();
            let mut cost_tracker = self.write_cost_tracker().unwrap();
            let account_cost_limit = cost_tracker.get_account_limit();
            let vote_cost_limit = cost_tracker.get_vote_limit();
            cost_tracker.set_limits(account_cost_limit, block_cost_limit, vote_cost_limit);
            drop(cost_tracker);

            if self
                .feature_set
                .is_active(&feature_set::raise_account_cu_limit::id())
            {
                self.apply_simd_0306_cost_tracker_changes();
            }
        }

        if new_feature_activations.contains(&feature_set::raise_account_cu_limit::id()) {
            self.apply_simd_0306_cost_tracker_changes();
        }
    }

    fn adjust_sysvar_balance_for_rent(&self, account: &mut AccountSharedData) {
        account.set_lamports(
            self.get_minimum_balance_for_rent_exemption(account.data().len())
                .max(account.lamports()),
        );
    }

    /// Compute the active feature set based on the current bank state,
    /// and return it together with the set of newly activated features.
    fn compute_active_feature_set(&self, include_pending: bool) -> (FeatureSet, AHashSet<Pubkey>) {
        let mut active = self.feature_set.active().clone();
        let mut inactive = AHashSet::new();
        let mut pending = AHashSet::new();
        let slot = self.slot();

        for feature_id in self.feature_set.inactive() {
            let mut activated = None;
            if let Some(account) = self.get_account_with_fixed_root(feature_id) {
                if let Some(feature) = feature::from_account(&account) {
                    match feature.activated_at {
                        None if include_pending => {
                            // Feature activation is pending
                            pending.insert(*feature_id);
                            activated = Some(slot);
                        }
                        Some(activation_slot) if slot >= activation_slot => {
                            // Feature has been activated already
                            activated = Some(activation_slot);
                        }
                        _ => {}
                    }
                }
            }
            if let Some(slot) = activated {
                active.insert(*feature_id, slot);
            } else {
                inactive.insert(*feature_id);
            }
        }

        (FeatureSet::new(active, inactive), pending)
    }

    fn apply_builtin_program_feature_transitions(
        &mut self,
        only_apply_transitions_for_new_features: bool,
        new_feature_activations: &AHashSet<Pubkey>,
    ) {
        for builtin in BUILTINS.iter() {
            // The `builtin_is_bpf` flag is used to handle the case where a
            // builtin is scheduled to be enabled by one feature gate and
            // later migrated to Core BPF by another.
            //
            // There should never be a case where a builtin is set to be
            // migrated to Core BPF and is also set to be enabled on feature
            // activation on the same feature gate. However, the
            // `builtin_is_bpf` flag will handle this case as well, electing
            // to first attempt the migration to Core BPF.
            //
            // The migration to Core BPF will fail gracefully because the
            // program account will not exist. The builtin will subsequently
            // be enabled, but it will never be migrated to Core BPF.
            //
            // Using the same feature gate for both enabling and migrating a
            // builtin to Core BPF should be strictly avoided.
            let mut builtin_is_bpf = false;
            if let Some(core_bpf_migration_config) = &builtin.core_bpf_migration_config {
                // If the builtin is set to be migrated to Core BPF on feature
                // activation, perform the migration and do not add the program
                // to the bank's builtins. The migration will remove it from
                // the builtins list and the cache.
                if new_feature_activations.contains(&core_bpf_migration_config.feature_id) {
                    if let Err(e) = self
                        .migrate_builtin_to_core_bpf(&builtin.program_id, core_bpf_migration_config)
                    {
                        warn!(
                            "Failed to migrate builtin {} to Core BPF: {}",
                            builtin.name, e
                        );
                    } else {
                        builtin_is_bpf = true;
                    }
                } else {
                    // If the builtin has already been migrated to Core BPF, do not
                    // add it to the bank's builtins.
                    builtin_is_bpf = self
                        .get_account(&builtin.program_id)
                        .map(|a| a.owner() == &bpf_loader_upgradeable::id())
                        .unwrap_or(false);
                }
            };

            if let Some(feature_id) = builtin.enable_feature_id {
                let should_enable_builtin_on_feature_transition = !builtin_is_bpf
                    && if only_apply_transitions_for_new_features {
                        new_feature_activations.contains(&feature_id)
                    } else {
                        self.feature_set.is_active(&feature_id)
                    };

                if should_enable_builtin_on_feature_transition {
                    self.transaction_processor.add_builtin(
                        self,
                        builtin.program_id,
                        builtin.name,
                        ProgramCacheEntry::new_builtin(
                            self.feature_set.activated_slot(&feature_id).unwrap_or(0),
                            builtin.name.len(),
                            builtin.entrypoint,
                        ),
                    );
                }
            }
        }

        // Migrate any necessary stateless builtins to core BPF.
        // Stateless builtins do not have an `enable_feature_id` since they
        // do not exist on-chain.
        for stateless_builtin in STATELESS_BUILTINS.iter() {
            if let Some(core_bpf_migration_config) = &stateless_builtin.core_bpf_migration_config {
                if new_feature_activations.contains(&core_bpf_migration_config.feature_id) {
                    if let Err(e) = self.migrate_builtin_to_core_bpf(
                        &stateless_builtin.program_id,
                        core_bpf_migration_config,
                    ) {
                        warn!(
                            "Failed to migrate stateless builtin {} to Core BPF: {}",
                            stateless_builtin.name, e
                        );
                    }
                }
            }
        }

        for precompile in get_precompiles() {
            let should_add_precompile = precompile
                .feature
                .as_ref()
                .map(|feature_id| self.feature_set.is_active(feature_id))
                .unwrap_or(false);
            if should_add_precompile {
                self.add_precompile(&precompile.program_id);
            }
        }
    }

    /// Use to replace programs by feature activation
    #[allow(dead_code)]
    fn replace_program_account(
        &mut self,
        old_address: &Pubkey,
        new_address: &Pubkey,
        datapoint_name: &'static str,
    ) {
        if let Some(old_account) = self.get_account_with_fixed_root(old_address) {
            if let Some(new_account) = self.get_account_with_fixed_root(new_address) {
                datapoint_info!(datapoint_name, ("slot", self.slot, i64));

                // Burn lamports in the old account
                self.capitalization
                    .fetch_sub(old_account.lamports(), Relaxed);

                // Transfer new account to old account
                self.store_account(old_address, &new_account);

                // Clear new account
                self.store_account(new_address, &AccountSharedData::default());

                // Unload a program from the bank's cache
                self.transaction_processor
                    .program_cache
                    .write()
                    .unwrap()
                    .remove_programs([*old_address].into_iter());

                self.calculate_and_update_accounts_data_size_delta_off_chain(
                    old_account.data().len(),
                    new_account.data().len(),
                );
            }
        }
    }

    /// Calculates the accounts data size of all accounts
    ///
    /// Panics if total overflows a u64.
    ///
    /// Note, this may be *very* expensive, as *all* accounts are collected
    /// into a Vec before summing each account's data size.
    ///
    /// Only intended to be called by tests or when the number of accounts is small.
    pub fn calculate_accounts_data_size(&self) -> ScanResult<u64> {
        let accounts = self.get_all_accounts(false)?;
        let accounts_data_size = accounts
            .into_iter()
            .map(|(_pubkey, account, _slot)| account.data().len() as u64)
            .try_fold(0, u64::checked_add)
            .expect("accounts data size cannot overflow");
        Ok(accounts_data_size)
    }

    pub fn is_in_slot_hashes_history(&self, slot: &Slot) -> bool {
        if slot < &self.slot {
            if let Ok(slot_hashes) = self.transaction_processor.sysvar_cache().get_slot_hashes() {
                return slot_hashes.get(slot).is_some();
            }
        }
        false
    }

    pub fn check_program_modification_slot(&self) -> bool {
        self.check_program_modification_slot
    }

    pub fn set_check_program_modification_slot(&mut self, check: bool) {
        self.check_program_modification_slot = check;
    }

    pub fn fee_structure(&self) -> &FeeStructure {
        &self.fee_structure
    }

    pub fn block_id(&self) -> Option<Hash> {
        *self.block_id.read().unwrap()
    }

    pub fn set_block_id(&self, block_id: Option<Hash>) {
        *self.block_id.write().unwrap() = block_id;
    }

    pub fn compute_budget(&self) -> Option<ComputeBudget> {
        self.compute_budget
    }

    pub fn add_builtin(&self, program_id: Pubkey, name: &str, builtin: ProgramCacheEntry) {
        self.transaction_processor
            .add_builtin(self, program_id, name, builtin)
    }

    pub fn get_bank_hash_stats(&self) -> BankHashStats {
        self.bank_hash_stats.load()
    }

    pub fn clear_epoch_rewards_cache(&self) {
        self.epoch_rewards_calculation_cache.lock().unwrap().clear();
    }

    /// Sets the accounts lt hash, only to be used by SnapshotMinimizer
    pub fn set_accounts_lt_hash_for_snapshot_minimizer(&self, accounts_lt_hash: AccountsLtHash) {
        *self.accounts_lt_hash.lock().unwrap() = accounts_lt_hash;
    }

    /// Return total transaction fee collected
    pub fn get_collector_fee_details(&self) -> CollectorFeeDetails {
        self.collector_fee_details.read().unwrap().clone()
    }
}

impl InvokeContextCallback for Bank {
    fn get_epoch_stake(&self) -> u64 {
        self.get_current_epoch_total_stake()
    }

    fn get_epoch_stake_for_vote_account(&self, vote_address: &Pubkey) -> u64 {
        self.get_current_epoch_vote_accounts()
            .get(vote_address)
            .map(|(stake, _)| (*stake))
            .unwrap_or(0)
    }

    fn is_precompile(&self, program_id: &Pubkey) -> bool {
        is_precompile(program_id, |feature_id: &Pubkey| {
            self.feature_set.is_active(feature_id)
        })
    }

    fn process_precompile(
        &self,
        program_id: &Pubkey,
        data: &[u8],
        instruction_datas: Vec<&[u8]>,
    ) -> std::result::Result<(), PrecompileError> {
        if let Some(precompile) = get_precompile(program_id, |feature_id: &Pubkey| {
            self.feature_set.is_active(feature_id)
        }) {
            precompile.verify(data, &instruction_datas, &self.feature_set)
        } else {
            Err(PrecompileError::InvalidPublicKey)
        }
    }
}

impl TransactionProcessingCallback for Bank {
    fn account_matches_owners(&self, account: &Pubkey, owners: &[Pubkey]) -> Option<usize> {
        self.rc
            .accounts
            .accounts_db
            .account_matches_owners(&self.ancestors, account, owners)
            .ok()
    }

    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<AccountSharedData> {
        self.rc
            .accounts
            .accounts_db
            .load_with_fixed_root(&self.ancestors, pubkey)
            .map(|(acc, _)| acc)
    }

    // NOTE: must hold idempotent for the same set of arguments
    /// Add a builtin program account
    fn add_builtin_account(&self, name: &str, program_id: &Pubkey) {
        let existing_genuine_program =
            self.get_account_with_fixed_root(program_id)
                .and_then(|account| {
                    // it's very unlikely to be squatted at program_id as non-system account because of burden to
                    // find victim's pubkey/hash. So, when account.owner is indeed native_loader's, it's
                    // safe to assume it's a genuine program.
                    if native_loader::check_id(account.owner()) {
                        Some(account)
                    } else {
                        // malicious account is pre-occupying at program_id
                        self.burn_and_purge_account(program_id, account);
                        None
                    }
                });

        // introducing builtin program
        if existing_genuine_program.is_some() {
            // The existing account is sufficient
            return;
        }

        assert!(
            !self.freeze_started(),
            "Can't change frozen bank by adding not-existing new builtin program ({name}, {program_id}). \
            Maybe, inconsistent program activation is detected on snapshot restore?"
        );

        // Add a bogus executable builtin account, which will be loaded and ignored.
        let (lamports, rent_epoch) =
            self.inherit_specially_retained_account_fields(&existing_genuine_program);
        let account: AccountSharedData = AccountSharedData::from(Account {
            lamports,
            data: name.as_bytes().to_vec(),
            owner: solana_sdk_ids::native_loader::id(),
            executable: true,
            rent_epoch,
        });
        self.store_account_and_update_capitalization(program_id, &account);
    }

    fn inspect_account(&self, address: &Pubkey, account_state: AccountState, is_writable: bool) {
        self.inspect_account_for_accounts_lt_hash(address, &account_state, is_writable);
    }
}

impl fmt::Debug for Bank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bank")
            .field("slot", &self.slot)
            .field("bank_id", &self.bank_id)
            .field("block_height", &self.block_height)
            .field("parent_slot", &self.parent_slot)
            .field("capitalization", &self.capitalization())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl Bank {
    pub fn wrap_with_bank_forks_for_tests(self) -> (Arc<Self>, Arc<RwLock<BankForks>>) {
        let bank_forks = BankForks::new_rw_arc(self);
        let bank = bank_forks.read().unwrap().root_bank();
        (bank, bank_forks)
    }

    pub fn default_for_tests() -> Self {
        let accounts_db = AccountsDb::default_for_tests();
        let accounts = Accounts::new(Arc::new(accounts_db));
        Self::default_with_accounts(accounts)
    }

    pub fn new_with_bank_forks_for_tests(
        genesis_config: &GenesisConfig,
    ) -> (Arc<Self>, Arc<RwLock<BankForks>>) {
        let bank = Self::new_for_tests(genesis_config);
        bank.wrap_with_bank_forks_for_tests()
    }

    pub fn new_for_tests(genesis_config: &GenesisConfig) -> Self {
        Self::new_with_config_for_tests(genesis_config, BankTestConfig::default())
    }

    pub fn new_with_mockup_builtin_for_tests(
        genesis_config: &GenesisConfig,
        program_id: Pubkey,
        builtin_function: BuiltinFunctionWithContext,
    ) -> (Arc<Self>, Arc<RwLock<BankForks>>) {
        let mut bank = Self::new_for_tests(genesis_config);
        bank.add_mockup_builtin(program_id, builtin_function);
        bank.wrap_with_bank_forks_for_tests()
    }

    pub fn new_no_wallclock_throttle_for_tests(
        genesis_config: &GenesisConfig,
    ) -> (Arc<Self>, Arc<RwLock<BankForks>>) {
        let mut bank = Self::new_for_tests(genesis_config);

        bank.ns_per_slot = u128::MAX;
        bank.wrap_with_bank_forks_for_tests()
    }

    pub fn new_with_config_for_tests(
        genesis_config: &GenesisConfig,
        test_config: BankTestConfig,
    ) -> Self {
        Self::new_with_paths_for_tests(
            genesis_config,
            Arc::new(RuntimeConfig::default()),
            test_config,
            Vec::new(),
        )
    }

    pub fn new_with_paths_for_tests(
        genesis_config: &GenesisConfig,
        runtime_config: Arc<RuntimeConfig>,
        test_config: BankTestConfig,
        paths: Vec<PathBuf>,
    ) -> Self {
        Self::new_with_paths(
            genesis_config,
            runtime_config,
            paths,
            None,
            None,
            false,
            Some(test_config.accounts_db_config),
            None,
            Some(Pubkey::new_unique()),
            Arc::default(),
            None,
            None,
        )
    }

    pub fn new_for_benches(genesis_config: &GenesisConfig) -> Self {
        Self::new_with_paths_for_benches(genesis_config, Vec::new())
    }

    /// Intended for use by benches only.
    /// create new bank with the given config and paths.
    pub fn new_with_paths_for_benches(genesis_config: &GenesisConfig, paths: Vec<PathBuf>) -> Self {
        Self::new_with_paths(
            genesis_config,
            Arc::<RuntimeConfig>::default(),
            paths,
            None,
            None,
            false,
            Some(ACCOUNTS_DB_CONFIG_FOR_BENCHMARKS),
            None,
            Some(Pubkey::new_unique()),
            Arc::default(),
            None,
            None,
        )
    }

    /// Prepare a transaction batch from a list of legacy transactions. Used for tests only.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn prepare_batch_for_tests(
        &self,
        txs: Vec<Transaction>,
    ) -> TransactionBatch<RuntimeTransaction<SanitizedTransaction>> {
        let sanitized_txs = txs
            .into_iter()
            .map(RuntimeTransaction::from_transaction_for_tests)
            .collect::<Vec<_>>();
        TransactionBatch::new(
            self.try_lock_accounts(&sanitized_txs),
            self,
            OwnedOrBorrowed::Owned(sanitized_txs),
        )
    }

    /// Set the initial accounts data size
    /// NOTE: This fn is *ONLY FOR TESTS*
    pub fn set_accounts_data_size_initial_for_tests(&mut self, amount: u64) {
        self.accounts_data_size_initial = amount;
    }

    /// Update the accounts data size off-chain delta
    /// NOTE: This fn is *ONLY FOR TESTS*
    pub fn update_accounts_data_size_delta_off_chain_for_tests(&self, amount: i64) {
        self.update_accounts_data_size_delta_off_chain(amount)
    }

    /// Process multiple transaction in a single batch. This is used for benches and unit tests.
    ///
    /// # Panics
    ///
    /// Panics if any of the transactions do not pass sanitization checks.
    #[must_use]
    pub fn process_transactions<'a>(
        &self,
        txs: impl Iterator<Item = &'a Transaction>,
    ) -> Vec<Result<()>> {
        self.try_process_transactions(txs).unwrap()
    }

    /// Process entry transactions in a single batch. This is used for benches and unit tests.
    ///
    /// # Panics
    ///
    /// Panics if any of the transactions do not pass sanitization checks.
    #[must_use]
    pub fn process_entry_transactions(&self, txs: Vec<VersionedTransaction>) -> Vec<Result<()>> {
        self.try_process_entry_transactions(txs).unwrap()
    }

    #[cfg(test)]
    pub fn flush_accounts_cache_slot_for_tests(&self) {
        self.rc
            .accounts
            .accounts_db
            .flush_accounts_cache_slot_for_tests(self.slot())
    }

    /// This is only valid to call from tests.
    /// block until initial accounts hash verification has completed
    pub fn wait_for_initial_accounts_hash_verification_completed_for_tests(&self) {
        self.rc
            .accounts
            .accounts_db
            .verify_accounts_hash_in_bg
            .join_background_thread()
    }

    pub fn get_sysvar_cache_for_tests(&self) -> SysvarCache {
        self.transaction_processor.get_sysvar_cache_for_tests()
    }

    pub fn calculate_accounts_lt_hash_for_tests(&self) -> AccountsLtHash {
        self.rc
            .accounts
            .accounts_db
            .calculate_accounts_lt_hash_at_startup_from_index(&self.ancestors, self.slot)
    }

    pub fn new_program_cache_for_tx_batch_for_slot(&self, slot: Slot) -> ProgramCacheForTxBatch {
        ProgramCacheForTxBatch::new_from_cache(
            slot,
            self.epoch_schedule.get_epoch(slot),
            &self.transaction_processor.program_cache.read().unwrap(),
        )
    }

    pub fn get_transaction_processor(&self) -> &TransactionBatchProcessor<BankForks> {
        &self.transaction_processor
    }

    pub fn set_fee_structure(&mut self, fee_structure: &FeeStructure) {
        self.fee_structure = fee_structure.clone();
    }

    pub fn load_program(
        &self,
        pubkey: &Pubkey,
        reload: bool,
        effective_epoch: Epoch,
    ) -> Option<Arc<ProgramCacheEntry>> {
        let environments = self
            .transaction_processor
            .get_environments_for_epoch(effective_epoch)?;
        load_program_with_pubkey(
            self,
            &environments,
            pubkey,
            self.slot(),
            &mut ExecuteTimings::default(), // Called by ledger-tool, metrics not accumulated.
            reload,
        )
    }

    pub fn withdraw(&self, pubkey: &Pubkey, lamports: u64) -> Result<()> {
        match self.get_account_with_fixed_root(pubkey) {
            Some(mut account) => {
                let min_balance = match get_system_account_kind(&account) {
                    Some(SystemAccountKind::Nonce) => self
                        .rent_collector
                        .rent
                        .minimum_balance(nonce::state::State::size()),
                    _ => 0,
                };

                lamports
                    .checked_add(min_balance)
                    .filter(|required_balance| *required_balance <= account.lamports())
                    .ok_or(TransactionError::InsufficientFundsForFee)?;
                account
                    .checked_sub_lamports(lamports)
                    .map_err(|_| TransactionError::InsufficientFundsForFee)?;
                self.store_account(pubkey, &account);

                Ok(())
            }
            None => Err(TransactionError::AccountNotFound),
        }
    }

    pub fn set_hash_overrides(&self, hash_overrides: HashOverrides) {
        *self.hash_overrides.lock().unwrap() = hash_overrides;
    }

    /// Get stake and stake node accounts
    pub(crate) fn get_stake_accounts(&self, minimized_account_set: &DashSet<Pubkey>) {
        self.stakes_cache
            .stakes()
            .stake_delegations()
            .iter()
            .for_each(|(pubkey, _)| {
                minimized_account_set.insert(*pubkey);
            });

        self.stakes_cache
            .stakes()
            .staked_nodes()
            .par_iter()
            .for_each(|(pubkey, _)| {
                minimized_account_set.insert(*pubkey);
            });
    }
}

/// Compute how much an account has changed size.  This function is useful when the data size delta
/// needs to be computed and passed to an `update_accounts_data_size_delta` function.
fn calculate_data_size_delta(old_data_size: usize, new_data_size: usize) -> i64 {
    assert!(old_data_size <= i64::MAX as usize);
    assert!(new_data_size <= i64::MAX as usize);
    let old_data_size = old_data_size as i64;
    let new_data_size = new_data_size as i64;

    new_data_size.saturating_sub(old_data_size)
}

/// Since `apply_feature_activations()` has different behavior depending on its caller, enumerate
/// those callers explicitly.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum ApplyFeatureActivationsCaller {
    FinishInit,
    NewFromParent,
    WarpFromParent,
}

impl Drop for Bank {
    fn drop(&mut self) {
        if let Some(drop_callback) = self.drop_callback.read().unwrap().0.as_ref() {
            drop_callback.callback(self);
        } else {
            // Default case for tests
            self.rc
                .accounts
                .accounts_db
                .purge_slot(self.slot(), self.bank_id(), false);
        }
    }
}

/// utility function used for testing and benchmarking.
pub mod test_utils {
    use {
        super::Bank,
        crate::installed_scheduler_pool::BankWithScheduler,
        solana_account::{ReadableAccount, WritableAccount},
        solana_instruction::error::LamportsError,
        solana_pubkey::Pubkey,
        solana_sha256_hasher::hashv,
        solana_vote_program::vote_state::{self, BlockTimestamp, VoteStateVersions},
        std::sync::Arc,
    };
    pub fn goto_end_of_slot(bank: Arc<Bank>) {
        goto_end_of_slot_with_scheduler(&BankWithScheduler::new_without_scheduler(bank))
    }

    pub fn goto_end_of_slot_with_scheduler(bank: &BankWithScheduler) {
        let mut tick_hash = bank.last_blockhash();
        loop {
            tick_hash = hashv(&[tick_hash.as_ref(), &[42]]);
            bank.register_tick(&tick_hash);
            if tick_hash == bank.last_blockhash() {
                bank.freeze();
                return;
            }
        }
    }

    pub fn update_vote_account_timestamp(
        timestamp: BlockTimestamp,
        bank: &Bank,
        vote_pubkey: &Pubkey,
    ) {
        let mut vote_account = bank.get_account(vote_pubkey).unwrap_or_default();
        let mut vote_state = vote_state::from(&vote_account).unwrap_or_default();
        vote_state.last_timestamp = timestamp;
        let versioned = VoteStateVersions::new_current(vote_state);
        vote_state::to(&versioned, &mut vote_account).unwrap();
        bank.store_account(vote_pubkey, &vote_account);
    }

    pub fn deposit(
        bank: &Bank,
        pubkey: &Pubkey,
        lamports: u64,
    ) -> std::result::Result<u64, LamportsError> {
        // This doesn't collect rents intentionally.
        // Rents should only be applied to actual TXes
        let mut account = bank
            .get_account_with_fixed_root_no_cache(pubkey)
            .unwrap_or_default();
        account.checked_add_lamports(lamports)?;
        bank.store_account(pubkey, &account);
        Ok(account.lamports())
    }
}
