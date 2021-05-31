pub mod host_env;
mod memory;

use std::collections::HashSet;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::slice;

use anoma_shared::types::{Address, Key};
use anoma_shared::vm_memory::{TxInput, VpInput};
use parity_wasm::elements;
use pwasm_utils::{self, rules};
use thiserror::Error;
use tokio::sync::mpsc::Sender;
use wasmer::Instance;
use wasmparser::{Validator, WasmFeatures};

use self::host_env::prefix_iter::PrefixIterators;
use self::host_env::write_log::WriteLog;
use self::host_env::VpEnv;
use crate::node::shell::gas::{BlockGasMeter, VpGasMeter};
use crate::node::shell::storage::{self, Storage};
use crate::types::MatchmakerMessage;

const TX_ENTRYPOINT: &str = "_apply_tx";
const VP_ENTRYPOINT: &str = "_validate_tx";
const MATCHMAKER_ENTRYPOINT: &str = "_match_intent";
const FILTER_ENTRYPOINT: &str = "_validate_intent";
const WASM_STACK_LIMIT: u32 = u16::MAX as u32;

/// This is used to attach the Ledger's host structures to wasm environment,
/// which is used for implementing some host calls. It wraps an immutable
/// reference, so the access is thread-safe, but because of the unsafe
/// reference conversion, care must be taken that while this reference is
/// borrowed, no other process can modify it.
#[derive(Clone)]
pub struct EnvHostWrapper<'a, T: 'a> {
    data: *const c_void,
    phantom: PhantomData<&'a T>,
}
unsafe impl<T> Send for EnvHostWrapper<'_, T> {}
unsafe impl<T> Sync for EnvHostWrapper<'_, T> {}

impl<'a, T: 'a> EnvHostWrapper<'a, &T> {
    /// Wrap a reference for VM environment.
    ///
    /// # Safety
    ///
    /// Because this is unsafe, care must be taken that while this reference
    /// is borrowed, no other process can modify it.
    unsafe fn new(host_structure: &T) -> Self {
        Self {
            data: host_structure as *const T as *const c_void,
            phantom: PhantomData,
        }
    }

    /// Get a reference from VM environment.
    ///
    /// # Safety
    ///
    /// Because this is unsafe, care must be taken that while this reference
    /// is borrowed, no other process can modify it.
    unsafe fn get(&self) -> &'a T {
        &*(self.data as *const T)
    }
}

/// This is used to attach the Ledger's host structures to wasm environment,
/// which is used for implementing some host calls. It wraps an immutable
/// slice, so the access is thread-safe, but because of the unsafe slice
/// conversion, care must be taken that while this slice is borrowed, no other
/// process can modify it.
#[derive(Clone)]
pub struct EnvHostSliceWrapper<'a, T: 'a> {
    data: *const c_void,
    len: usize,
    phantom: PhantomData<&'a T>,
}
unsafe impl<T> Send for EnvHostSliceWrapper<'_, T> {}
unsafe impl<T> Sync for EnvHostSliceWrapper<'_, T> {}

impl<'a, T: 'a> EnvHostSliceWrapper<'a, &[T]> {
    /// Wrap a slice for VM environment.
    ///
    /// # Safety
    ///
    /// Because this is unsafe, care must be taken that while this slice is
    /// borrowed, no other process can modify it.
    unsafe fn new(host_structure: &[T]) -> Self {
        Self {
            data: host_structure as *const [T] as *const c_void,
            len: host_structure.len(),
            phantom: PhantomData,
        }
    }

    /// Get a slice from VM environment.
    ///
    /// # Safety
    ///
    /// Because this is unsafe, care must be taken that while this slice is
    /// borrowed, no other process can modify it.
    pub unsafe fn get(&self) -> &'a [T] {
        slice::from_raw_parts(self.data as *const T, self.len)
    }
}

/// This is used to attach the Ledger's host structures to wasm environment,
/// which is used for implementing some host calls. Because it's mutable, it's
/// not thread-safe. Also, care must be taken that while this reference is
/// borrowed, no other process can read or modify it.
#[derive(Clone)]
pub struct MutEnvHostWrapper<'a, T: 'a> {
    data: *mut c_void,
    phantom: PhantomData<&'a T>,
}
unsafe impl<T> Send for MutEnvHostWrapper<'_, T> {}
unsafe impl<T> Sync for MutEnvHostWrapper<'_, T> {}

