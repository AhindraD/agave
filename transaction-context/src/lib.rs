//! Data shared between program runtime and built-in programs as well as SBF programs.
#![deny(clippy::indexing_slicing)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

#[cfg(not(target_os = "solana"))]
use {solana_account::WritableAccount, solana_rent::Rent};
use {
    solana_account::{AccountSharedData, ReadableAccount},
    solana_instruction::error::InstructionError,
    solana_instructions_sysvar as instructions,
    solana_pubkey::Pubkey,
    solana_sbpf::memory_region::{AccessType, AccessViolationHandler, MemoryRegion},
    std::{
        cell::{Ref, RefCell, RefMut},
        collections::HashSet,
        pin::Pin,
        rc::Rc,
    },
};

// Inlined to avoid solana_system_interface dep
#[cfg(not(target_os = "solana"))]
const MAX_PERMITTED_DATA_LENGTH: u64 = 10 * 1024 * 1024;
#[cfg(test)]
static_assertions::const_assert_eq!(
    MAX_PERMITTED_DATA_LENGTH,
    solana_system_interface::MAX_PERMITTED_DATA_LENGTH
);

// Inlined to avoid solana_system_interface dep
#[cfg(not(target_os = "solana"))]
const MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION: i64 =
    MAX_PERMITTED_DATA_LENGTH as i64 * 2;
// Note: With direct mapping programs can grow accounts faster than they intend to,
// because the AccessViolationHandler might grow an account up to
// MAX_PERMITTED_DATA_LENGTH at once.
#[cfg(test)]
static_assertions::const_assert_eq!(
    MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION,
    solana_system_interface::MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION
);

// Inlined to avoid solana_account_info dep
#[cfg(not(target_os = "solana"))]
const MAX_PERMITTED_DATA_INCREASE: usize = 1_024 * 10;
#[cfg(test)]
static_assertions::const_assert_eq!(
    MAX_PERMITTED_DATA_INCREASE,
    solana_account_info::MAX_PERMITTED_DATA_INCREASE
);

/// Index of an account inside of the TransactionContext or an InstructionContext.
pub type IndexOfAccount = u16;

/// Contains account meta data which varies between instruction.
///
/// It also contains indices to other structures for faster lookup.
#[repr(C)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionAccount {
    /// Points to the account and its key in the `TransactionContext`
    pub index_in_transaction: IndexOfAccount,
    /// Points to the first occurrence in the current `InstructionContext`
    ///
    /// This excludes the program accounts.
    pub index_in_callee: IndexOfAccount,
    /// Is this account supposed to sign
    is_signer: u8,
    /// Is this account allowed to become writable
    is_writable: u8,
}

impl InstructionAccount {
    pub fn new(
        index_in_transaction: IndexOfAccount,
        index_in_callee: IndexOfAccount,
        is_signer: bool,
        is_writable: bool,
    ) -> InstructionAccount {
        InstructionAccount {
            index_in_transaction,
            index_in_callee,
            is_signer: is_signer as u8,
            is_writable: is_writable as u8,
        }
    }

    pub fn is_signer(&self) -> bool {
        self.is_signer != 0
    }

    pub fn is_writable(&self) -> bool {
        self.is_writable != 0
    }

    pub fn set_is_signer(&mut self, value: bool) {
        self.is_signer = value as u8;
    }

    pub fn set_is_writable(&mut self, value: bool) {
        self.is_writable = value as u8;
    }
}

/// An account key and the matching account
pub type TransactionAccount = (Pubkey, AccountSharedData);

#[derive(Clone, Debug, PartialEq)]
pub struct TransactionAccounts {
    accounts: Vec<RefCell<AccountSharedData>>,
    touched_flags: RefCell<Box<[bool]>>,
    resize_delta: RefCell<i64>,
}

impl TransactionAccounts {
    #[cfg(not(target_os = "solana"))]
    fn new(accounts: Vec<RefCell<AccountSharedData>>) -> TransactionAccounts {
        let touched_flags = vec![false; accounts.len()].into_boxed_slice();
        TransactionAccounts {
            accounts,
            touched_flags: RefCell::new(touched_flags),
            resize_delta: RefCell::new(0),
        }
    }

    fn len(&self) -> usize {
        self.accounts.len()
    }

    fn get(&self, index: IndexOfAccount) -> Option<&RefCell<AccountSharedData>> {
        self.accounts.get(index as usize)
    }

    #[cfg(not(target_os = "solana"))]
    pub fn touch(&self, index: IndexOfAccount) -> Result<(), InstructionError> {
        *self
            .touched_flags
            .borrow_mut()
            .get_mut(index as usize)
            .ok_or(InstructionError::NotEnoughAccountKeys)? = true;
        Ok(())
    }

    fn update_accounts_resize_delta(
        &self,
        old_len: usize,
        new_len: usize,
    ) -> Result<(), InstructionError> {
        let mut accounts_resize_delta = self
            .resize_delta
            .try_borrow_mut()
            .map_err(|_| InstructionError::GenericError)?;
        *accounts_resize_delta =
            accounts_resize_delta.saturating_add((new_len as i64).saturating_sub(old_len as i64));
        Ok(())
    }

    fn can_data_be_resized(&self, old_len: usize, new_len: usize) -> Result<(), InstructionError> {
        // The new length can not exceed the maximum permitted length
        if new_len > MAX_PERMITTED_DATA_LENGTH as usize {
            return Err(InstructionError::InvalidRealloc);
        }
        // The resize can not exceed the per-transaction maximum
        let length_delta = (new_len as i64).saturating_sub(old_len as i64);
        if self
            .resize_delta
            .try_borrow()
            .map_err(|_| InstructionError::GenericError)
            .map(|value_ref| *value_ref)?
            .saturating_add(length_delta)
            > MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION
        {
            return Err(InstructionError::MaxAccountsDataAllocationsExceeded);
        }
        Ok(())
    }

