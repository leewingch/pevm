use ahash::{AHashMap, HashMapExt};
use alloy_rpc_types::Receipt;
use dashmap::DashMap;
use defer_drop::DeferDrop;
use revm::{
    primitives::{
        AccountInfo, Address, BlockEnv, Bytecode, CfgEnv, EVMError, Env, InvalidTransaction,
        ResultAndState, SpecId, TransactTo, TxEnv, B256, KECCAK_EMPTY, U256,
    },
    Context, Database, Evm, EvmContext,
};
use std::collections::HashMap;

use crate::{
    chain::{PevmChain, RewardPolicy},
    mv_memory::MvMemory,
    AccountBasic, BuildIdentityHasher, EvmAccount, MemoryEntry, MemoryLocation, MemoryLocationHash,
    MemoryValue, NewLazyAddresses, ReadError, ReadOrigin, ReadSet, Storage, TxIdx, TxVersion,
    WriteSet,
};

/// The execution error from the underlying EVM executor.
// Will there be DB errors outside of read?
pub type ExecutionError = EVMError<ReadError>;

/// Represents the state transitions of the EVM accounts after execution.
/// If the value is [None], it indicates that the account is marked for removal.
/// If the value is [Some(new_state)], it indicates that the account has become [new_state].
type EvmStateTransitions = AHashMap<Address, Option<EvmAccount>>;

/// Execution result of a transaction
#[derive(Debug, Clone, PartialEq)]
pub struct PevmTxExecutionResult {
    /// Receipt of execution
    // TODO: Consider promoting to [ReceiptEnvelope] if there is high demand
    pub receipt: Receipt,
    /// State that got updated
    pub state: EvmStateTransitions,
}

impl PevmTxExecutionResult {
    /// Construct a Pevm execution result from a raw Revm result.
    /// Note that [cumulative_gas_used] is preset to the gas used of this transaction.
    /// It should be post-processed with the remaining transactions in the block.
    pub fn from_revm(spec_id: SpecId, ResultAndState { result, state }: ResultAndState) -> Self {
        Self {
            receipt: Receipt {
                status: result.is_success().into(),
                cumulative_gas_used: result.gas_used() as u128,
                logs: result.into_logs(),
            },
            state: state
                .into_iter()
                .filter(|(_, account)| account.is_touched())
                .map(|(address, account)| {
                    if account.is_selfdestructed()
                    // https://github.com/ethereum/EIPs/blob/96523ef4d76ca440f73f0403ddb5c9cb3b24dcae/EIPS/eip-161.md
                    || account.is_empty() && spec_id.is_enabled_in(SpecId::SPURIOUS_DRAGON)
                    {
                        (address, None)
                    } else {
                        (address, Some(EvmAccount::from(account)))
                    }
                })
                .collect(),
        }
    }
}

// TODO: Rewrite as [Result]
pub(crate) enum VmExecutionResult {
    Retry,
    FallbackToSequential,
    ReadError {
        blocking_tx_idx: TxIdx,
    },
    ExecutionError(ExecutionError),
    Ok {
        execution_result: PevmTxExecutionResult,
        read_set: ReadSet,
        write_set: WriteSet,
        lazy_addresses: NewLazyAddresses,
        // From which transaction index do we need to validate from after
        // this execution. This is [None] when no validation is required.
        // For instance, for transactions that only read and write to the
        // from and to addresses, which preprocessing & lazy evaluation has
        // already covered. Note that this is used to set the min validation
        // index in the scheduler, meaning a `None` here will still be validated
        // if there was a lower transaction that has broken the preprocessed
        // dependency chain and returned [Some].
        // TODO: Better name & doc
        next_validation_idx: Option<TxIdx>,
    },
}