impl<'a, T: 'a> MutEnvHostWrapper<'a, &T> {
    /// Wrap a mutable reference for VM environment.
    ///
    /// # Safety
    ///
    /// This is not thread-safe. Also, because this is unsafe, care must be
    /// taken that while this reference is borrowed, no other process can read
    /// or modify it.
    unsafe fn new(host_structure: &mut T) -> Self {
        Self {
            data: host_structure as *mut T as *mut c_void,
            phantom: PhantomData,
        }
    }

    /// Get a mutable reference from VM environment.
    ///
    /// # Safety
    ///
    /// This is not thread-safe. Also, because this is unsafe, care must be
    /// taken that while this reference is borrowed, no other process can read
    /// or modify it.
    unsafe fn get(&self) -> &'a mut T {
        &mut *(self.data as *mut T)
    }
}

/// This is used to attach the Ledger's host structures to wasm environment,
/// which is used for implementing some host calls. It wraps an mutable
/// slice, so the access is thread-safe, but because of the unsafe slice
/// conversion, care must be taken that while this slice is borrowed, no other
/// process can modify it.
#[derive(Clone)]
pub struct MutEnvHostSliceWrapper<'a, T: 'a> {
    data: *mut c_void,
    len: usize,
    phantom: PhantomData<&'a T>,
}
unsafe impl<T> Send for MutEnvHostSliceWrapper<'_, T> {}
unsafe impl<T> Sync for MutEnvHostSliceWrapper<'_, T> {}

impl<'a, T: 'a> MutEnvHostSliceWrapper<'a, &[T]> {
    /// Wrap a slice for VM environment.
    ///
    /// # Safety
    ///
    /// Because this is unsafe, care must be taken that while this slice is
    /// borrowed, no other process can modify it.
    #[allow(dead_code)]
    unsafe fn new(host_structure: &mut [T]) -> Self {
        Self {
            data: host_structure as *mut [T] as *mut c_void,
            len: host_structure.len(),
            phantom: PhantomData,
        }
    }

    /// Get a slice from VM environment.
    ///
    /// # Safety
    ///
    /// Because this is unsafe, care must be taken that while this slice is
    /// borrowed, no other process can modify it.
    pub unsafe fn get(&self) -> &'a mut [T] {
        slice::from_raw_parts_mut(self.data as *mut T, self.len)
    }
}

#[derive(Clone, Debug)]
pub struct TxRunner {
    wasm_store: wasmer::Store,
}

#[derive(Error, Debug)]
pub enum Error {
    // 1. Common error types
    #[error("Memory error: {0}")]
    MemoryError(memory::Error),
    #[error("Unable to inject gas meter")]
    StackLimiterInjection,
    #[error("Wasm deserialization error: {0}")]
    DeserializationError(elements::Error),
    #[error("Wasm serialization error: {0}")]
    SerializationError(elements::Error),
    #[error("Unable to inject gas meter")]
    GasMeterInjection,
    #[error("Wasm compilation error: {0}")]
    CompileError(wasmer::CompileError),
    #[error("Missing wasm memory export, failed with: {0}")]
    MissingModuleMemory(wasmer::ExportError),
    #[error("Missing wasm entrypoint: {0}")]
    MissingModuleEntrypoint(wasmer::ExportError),
    #[error("Failed running wasm with: {0}")]
    RuntimeError(wasmer::RuntimeError),
    #[error("Failed instantiating wasm module with: {0}")]
    InstantiationError(wasmer::InstantiationError),
    #[error(
        "Unexpected module entrypoint interface {entrypoint}, failed with: \
         {error}"
    )]
    UnexpectedModuleEntrypointInterface {
        entrypoint: &'static str,
        error: wasmer::RuntimeError,
    },
    #[error("Wasm validation error: {0}")]
    ValidationError(wasmparser::BinaryReaderError),
}

pub type Result<T> = std::result::Result<T, Error>;