    pub fn try_borrow(
        &self,
        index: IndexOfAccount,
    ) -> Result<Ref<'_, AccountSharedData>, InstructionError> {
        self.accounts
            .get(index as usize)
            .ok_or(InstructionError::MissingAccount)?
            .try_borrow()
            .map_err(|_| InstructionError::AccountBorrowFailed)
    }
}

/// Loaded transaction shared between runtime and programs.
///
/// This context is valid for the entire duration of a transaction being processed.
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionContext {
    account_keys: Pin<Box<[Pubkey]>>,
    accounts: Rc<TransactionAccounts>,
    instruction_stack_capacity: usize,
    instruction_trace_capacity: usize,
    instruction_stack: Vec<usize>,
    instruction_trace: Vec<InstructionContext>,
    top_level_instruction_index: usize,
    return_data: TransactionReturnData,
    #[cfg(not(target_os = "solana"))]
    remove_accounts_executable_flag_checks: bool,
    #[cfg(not(target_os = "solana"))]
    rent: Rent,
}

impl TransactionContext {
    /// Constructs a new TransactionContext
    #[cfg(not(target_os = "solana"))]
    pub fn new(
        transaction_accounts: Vec<TransactionAccount>,
        rent: Rent,
        instruction_stack_capacity: usize,
        instruction_trace_capacity: usize,
    ) -> Self {
        let (account_keys, accounts): (Vec<_>, Vec<_>) = transaction_accounts
            .into_iter()
            .map(|(key, account)| (key, RefCell::new(account)))
            .unzip();
        Self {
            account_keys: Pin::new(account_keys.into_boxed_slice()),
            accounts: Rc::new(TransactionAccounts::new(accounts)),
            instruction_stack_capacity,
            instruction_trace_capacity,
            instruction_stack: Vec::with_capacity(instruction_stack_capacity),
            instruction_trace: vec![InstructionContext::default()],
            top_level_instruction_index: 0,
            return_data: TransactionReturnData::default(),
            remove_accounts_executable_flag_checks: true,
            rent,
        }
    }

    #[cfg(not(target_os = "solana"))]
    pub fn set_remove_accounts_executable_flag_checks(&mut self, enabled: bool) {
        self.remove_accounts_executable_flag_checks = enabled;
    }

    /// Used in mock_process_instruction
    #[cfg(not(target_os = "solana"))]
    pub fn deconstruct_without_keys(self) -> Result<Vec<AccountSharedData>, InstructionError> {
        if !self.instruction_stack.is_empty() {
            return Err(InstructionError::CallDepth);
        }

        Ok(Rc::try_unwrap(self.accounts)
            .expect("transaction_context.accounts has unexpected outstanding refs")
            .accounts
            .into_iter()
            .map(RefCell::into_inner)
            .collect())
    }

    #[cfg(not(target_os = "solana"))]
    pub fn accounts(&self) -> &Rc<TransactionAccounts> {
        &self.accounts
    }

    /// Returns the total number of accounts loaded in this Transaction
    pub fn get_number_of_accounts(&self) -> IndexOfAccount {
        self.accounts.len() as IndexOfAccount
    }

    /// Searches for an account by its key
    pub fn get_key_of_account_at_index(
        &self,
        index_in_transaction: IndexOfAccount,
    ) -> Result<&Pubkey, InstructionError> {
        self.account_keys
            .get(index_in_transaction as usize)
            .ok_or(InstructionError::NotEnoughAccountKeys)
    }

