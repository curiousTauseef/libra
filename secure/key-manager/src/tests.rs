// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    libra_interface::JsonRpcLibraInterface, Action, Error, KeyManager, LibraInterface,
    GAS_UNIT_PRICE, MAX_GAS_AMOUNT,
};
use anyhow::Result;
use executor::{db_bootstrapper, Executor};
use executor_types::BlockExecutor;
use futures::{channel::mpsc::channel, StreamExt};
use libra_config::{
    config::{KeyManagerConfig, NodeConfig},
    utils,
    utils::get_genesis_txn,
};
use libra_crypto::{ed25519::Ed25519PrivateKey, x25519, HashValue, PrivateKey, Uniform};
use libra_global_constants::{OPERATOR_ACCOUNT, OPERATOR_KEY};
use libra_network_address::RawNetworkAddress;
use libra_secure_storage::{InMemoryStorageInternal, KVStorage, Value};
use libra_secure_time::{MockTimeService, TimeService};
use libra_types::{
    account_address::AccountAddress,
    account_config,
    account_config::{association_address, LBR_NAME},
    account_state::AccountState,
    block_info::BlockInfo,
    block_metadata::{BlockMetadata, LibraBlockResource},
    ledger_info::{LedgerInfo, LedgerInfoWithSignatures},
    mempool_status::{MempoolStatus, MempoolStatusCode},
    on_chain_config::{ConfigurationResource, ValidatorSet},
    transaction::{RawTransaction, Script, Transaction},
    validator_config::ValidatorConfig,
    validator_info::ValidatorInfo,
};
use libra_vm::LibraVM;
use libradb::LibraDB;
use rand::{rngs::StdRng, SeedableRng};
use std::{cell::RefCell, collections::BTreeMap, convert::TryFrom, sync::Arc, time::Duration};
use storage_interface::{DbReader, DbReaderWriter};
use tokio::runtime::Runtime;
use vm_validator::{
    mocks::mock_vm_validator::MockVMValidator, vm_validator::TransactionValidation,
};

const TXN_EXPIRATION_SECS: u64 = 100;

struct Node<T: LibraInterface> {
    account: AccountAddress,
    executor: Executor<LibraVM>,
    libra: LibraInterfaceTestHarness<T>,
    key_manager: KeyManager<
        LibraInterfaceTestHarness<T>,
        InMemoryStorageInternal<MockTimeService>,
        MockTimeService,
    >,
    time: MockTimeService,
}

impl<T: LibraInterface> Node<T> {
    pub fn new(
        account: AccountAddress,
        executor: Executor<LibraVM>,
        libra: LibraInterfaceTestHarness<T>,
        key_manager: KeyManager<
            LibraInterfaceTestHarness<T>,
            InMemoryStorageInternal<MockTimeService>,
            MockTimeService,
        >,
        time: MockTimeService,
    ) -> Self {
        Self {
            account,
            executor,
            libra,
            key_manager,
            time,
        }
    }

    // Increments the libra_timestamp on the blockchain by executing an empty block.
    fn update_libra_timestamp(&mut self) {
        self.execute_and_commit(vec![]);
    }

    fn execute_and_commit(&mut self, mut block: Vec<Transaction>) {
        // 1) Update the clock for potential reconfigurations
        self.time.increment();
        // Clock is supposed to be in microseconds
        let clock = self.time.now() * 1_000_000;

        let block_id = HashValue::zero();
        let block_metadata = BlockMetadata::new(block_id, 0, clock, vec![], self.account);
        let prologue = Transaction::BlockMetadata(block_metadata);
        block.insert(0, prologue);

        // 2) Execute the transactions
        let output = self
            .executor
            .execute_block((block_id, block), self.executor.committed_block_id())
            .unwrap();

        // 3) Produce a new LI and commit the executed blocks
        let ledger_info = LedgerInfo::new(
            BlockInfo::new(
                1,
                0,
                block_id,
                output.root_hash(),
                output.version(),
                0,
                output.epoch_state().clone(),
            ),
            HashValue::zero(),
        );
        let ledger_info_with_sigs = LedgerInfoWithSignatures::new(ledger_info, BTreeMap::new());
        self.executor
            .commit_blocks(vec![block_id], ledger_info_with_sigs)
            .unwrap();
    }
}

