use borsh::{BorshDeserialize, BorshSerialize};
pub use ethereum_types::U256;
use ethereum_types::{Address, H160};
use evm::CreateContractAddress;

use near_primitives::trie_key::TrieKey;
use near_primitives::types::{AccountId, Balance};
use near_store::TrieUpdate;
use near_vm_errors::VMError;
use near_vm_logic::VMOutcome;

pub use crate::errors::EvmError;
use crate::evm_state::{EvmAccount, EvmState, StateStore};
use crate::types::{GetCodeArgs, GetStorageAtArgs, TransferArgs, WithdrawArgs};
use near_primitives::receipt::Receipt;
use near_vm_errors::FunctionCallError;
use near_vm_logic::types::ReturnData;

mod builtins;
mod errors;
mod evm_state;
mod interpreter;
mod near_ext;
pub mod types;
pub mod utils;

pub struct EvmContext<'a> {
    trie_update: &'a mut TrieUpdate,
    account_id: AccountId,
    predecessor_id: AccountId,
    attached_deposit: Balance,
}

enum KeyPrefix {
    Account = 0,
    Contract = 1,
}

fn address_to_key(prefix: KeyPrefix, address: &H160) -> Vec<u8> {
    let mut result = Vec::with_capacity(21);
    result.push(prefix as u8);
    result.extend_from_slice(&address.0);
    result
}

impl<'a> EvmState for EvmContext<'a> {
    fn code_at(&self, address: &H160) -> Option<Vec<u8>> {
        self.trie_update
            .get(&TrieKey::ContractData {
                account_id: self.account_id.clone(),
                key: address_to_key(KeyPrefix::Contract, address),
            })
            .unwrap_or(None)
    }

    fn set_code(&mut self, address: &H160, bytecode: &[u8]) {
        self.trie_update.set(
            TrieKey::ContractData {
                account_id: self.account_id.clone(),
                key: address_to_key(KeyPrefix::Contract, address),
            },
            bytecode.to_vec(),
        )
    }

    fn set_account(&mut self, address: &Address, account: &EvmAccount) {
        self.trie_update.set(
            TrieKey::ContractData {
                account_id: self.account_id.clone(),
                key: address_to_key(KeyPrefix::Account, address),
            },
            account.try_to_vec().expect("Failed to serialize"),
        )
    }

    fn get_account(&self, address: &Address) -> Option<EvmAccount> {
        // TODO: handle error propagation?
        self.trie_update
            .get(&TrieKey::ContractData {
                account_id: self.account_id.clone(),
                key: address_to_key(KeyPrefix::Account, address),
            })
            .unwrap_or_else(|_| None)
            .map(|value| EvmAccount::try_from_slice(&value).unwrap_or_default())
    }

    fn _read_contract_storage(&self, key: [u8; 52]) -> Option<[u8; 32]> {
        self.trie_update
            .get(&TrieKey::ContractData { account_id: self.account_id.clone(), key: key.to_vec() })
            .unwrap_or_else(|_| None)
            .map(utils::vec_to_arr_32)
    }

    fn _set_contract_storage(&mut self, key: [u8; 52], value: [u8; 32]) {
        self.trie_update.set(
            TrieKey::ContractData { account_id: self.account_id.clone(), key: key.to_vec() },
            value.to_vec(),
        );
    }

    fn commit_changes(&mut self, other: &StateStore) {
        //        self.commit_self_destructs(&other.self_destructs);
        //        self.commit_self_destructs(&other.recreated);
        for (address, code) in other.code.iter() {
            self.set_code(&H160(*address), code);
        }
        for (address, account) in other.accounts.iter() {
            self.set_account(&H160(*address), account);
        }
        for (key, value) in other.storages.iter() {
            let mut arr = [0; 52];
            arr.copy_from_slice(&key);
            self._set_contract_storage(arr, *value);
        }
        //        for log in &other.logs {
        //            near_sdk::env::log(format!("evm log: {}", log).as_bytes());
        //        }
    }

    fn recreate(&mut self, _address: [u8; 20]) {
        unreachable!()
    }
}

impl<'a> EvmContext<'a> {
    pub fn new(
        state_update: &'a mut TrieUpdate,
        account_id: AccountId,
        predecessor_id: AccountId,
        attached_deposit: Balance,
    ) -> Self {
        Self {
            trie_update: state_update,
            account_id,
            predecessor_id: predecessor_id,
            attached_deposit,
        }
    }

    pub fn deploy_code(&mut self, bytecode: Vec<u8>) -> Result<Address, EvmError> {
        let sender = utils::near_account_id_to_evm_address(&self.predecessor_id);
        interpreter::deploy_code(
            self,
            &sender,
            &sender,
            U256::from(self.attached_deposit),
            0,
            CreateContractAddress::FromSenderAndNonce,
            false,
            &bytecode,
        )
    }

    pub fn call_function(&mut self, args: Vec<u8>) -> Result<Vec<u8>, EvmError> {
        let contract_address = Address::from_slice(&args[..20]);
        let input = &args[20..];
        let sender = utils::near_account_id_to_evm_address(&self.predecessor_id);
        let value =
            if self.attached_deposit == 0 { None } else { Some(U256::from(self.attached_deposit)) };
        interpreter::call(self, &sender, &sender, value, 0, &contract_address, &input, true)
            .map(|rd| rd.to_vec())
    }