    /// Searches for an account by its key
    #[cfg(all(
        not(target_os = "solana"),
        any(test, feature = "dev-context-only-utils")
    ))]
    pub fn get_account_at_index(
        &self,
        index_in_transaction: IndexOfAccount,
    ) -> Result<&RefCell<AccountSharedData>, InstructionError> {
        self.accounts
            .get(index_in_transaction)
            .ok_or(InstructionError::NotEnoughAccountKeys)
    }

    /// Searches for an account by its key
    pub fn find_index_of_account(&self, pubkey: &Pubkey) -> Option<IndexOfAccount> {
        self.account_keys
            .iter()
            .position(|key| key == pubkey)
            .map(|index| index as IndexOfAccount)
    }

    /// Searches for a program account by its key
    pub fn find_index_of_program_account(&self, pubkey: &Pubkey) -> Option<IndexOfAccount> {
        self.account_keys
            .iter()
            .rposition(|key| key == pubkey)
            .map(|index| index as IndexOfAccount)
    }

    /// Gets the max length of the InstructionContext trace
    pub fn get_instruction_trace_capacity(&self) -> usize {
        self.instruction_trace_capacity
    }

    /// Returns the instruction trace length.
    ///
    /// Not counting the last empty InstructionContext which is always pre-reserved for the next instruction.
    /// See also `get_next_instruction_context()`.
    pub fn get_instruction_trace_length(&self) -> usize {
        self.instruction_trace.len().saturating_sub(1)
    }

    /// Gets an InstructionContext by its index in the trace
    pub fn get_instruction_context_at_index_in_trace(
        &self,
        index_in_trace: usize,
    ) -> Result<&InstructionContext, InstructionError> {
        self.instruction_trace
            .get(index_in_trace)
            .ok_or(InstructionError::CallDepth)
    }

    /// Gets an InstructionContext by its nesting level in the stack
    pub fn get_instruction_context_at_nesting_level(
        &self,
        nesting_level: usize,
    ) -> Result<&InstructionContext, InstructionError> {
        let index_in_trace = *self
            .instruction_stack
            .get(nesting_level)
            .ok_or(InstructionError::CallDepth)?;
        let instruction_context = self.get_instruction_context_at_index_in_trace(index_in_trace)?;
        debug_assert_eq!(instruction_context.nesting_level, nesting_level);
        Ok(instruction_context)
    }

    /// Gets the max height of the InstructionContext stack
    pub fn get_instruction_stack_capacity(&self) -> usize {
        self.instruction_stack_capacity
    }

    /// Gets instruction stack height, top-level instructions are height
    /// `solana_instruction::TRANSACTION_LEVEL_STACK_HEIGHT`
    pub fn get_instruction_context_stack_height(&self) -> usize {
        self.instruction_stack.len()
    }

    /// Returns the current InstructionContext
    pub fn get_current_instruction_context(&self) -> Result<&InstructionContext, InstructionError> {
        let level = self
            .get_instruction_context_stack_height()
            .checked_sub(1)
            .ok_or(InstructionError::CallDepth)?;
        self.get_instruction_context_at_nesting_level(level)
    }

    /// Returns the mutable InstructionContext to configure for the next invocation.
    ///
    /// The last InstructionContext is always empty and pre-reserved for the next instruction.
    pub fn get_next_instruction_context_mut(
        &mut self,
    ) -> Result<&mut InstructionContext, InstructionError> {
        self.instruction_trace
            .last_mut()
            .ok_or(InstructionError::CallDepth)
    }

    /// Returns the immutable InstructionContext. This function assumes it has already been
    /// configured with the correct values in `prepare_next_instruction` or
    /// `prepare_next_top_level_instruction`
    pub fn get_next_instruction_context(&self) -> Result<&InstructionContext, InstructionError> {
        self.instruction_trace
            .last()
            .ok_or(InstructionError::CallDepth)
    }

    /// Pushes the next InstructionContext
    #[cfg(not(target_os = "solana"))]
    pub fn push(&mut self) -> Result<(), InstructionError> {
        let nesting_level = self.get_instruction_context_stack_height();
        let caller_instruction_context = self
            .instruction_trace
            .last()
            .ok_or(InstructionError::CallDepth)?;
        let callee_instruction_accounts_lamport_sum =
            self.instruction_accounts_lamport_sum(caller_instruction_context)?;
        if !self.instruction_stack.is_empty() {
            let caller_instruction_context = self.get_current_instruction_context()?;
            let original_caller_instruction_accounts_lamport_sum =
                caller_instruction_context.instruction_accounts_lamport_sum;
            let current_caller_instruction_accounts_lamport_sum =
                self.instruction_accounts_lamport_sum(caller_instruction_context)?;
            if original_caller_instruction_accounts_lamport_sum
                != current_caller_instruction_accounts_lamport_sum
            {
                return Err(InstructionError::UnbalancedInstruction);
            }
        }
        {
            let instruction_context = self.get_next_instruction_context_mut()?;
            instruction_context.nesting_level = nesting_level;
            instruction_context.instruction_accounts_lamport_sum =
                callee_instruction_accounts_lamport_sum;
        }
        let index_in_trace = self.get_instruction_trace_length();
        if index_in_trace >= self.instruction_trace_capacity {
            return Err(InstructionError::MaxInstructionTraceLengthExceeded);
        }
        self.instruction_trace.push(InstructionContext::default());
        if nesting_level >= self.instruction_stack_capacity {
            return Err(InstructionError::CallDepth);
        }
        self.instruction_stack.push(index_in_trace);
        if let Some(index_in_transaction) = self.find_index_of_account(&instructions::id()) {
            let mut mut_account_ref = self
                .accounts
                .get(index_in_transaction)
                .ok_or(InstructionError::NotEnoughAccountKeys)?
                .try_borrow_mut()
                .map_err(|_| InstructionError::AccountBorrowFailed)?;
            if mut_account_ref.owner() != &solana_sdk_ids::sysvar::id() {
                return Err(InstructionError::InvalidAccountOwner);
            }
            instructions::store_current_index_checked(
                mut_account_ref.data_as_mut_slice(),
                self.top_level_instruction_index as u16,
            )?;
        }
        Ok(())
    }

    /// Pops the current InstructionContext
    #[cfg(not(target_os = "solana"))]
    pub fn pop(&mut self) -> Result<(), InstructionError> {
        if self.instruction_stack.is_empty() {
            return Err(InstructionError::CallDepth);
        }
        // Verify (before we pop) that the total sum of all lamports in this instruction did not change
        let detected_an_unbalanced_instruction =
            self.get_current_instruction_context()
                .and_then(|instruction_context| {
                    // Verify all executable accounts have no outstanding refs
                    for index_in_transaction in instruction_context.program_accounts.iter() {
                        self.accounts
                            .get(*index_in_transaction)
                            .ok_or(InstructionError::NotEnoughAccountKeys)?
                            .try_borrow_mut()
                            .map_err(|_| InstructionError::AccountBorrowOutstanding)?;
                    }
                    self.instruction_accounts_lamport_sum(instruction_context)
                        .map(|instruction_accounts_lamport_sum| {
                            instruction_context.instruction_accounts_lamport_sum
                                != instruction_accounts_lamport_sum
                        })
                });
        // Always pop, even if we `detected_an_unbalanced_instruction`
        self.instruction_stack.pop();
        if self.instruction_stack.is_empty() {
            self.top_level_instruction_index = self.top_level_instruction_index.saturating_add(1);
        }
        if detected_an_unbalanced_instruction? {
            Err(InstructionError::UnbalancedInstruction)
        } else {
            Ok(())
        }
    }

    /// Gets the return data of the current InstructionContext or any above
    pub fn get_return_data(&self) -> (&Pubkey, &[u8]) {
        (&self.return_data.program_id, &self.return_data.data)
    }

    /// Set the return data of the current InstructionContext
    pub fn set_return_data(
        &mut self,
        program_id: Pubkey,
        data: Vec<u8>,
    ) -> Result<(), InstructionError> {
        self.return_data = TransactionReturnData { program_id, data };
        Ok(())
    }

    /// Calculates the sum of all lamports within an instruction
    #[cfg(not(target_os = "solana"))]
    fn instruction_accounts_lamport_sum(
        &self,
        instruction_context: &InstructionContext,
    ) -> Result<u128, InstructionError> {
        let mut instruction_accounts_lamport_sum: u128 = 0;
        for instruction_account_index in 0..instruction_context.get_number_of_instruction_accounts()
        {
            if instruction_context
                .is_instruction_account_duplicate(instruction_account_index)?
                .is_some()
            {
                continue; // Skip duplicate account
            }
            let index_in_transaction = instruction_context
                .get_index_of_instruction_account_in_transaction(instruction_account_index)?;
            instruction_accounts_lamport_sum = (self
                .accounts
                .get(index_in_transaction)
                .ok_or(InstructionError::NotEnoughAccountKeys)?
                .try_borrow()
                .map_err(|_| InstructionError::AccountBorrowOutstanding)?
                .lamports() as u128)
                .checked_add(instruction_accounts_lamport_sum)
                .ok_or(InstructionError::ArithmeticOverflow)?;
        }
        Ok(instruction_accounts_lamport_sum)
    }

    /// Returns the accounts resize delta
    pub fn accounts_resize_delta(&self) -> Result<i64, InstructionError> {
        self.accounts
            .resize_delta
            .try_borrow()
            .map_err(|_| InstructionError::GenericError)
            .map(|value_ref| *value_ref)
    }

    /// Returns a new account data write access handler
    pub fn access_violation_handler(&self) -> AccessViolationHandler {
        let accounts = Rc::clone(&self.accounts);
        Box::new(
            move |region: &mut MemoryRegion,
                  address_space_reserved_for_account: u64,
                  access_type: AccessType,
                  vm_addr: u64,
                  len: u64| {
                if access_type == AccessType::Load {
                    return;
                }
                let Some(index_in_transaction) = region.access_violation_handler_payload else {
                    // This region is not a writable account.
                    return;
                };
                let requested_length =
                    vm_addr.saturating_add(len).saturating_sub(region.vm_addr) as usize;
                if requested_length > address_space_reserved_for_account as usize {
                    // Requested access goes further than the account region.
                    return;
                }

                // The four calls below can't really fail. If they fail because of a bug,
                // whatever is writing will trigger an EbpfError::AccessViolation like
                // if the region was readonly, and the transaction will fail gracefully.
                let Some(account) = accounts.accounts.get(index_in_transaction as usize) else {
                    debug_assert!(false);
                    return;
                };
                let Ok(mut account) = account.try_borrow_mut() else {
                    debug_assert!(false);
                    return;
                };
                if accounts.touch(index_in_transaction).is_err() {
                    debug_assert!(false);
                    return;
                }
                let Ok(remaining_allowed_growth) =
                    accounts.resize_delta.try_borrow().map(|resize_delta| {
                        MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION
                            .saturating_sub(*resize_delta)
                            .max(0) as usize
                    })
                else {
                    debug_assert!(false);
                    return;
                };

                if requested_length > region.len as usize {
                    // Realloc immediately here to fit the requested access,
                    // then later in CPI or deserialization realloc again to the
                    // account length the program stored in AccountInfo.
                    let old_len = account.data().len();
                    let new_len = (address_space_reserved_for_account as usize)
                        .min(MAX_PERMITTED_DATA_LENGTH as usize)
                        .min(old_len.saturating_add(remaining_allowed_growth));
                    // The last two min operations ensure the following:
                    debug_assert!(accounts.can_data_be_resized(old_len, new_len).is_ok());
                    if accounts
                        .update_accounts_resize_delta(old_len, new_len)
                        .is_err()
                    {
                        return;
                    }
                    account.resize(new_len, 0);
                    region.len = new_len as u64;
                }

                // Potentially unshare / make the account shared data unique (CoW logic).
                region.host_addr = account.data_as_mut_slice().as_mut_ptr() as u64;
                region.writable = true;
            },
        )
    }
}