/// The struct below is a LibraInterface wrapper that exposes several additional methods to better
/// test the internal state of a LibraInterface implementation (e.g., during end-to-end and
/// integration tests).
#[derive(Clone)]
struct LibraInterfaceTestHarness<T: LibraInterface> {
    libra: T,
    submitted_transactions: Arc<RefCell<Vec<Transaction>>>,
}

impl<T: LibraInterface> LibraInterfaceTestHarness<T> {
    fn new(libra: T) -> Self {
        Self {
            libra,
            submitted_transactions: Arc::new(RefCell::new(Vec::new())),
        }
    }

    /// Returns the validator set associated with the validator set address.
    fn retrieve_validator_set(&self) -> Result<ValidatorSet, Error> {
        let account = account_config::validator_set_address();
        let account_state = self.libra.retrieve_account_state(account)?;
        Ok(account_state
            .get_validator_set()?
            .ok_or_else(|| Error::DataDoesNotExist("ValidatorSetResource".into()))?)
    }

    /// Returns the libra block resource associated with the association address.
    fn retrieve_libra_block_resource(&self) -> Result<LibraBlockResource, Error> {
        let account = account_config::association_address();
        let account_state = self.libra.retrieve_account_state(account)?;
        account_state
            .get_libra_block_resource()?
            .ok_or_else(|| Error::DataDoesNotExist("BlockMetadata".into()))
    }

    /// Return (and clear) all transactions that have been submitted to this interface since the
    /// last time this method was called.
    fn take_all_transactions(&self) -> Vec<Transaction> {
        self.submitted_transactions.borrow_mut().drain(..).collect()
    }
}

impl<T: LibraInterface> LibraInterface for LibraInterfaceTestHarness<T> {
    fn libra_timestamp(&self) -> Result<u64, Error> {
        self.libra.libra_timestamp()
    }

    fn last_reconfiguration(&self) -> Result<u64, Error> {
        self.libra.last_reconfiguration()
    }

    fn retrieve_sequence_number(&self, account: AccountAddress) -> Result<u64, Error> {
        self.libra.retrieve_sequence_number(account)
    }

    fn submit_transaction(&self, transaction: Transaction) -> Result<(), Error> {
        self.submitted_transactions
            .borrow_mut()
            .push(transaction.clone());
        self.libra.submit_transaction(transaction)
    }

    fn retrieve_validator_config(&self, account: AccountAddress) -> Result<ValidatorConfig, Error> {
        self.libra.retrieve_validator_config(account)
    }

    fn retrieve_validator_info(&self, account: AccountAddress) -> Result<ValidatorInfo, Error> {
        self.libra.retrieve_validator_info(account)
    }

    fn retrieve_account_state(&self, account: AccountAddress) -> Result<AccountState, Error> {
        self.libra.retrieve_account_state(account)
    }
}

/// A mock libra interface implementation that stores a pointer to the LibraDB from which to
/// process API requests.
#[derive(Clone)]
struct MockLibraInterface {
    storage: Arc<LibraDB>,
}

impl MockLibraInterface {
    fn retrieve_validator_set_resource(&self) -> Result<ValidatorSet, Error> {
        let account = account_config::validator_set_address();
        let account_state = self.retrieve_account_state(account)?;
        account_state
            .get_validator_set()?
            .ok_or_else(|| Error::DataDoesNotExist("ValidatorSet".into()))
    }

    pub fn retrieve_configuration_resource(&self) -> Result<ConfigurationResource, Error> {
        let account = libra_types::on_chain_config::config_address();
        let account_state = self.retrieve_account_state(account)?;
        account_state
            .get_configuration_resource()?
            .ok_or_else(|| Error::DataDoesNotExist("Configuration".into()))
    }
}