// A database interface that intercepts reads while executing a specific
// transaction with Revm. It provides values from the multi-version data
// structure & storage, and tracks the read set of the current execution.
// TODO: Simplify this type, like grouping [from] and [to] into a
// [preprocessed_addresses] or a [preprocessed_locations] vector.
struct VmDb<'a, S: Storage, C: PevmChain> {
    vm: &'a Vm<'a, S, C>,
    tx_idx: &'a TxIdx,
    nonce: u64,
    from: &'a Address,
    from_hash: MemoryLocationHash,
    to: Option<&'a Address>,
    to_hash: Option<MemoryLocationHash>,
    // Indicates if we lazy update this transaction.
    // Only applied to raw transfers' senders & recipients at the moment.
    is_lazy: bool,
    read_set: ReadSet,
    // TODO: Clearer type for [AccountBasic] plus code hash
    read_accounts: HashMap<MemoryLocationHash, (AccountBasic, Option<B256>), BuildIdentityHasher>,
}

impl<'a, S: Storage, C: PevmChain> VmDb<'a, S, C> {
    fn new(
        vm: &'a Vm<'a, S, C>,
        tx_idx: &'a TxIdx,
        nonce: u64,
        from: &'a Address,
        from_hash: MemoryLocationHash,
        to: Option<&'a Address>,
        to_hash: Option<MemoryLocationHash>,
    ) -> Result<Self, ReadError> {
        let mut db = Self {
            vm,
            tx_idx,
            nonce,
            from,
            from_hash,
            to,
            to_hash,
            is_lazy: false,
            // Unless it is a raw transfer that is lazy updated, we'll
            // read at least from the sender and recipient accounts.
            read_set: ReadSet::with_capacity(2),
            read_accounts: HashMap::with_capacity_and_hasher(2, BuildIdentityHasher::default()),
        };
        // We only lazy update raw transfers that already have the sender
        // or recipient in [MvMemory] since sequentially evaluating memory
        // locations with only one entry is much costlier than fully
        // evaluating it concurrently.
        // TODO: Re-order conditions (put most likely true condition ahead)
        // to avoid [MvMemory] locks.
        // TODO: Only lazy update in block syncing mode, not for block
        // building.
        if let Some(to) = to {
            db.is_lazy = (vm.mv_memory.have_location(&from_hash)
                || vm.mv_memory.have_location(&to_hash.unwrap()))
                && db.get_code_hash(*to)?.is_none();
        }
        Ok(db)
    }

    fn hash_basic(&self, address: &Address) -> MemoryLocationHash {
        if address == self.from {
            self.from_hash
        } else if Some(address) == self.to {
            self.to_hash.unwrap()
        } else {
            self.vm.hash_basic(address)
        }
    }

    fn get_code_hash(&mut self, address: Address) -> Result<Option<B256>, ReadError> {
        let location_hash = self.vm.hasher.hash_one(MemoryLocation::CodeHash(address));
        let read_origins = self.read_set.entry(location_hash).or_default();
        let prev_origin = read_origins.last();

        // Try to read the latest code hash in [MvMemory]
        // TODO: Memoize read locations (expected to be small) here in [Vm] to avoid
        // contention in [MvMemory]
        if let Some(written_transactions) = self.vm.mv_memory.read_location(&location_hash) {
            if let Some((
                tx_idx,
                MemoryEntry::Data(tx_incarnation, MemoryValue::CodeHash(code_hash)),
            )) = written_transactions.range(..self.tx_idx).next_back()
            {
                if code_hash.is_none() {
                    return Err(ReadError::SelfDestructedAccount);
                }
                let origin = ReadOrigin::MvMemory(TxVersion {
                    tx_idx: *tx_idx,
                    tx_incarnation: *tx_incarnation,
                });
                if let Some(prev_origin) = prev_origin {
                    if prev_origin != &origin {
                        return Err(ReadError::InconsistentRead);
                    }
                } else {
                    read_origins.push(origin);
                }
                return Ok(*code_hash);
            }
        };

        // Fallback to storage
        if let Some(prev_origin) = prev_origin {
            if prev_origin != &ReadOrigin::Storage {
                return Err(ReadError::InconsistentRead);
            }
        } else {
            read_origins.push(ReadOrigin::Storage);
        }
        self.vm
            .storage
            .code_hash(&address)
            .map_err(|err| ReadError::StorageError(err.to_string()))
    }
}