impl TxRunner {
    /// TODO remove the `new`, it's not very useful
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        // Use Singlepass compiler with the default settings
        let compiler = wasmer_compiler_singlepass::Singlepass::default();
        // TODO Could we pass the modified accounts sub-spaces via WASM store
        // directly to VPs' wasm scripts to avoid passing it through the
        // host?
        let wasm_store =
            wasmer::Store::new(&wasmer_engine_jit::JIT::new(compiler).engine());
        Self { wasm_store }
    }

    /// Execute a transaction code. Returns verifiers requested by the
    /// transaction.
    pub fn run<DB>(
        &self,
        storage: &Storage<DB>,
        write_log: &mut WriteLog,
        gas_meter: &mut BlockGasMeter,
        tx_code: Vec<u8>,
        tx_data: Vec<u8>,
    ) -> Result<HashSet<Address>>
    where
        DB: 'static + storage::DB + for<'iter> storage::DBIter<'iter>,
    {
        validate_untrusted_wasm(&tx_code)?;

        // This is not thread-safe, we're assuming single-threaded Tx runner.
        let storage: EnvHostWrapper<'_, &Storage<DB>> =
            unsafe { EnvHostWrapper::new(storage) };
        // This is also not thread-safe, we're assuming single-threaded Tx
        // runner.
        let write_log = unsafe { MutEnvHostWrapper::new(write_log) };
        // This is also not thread-safe, we're assuming single-threaded Tx
        // runner.
        let mut iterators: PrefixIterators<'_, DB> = PrefixIterators::new();
        let iterators = unsafe { MutEnvHostWrapper::new(&mut iterators) };
        let mut verifiers = HashSet::new();
        // This is also not thread-safe, we're assuming single-threaded Tx
        // runner.
        let env_verifiers = unsafe { MutEnvHostWrapper::new(&mut verifiers) };
        // This is also not thread-safe, we're assuming single-threaded Tx
        // runner.
        let gas_meter = unsafe { MutEnvHostWrapper::new(gas_meter) };

        let tx_code = prepare_wasm_code(&tx_code)?;

        let tx_module = wasmer::Module::new(&self.wasm_store, &tx_code)
            .map_err(Error::CompileError)?;
        let initial_memory = memory::prepare_tx_memory(&self.wasm_store)
            .map_err(Error::MemoryError)?;
        let tx_imports = host_env::prepare_tx_imports(
            &self.wasm_store,
            storage,
            write_log,
            iterators,
            env_verifiers,
            gas_meter,
            initial_memory,
        );

        // compile and run the transaction wasm code
        let tx_code = wasmer::Instance::new(&tx_module, &tx_imports)
            .map_err(Error::InstantiationError)?;
        Self::run_with_input(tx_code, tx_data)?;
        Ok(verifiers)
    }

    fn run_with_input(tx_code: Instance, tx_data: TxInput) -> Result<()> {
        // We need to write the inputs in the memory exported from the wasm
        // module
        let memory = tx_code
            .exports
            .get_memory("memory")
            .map_err(Error::MissingModuleMemory)?;
        let memory::TxCallInput {
            tx_data_ptr,
            tx_data_len,
        } = memory::write_tx_inputs(memory, tx_data)
            .map_err(Error::MemoryError)?;

        // Get the module's entrypoint to be called
        let apply_tx = tx_code
            .exports
            .get_function(TX_ENTRYPOINT)
            .map_err(Error::MissingModuleEntrypoint)?
            .native::<(u64, u64), ()>()
            .map_err(|error| Error::UnexpectedModuleEntrypointInterface {
                entrypoint: TX_ENTRYPOINT,
                error,
            })?;
        apply_tx
            .call(tx_data_ptr, tx_data_len)
            .map_err(Error::RuntimeError)
    }
}

#[derive(Clone, Debug)]
pub struct VpRunner {
    wasm_store: wasmer::Store,
}