/// Return data at the end of a transaction
#[cfg_attr(
    feature = "serde",
    derive(serde_derive::Deserialize, serde_derive::Serialize)
)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransactionReturnData {
    pub program_id: Pubkey,
    pub data: Vec<u8>,
}

/// Loaded instruction shared between runtime and programs.
///
/// This context is valid for the entire duration of a (possibly cross program) instruction being processed.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct InstructionContext {
    nesting_level: usize,
    instruction_accounts_lamport_sum: u128,
    program_accounts: Vec<IndexOfAccount>,
    instruction_accounts: Vec<InstructionAccount>,
    instruction_data: Vec<u8>,
}

impl InstructionContext {
    /// Used together with TransactionContext::get_next_instruction_context()
    #[cfg(not(target_os = "solana"))]
    pub fn configure(
        &mut self,
        program_accounts: Vec<IndexOfAccount>,
        instruction_accounts: Vec<InstructionAccount>,
        instruction_data: &[u8],
    ) {
        self.program_accounts = program_accounts;
        self.instruction_accounts = instruction_accounts;
        self.instruction_data = instruction_data.to_vec();
    }

    /// How many Instructions were on the stack after this one was pushed
    ///
    /// That is the number of nested parent Instructions plus one (itself).
    pub fn get_stack_height(&self) -> usize {
        self.nesting_level.saturating_add(1)
    }

    /// Number of program accounts
    pub fn get_number_of_program_accounts(&self) -> IndexOfAccount {
        self.program_accounts.len() as IndexOfAccount
    }

    /// Number of accounts in this Instruction (without program accounts)
    pub fn get_number_of_instruction_accounts(&self) -> IndexOfAccount {
        self.instruction_accounts.len() as IndexOfAccount
    }

    /// Assert that enough accounts were supplied to this Instruction
    pub fn check_number_of_instruction_accounts(
        &self,
        expected_at_least: IndexOfAccount,
    ) -> Result<(), InstructionError> {
        if self.get_number_of_instruction_accounts() < expected_at_least {
            Err(InstructionError::NotEnoughAccountKeys)
        } else {
            Ok(())
        }
    }

    /// Data parameter for the programs `process_instruction` handler
    pub fn get_instruction_data(&self) -> &[u8] {
        &self.instruction_data
    }