impl<'a, S: Storage, C: PevmChain> Database for VmDb<'a, S, C> {
    type Error = ReadError;

    // TODO: More granularity here to ensure we only record dependencies for,
    // say, only an account's balance instead of the whole account info.
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let location_hash = self.hash_basic(&address);

        // We return a mock for non-contract addresses (for lazy updates) to avoid
        // unncessarily evaluating its balance here.
        if self.is_lazy {
            if location_hash == self.from_hash {
                return Ok(Some(AccountInfo {
                    nonce: self.nonce,
                    balance: U256::MAX,
                    code: None,
                    code_hash: KECCAK_EMPTY,
                }));
            } else if Some(location_hash) == self.to_hash {
                return Ok(None);
            }
        }

        let read_origins = self.read_set.entry(location_hash).or_default();
        let has_prev_origins = !read_origins.is_empty();
        // We accumulate new origins to either:
        // - match with the previous origins to check consistency
        // - register origins on the first read
        let mut new_origins = Vec::new();

        let mut final_account = None;
        let mut balance_addition = U256::ZERO;
        // The sign of [balance_addition] since it can be negative for lazy senders.
        let mut positive_addition = true;
        let mut nonce_addition = 0;

        // Try reading from multi-version data
        if self.tx_idx > &0 {
            // We enforce consecutive indexes for locations that all transactions write to like
            // the beneficiary balance. The goal is to not wastefully evaluate when we know
            // we're missing data -- let's just depend on the missing data instead.
            let need_consecutive_idxs = location_hash == self.vm.beneficiary_location_hash;
            // While we can depend on the precise missing transaction index (known during lazy evaluation),
            // through benchmark constantly retrying via the previous transaction index performs much better.
            // TODO: Fine-tune this now that we can also retry directly without waiting for a lower tx.
            let reschedule = Err(ReadError::BlockingIndex(self.tx_idx - 1));

            if let Some(written_transactions) = self.vm.mv_memory.read_location(&location_hash) {
                let mut current_idx = self.tx_idx;
                let mut iter = written_transactions.range(..current_idx);

                // Fully evaluate lazy updates
                loop {
                    match iter.next_back() {
                        Some((blocking_idx, MemoryEntry::Estimate)) => {
                            return if need_consecutive_idxs {
                                reschedule
                            } else {
                                Err(ReadError::BlockingIndex(*blocking_idx))
                            }
                        }
                        Some((closest_idx, MemoryEntry::Data(tx_incarnation, value))) => {
                            if need_consecutive_idxs && closest_idx != &(current_idx - 1) {
                                return reschedule;
                            }
                            // About to push a new origin
                            // Inconsistent: new origin will be longer than the previous!
                            if has_prev_origins && read_origins.len() == new_origins.len() {
                                return Err(ReadError::InconsistentRead);
                            }
                            let origin = ReadOrigin::MvMemory(TxVersion {
                                tx_idx: *closest_idx,
                                tx_incarnation: *tx_incarnation,
                            });
                            // Inconsistent: new origin is different from the previous!
                            if has_prev_origins
                                && unsafe { read_origins.get_unchecked(new_origins.len()) }
                                    != &origin
                            {
                                return Err(ReadError::InconsistentRead);
                            }
                            new_origins.push(origin);
                            match value {
                                MemoryValue::Basic(basic) => {
                                    if basic.is_none() {
                                        return Err(ReadError::SelfDestructedAccount);
                                    }
                                    final_account.clone_from(basic);
                                    break;
                                }
                                MemoryValue::LazyRecipient(addition) => {
                                    if positive_addition {
                                        balance_addition += addition;
                                    } else {
                                        positive_addition = *addition >= balance_addition;
                                        balance_addition = balance_addition.abs_diff(*addition);
                                    }
                                    current_idx = closest_idx;
                                }
                                MemoryValue::LazySender(subtraction) => {
                                    if positive_addition {
                                        positive_addition = balance_addition >= *subtraction;
                                        balance_addition = balance_addition.abs_diff(*subtraction);
                                    } else {
                                        balance_addition += subtraction;
                                    }
                                    nonce_addition += 1;
                                    current_idx = closest_idx;
                                }
                                _ => return Err(ReadError::InvalidMemoryLocationType),
                            }
                        }
                        None => {
                            if need_consecutive_idxs && current_idx > &0 {
                                return reschedule;
                            }
                            break;
                        }
                    }
                }
            } else if need_consecutive_idxs {
                return reschedule;
            }
        }