impl VpRunner {
    /// TODO remove the `new`, it's not very useful
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        // Use Singlepass compiler with the default settings
        let compiler = wasmer_compiler_singlepass::Singlepass::default();
        // TODO: Maybe refactor wasm_store: not necessary to do in two steps
        let wasm_store =
            wasmer::Store::new(&wasmer_engine_jit::JIT::new(compiler).engine());
        Self { wasm_store }
    }

    // TODO consider using a wrapper object for all the host env references
    #[allow(clippy::too_many_arguments)]
    pub fn run<DB>(
        &self,
        vp_code: impl AsRef<[u8]>,
        tx_data: impl AsRef<[u8]>,
        tx_code: impl AsRef<[u8]>,
        addr: &Address,
        storage: &Storage<DB>,
        write_log: &WriteLog,
        vp_gas_meter: &mut VpGasMeter,
        storage_keys: &[Key],
        verifiers: &HashSet<Address>,
    ) -> Result<bool>
    where
        DB: 'static + storage::DB + for<'iter> storage::DBIter<'iter>,
    {
        validate_untrusted_wasm(vp_code.as_ref())?;

        // Read-only access from parallel Vp runners
        let storage: EnvHostWrapper<&Storage<DB>> =
            unsafe { EnvHostWrapper::new(storage) };
        // Read-only access from parallel Vp runners
        let write_log = unsafe { EnvHostWrapper::new(write_log) };
        // Read-only access from parallel Vp runners
        let tx_code = unsafe { EnvHostSliceWrapper::new(tx_code.as_ref()) };
        // This is not thread-safe, but because each VP has its own instance
        // there is no shared access
        let mut iterators: PrefixIterators<'_, DB> = PrefixIterators::new();
        let iterators = unsafe { MutEnvHostWrapper::new(&mut iterators) };
        // This is not thread-safe, but because each VP has its own instance
        // there is no shared access
        let gas_meter = unsafe { MutEnvHostWrapper::new(vp_gas_meter) };
        // Read-only access from parallel Vp runners
        let env_storage_keys =
            unsafe { EnvHostSliceWrapper::new(storage_keys) };
        // Read-only access from parallel Vp runners
        let env_verifiers = unsafe { EnvHostWrapper::new(verifiers) };

        let vp_code = prepare_wasm_code(vp_code)?;

        let vp_module = wasmer::Module::new(&self.wasm_store, &vp_code)
            .map_err(Error::CompileError)?;
        let initial_memory = memory::prepare_vp_memory(&self.wasm_store)
            .map_err(Error::MemoryError)?;
        let input: VpInput = VpInput {
            addr: &addr,
            data: tx_data.as_ref(),
            keys_changed: storage_keys,
            verifiers,
        };
        let vp_imports = host_env::prepare_vp_env(
            &self.wasm_store,
            addr.clone(),
            storage,
            write_log,
            iterators,
            gas_meter,
            tx_code,
            initial_memory,
            env_storage_keys,
            env_verifiers,
        );

        // compile and run the transaction wasm code
        let vp_instance = wasmer::Instance::new(&vp_module, &vp_imports)
            .map_err(Error::InstantiationError)?;
        VpRunner::run_with_input(vp_instance, input)
    }

    fn run_eval<DB>(
        &self,
        // we read the validity predicate from wasm memory as bytes
        vp_code: Vec<u8>,
        input_data: &[u8],
        vp_env: VpEnv<'static, DB>,
    ) -> Result<bool>
    where
        DB: 'static + storage::DB + for<'iter> storage::DBIter<'iter>,
    {
        let vp_code = prepare_wasm_code(&vp_code)?;
        let vp_module = wasmer::Module::new(&self.wasm_store, &vp_code)
            .map_err(Error::CompileError)?;
        let initial_memory = memory::prepare_vp_memory(&self.wasm_store)
            .map_err(Error::MemoryError)?;

        let keys_changed = unsafe { &*(vp_env.keys_changed.get()) };
        let verifiers = unsafe { &*(vp_env.verifiers.get()) };
        let input: VpInput = VpInput {
            addr: &vp_env.addr,
            data: input_data,
            keys_changed,
            verifiers,
        };

        let vp_imports = host_env::prepare_vp_imports(
            &self.wasm_store,
            initial_memory,
            &vp_env,
        );

        // compile and run the transaction wasm code
        let vp_instance = wasmer::Instance::new(&vp_module, &vp_imports)
            .map_err(Error::InstantiationError)?;
        VpRunner::run_with_input(vp_instance, input)
    }

    fn run_with_input(vp_code: Instance, input: VpInput) -> Result<bool> {
        // We need to write the inputs in the memory exported from the wasm
        // module
        let memory = vp_code
            .exports
            .get_memory("memory")
            .map_err(Error::MissingModuleMemory)?;
        let memory::VpCallInput {
            addr_ptr,
            addr_len,
            data_ptr,
            data_len,
            keys_changed_ptr,
            keys_changed_len,
            verifiers_ptr,
            verifiers_len,
        } = memory::write_vp_inputs(memory, input)
            .map_err(Error::MemoryError)?;

        // Get the module's entrypoint to be called
        let validate_tx = vp_code
            .exports
            .get_function(VP_ENTRYPOINT)
            .map_err(Error::MissingModuleEntrypoint)?
            .native::<(u64, u64, u64, u64, u64, u64, u64, u64), u64>()
            .map_err(|error| Error::UnexpectedModuleEntrypointInterface {
                entrypoint: VP_ENTRYPOINT,
                error,
            })?;
        let is_valid = validate_tx
            .call(
                addr_ptr,
                addr_len,
                data_ptr,
                data_len,
                keys_changed_ptr,
                keys_changed_len,
                verifiers_ptr,
                verifiers_len,
            )
            .map_err(Error::RuntimeError)?;
        tracing::debug!("is_valid {}", is_valid);
        Ok(is_valid == 1)
    }
}