    /// Make an EVM transaction. Calls `contract_address` with `encoded_input`. Execution
    /// continues until all EVM messages have been processed. We expect this to behave identically
    /// to an Ethereum transaction, however there may be some edge cases.
    ///
    /// This function serves the eth_call functionality, and will NOT apply state changes.
    pub fn view_call_function(&mut self, args: Vec<u8>) -> Result<Vec<u8>, EvmError> {
        let contract_address = Address::from_slice(&args[..20]);
        let input = &args[20..];
        let sender = utils::near_account_id_to_evm_address(&self.predecessor_id);
        interpreter::call(self, &sender, &sender, None, 0, &contract_address, &input, false)
            .map(|rd| rd.to_vec())
    }

    pub fn get_code(&self, args: Vec<u8>) -> Result<Vec<u8>, EvmError> {
        let args = GetCodeArgs::try_from_slice(&args).map_err(|_| EvmError::ArgumentParseError)?;
        Ok(self.code_at(&Address::from_slice(&args.address)).unwrap_or(vec![]))
    }

    pub fn get_storage_at(&self, args: Vec<u8>) -> Result<Vec<u8>, EvmError> {
        let args =
            GetStorageAtArgs::try_from_slice(&args).map_err(|_| EvmError::ArgumentParseError)?;
        Ok(self
            .read_contract_storage(&Address::from_slice(&args.address), args.key)
            .unwrap_or([0u8; 32])
            .to_vec())
    }

    pub fn get_balance(&self, args: Vec<u8>) -> Result<U256, EvmError> {
        Ok(self.balance_of(&Address::from_slice(&args)))
    }

    pub fn get_nonce(&self, args: Vec<u8>) -> Result<U256, EvmError> {
        Ok(self.nonce_of(&Address::from_slice(&args)))
    }

    pub fn deposit(&mut self, args: Vec<u8>) -> Result<U256, EvmError> {
        if self.attached_deposit == 0 {
            return Err(EvmError::MissingDeposit);
        }
        let address = Address::from_slice(&args);
        self.add_balance(&address, U256::from(self.attached_deposit))?;
        Ok(self.balance_of(&address))
    }

    pub fn withdraw(&mut self, args: Vec<u8>) -> Result<(), EvmError> {
        let args = WithdrawArgs::try_from_slice(&args).map_err(|_| EvmError::ArgumentParseError)?;
        let sender = utils::near_account_id_to_evm_address(&self.predecessor_id);
        let amount = U256::from(args.amount);
        if amount > self.balance_of(&sender) {
            return Err(EvmError::InsufficientFunds);
        }
        self.sub_balance(&sender, amount)?;
        // TODO: add outgoing promise.
        Ok(())
    }

    /// Transfer tokens from sender to given EVM address.
    pub fn transfer(&mut self, args: Vec<u8>) -> Result<(), EvmError> {
        let args = TransferArgs::try_from_slice(&args).map_err(|_| EvmError::ArgumentParseError)?;
        let sender = utils::near_account_id_to_evm_address(&self.predecessor_id);
        let amount = U256::from(args.amount);
        if amount > self.balance_of(&sender) {
            return Err(EvmError::InsufficientFunds);
        }
        self.transfer_balance(&sender, &Address::from(args.account_id), amount)
    }
}

pub fn run_evm(
    mut state_update: &mut TrieUpdate,
    account_id: &AccountId,
    predecessor_id: &AccountId,
    amount: Balance,
    attached_deposit: Balance,
    method_name: String,
    args: Vec<u8>,
) -> (Option<VMOutcome>, Option<VMError>, Vec<Receipt>) {
    let mut context = EvmContext::new(
        &mut state_update,
        account_id.clone(),
        predecessor_id.clone(),
        attached_deposit,
    );
    let result = match method_name.as_str() {
        "deploy_code" => context.deploy_code(args).map(|address| utils::address_to_vec(&address)),
        "get_code" => context.get_code(args),
        "call_function" => context.call_function(args),
        "view_call_contract" => context.view_call_function(args),
        "get_storage_at" => context.get_storage_at(args),
        "get_balance" => context.get_balance(args).map(|balance| utils::u256_to_vec(&balance)),
        "deposit" => context.deposit(args).map(|balance| utils::u256_to_vec(&balance)),
        "withdraw" => context.withdraw(args).map(|_| vec![]),
        "transfer" => context.transfer(args).map(|_| vec![]),
        _ => Err(EvmError::UnknownError),
    };
    match result {
        Ok(value) => {
            let outcome = VMOutcome {
                balance: amount,
                storage_usage: 0,
                return_data: ReturnData::Value(value),
                burnt_gas: 0,
                used_gas: 0,
                logs: vec![],
            };
            (Some(outcome), None, vec![])
        }
        Err(err) => {
            (None, Some(VMError::FunctionCallError(FunctionCallError::WasmUnknownError)), vec![])
        }
    }
}