    /// Searches for a program account by its key
    pub fn find_index_of_program_account(
        &self,
        transaction_context: &TransactionContext,
        pubkey: &Pubkey,
    ) -> Option<IndexOfAccount> {
        self.program_accounts
            .iter()
            .position(|index_in_transaction| {
                transaction_context
                    .account_keys
                    .get(*index_in_transaction as usize)
                    == Some(pubkey)
            })
            .map(|index| index as IndexOfAccount)
    }

    /// Searches for an instruction account by its key
    pub fn find_index_of_instruction_account(
        &self,
        transaction_context: &TransactionContext,
        pubkey: &Pubkey,
    ) -> Option<IndexOfAccount> {
        self.instruction_accounts
            .iter()
            .position(|instruction_account| {
                transaction_context
                    .account_keys
                    .get(instruction_account.index_in_transaction as usize)
                    == Some(pubkey)
            })
            .map(|index| index as IndexOfAccount)
    }

    /// Translates the given instruction wide program_account_index into a transaction wide index
    pub fn get_index_of_program_account_in_transaction(
        &self,
        program_account_index: IndexOfAccount,
    ) -> Result<IndexOfAccount, InstructionError> {
        Ok(*self
            .program_accounts
            .get(program_account_index as usize)
            .ok_or(InstructionError::NotEnoughAccountKeys)?)
    }

    /// Translates the given instruction wide instruction_account_index into a transaction wide index
    pub fn get_index_of_instruction_account_in_transaction(
        &self,
        instruction_account_index: IndexOfAccount,
    ) -> Result<IndexOfAccount, InstructionError> {
        Ok(self
            .instruction_accounts
            .get(instruction_account_index as usize)
            .ok_or(InstructionError::NotEnoughAccountKeys)?
            .index_in_transaction as IndexOfAccount)
    }

    /// Get the index of account in instruction from the index in transaction
    pub fn get_index_of_account_in_instruction(
        &self,
        index_in_transaction: IndexOfAccount,
    ) -> Result<IndexOfAccount, InstructionError> {
        self.instruction_accounts
            .iter()
            .position(|account| account.index_in_transaction == index_in_transaction)
            .map(|idx| idx as IndexOfAccount)
            .ok_or(InstructionError::MissingAccount)
    }

    /// Returns `Some(instruction_account_index)` if this is a duplicate
    /// and `None` if it is the first account with this key
    pub fn is_instruction_account_duplicate(
        &self,
        instruction_account_index: IndexOfAccount,
    ) -> Result<Option<IndexOfAccount>, InstructionError> {
        let index_in_callee = self
            .instruction_accounts
            .get(instruction_account_index as usize)
            .ok_or(InstructionError::NotEnoughAccountKeys)?
            .index_in_callee;
        Ok(if index_in_callee == instruction_account_index {
            None
        } else {
            Some(index_in_callee)
        })
    }

    /// Gets the key of the last program account of this Instruction
    pub fn get_last_program_key<'a, 'b: 'a>(
        &'a self,
        transaction_context: &'b TransactionContext,
    ) -> Result<&'b Pubkey, InstructionError> {
        self.get_index_of_program_account_in_transaction(
            self.get_number_of_program_accounts().saturating_sub(1),
        )
        .and_then(|index_in_transaction| {
            transaction_context.get_key_of_account_at_index(index_in_transaction)
        })
    }

    fn try_borrow_account<'a, 'b: 'a>(
        &'a self,
        transaction_context: &'b TransactionContext,
        index_in_transaction: IndexOfAccount,
        index_in_instruction: Option<IndexOfAccount>,
    ) -> Result<BorrowedAccount<'a>, InstructionError> {
        let account = transaction_context
            .accounts
            .get(index_in_transaction)
            .ok_or(InstructionError::MissingAccount)?
            .try_borrow_mut()
            .map_err(|_| InstructionError::AccountBorrowFailed)?;
        Ok(BorrowedAccount {
            transaction_context,
            instruction_context: self,
            index_in_transaction,
            index_in_instruction_accounts: index_in_instruction,
            account,
        })
    }

    /// Gets the last program account of this Instruction
    pub fn try_borrow_last_program_account<'a, 'b: 'a>(
        &'a self,
        transaction_context: &'b TransactionContext,
    ) -> Result<BorrowedAccount<'a>, InstructionError> {
        let result = self.try_borrow_program_account(
            transaction_context,
            self.get_number_of_program_accounts().saturating_sub(1),
        );
        debug_assert!(result.is_ok());
        result
    }

    /// Tries to borrow a program account from this Instruction
    pub fn try_borrow_program_account<'a, 'b: 'a>(
        &'a self,
        transaction_context: &'b TransactionContext,
        program_account_index: IndexOfAccount,
    ) -> Result<BorrowedAccount<'a>, InstructionError> {
        let index_in_transaction =
            self.get_index_of_program_account_in_transaction(program_account_index)?;
        self.try_borrow_account(transaction_context, index_in_transaction, None)
    }

    /// Gets an instruction account of this Instruction
    pub fn try_borrow_instruction_account<'a, 'b: 'a>(
        &'a self,
        transaction_context: &'b TransactionContext,
        instruction_account_index: IndexOfAccount,
    ) -> Result<BorrowedAccount<'a>, InstructionError> {
        let index_in_transaction =
            self.get_index_of_instruction_account_in_transaction(instruction_account_index)?;
        self.try_borrow_account(
            transaction_context,
            index_in_transaction,
            Some(instruction_account_index),
        )
    }

    /// Returns whether an instruction account is a signer
    pub fn is_instruction_account_signer(
        &self,
        instruction_account_index: IndexOfAccount,
    ) -> Result<bool, InstructionError> {
        Ok(self
            .instruction_accounts
            .get(instruction_account_index as usize)
            .ok_or(InstructionError::MissingAccount)?
            .is_signer())
    }

    /// Returns whether an instruction account is writable
    pub fn is_instruction_account_writable(
        &self,
        instruction_account_index: IndexOfAccount,
    ) -> Result<bool, InstructionError> {
        Ok(self
            .instruction_accounts
            .get(instruction_account_index as usize)
            .ok_or(InstructionError::MissingAccount)?
            .is_writable())
    }

    /// Calculates the set of all keys of signer instruction accounts in this Instruction
    pub fn get_signers(
        &self,
        transaction_context: &TransactionContext,
    ) -> Result<HashSet<Pubkey>, InstructionError> {
        let mut result = HashSet::new();
        for instruction_account in self.instruction_accounts.iter() {
            if instruction_account.is_signer() {
                result.insert(
                    *transaction_context
                        .get_key_of_account_at_index(instruction_account.index_in_transaction)?,
                );
            }
        }
        Ok(result)
    }

    pub fn instruction_accounts(&self) -> &[InstructionAccount] {
        &self.instruction_accounts
    }
}