impl LibraInterface for MockLibraInterface {
    fn libra_timestamp(&self) -> Result<u64, Error> {
        let account = account_config::association_address();
        let blob = self
            .storage
            .get_latest_account_state(account)?
            .ok_or_else(|| Error::DataDoesNotExist("AccountState".into()))?;
        let account_state = AccountState::try_from(&blob)?;
        Ok(account_state
            .get_libra_timestamp_resource()?
            .ok_or_else(|| Error::DataDoesNotExist("LibraTimestampResource".into()))?
            .libra_timestamp
            .microseconds)
    }

    fn last_reconfiguration(&self) -> Result<u64, Error> {
        self.retrieve_configuration_resource()
            .map(|v| v.last_reconfiguration_time())
    }

    fn retrieve_sequence_number(&self, account: AccountAddress) -> Result<u64, Error> {
        let blob = self
            .storage
            .get_latest_account_state(account)?
            .ok_or_else(|| Error::DataDoesNotExist("AccountState".into()))?;
        let account_state = AccountState::try_from(&blob)?;
        Ok(account_state
            .get_account_resource()?
            .ok_or_else(|| Error::DataDoesNotExist("AccountResource".into()))?
            .sequence_number())
    }

    fn submit_transaction(&self, _transaction: Transaction) -> Result<(), Error> {
        Ok(())
    }

    fn retrieve_validator_config(&self, account: AccountAddress) -> Result<ValidatorConfig, Error> {
        let blob = self
            .storage
            .get_latest_account_state(account)?
            .ok_or_else(|| Error::DataDoesNotExist("AccountState".into()))?;
        let account_state = AccountState::try_from(&blob)?;
        Ok(account_state
            .get_validator_config_resource()?
            .ok_or_else(|| Error::DataDoesNotExist("ValidatorConfigResource".into()))?
            .validator_config
            .ok_or_else(|| {
                Error::DataDoesNotExist(format!(
                    "ValidatorConfigResource not found for account: {:?}",
                    account
                ))
            })?)
    }

    fn retrieve_validator_info(
        &self,
        validator_account: AccountAddress,
    ) -> Result<ValidatorInfo, Error> {
        self.retrieve_validator_set_resource()?
            .payload()
            .iter()
            .find(|vi| vi.account_address() == &validator_account)
            .cloned()
            .ok_or(Error::ValidatorInfoNotFound(validator_account))
    }

    fn retrieve_account_state(&self, account: AccountAddress) -> Result<AccountState, Error> {
        let blob = self
            .storage
            .get_latest_account_state(account)?
            .ok_or_else(|| Error::DataDoesNotExist("AccountState".into()))?;
        Ok(AccountState::try_from(&blob)?)
    }
}

// Creates and returns NodeConfig and KeyManagerConfig structs that are consistent for testing.
fn get_test_configs() -> (NodeConfig, KeyManagerConfig) {
    let (node_config, _) = config_builder::test_config();
    let key_manager_config = KeyManagerConfig::default();
    (node_config, key_manager_config)
}

// Returns the association private key used for testing.
fn get_test_association_key() -> Ed25519PrivateKey {
    let (_, association_key) = config_builder::test_config();
    association_key
}

// Creates and returns a test node that uses the JsonRpcLibraInterface.
// This setup is useful for testing nodes as they operate in a production environment.
fn setup_node_using_json_rpc() -> (Node<JsonRpcLibraInterface>, Runtime) {
    let (node_config, key_manager_config) = get_test_configs();

    let (_storage, db_rw) = setup_libra_db(&node_config);
    let (libra, server) = setup_libra_interface_and_json_server(db_rw.clone());
    let executor = Executor::new(db_rw);

    (
        setup_node(&node_config, &key_manager_config, executor, libra),
        server,
    )
}