        // Fall back to storage
        if final_account.is_none() {
            // Populate [Storage] on the first read
            if !has_prev_origins {
                new_origins.push(ReadOrigin::Storage);
            }
            // Inconsistent: previous origin is longer or didn't read
            // from storage for the last origin.
            else if read_origins.len() != new_origins.len() + 1
                || read_origins.last() != Some(&ReadOrigin::Storage)
            {
                return Err(ReadError::InconsistentRead);
            }
            final_account = match self.vm.storage.basic(&address) {
                Ok(Some(basic)) => Some(basic),
                Ok(None) => {
                    if balance_addition > U256::ZERO {
                        Some(AccountBasic::default())
                    } else {
                        None
                    }
                }
                Err(err) => return Err(ReadError::StorageError(err.to_string())),
            };
        }

        // Populate read origins on the first read.
        // Otherwise [read_origins] matches [new_origins] already.
        if !has_prev_origins {
            *read_origins = new_origins;
        }

        if let Some(mut account) = final_account {
            // Check sender nonce
            account.nonce += nonce_addition;
            if location_hash == self.from_hash && account.nonce != self.nonce {
                if self.tx_idx > &0 {
                    // TODO: Better retry strategy -- immediately, to the
                    // closest sender tx, to the missing sender tx, etc.
                    return Err(ReadError::BlockingIndex(self.tx_idx - 1));
                } else {
                    return Err(ReadError::InvalidNonce);
                }
            }

            // Fully evaluate the account and register it to read cache
            // to later check if they have changed (been written to).
            if positive_addition {
                account.balance += balance_addition;
            } else {
                account.balance -= balance_addition;
            };

            let code_hash = self.get_code_hash(address)?;
            let code = if let Some(code_hash) = &code_hash {
                if let Some(code) = self.vm.new_bytecodes.get(code_hash) {
                    Some(code.clone())
                } else {
                    match self.vm.storage.code_by_hash(code_hash) {
                        Ok(code) => code.map(Bytecode::from),
                        Err(err) => return Err(ReadError::StorageError(err.to_string())),
                    }
                }
            } else {
                None
            };
            self.read_accounts
                .insert(location_hash, (account.clone(), code_hash));

            return Ok(Some(AccountInfo {
                balance: account.balance,
                nonce: account.nonce,
                code_hash: code_hash.unwrap_or(KECCAK_EMPTY),
                code,
            }));
        }