/// Shared account borrowed from the TransactionContext and an InstructionContext.
#[derive(Debug)]
pub struct BorrowedAccount<'a> {
    transaction_context: &'a TransactionContext,
    instruction_context: &'a InstructionContext,
    index_in_transaction: IndexOfAccount,
    // Program accounts are not part of the instruction_accounts vector, and thus None
    index_in_instruction_accounts: Option<IndexOfAccount>,
    account: RefMut<'a, AccountSharedData>,
}

impl BorrowedAccount<'_> {
    /// Returns the transaction context
    pub fn transaction_context(&self) -> &TransactionContext {
        self.transaction_context
    }

    /// Returns the index of this account (transaction wide)
    #[inline]
    pub fn get_index_in_transaction(&self) -> IndexOfAccount {
        self.index_in_transaction
    }

    /// Returns the public key of this account (transaction wide)
    #[inline]
    pub fn get_key(&self) -> &Pubkey {
        self.transaction_context
            .get_key_of_account_at_index(self.index_in_transaction)
            .unwrap()
    }

    /// Returns the owner of this account (transaction wide)
    #[inline]
    pub fn get_owner(&self) -> &Pubkey {
        self.account.owner()
    }

    /// Assignes the owner of this account (transaction wide)
    #[cfg(not(target_os = "solana"))]
    pub fn set_owner(&mut self, pubkey: &[u8]) -> Result<(), InstructionError> {
        // Only the owner can assign a new owner
        if !self.is_owned_by_current_program() {
            return Err(InstructionError::ModifiedProgramId);
        }
        // and only if the account is writable
        if !self.is_writable() {
            return Err(InstructionError::ModifiedProgramId);
        }
        // and only if the account is not executable
        if self.is_executable_internal() {
            return Err(InstructionError::ModifiedProgramId);
        }
        // and only if the data is zero-initialized or empty
        if !is_zeroed(self.get_data()) {
            return Err(InstructionError::ModifiedProgramId);
        }
        // don't touch the account if the owner does not change
        if self.get_owner().to_bytes() == pubkey {
            return Ok(());
        }
        self.touch()?;
        self.account.copy_into_owner_from_slice(pubkey);
        Ok(())
    }

    /// Returns the number of lamports of this account (transaction wide)
    #[inline]
    pub fn get_lamports(&self) -> u64 {
        self.account.lamports()
    }

    /// Overwrites the number of lamports of this account (transaction wide)
    #[cfg(not(target_os = "solana"))]
    pub fn set_lamports(&mut self, lamports: u64) -> Result<(), InstructionError> {
        // An account not owned by the program cannot have its balance decrease
        if !self.is_owned_by_current_program() && lamports < self.get_lamports() {
            return Err(InstructionError::ExternalAccountLamportSpend);
        }
        // The balance of read-only may not change
        if !self.is_writable() {
            return Err(InstructionError::ReadonlyLamportChange);
        }
        // The balance of executable accounts may not change
        if self.is_executable_internal() {
            return Err(InstructionError::ExecutableLamportChange);
        }
        // don't touch the account if the lamports do not change
        if self.get_lamports() == lamports {
            return Ok(());
        }
        self.touch()?;
        self.account.set_lamports(lamports);
        Ok(())
    }

    /// Adds lamports to this account (transaction wide)
    #[cfg(not(target_os = "solana"))]
    pub fn checked_add_lamports(&mut self, lamports: u64) -> Result<(), InstructionError> {
        self.set_lamports(
            self.get_lamports()
                .checked_add(lamports)
                .ok_or(InstructionError::ArithmeticOverflow)?,
        )
    }

    /// Subtracts lamports from this account (transaction wide)
    #[cfg(not(target_os = "solana"))]
    pub fn checked_sub_lamports(&mut self, lamports: u64) -> Result<(), InstructionError> {
        self.set_lamports(
            self.get_lamports()
                .checked_sub(lamports)
                .ok_or(InstructionError::ArithmeticOverflow)?,
        )
    }

    /// Returns a read-only slice of the account data (transaction wide)
    #[inline]
    pub fn get_data(&self) -> &[u8] {
        self.account.data()
    }

    /// Returns a writable slice of the account data (transaction wide)
    #[cfg(not(target_os = "solana"))]
    pub fn get_data_mut(&mut self) -> Result<&mut [u8], InstructionError> {
        self.can_data_be_changed()?;
        self.touch()?;
        self.make_data_mut();
        Ok(self.account.data_as_mut_slice())
    }

    /// Overwrites the account data and size (transaction wide).
    ///
    /// You should always prefer set_data_from_slice(). Calling this method is
    /// currently safe but requires some special casing during CPI when direct
    /// account mapping is enabled.
    #[cfg(all(
        not(target_os = "solana"),
        any(test, feature = "dev-context-only-utils")
    ))]
    pub fn set_data(&mut self, data: Vec<u8>) -> Result<(), InstructionError> {
        self.can_data_be_resized(data.len())?;
        self.touch()?;

        self.update_accounts_resize_delta(data.len())?;
        self.account.set_data(data);
        Ok(())
    }

    /// Overwrites the account data and size (transaction wide).
    ///
    /// Call this when you have a slice of data you do not own and want to
    /// replace the account data with it.
    #[cfg(not(target_os = "solana"))]
    pub fn set_data_from_slice(&mut self, data: &[u8]) -> Result<(), InstructionError> {
        self.can_data_be_resized(data.len())?;
        self.touch()?;
        self.update_accounts_resize_delta(data.len())?;
        // Note that we intentionally don't call self.make_data_mut() here.  make_data_mut() will
        // allocate + memcpy the current data if self.account is shared. We don't need the memcpy
        // here tho because account.set_data_from_slice(data) is going to replace the content
        // anyway.
        self.account.set_data_from_slice(data);

        Ok(())
    }

    /// Resizes the account data (transaction wide)
    ///
    /// Fills it with zeros at the end if is extended or truncates at the end otherwise.
    #[cfg(not(target_os = "solana"))]
    pub fn set_data_length(&mut self, new_length: usize) -> Result<(), InstructionError> {
        self.can_data_be_resized(new_length)?;
        // don't touch the account if the length does not change
        if self.get_data().len() == new_length {
            return Ok(());
        }
        self.touch()?;
        self.update_accounts_resize_delta(new_length)?;
        self.account.resize(new_length, 0);
        Ok(())
    }

    /// Appends all elements in a slice to the account
    #[cfg(not(target_os = "solana"))]
    pub fn extend_from_slice(&mut self, data: &[u8]) -> Result<(), InstructionError> {
        let new_len = self.get_data().len().saturating_add(data.len());
        self.can_data_be_resized(new_len)?;

        if data.is_empty() {
            return Ok(());
        }

        self.touch()?;
        self.update_accounts_resize_delta(new_len)?;
        // Even if extend_from_slice never reduces capacity, still realloc using
        // make_data_mut() if necessary so that we grow the account of the full
        // max realloc length in one go, avoiding smaller reallocations.
        self.make_data_mut();
        self.account.extend_from_slice(data);
        Ok(())
    }

    /// Returns whether the underlying AccountSharedData is shared.
    ///
    /// The data is shared if the account has been loaded from the accounts database and has never
    /// been written to. Writing to an account unshares it.
    ///
    /// During account serialization, if an account is shared it'll get mapped as CoW, else it'll
    /// get mapped directly as writable.
    #[cfg(not(target_os = "solana"))]
    pub fn is_shared(&self) -> bool {
        self.account.is_shared()
    }

    #[cfg(not(target_os = "solana"))]
    fn make_data_mut(&mut self) {
        // if the account is still shared, it means this is the first time we're
        // about to write into it. Make the account mutable by copying it in a
        // buffer with MAX_PERMITTED_DATA_INCREASE capacity so that if the
        // transaction reallocs, we don't have to copy the whole account data a
        // second time to fullfill the realloc.
        //
        // NOTE: The account memory region CoW code in bpf_loader::create_vm() implements the same
        // logic and must be kept in sync.
        if self.account.is_shared() {
            self.account.reserve(MAX_PERMITTED_DATA_INCREASE);
        }
    }

    /// Deserializes the account data into a state
    #[cfg(all(not(target_os = "solana"), feature = "bincode"))]
    pub fn get_state<T: serde::de::DeserializeOwned>(&self) -> Result<T, InstructionError> {
        self.account
            .deserialize_data()
            .map_err(|_| InstructionError::InvalidAccountData)
    }

    /// Serializes a state into the account data
    #[cfg(all(not(target_os = "solana"), feature = "bincode"))]
    pub fn set_state<T: serde::Serialize>(&mut self, state: &T) -> Result<(), InstructionError> {
        let data = self.get_data_mut()?;
        let serialized_size =
            bincode::serialized_size(state).map_err(|_| InstructionError::GenericError)?;
        if serialized_size > data.len() as u64 {
            return Err(InstructionError::AccountDataTooSmall);
        }
        bincode::serialize_into(&mut *data, state).map_err(|_| InstructionError::GenericError)?;
        Ok(())
    }

    // Returns whether or the lamports currently in the account is sufficient for rent exemption should the
    // data be resized to the given size
    #[cfg(not(target_os = "solana"))]
    pub fn is_rent_exempt_at_data_length(&self, data_length: usize) -> bool {
        self.transaction_context
            .rent
            .is_exempt(self.get_lamports(), data_length)
    }

    /// Returns whether this account is executable (transaction wide)
    #[inline]
    #[deprecated(since = "2.1.0", note = "Use `get_owner` instead")]
    pub fn is_executable(&self) -> bool {
        self.account.executable()
    }

    /// Feature gating to remove `is_executable` flag related checks
    #[cfg(not(target_os = "solana"))]
    #[inline]
    fn is_executable_internal(&self) -> bool {
        !self
            .transaction_context
            .remove_accounts_executable_flag_checks
            && self.account.executable()
    }

    /// Configures whether this account is executable (transaction wide)
    #[cfg(not(target_os = "solana"))]
    pub fn set_executable(&mut self, is_executable: bool) -> Result<(), InstructionError> {
        // To become executable an account must be rent exempt
        if !self
            .transaction_context
            .rent
            .is_exempt(self.get_lamports(), self.get_data().len())
        {
            return Err(InstructionError::ExecutableAccountNotRentExempt);
        }
        // Only the owner can set the executable flag
        if !self.is_owned_by_current_program() {
            return Err(InstructionError::ExecutableModified);
        }
        // and only if the account is writable
        if !self.is_writable() {
            return Err(InstructionError::ExecutableModified);
        }
        // one can not clear the executable flag
        if self.is_executable_internal() && !is_executable {
            return Err(InstructionError::ExecutableModified);
        }
        // don't touch the account if the executable flag does not change
        #[allow(deprecated)]
        if self.is_executable() == is_executable {
            return Ok(());
        }
        self.touch()?;
        self.account.set_executable(is_executable);
        Ok(())
    }

    /// Returns the rent epoch of this account (transaction wide)
    #[cfg(not(target_os = "solana"))]
    #[inline]
    pub fn get_rent_epoch(&self) -> u64 {
        self.account.rent_epoch()
    }

    /// Returns whether this account is a signer (instruction wide)
    pub fn is_signer(&self) -> bool {
        if let Some(index_in_instruction_accounts) = self.index_in_instruction_accounts {
            self.instruction_context
                .is_instruction_account_signer(index_in_instruction_accounts)
                .unwrap_or_default()
        } else {
            false
        }
    }

    /// Returns whether this account is writable (instruction wide)
    pub fn is_writable(&self) -> bool {
        if let Some(index_in_instruction_accounts) = self.index_in_instruction_accounts {
            self.instruction_context
                .is_instruction_account_writable(index_in_instruction_accounts)
                .unwrap_or_default()
        } else {
            false
        }
    }

    /// Returns true if the owner of this account is the current `InstructionContext`s last program (instruction wide)
    pub fn is_owned_by_current_program(&self) -> bool {
        self.instruction_context
            .get_last_program_key(self.transaction_context)
            .map(|key| key == self.get_owner())
            .unwrap_or_default()
    }

    /// Returns an error if the account data can not be mutated by the current program
    #[cfg(not(target_os = "solana"))]
    pub fn can_data_be_changed(&self) -> Result<(), InstructionError> {
        // Only non-executable accounts data can be changed
        if self.is_executable_internal() {
            return Err(InstructionError::ExecutableDataModified);
        }
        // and only if the account is writable
        if !self.is_writable() {
            return Err(InstructionError::ReadonlyDataModified);
        }
        // and only if we are the owner
        if !self.is_owned_by_current_program() {
            return Err(InstructionError::ExternalAccountDataModified);
        }
        Ok(())
    }

    /// Returns an error if the account data can not be resized to the given length
    #[cfg(not(target_os = "solana"))]
    pub fn can_data_be_resized(&self, new_len: usize) -> Result<(), InstructionError> {
        let old_len = self.get_data().len();
        // Only the owner can change the length of the data
        if new_len != old_len && !self.is_owned_by_current_program() {
            return Err(InstructionError::AccountDataSizeChanged);
        }
        self.transaction_context
            .accounts
            .can_data_be_resized(old_len, new_len)?;
        self.can_data_be_changed()
    }

    #[cfg(not(target_os = "solana"))]
    fn touch(&self) -> Result<(), InstructionError> {
        self.transaction_context
            .accounts
            .touch(self.index_in_transaction)
    }

    #[cfg(not(target_os = "solana"))]
    fn update_accounts_resize_delta(&mut self, new_len: usize) -> Result<(), InstructionError> {
        self.transaction_context
            .accounts
            .update_accounts_resize_delta(self.get_data().len(), new_len)
    }
}