// Creates and returns a Node using the MockLibraInterface implementation.
// This setup is useful for testing and verifying new development features quickly.
fn setup_node_using_test_mocks() -> Node<MockLibraInterface> {
    let (node_config, key_manager_config) = get_test_configs();
    let (storage, db_rw) = setup_libra_db(&node_config);
    let libra = MockLibraInterface { storage };
    let executor = Executor::new(db_rw);

    setup_node(&node_config, &key_manager_config, executor, libra)
}

// Creates and returns a libra database and database reader/writer pair bootstrapped with genesis.
fn setup_libra_db(config: &NodeConfig) -> (Arc<LibraDB>, DbReaderWriter) {
    let (storage, db_rw) = DbReaderWriter::wrap(LibraDB::new_for_test(&config.storage.dir()));
    db_bootstrapper::bootstrap_db_if_empty::<LibraVM>(&db_rw, get_genesis_txn(config).unwrap())
        .expect("Failed to execute genesis");

    (storage, db_rw)
}

// Creates and returns a node for testing using the given config, executor and libra interface
// wrapper implementation.
fn setup_node<T: LibraInterface + Clone>(
    node_config: &NodeConfig,
    key_manager_config: &KeyManagerConfig,
    executor: Executor<LibraVM>,
    libra: T,
) -> Node<T> {
    let time = MockTimeService::new();
    let libra_test_harness = LibraInterfaceTestHarness::new(libra);
    let storage = setup_secure_storage(&node_config, time.clone());
    let account = AccountAddress::try_from(
        storage
            .get(OPERATOR_ACCOUNT)
            .unwrap()
            .value
            .string()
            .unwrap(),
    )
    .unwrap();

    let key_manager = KeyManager::new(
        libra_test_harness.clone(),
        storage,
        time.clone(),
        key_manager_config.rotation_period_secs,
        key_manager_config.sleep_period_secs,
        key_manager_config.txn_expiration_secs,
    );

    Node::new(account, executor, libra_test_harness, key_manager, time)
}

// Creates and returns a secure storage implementation (based on an in memory storage engine) for
// testing. As part of the initialization, the consensus key is created.
fn setup_secure_storage(
    config: &NodeConfig,
    time: MockTimeService,
) -> InMemoryStorageInternal<MockTimeService> {
    let mut sec_storage = InMemoryStorageInternal::new_with_time_service(time);
    let test_config = config.clone().test.unwrap();

    let mut a_keypair = test_config.operator_keypair.unwrap();
    let a_prikey = Value::Ed25519PrivateKey(a_keypair.take_private().unwrap());
    sec_storage.set(OPERATOR_KEY, a_prikey).unwrap();

    let operator_account = libra_types::account_address::from_public_key(&a_keypair.public_key());
    sec_storage
        .set(
            OPERATOR_ACCOUNT,
            Value::String(operator_account.to_string()),
        )
        .unwrap();

    let sr_test_config = config.consensus.safety_rules.test.as_ref().unwrap();
    let c_keypair = sr_test_config.consensus_keypair.as_ref().unwrap();
    let c_prikey = c_keypair.clone().take_private().unwrap();
    sec_storage
        .set(crate::CONSENSUS_KEY, Value::Ed25519PrivateKey(c_prikey))
        .unwrap();

    sec_storage
}

// Generates and returns a (libra interface, server) pair, where the libra interface is a JSON RPC
// based interface using the lightweight JSON client internally, and the server is a JSON server
// that serves the JSON RPC requests. The server communicates with the given database reader/writer
// to handle each JSON RPC request.
fn setup_libra_interface_and_json_server(
    db_rw: DbReaderWriter,
) -> (JsonRpcLibraInterface, Runtime) {
    let address = "0.0.0.0";
    let port = utils::get_available_port();
    let host = format!("{}:{}", address, port);

    let (mp_sender, mut mp_events) = channel(1024);
    let server = libra_json_rpc::test_bootstrap(host.parse().unwrap(), db_rw.reader, mp_sender);

    // Provide a VMValidator to the runtime.
    server.spawn(async move {
        while let Some((txn, cb)) = mp_events.next().await {
            let vm_status = MockVMValidator.validate_transaction(txn).unwrap().status();
            let result = if vm_status.is_some() {
                (MempoolStatus::new(MempoolStatusCode::VmError), vm_status)
            } else {
                (MempoolStatus::new(MempoolStatusCode::Accepted), None)
            };
            cb.send(Ok(result)).unwrap();
        }
    });

    let json_rpc_endpoint = format!("http://{}", host);
    let libra = JsonRpcLibraInterface::new(json_rpc_endpoint);

    (libra, server)
}