#[derive(Clone, Debug)]
pub struct MatchmakerRunner {
    wasm_store: wasmer::Store,
}

impl MatchmakerRunner {
    /// TODO remove the `new`, it's not very useful
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        // TODO for the matchmaker we could use a compiler that does more
        // optimisation.
        let compiler = wasmer_compiler_singlepass::Singlepass::default();
        let wasm_store =
            wasmer::Store::new(&wasmer_engine_jit::JIT::new(compiler).engine());
        Self { wasm_store }
    }

    pub fn run(
        &self,
        matchmaker_code: impl AsRef<[u8]>,
        data: impl AsRef<[u8]>,
        intent_id: impl AsRef<[u8]>,
        intent_data: impl AsRef<[u8]>,
        tx_code: impl AsRef<[u8]>,
        inject_mm_message: Sender<MatchmakerMessage>,
    ) -> Result<bool> {
        let matchmaker_module: wasmer::Module =
            wasmer::Module::new(&self.wasm_store, &matchmaker_code)
                .map_err(Error::CompileError)?;

        let initial_memory =
            memory::prepare_matchmaker_memory(&self.wasm_store)
                .map_err(Error::MemoryError)?;

        let matchmaker_imports = host_env::prepare_matchmaker_imports(
            &self.wasm_store,
            initial_memory,
            tx_code,
            inject_mm_message,
        );

        // compile and run the matchmaker wasm code
        let matchmaker_code =
            wasmer::Instance::new(&matchmaker_module, &matchmaker_imports)
                .map_err(Error::InstantiationError)?;

        Self::run_with_input(&matchmaker_code, data, intent_id, intent_data)
    }

    fn run_with_input(
        code: &Instance,
        data: impl AsRef<[u8]>,
        intent_id: impl AsRef<[u8]>,
        intent_data: impl AsRef<[u8]>,
    ) -> Result<bool> {
        let memory = code
            .exports
            .get_memory("memory")
            .map_err(Error::MissingModuleMemory)?;
        let memory::MatchmakerCallInput {
            data_ptr,
            data_len,
            intent_id_ptr,
            intent_id_len,
            intent_data_ptr,
            intent_data_len,
        }: memory::MatchmakerCallInput = memory::write_matchmaker_inputs(
            &memory,
            data,
            intent_id,
            intent_data,
        )
        .map_err(Error::MemoryError)?;
        let apply_matchmaker = code
            .exports
            .get_function(MATCHMAKER_ENTRYPOINT)
            .map_err(Error::MissingModuleEntrypoint)?
            .native::<(u64, u64, u64, u64, u64, u64), u64>()
            .map_err(|error| Error::UnexpectedModuleEntrypointInterface {
                entrypoint: MATCHMAKER_ENTRYPOINT,
                error,
            })?;
        let found_match = apply_matchmaker
            .call(
                data_ptr,
                data_len,
                intent_id_ptr,
                intent_id_len,
                intent_data_ptr,
                intent_data_len,
            )
            .map_err(Error::RuntimeError)?;
        Ok(found_match == 0)
    }
}