        Ok(None)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.vm
            .storage
            .code_by_hash(&code_hash)
            .map(|code| code.map(Bytecode::from).unwrap_or_default())
            .map_err(|err| ReadError::StorageError(err.to_string()))
    }

    fn has_storage(&mut self, address: Address) -> Result<bool, Self::Error> {
        self.vm
            .storage
            .has_storage(&address)
            .map_err(|err| ReadError::StorageError(err.to_string()))
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let location_hash = self
            .vm
            .hasher
            .hash_one(MemoryLocation::Storage(address, index));

        let read_origins = self.read_set.entry(location_hash).or_default();
        let prev_origin = read_origins.last();

        // Try reading from multi-version data
        if self.tx_idx > &0 {
            if let Some(written_transactions) = self.vm.mv_memory.read_location(&location_hash) {
                if let Some((closest_idx, entry)) =
                    written_transactions.range(..self.tx_idx).next_back()
                {
                    match entry {
                        MemoryEntry::Data(tx_incarnation, MemoryValue::Storage(value)) => {
                            let origin = ReadOrigin::MvMemory(TxVersion {
                                tx_idx: *closest_idx,
                                tx_incarnation: *tx_incarnation,
                            });
                            if let Some(prev_origin) = prev_origin {
                                if prev_origin != &origin {
                                    return Err(ReadError::InconsistentRead);
                                }
                            } else {
                                read_origins.push(origin);
                            }
                            return Ok(*value);
                        }
                        MemoryEntry::Estimate => {
                            return Err(ReadError::BlockingIndex(*closest_idx))
                        }
                        _ => return Err(ReadError::InvalidMemoryLocationType),
                    }
                }
            }
        }

        // Fall back to storage
        if let Some(prev_origin) = prev_origin {
            if prev_origin != &ReadOrigin::Storage {
                return Err(ReadError::InconsistentRead);
            }
        } else {
            read_origins.push(ReadOrigin::Storage);
        }
        self.vm
            .storage
            .storage(&address, &index)
            .map_err(|err| ReadError::StorageError(err.to_string()))
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.vm
            .storage
            .block_hash(&number)
            .map_err(|err| ReadError::StorageError(err.to_string()))
    }
}

pub(crate) struct Vm<'a, S: Storage, C: PevmChain> {
    hasher: &'a ahash::RandomState,
    storage: &'a S,
    mv_memory: &'a MvMemory,
    chain: &'a C,
    block_env: &'a BlockEnv,
    txs: &'a [TxEnv],
    spec_id: SpecId,
    beneficiary_location_hash: MemoryLocationHash,
    reward_policy: RewardPolicy,
    new_bytecodes: DeferDrop<DashMap<B256, Bytecode>>,
}

impl<'a, S: Storage, C: PevmChain> Vm<'a, S, C> {
    pub(crate) fn new(
        hasher: &'a ahash::RandomState,
        storage: &'a S,
        mv_memory: &'a MvMemory,
        chain: &'a C,
        block_env: &'a BlockEnv,
        txs: &'a [TxEnv],
        spec_id: SpecId,
    ) -> Self {
        Self {
            hasher,
            storage,
            mv_memory,
            chain,
            block_env,
            txs,
            spec_id,
            beneficiary_location_hash: hasher.hash_one(MemoryLocation::Basic(block_env.coinbase)),
            reward_policy: chain.get_reward_policy(hasher),
            // TODO: Fine-tune the number of shards, like to the next number of two from the
            // number of worker threads.
            new_bytecodes: DeferDrop::new(DashMap::default()),
        }
    }

    #[inline(always)]
    fn hash_basic(&self, address: &Address) -> MemoryLocationHash {
        self.hasher.hash_one(MemoryLocation::Basic(*address))
    }