#[test]
// This simple test just proves that genesis took effect and the values are on-chain (in storage).
fn test_ability_to_read_move_data() {
    // Test the mock libra interface implementation
    let node = setup_node_using_test_mocks();
    verify_ability_to_read_move_data(node);

    // Test the json libra interface implementation
    let (node, _runtime) = setup_node_using_json_rpc();
    verify_ability_to_read_move_data(node);
}

fn verify_ability_to_read_move_data<T: LibraInterface>(node: Node<T>) {
    node.libra.last_reconfiguration().unwrap();
    node.libra.retrieve_validator_set().unwrap();
    node.libra.retrieve_validator_config(node.account).unwrap();
    node.libra.retrieve_validator_info(node.account).unwrap();
    node.libra.retrieve_libra_block_resource().unwrap();
}

#[test]
// This tests that a manual on-chain consensus key rotation can be performed (in the event of an
// unexpected failure). To do this, the test generates a new keypair locally, creates a new rotation
// transaction manually and executes the transaction on-chain.
fn test_manual_rotation_on_chain() {
    // Test the mock libra interface implementation
    let node = setup_node_using_test_mocks();
    verify_manual_rotation_on_chain(node);

    // Test the json libra interface implementation
    let (node, _runtime) = setup_node_using_json_rpc();
    verify_manual_rotation_on_chain(node);
}

fn verify_manual_rotation_on_chain<T: LibraInterface>(mut node: Node<T>) {
    let (node_config, _) = get_test_configs();

    let test_config = node_config.test.unwrap();
    let account_prikey = test_config
        .operator_keypair
        .unwrap()
        .take_private()
        .unwrap();

    let sr_test_config = node_config.consensus.safety_rules.test.unwrap();
    let genesis_pubkey = sr_test_config
        .consensus_keypair
        .unwrap()
        .take_private()
        .unwrap()
        .public_key();

    let genesis_config = node.libra.retrieve_validator_config(node.account).unwrap();
    let genesis_info = node.libra.retrieve_validator_info(node.account).unwrap();

    // Check on-chain consensus state matches the genesis state
    assert_eq!(genesis_pubkey, genesis_config.consensus_public_key);
    assert_eq!(&genesis_pubkey, genesis_info.consensus_public_key());
    assert_eq!(&node.account, genesis_info.account_address());

    // Perform on-chain rotation
    let mut rng = StdRng::from_seed([44u8; 32]);
    let new_privkey = Ed25519PrivateKey::generate(&mut rng);
    let new_pubkey = new_privkey.public_key();
    let new_network_pubkey = x25519::PrivateKey::generate(&mut rng).public_key();
    let txn1 = crate::build_rotation_transaction(
        node.account,
        0,
        &new_pubkey,
        &new_network_pubkey,
        &RawNetworkAddress::new(Vec::new()),
        &new_network_pubkey,
        &RawNetworkAddress::new(Vec::new()),
        Duration::from_secs(node.time.now() + TXN_EXPIRATION_SECS),
    );
    let txn1 = txn1
        .sign(&account_prikey, account_prikey.public_key())
        .unwrap();
    let txn1 = Transaction::UserTransaction(txn1.into_inner());

    let association_prikey = get_test_association_key();
    let txn2 = build_reconfiguration_transaction(
        account_config::association_address(),
        1,
        &association_prikey,
        Duration::from_secs(node.time.now() + TXN_EXPIRATION_SECS),
    );

    node.execute_and_commit(vec![txn1, txn2]);

    let new_config = node.libra.retrieve_validator_config(node.account).unwrap();
    let new_info = node.libra.retrieve_validator_info(node.account).unwrap();

    // Check on-chain consensus state has been rotated
    assert_ne!(new_pubkey, genesis_pubkey);
    assert_eq!(new_pubkey, new_config.consensus_public_key);
    assert_eq!(&new_pubkey, new_info.consensus_public_key());
}