#[derive(Clone, Debug)]
pub struct FilterRunner {
    wasm_store: wasmer::Store,
}

impl FilterRunner {
    /// TODO remove the `new`, it's not very useful
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        // TODO replace to use a better compiler because this program is local
        let compiler = wasmer_compiler_singlepass::Singlepass::default();
        let wasm_store =
            wasmer::Store::new(&wasmer_engine_jit::JIT::new(compiler).engine());
        Self { wasm_store }
    }

    pub fn run(
        &self,
        code: impl AsRef<[u8]>,
        intent_data: impl AsRef<[u8]>,
    ) -> Result<bool> {
        validate_untrusted_wasm(code.as_ref())?;
        let code = prepare_wasm_code(code)?;
        let filter_module: wasmer::Module =
            wasmer::Module::new(&self.wasm_store, &code)
                .map_err(Error::CompileError)?;
        let initial_memory = memory::prepare_filter_memory(&self.wasm_store)
            .map_err(Error::MemoryError)?;

        let filter_imports =
            host_env::prepare_filter_imports(&self.wasm_store, initial_memory);
        let filter_code =
            wasmer::Instance::new(&filter_module, &filter_imports)
                .map_err(Error::InstantiationError)?;

        Self::run_with_input(&filter_code, intent_data)
    }

    fn run_with_input(
        code: &Instance,
        intent_data: impl AsRef<[u8]>,
    ) -> Result<bool> {
        let memory = code
            .exports
            .get_memory("memory")
            .map_err(Error::MissingModuleMemory)?;
        let memory::FilterCallInput {
            intent_data_ptr,
            intent_data_len,
        }: memory::FilterCallInput =
            memory::write_filter_inputs(&memory, intent_data)
                .map_err(Error::MemoryError)?;
        let apply_filter = code
            .exports
            .get_function(FILTER_ENTRYPOINT)
            .map_err(Error::MissingModuleEntrypoint)?
            .native::<(u64, u64), u64>()
            .map_err(|error| Error::UnexpectedModuleEntrypointInterface {
                entrypoint: FILTER_ENTRYPOINT,
                error,
            })?;
        let found_match = apply_filter
            .call(intent_data_ptr, intent_data_len)
            .map_err(Error::RuntimeError)?;
        Ok(found_match == 0)
    }
}

/// Inject gas counter and stack-height limiter into the given wasm code
fn prepare_wasm_code<T: AsRef<[u8]>>(code: T) -> Result<Vec<u8>> {
    let module: elements::Module = elements::deserialize_buffer(code.as_ref())
        .map_err(Error::DeserializationError)?;
    let module =
        pwasm_utils::inject_gas_counter(module, &get_gas_rules(), "env")
            .map_err(|_original_module| Error::GasMeterInjection)?;
    let module =
        pwasm_utils::stack_height::inject_limiter(module, WASM_STACK_LIMIT)
            .map_err(|_original_module| Error::StackLimiterInjection)?;
    elements::serialize(module).map_err(Error::SerializationError)
}

/// Get the gas rules used to meter wasm operations
fn get_gas_rules() -> rules::Set {
    rules::Set::default().with_grow_cost(1)
}