    // Execute a transaction. This can read from memory but cannot modify any state.
    // A successful execution returns:
    //   - A write-set consisting of memory locations and their updated values.
    //   - A read-set consisting of memory locations and their origins.
    //
    // An execution may observe a read dependency on a lower transaction. This happens
    // when the last incarnation of the dependency wrote to a memory location that
    // this transaction reads, but it aborted before the read. In this case, the
    // dependency index is returned via [blocking_tx_idx]. An execution task for this
    // transaction is re-scheduled after the blocking dependency finishes its
    // next incarnation.
    //
    // When a transaction attempts to write a value to a location, the location and
    // value are added to the write set, possibly replacing a pair with a prior value
    // (if it is not the first time the transaction wrote to this location during the
    // execution).
    pub(crate) fn execute(&self, tx_idx: TxIdx) -> VmExecutionResult {
        // SAFETY: A correct scheduler would guarantee this index to be inbound.
        let tx = unsafe { self.txs.get_unchecked(tx_idx) };
        let from = &tx.caller;
        let from_hash = self.hash_basic(from);
        let (to, to_hash) = match &tx.transact_to {
            TransactTo::Call(address) => (Some(address), Some(self.hash_basic(address))),
            TransactTo::Create => (None, None),
        };

        // Execute
        let mut db = match VmDb::new(
            self,
            &tx_idx,
            tx.nonce.unwrap_or(1),
            from,
            from_hash,
            to,
            to_hash,
        ) {
            Ok(db) => db,
            // TODO: Handle different errors differently
            Err(_) => return VmExecutionResult::FallbackToSequential,
        };
        // TODO: Share as much Evm, Context, Handler, etc. among threads as possible
        // as creating them is very expensive.
        let mut evm = build_evm(
            &mut db,
            self.chain,
            self.spec_id,
            self.block_env.clone(),
            false,
        );
        *evm.tx_mut() = tx.clone();
        match evm.transact() {
            Ok(result_and_state) => {
                // There are at least three locations most of the time: the sender,
                // the recipient, and the beneficiary accounts.
                // TODO: Allocate up to [result_and_state.state.len()] anyway?
                let mut write_set = WriteSet::with_capacity(3);
                let mut lazy_addresses = NewLazyAddresses::new();
                for (address, account) in result_and_state.state.iter() {
                    if account.is_selfdestructed() {
                        write_set.push((self.hash_basic(address), MemoryValue::Basic(None)));
                        write_set.push((
                            self.hasher.hash_one(MemoryLocation::CodeHash(*address)),
                            MemoryValue::CodeHash(None),
                        ));
                        continue;
                    }

                    if account.is_touched() {
                        let account_location_hash = self.hash_basic(address);
                        let read_account = evm.db().read_accounts.get(&account_location_hash);

                        let has_code = !account.info.is_empty_code_hash();
                        let is_new_code = has_code
                            && read_account.map_or(true, |(_, code_hash)| code_hash.is_none());

                        // Write new account changes
                        if is_new_code
                            || read_account.is_none()
                            || read_account.is_some_and(|(basic, _)| {
                                basic.nonce != account.info.nonce
                                    || basic.balance != account.info.balance
                            })
                        {
                            if evm.db().is_lazy {
                                if account_location_hash == from_hash {
                                    write_set.push((
                                        account_location_hash,
                                        MemoryValue::LazySender(U256::MAX - account.info.balance),
                                    ));
                                } else if Some(account_location_hash) == to_hash {
                                    write_set.push((
                                        account_location_hash,
                                        MemoryValue::LazyRecipient(tx.value),
                                    ));
                                }
                                lazy_addresses.push(*address);
                            } else {
                                // TODO: More granularity here to ensure we only notify new
                                // memory writes, for instance, only an account's balance instead
                                // of the whole account.
                                write_set.push((
                                    account_location_hash,
                                    MemoryValue::Basic(Some(AccountBasic {
                                        balance: account.info.balance,
                                        nonce: account.info.nonce,
                                    })),
                                ));
                            }
                        }

                        // Write new contract
                        if is_new_code {
                            write_set.push((
                                self.hasher.hash_one(MemoryLocation::CodeHash(*address)),
                                MemoryValue::CodeHash(Some(account.info.code_hash)),
                            ));
                            self.new_bytecodes
                                .entry(account.info.code_hash)
                                .or_insert_with(|| account.info.code.clone().unwrap());
                        }
                    }

                    // TODO: We should move this changed check to our read set like for account info?
                    for (slot, value) in account.changed_storage_slots() {
                        write_set.push((
                            self.hasher
                                .hash_one(MemoryLocation::Storage(*address, *slot)),
                            MemoryValue::Storage(value.present_value),
                        ));
                    }
                }

                self.apply_rewards(
                    &mut write_set,
                    tx,
                    U256::from(result_and_state.result.gas_used()),
                );

                drop(evm); // release db

                VmExecutionResult::Ok {
                    execution_result: PevmTxExecutionResult::from_revm(
                        self.spec_id,
                        result_and_state,
                    ),
                    read_set: db.read_set,
                    write_set,
                    lazy_addresses,
                    next_validation_idx: if tx_idx == 0 || db.is_lazy {
                        None
                    } else {
                        Some(tx_idx)
                    },
                }
            }
            Err(EVMError::Database(ReadError::InconsistentRead)) => VmExecutionResult::Retry,
            Err(EVMError::Database(ReadError::SelfDestructedAccount)) => {
                VmExecutionResult::FallbackToSequential
            }
            Err(EVMError::Database(ReadError::BlockingIndex(blocking_tx_idx))) => {
                VmExecutionResult::ReadError { blocking_tx_idx }
            }
            Err(err) => {
                // Optimistically retry in case some previous internal transactions send
                // more fund to the sender but hasn't been executed yet.
                // TODO: Let users define this behaviour through a mode enum or something.
                // Since this retry is safe for syncing canonical blocks but can deadlock
                // on new or faulty blocks. We can skip the transaction for new blocks and
                // error out after a number of tries for the latter.
                if tx_idx > 0
                    && matches!(
                        err,
                        EVMError::Transaction(InvalidTransaction::LackOfFundForMaxFee { .. })
                            | EVMError::Transaction(InvalidTransaction::NonceTooHigh { .. })
                    )
                {
                    VmExecutionResult::ReadError {
                        blocking_tx_idx: tx_idx - 1,
                    }
                } else {
                    VmExecutionResult::ExecutionError(err)
                }
            }
        }
    }