#[test]
// This verifies that the key manager is properly setup and that a basic rotation can be performed.
fn test_key_manager_init_and_basic_rotation() {
    // Test the mock libra interface implementation
    let node = setup_node_using_test_mocks();
    verify_init_and_basic_rotation(node);

    // Test the json libra interface implementation
    let (node, _runtime) = setup_node_using_json_rpc();
    verify_init_and_basic_rotation(node);
}

fn verify_init_and_basic_rotation<T: LibraInterface>(mut node: Node<T>) {
    // Verify correct initialization (on-chain and in storage)
    node.key_manager.compare_storage_to_config().unwrap();
    node.key_manager.compare_info_to_config().unwrap();
    assert_eq!(node.time.now(), node.key_manager.last_rotation().unwrap());
    // No executions yet
    assert_eq!(0, node.key_manager.last_reconfiguration().unwrap());
    assert_eq!(0, node.key_manager.libra_timestamp().unwrap());

    // Perform key rotation locally
    let genesis_info = node.libra.retrieve_validator_info(node.account).unwrap();
    let new_key = node.key_manager.rotate_consensus_key().unwrap();
    let pre_exe_rotated_info = node.libra.retrieve_validator_info(node.account).unwrap();
    assert_eq!(
        genesis_info.consensus_public_key(),
        pre_exe_rotated_info.consensus_public_key()
    );
    assert_ne!(pre_exe_rotated_info.consensus_public_key(), &new_key);

    // Execute key rotation on-chain
    submit_reconfiguration_transaction(&node);
    node.execute_and_commit(node.libra.take_all_transactions());
    let rotated_info = node.libra.retrieve_validator_info(node.account).unwrap();
    assert_ne!(
        genesis_info.consensus_public_key(),
        rotated_info.consensus_public_key()
    );
    assert_eq!(rotated_info.consensus_public_key(), &new_key);

    // Executions have occurred but nothing after our rotation
    assert_ne!(0, node.key_manager.libra_timestamp().unwrap());
    assert_eq!(
        node.key_manager.last_reconfiguration(),
        node.key_manager.libra_timestamp()
    );

    // Executions have occurred after our rotation
    node.execute_and_commit(node.libra.take_all_transactions());
    assert_ne!(
        node.key_manager.last_reconfiguration(),
        node.key_manager.libra_timestamp()
    );
}

#[test]
// This tests the application's main loop to ensure it handles basic operations and reliabilities.
// To do this, the test repeatedly calls "execute_once_and_sleep" -- identical to the main "execute"
// loop.
fn test_execute() {
    // Test the mock libra interface implementation
    let node = setup_node_using_test_mocks();
    verify_execute(node);

    // Test the json libra interface implementation
    let (node, _runtime) = setup_node_using_json_rpc();
    verify_execute(node);
}