/// Validate an untrusted wasm code with restrictions that we place such code
/// (e.g. transaction and validity predicates)
pub fn validate_untrusted_wasm(wasm_code: impl AsRef<[u8]>) -> Result<()> {
    let mut validator = Validator::new();

    let features = WasmFeatures {
        reference_types: false,
        multi_value: false,
        bulk_memory: false,
        module_linking: false,
        simd: false,
        threads: false,
        tail_call: false,
        deterministic_only: true,
        multi_memory: false,
        exceptions: false,
        memory64: false,
    };
    validator.wasm_features(features);

    validator
        .validate_all(wasm_code.as_ref())
        .map_err(Error::ValidationError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::shell::storage::TestStorage;

    /// Test that when a transaction wasm goes over the stack-height limit, the
    /// execution is aborted.
    #[test]
    fn test_tx_stack_limiter() {
        // Because each call into `$loop` inside the wasm consumes 4 stack
        // heights, this should trigger stack limiter. If we were to subtract
        // one from this value, we should be just under the limit.
        let loops = WASM_STACK_LIMIT / 4;
        // A transaction with a recursive loop.
        // The boilerplate code is generated from tx.wasm using `wasm2wat` and
        // the loop code is hand-written.
        let tx_code = wasmer::wat2wasm(
            format!(
                r#"
            (module
                (type (;0;) (func (param i64 i64) (result i64)))

                ;; recursive loop, the param is the number of loops
                (func $loop (param i64) (result i64)
                (if
                (result i64)
                (i64.eqz (get_local 0))
                (then (get_local 0))
                (else (call $loop (i64.sub (get_local 0) (i64.const 1))))))

                (func $apply_tx (type 0) (param i64 i64) (result i64)
                (call $loop (i64.const {})))

                (table (;0;) 1 1 funcref)
                (memory (;0;) 16)
                (global (;0;) (mut i32) (i32.const 1048576))
                (export "memory" (memory 0))
                (export "apply_tx" (func $apply_tx)))
            "#,
                loops
            )
            .as_bytes(),
        )
        .expect("unexpected error converting wat2wasm")
        .into_owned();

        let runner = TxRunner::new();
        let tx_data = vec![];
        let mut storage = TestStorage::default();
        let mut write_log = WriteLog::new();
        let mut gas_meter = BlockGasMeter::default();
        let error = runner
            .run(
                &mut storage,
                &mut write_log,
                &mut gas_meter,
                tx_code,
                tx_data,
            )
            .expect_err(
                "Expecting runtime error \"unreachable\" caused by \
                 stack-height overflow",
            );
        if let Error::RuntimeError(err) = &error {
            if let Some(trap_code) = err.clone().to_trap() {
                return assert_eq!(
                    trap_code,
                    wasmer_vm::TrapCode::UnreachableCodeReached
                );
            }
        }
        println!("Failed with unexpected error: {}", error);
    }

    /// Test that when a VP wasm goes over the stack-height limit, the execution
    /// is aborted.
    #[test]
    fn test_vp_stack_limiter() {
        // Because each call into `$loop` inside the wasm consumes 4 stack
        // heights, this should trigger stack limiter. If we were to subtract
        // one from this value, we should be just under the limit.
        let loops = WASM_STACK_LIMIT / 4;
        // A validity predicate with a recursive loop.
        // The boilerplate code is generated from vp.wasm using `wasm2wat` and
        // the loop code is hand-written.
        let vp_code = wasmer::wat2wasm(format!(
            r#"
            (module
                (type (;0;) (func (param i64 i64 i64 i64 i64 i64) (result i64)))

                ;; recursive loop, the param is the number of loops
                (func $loop (param i64) (result i64)
                (if
                (result i64)
                (i64.eqz (get_local 0))
                (then (get_local 0))
                (else (call $loop (i64.sub (get_local 0) (i64.const 1))))))

                (func $validate_tx (type 0) (param i64 i64 i64 i64 i64 i64) (result i64)
                (call $loop (i64.const {})))

                (table (;0;) 1 1 funcref)
                (memory (;0;) 16)
                (global (;0;) (mut i32) (i32.const 1048576))
                (export "memory" (memory 0))
                (export "validate_tx" (func $validate_tx)))
            "#, loops).as_bytes(),
        )
        .expect("unexpected error converting wat2wasm").into_owned();

        let runner = VpRunner::new();
        let tx_data = vec![];
        let tx_code = vec![];
        let mut storage = TestStorage::default();
        let addr = storage.address_gen.generate_address("rng seed");
        let write_log = WriteLog::new();
        let mut gas_meter = VpGasMeter::new(0);
        let keys_changed = vec![];
        let verifiers = HashSet::new();
        let error = runner
            .run(
                vp_code,
                tx_data,
                &tx_code,
                &addr,
                &storage,
                &write_log,
                &mut gas_meter,
                &keys_changed[..],
                &verifiers,
            )
            .expect_err(
                "Expecting runtime error \"unreachable\" caused by \
                 stack-height overflow",
            );
        if let Error::RuntimeError(err) = &error {
            if let Some(trap_code) = err.clone().to_trap() {
                return assert_eq!(
                    trap_code,
                    wasmer_vm::TrapCode::UnreachableCodeReached
                );
            }
        }
        println!("Failed with unexpected error: {}", error);
    }
}