    // Apply rewards (balance increments) to beneficiary accounts, etc.
    fn apply_rewards(&self, write_set: &mut WriteSet, tx: &TxEnv, gas_used: U256) {
        let rewards: Vec<(MemoryLocationHash, U256)> = match self.reward_policy {
            RewardPolicy::Ethereum => {
                let mut gas_price = if let Some(priority_fee) = tx.gas_priority_fee {
                    std::cmp::min(tx.gas_price, priority_fee + self.block_env.basefee)
                } else {
                    tx.gas_price
                };
                if self.spec_id.is_enabled_in(SpecId::LONDON) {
                    gas_price = gas_price.saturating_sub(self.block_env.basefee);
                }
                vec![(self.beneficiary_location_hash, gas_price * gas_used)]
            }
        };

        for (recipient, amount) in rewards {
            if let Some((_, value)) = write_set
                .iter_mut()
                .find(|(location, _)| location == &recipient)
            {
                match value {
                    MemoryValue::Basic(basic) => {
                        basic.get_or_insert(AccountBasic::default()).balance += amount
                    }
                    MemoryValue::LazySender(addition) => *addition -= amount,
                    MemoryValue::LazyRecipient(addition) => *addition += amount,
                    _ => unreachable!(), // TODO: Better error handling
                }
            } else {
                write_set.push((recipient, MemoryValue::LazyRecipient(amount)));
            }
        }
    }
}

pub(crate) fn build_evm<'a, DB: Database, C: PevmChain>(
    db: DB,
    chain: &C,
    spec_id: SpecId,
    block_env: BlockEnv,
    with_reward_beneficiary: bool,
) -> Evm<'a, (), DB> {
    // This is much uglier than the builder interface but can be up to 50% faster!!
    let context = Context {
        evm: EvmContext::new_with_env(
            db,
            Env::boxed(
                CfgEnv::default().with_chain_id(chain.id()),
                block_env,
                TxEnv::default(),
            ),
        ),
        external: (),
    };

    let handler = chain.get_handler(spec_id, with_reward_beneficiary);
    Evm::new(context, handler)
}