fn verify_execute<T: LibraInterface>(mut node: Node<T>) {
    let (_, key_manager_config) = get_test_configs();

    // Verify correct initial state (i.e., nothing to be done by key manager)
    assert_eq!(0, node.time.now());
    assert_eq!(0, node.libra.last_reconfiguration().unwrap());

    // Verify rotation required after enough time
    node.time
        .increment_by(key_manager_config.rotation_period_secs);
    node.update_libra_timestamp();
    assert_eq!(
        Action::FullKeyRotation,
        node.key_manager.evaluate_status().unwrap()
    );

    // Verify a single execution iteration will perform the rotation and re-sync everything
    node.update_libra_timestamp();
    node.key_manager.execute_once().unwrap();

    // Verify nothing to be done after rotation
    node.update_libra_timestamp();
    assert_eq!(
        Action::NoAction,
        node.key_manager.evaluate_status().unwrap()
    );

    // Verify rotation transaction not executed, now expired
    node.time
        .increment_by(key_manager_config.txn_expiration_secs);
    node.update_libra_timestamp();
    assert_eq!(
        Action::SubmitKeyRotationTransaction,
        node.key_manager.evaluate_status().unwrap()
    );

    // Let's execute the expired transaction and see that a resubmission is still required
    node.execute_and_commit(node.libra.take_all_transactions());
    assert_eq!(
        Action::SubmitKeyRotationTransaction,
        node.key_manager.evaluate_status().unwrap()
    );

    // Verify that a single execution iteration will resubmit the transaction, which can then be
    // executed to re-sync everything up (on-chain).
    node.update_libra_timestamp();
    node.key_manager.execute_once().unwrap();
    submit_reconfiguration_transaction(&node);
    node.execute_and_commit(node.libra.take_all_transactions());
    assert_eq!(
        Action::NoAction,
        node.key_manager.evaluate_status().unwrap()
    );
    assert_ne!(0, node.libra.last_reconfiguration().unwrap());
}

#[test]
// This test ensures that execute() will return an error and halt the key manager if something goes
// wrong.
fn test_execute_error() {
    // Test the mock libra interface implementation
    let node = setup_node_using_test_mocks();
    verify_execute_error(node);

    // Test the json libra interface implementation
    let (node, _runtime) = setup_node_using_json_rpc();
    verify_execute_error(node);
}

fn verify_execute_error<T: LibraInterface>(mut node: Node<T>) {
    // Verify some correct initial state
    assert_eq!(0, node.time.now());
    assert_eq!(0, node.libra.last_reconfiguration().unwrap());

    // Verify nothing to be done by key manager
    node.update_libra_timestamp();
    assert_eq!(
        Action::NoAction,
        node.key_manager.evaluate_status().unwrap()
    );

    // Perform each execution iteration a few times to see that everything is working
    for _ in 0..5 {
        node.update_libra_timestamp();
        node.key_manager.execute_once().unwrap();
    }

    // Delete all keys in secure storage to emulate a failure (e.g., so that the key manager should
    // fail when trying to access something in secure storage on the next execution iteration.)
    node.key_manager.storage.reset_and_clear().unwrap();

    // Check that execute() now returns an error and doesn't spin forever.
    node.update_libra_timestamp();
    assert!(node.key_manager.execute().is_err());
}

// Creates and submits a reconfiguration transaction to the given libra interface.
fn submit_reconfiguration_transaction<T: LibraInterface>(node: &Node<T>) {
    let association_prikey = get_test_association_key();
    let association_account = association_address();
    let seq_id = node
        .libra
        .retrieve_sequence_number(association_account)
        .unwrap();
    let expiration = Duration::from_secs(node.time.now() + TXN_EXPIRATION_SECS);

    let txn = build_reconfiguration_transaction(
        association_account,
        seq_id,
        &association_prikey,
        expiration,
    );
    node.libra.submit_transaction(txn).unwrap();
}

fn build_reconfiguration_transaction(
    sender: AccountAddress,
    seq_id: u64,
    signing_key: &Ed25519PrivateKey,
    expiration: Duration,
) -> Transaction {
    let script = Script::new(
        libra_transaction_scripts::RECONFIGURE_TXN.clone(),
        vec![],
        vec![],
    );
    let raw_txn = RawTransaction::new_script(
        sender,
        seq_id,
        script,
        MAX_GAS_AMOUNT,
        GAS_UNIT_PRICE,
        LBR_NAME.to_owned(),
        expiration,
    );
    let signed_txn = raw_txn.sign(signing_key, signing_key.public_key()).unwrap();
    Transaction::UserTransaction(signed_txn.into_inner())
}