/// Everything that needs to be recorded from a TransactionContext after execution
#[cfg(not(target_os = "solana"))]
pub struct ExecutionRecord {
    pub accounts: Vec<TransactionAccount>,
    pub return_data: TransactionReturnData,
    pub touched_account_count: u64,
    pub accounts_resize_delta: i64,
}

/// Used by the bank in the runtime to write back the processed accounts and recorded instructions
#[cfg(not(target_os = "solana"))]
impl From<TransactionContext> for ExecutionRecord {
    fn from(context: TransactionContext) -> Self {
        let TransactionAccounts {
            accounts,
            touched_flags,
            resize_delta,
        } = Rc::try_unwrap(context.accounts)
            .expect("transaction_context.accounts has unexpected outstanding refs");
        let accounts = Vec::from(Pin::into_inner(context.account_keys))
            .into_iter()
            .zip(accounts.into_iter().map(RefCell::into_inner))
            .collect();
        let touched_account_count = touched_flags
            .borrow()
            .iter()
            .fold(0usize, |accumulator, was_touched| {
                accumulator.saturating_add(*was_touched as usize)
            }) as u64;
        Self {
            accounts,
            return_data: context.return_data,
            touched_account_count,
            accounts_resize_delta: RefCell::into_inner(resize_delta),
        }
    }
}

#[cfg(not(target_os = "solana"))]
fn is_zeroed(buf: &[u8]) -> bool {
    const ZEROS_LEN: usize = 1024;
    const ZEROS: [u8; ZEROS_LEN] = [0; ZEROS_LEN];
    let mut chunks = buf.chunks_exact(ZEROS_LEN);

    #[allow(clippy::indexing_slicing)]
    {
        chunks.all(|chunk| chunk == &ZEROS[..])
            && chunks.remainder() == &ZEROS[..chunks.remainder().len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instructions_sysvar_store_index_checked() {
        let build_transaction_context = |account: AccountSharedData| {
            TransactionContext::new(
                vec![
                    (Pubkey::new_unique(), AccountSharedData::default()),
                    (instructions::id(), account),
                ],
                Rent::default(),
                /* max_instruction_stack_depth */ 2,
                /* max_instruction_trace_length */ 2,
            )
        };

        let correct_space = 2;
        let rent_exempt_lamports = Rent::default().minimum_balance(correct_space);

        // First try it with the wrong owner.
        let account =
            AccountSharedData::new(rent_exempt_lamports, correct_space, &Pubkey::new_unique());
        assert_eq!(
            build_transaction_context(account).push(),
            Err(InstructionError::InvalidAccountOwner),
        );

        // Now with the wrong data length.
        let account =
            AccountSharedData::new(rent_exempt_lamports, 0, &solana_sdk_ids::sysvar::id());
        assert_eq!(
            build_transaction_context(account).push(),
            Err(InstructionError::AccountDataTooSmall),
        );

        // Finally provide the correct account setup.
        let account = AccountSharedData::new(
            rent_exempt_lamports,
            correct_space,
            &solana_sdk_ids::sysvar::id(),
        );
        assert_eq!(build_transaction_context(account).push(), Ok(()),);
    }
}
